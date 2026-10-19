//! `GET /api/entries/{id}/discography` — what an artist page lists: the
//! releases and tracks the library ties to the artist, gathered from two
//! kinds of edge on any of the artist's pairs (across its soft-identity
//! component):
//!
//! - `entry_child` — a provider's own listing under the artist (a Spotify
//!   artist's albums, a MusicBrainz artist's release groups, a YouTube
//!   channel's playlists);
//! - `contribution` — a release or track crediting the artist (listed
//!   artist, uploader, vocal, lyrics, ...).
//!
//! Releases fold into their release group when one is in the library (a
//! MusicBrainz release under its release group, a Discogs release under its
//! master), so an album shows once, not once per pressing. Entries in one
//! edition group (`musicdb::EDITION_KINDS`) fold into one row too — but
//! only among *this artist's* entries: an artist's cover of a song stays
//! their own row, never swapped for another artist's original the way
//! search's `dedupe_by_edition` would.

use std::collections::{BTreeMap, HashMap, HashSet};

use musiclib_rs::musicdb::{EditionProjection, MusicDb, SoftIdentityProjection, SourceRow};
use serde::Serialize;

use crate::{
    entries::{component_members, entry_titles_with},
    player::embed_for,
};

type Pair = (String, String);

#[derive(Serialize)]
pub struct Discography {
    pub releases: Vec<ReleaseRow>,
    pub tracks: Vec<TrackRow>,
}

#[derive(Serialize)]
pub struct ReleaseRow {
    pub entry_id: i64,
    pub title: Option<String>,
    /// `album`, `ep`, `single`, `compilation`, `broadcast`, `playlist` or
    /// `other` — see `release_kind`.
    pub kind: &'static str,
    /// False when the release credits other main artists but not this one
    /// (a compilation or split the artist appears on).
    pub main: bool,
    pub release_date: Option<String>,
    /// The longest tracklist any one provider lists for it; `None` when no
    /// provider's tracklist is in the library.
    pub track_count: Option<usize>,
    pub source_kinds: Vec<String>,
    /// How many library entries this row stands for (pressings, soft links,
    /// editions), itself included.
    pub entries: usize,
    /// Other entries folded into this row by title alone (`fold_by_title`):
    /// most likely the same release, but not linked in the library.
    pub also: Vec<i64>,
}

#[derive(Serialize)]
pub struct TrackRow {
    pub entry_id: i64,
    pub title: Option<String>,
    /// False when the track credits the artist only in a non-main role
    /// (`roles`), e.g. as a featured vocalist or lyricist.
    pub main: bool,
    /// The artist's non-main roles on the track, if any.
    pub roles: Vec<String>,
    pub release_date: Option<String>,
    pub playable: bool,
    /// Counted plays (`MIN_COUNTED_PLAY_SECONDS`) across the row's entries.
    pub plays: i64,
    pub source_kinds: Vec<String>,
}

/// A release's kind, from the most specific data its sources carry:
/// MusicBrainz's release-group primary type, then a release's own type
/// (Spotify's `album_type`), then the length of its tracklist. YouTube
/// calls every playlist a `playlist` — an album's too — so that only counts
/// when nothing else names a kind. Discogs' `release_type` is a moderation
/// status (`Accepted`), not a kind, and falls through.
pub fn release_kind<'a>(
    primary_types: impl IntoIterator<Item = &'a str>,
    release_types: impl IntoIterator<Item = &'a str>,
    track_count: Option<usize>,
) -> &'static str {
    fn known(t: &str) -> Option<&'static str> {
        Some(match t.to_ascii_lowercase().as_str() {
            "album" => "album",
            "ep" => "ep",
            "single" => "single",
            "compilation" => "compilation",
            "broadcast" => "broadcast",
            "playlist" => "playlist",
            "other" => "other",
            _ => return None,
        })
    }
    if let Some(kind) = primary_types.into_iter().find_map(known) {
        return kind;
    }
    let release_kinds: Vec<&'static str> = release_types.into_iter().filter_map(known).collect();
    if let Some(kind) = release_kinds.iter().find(|&&k| k != "playlist") {
        return kind;
    }
    if !release_kinds.is_empty() {
        return "playlist";
    }
    match track_count {
        Some(0) | None => "other",
        Some(1..=3) => "single",
        Some(4..=6) => "ep",
        Some(_) => "album",
    }
}

/// Entries folded into one row: `rep` is the id the row links to.
struct Group {
    rep: i64,
    entries: Vec<i64>,
    /// Soft-identity canonicals of `entries`, for attributing plays.
    components: HashSet<i64>,
}

/// Buckets `candidates` by edition group. Each candidate brings along
/// `extra[candidate]` (releases folded under a release group) and every
/// member of its own soft-identity component. The row links to the group's
/// default edition when that is one of the candidates, else to the lowest
/// candidate id.
fn group_by_edition(
    candidates: &[i64],
    extra: &HashMap<i64, Vec<i64>>,
    identity: &SoftIdentityProjection,
    editions: &EditionProjection,
) -> Vec<Group> {
    let mut buckets: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for &id in candidates {
        let canonical = identity.component_of(id).unwrap_or(id);
        buckets
            .entry(editions.group_of(canonical))
            .or_default()
            .push(id);
    }
    buckets
        .into_values()
        .map(|ids| {
            let canonicals: Vec<i64> = ids
                .iter()
                .map(|&id| identity.component_of(id).unwrap_or(id))
                .collect();
            let rep = canonicals
                .iter()
                .copied()
                .find(|&c| editions.default_member_of(c) == c)
                .or_else(|| canonicals.iter().copied().min())
                .expect("bucket is non-empty");
            let mut entries: Vec<i64> = Vec::new();
            let mut components = HashSet::new();
            for &id in ids.iter().chain(
                ids.iter()
                    .flat_map(|id| extra.get(id).into_iter().flatten()),
            ) {
                let canonical = identity.component_of(id).unwrap_or(id);
                components.insert(canonical);
                let members = identity
                    .members_by_component
                    .get(&canonical)
                    .cloned()
                    .unwrap_or_else(|| vec![id]);
                for m in members {
                    if !entries.contains(&m) {
                        entries.push(m);
                    }
                }
            }
            Group {
                rep,
                entries,
                components,
            }
        })
        .collect()
}

/// How the artist is credited on a group: `(main, non-main roles)`. Main
/// when a main credit names the artist — or when nothing names any main
/// artist, since then the row is only here because a provider lists it
/// under the artist.
fn credit_of(
    pairs: &[&Pair],
    contribs_by_pair: &HashMap<Pair, Vec<&musiclib_rs::musicdb::ContribRow>>,
    artist_pairs: &HashSet<Pair>,
) -> (bool, Vec<String>) {
    let (mut any_main, mut artist_main) = (false, false);
    let mut roles: Vec<String> = Vec::new();
    for c in pairs
        .iter()
        .flat_map(|p| contribs_by_pair.get(*p).into_iter().flatten())
    {
        let is_artist =
            artist_pairs.contains(&(c.artist_source.clone(), c.artist_identifier.clone()));
        if c.main_artist {
            any_main = true;
            artist_main |= is_artist;
        } else if is_artist && !roles.contains(&c.role) {
            roles.push(c.role.clone());
        }
    }
    roles.sort();
    (artist_main || !any_main, roles)
}

/// `title` as compared by `fold_by_title`: NFKC (full-width forms),
/// lowercase, whitespace dropped — `わためのうた vol.1` and
/// `わためのうた Vol. １` compare equal.
fn title_key(title: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    title
        .nfkc()
        .flat_map(char::to_lowercase)
        .filter(|c| !c.is_whitespace())
        .collect()
}

/// Folds rows with the same `title_key`, kind and credit into one. An
/// artist rarely puts out two singles of one name, but the library often
/// holds one release twice: a MusicBrainz release group and a Spotify or
/// Discogs release dedup never linked to it. The row with the most
/// providers stands for the rest, which are kept in `also`.
fn fold_by_title(rows: Vec<ReleaseRow>) -> Vec<ReleaseRow> {
    let mut out: Vec<ReleaseRow> = Vec::new();
    let mut index: HashMap<(String, &'static str, bool), usize> = HashMap::new();
    for row in rows {
        let Some(key) = row
            .title
            .as_deref()
            .map(|t| (title_key(t), row.kind, row.main))
        else {
            out.push(row);
            continue;
        };
        let Some(&i) = index.get(&key) else {
            index.insert(key, out.len());
            out.push(row);
            continue;
        };
        let (kept, mut other) = (&mut out[i], row);
        if other.source_kinds.len() > kept.source_kinds.len() {
            std::mem::swap(kept, &mut other);
        }
        kept.also.push(other.entry_id);
        kept.also.append(&mut other.also);
        kept.release_date = match (kept.release_date.take(), other.release_date) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        kept.track_count = kept.track_count.max(other.track_count);
        kept.entries += other.entries;
        for k in other.source_kinds {
            if !kept.source_kinds.contains(&k) {
                kept.source_kinds.push(k);
            }
        }
    }
    out
}

fn source_kinds(sources: &[crate::entries::SourceRef]) -> Vec<String> {
    let mut kinds: Vec<String> = Vec::new();
    for s in sources {
        if !kinds.contains(&s.source) {
            kinds.push(s.source.clone());
        }
    }
    kinds
}

pub async fn discography(db: &MusicDb, artist_id: i64) -> anyhow::Result<Discography> {
    let artist_members = component_members(db, artist_id).await?;
    let artist_pairs: HashSet<Pair> = db
        .pairs_by_entry_ids(&artist_members)
        .await?
        .into_iter()
        .collect();
    let mut artist_pair_list: Vec<Pair> = artist_pairs.iter().cloned().collect();
    artist_pair_list.sort();

    // Everything the artist's pairs list or are credited on.
    let mut linked: Vec<Pair> = db
        .child_rows_for_parent_pairs(&artist_pair_list)
        .await?
        .into_iter()
        .map(|c| (c.child_source, c.child_identifier))
        .collect();
    linked.extend(
        db.contrib_rows_for_artist_pairs(&artist_pair_list)
            .await?
            .into_iter()
            .map(|c| (c.source, c.identifier)),
    );
    linked.sort();
    linked.dedup();
    let linked_ids: Vec<i64> = db
        .source_rows_for_pairs(&linked)
        .await?
        .iter()
        .map(|s| s.entry_id)
        .filter(|id| !artist_members.contains(id))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let type_of: HashMap<i64, String> = db
        .entry_rows_by_ids(&linked_ids)
        .await?
        .into_iter()
        .map(|e| (e.id, e.entry_type))
        .collect();
    let of_type = |t: &str| -> Vec<i64> {
        let mut ids: Vec<i64> = linked_ids
            .iter()
            .copied()
            .filter(|id| type_of.get(id).is_some_and(|et| et == t))
            .collect();
        ids.sort_unstable();
        ids
    };
    let mut release_groups = of_type("release_group");
    let releases = of_type("release");
    let tracks = of_type("track");

    // Fold each release under a release group it belongs to, pulling that
    // group in even if the artist doesn't list it directly.
    let release_sources = db.source_rows_by_entry_ids(&releases).await?;
    let release_pairs: Vec<Pair> = release_sources
        .iter()
        .map(|s| (s.source.clone(), s.identifier.clone()))
        .collect();
    let entry_of_pair: HashMap<Pair, i64> = release_sources
        .iter()
        .map(|s| ((s.source.clone(), s.identifier.clone()), s.entry_id))
        .collect();
    let parent_rows = db.child_rows_for_child_pairs(&release_pairs).await?;
    let parent_pairs: Vec<Pair> = parent_rows
        .iter()
        .map(|c| (c.parent_source.clone(), c.parent_identifier.clone()))
        .collect();
    let parent_sources = db.source_rows_for_pairs(&parent_pairs).await?;
    let parent_entry_of: HashMap<Pair, i64> = parent_sources
        .iter()
        .map(|s| ((s.source.clone(), s.identifier.clone()), s.entry_id))
        .collect();
    let parent_ids: Vec<i64> = parent_entry_of.values().copied().collect();
    let parent_is_group: HashSet<i64> = db
        .entry_rows_by_ids(&parent_ids)
        .await?
        .into_iter()
        .filter(|e| e.entry_type == "release_group")
        .map(|e| e.id)
        .collect();
    let mut folded: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut has_group: HashSet<i64> = HashSet::new();
    for c in &parent_rows {
        let parent = (c.parent_source.clone(), c.parent_identifier.clone());
        let child = (c.child_source.clone(), c.child_identifier.clone());
        let (Some(&group), Some(&release)) =
            (parent_entry_of.get(&parent), entry_of_pair.get(&child))
        else {
            continue;
        };
        if !parent_is_group.contains(&group) || !has_group.insert(release) {
            continue;
        }
        folded.entry(group).or_default().push(release);
        if !release_groups.contains(&group) {
            release_groups.push(group);
        }
    }
    let mut release_rows: Vec<i64> = release_groups.clone();
    release_rows.extend(releases.iter().filter(|r| !has_group.contains(r)));

    let plays = db.counted_plays_by_entry().await?;
    let mut seeds: Vec<i64> = release_rows.clone();
    seeds.extend(&tracks);
    seeds.extend(plays.iter().map(|(id, _)| *id));
    seeds.extend(folded.values().flatten());
    let (identity, editions) = db.display_projections_around(&seeds).await?;

    let release_groups_out = group_by_edition(&release_rows, &folded, &identity, &editions);
    let track_groups = group_by_edition(&tracks, &HashMap::new(), &identity, &editions);

    // Every source of every grouped entry, then their credits and (for
    // releases) tracklists.
    let all_entries: Vec<i64> = release_groups_out
        .iter()
        .chain(&track_groups)
        .flat_map(|g| g.entries.iter().copied())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let all_sources = db.source_rows_by_entry_ids(&all_entries).await?;
    let mut sources_by_entry: HashMap<i64, Vec<&SourceRow>> = HashMap::new();
    for s in &all_sources {
        sources_by_entry.entry(s.entry_id).or_default().push(s);
    }
    let all_pairs: Vec<Pair> = all_sources
        .iter()
        .map(|s| (s.source.clone(), s.identifier.clone()))
        .collect();
    let contribs = db.contrib_rows_for_track_pairs(&all_pairs).await?;
    let mut contribs_by_pair: HashMap<Pair, Vec<&musiclib_rs::musicdb::ContribRow>> =
        HashMap::new();
    for c in &contribs {
        contribs_by_pair
            .entry((c.source.clone(), c.identifier.clone()))
            .or_default()
            .push(c);
    }
    let group_pairs = |g: &Group| -> Vec<Pair> {
        g.entries
            .iter()
            .flat_map(|e| sources_by_entry.get(e).into_iter().flatten())
            .map(|s| (s.source.clone(), s.identifier.clone()))
            .collect()
    };

    // Tracklist length per release pair: distinct track entries under it.
    let release_side_pairs: Vec<Pair> = release_groups_out.iter().flat_map(&group_pairs).collect();
    let tracklist_rows = db.child_rows_for_parent_pairs(&release_side_pairs).await?;
    let tracklist_children: Vec<Pair> = tracklist_rows
        .iter()
        .map(|c| (c.child_source.clone(), c.child_identifier.clone()))
        .collect();
    let child_sources = db.source_rows_for_pairs(&tracklist_children).await?;
    let child_entry_of: HashMap<Pair, i64> = child_sources
        .iter()
        .map(|s| ((s.source.clone(), s.identifier.clone()), s.entry_id))
        .collect();
    let child_ids: Vec<i64> = child_entry_of.values().copied().collect();
    let child_is_track: HashSet<i64> = db
        .entry_rows_by_ids(&child_ids)
        .await?
        .into_iter()
        .filter(|e| e.entry_type == "track")
        .map(|e| e.id)
        .collect();
    let mut tracklist: HashMap<Pair, HashSet<i64>> = HashMap::new();
    for c in &tracklist_rows {
        let child = (c.child_source.clone(), c.child_identifier.clone());
        if let Some(&id) = child_entry_of.get(&child)
            && child_is_track.contains(&id)
        {
            tracklist
                .entry((c.parent_source.clone(), c.parent_identifier.clone()))
                .or_default()
                .insert(id);
        }
    }

    let reps: Vec<i64> = release_groups_out
        .iter()
        .chain(&track_groups)
        .map(|g| g.rep)
        .collect();
    let titles = entry_titles_with(db, &reps, &identity).await?;

    let out_releases: Vec<ReleaseRow> = release_groups_out
        .iter()
        .map(|g| {
            let pairs = group_pairs(g);
            let rows: Vec<&SourceRow> = g
                .entries
                .iter()
                .flat_map(|e| sources_by_entry.get(e).into_iter().flatten().copied())
                .collect();
            let track_count = pairs
                .iter()
                .filter_map(|p| tracklist.get(p).map(HashSet::len))
                .max();
            let kind = release_kind(
                rows.iter().filter_map(|s| s.primary_type.as_deref()),
                rows.iter().filter_map(|s| s.release_type.as_deref()),
                track_count,
            );
            let (main, _) = credit_of(
                &pairs.iter().collect::<Vec<_>>(),
                &contribs_by_pair,
                &artist_pairs,
            );
            let (title, sources) = titles.get(&g.rep).cloned().unwrap_or_default();
            ReleaseRow {
                entry_id: g.rep,
                title,
                kind,
                main,
                release_date: rows.iter().filter_map(|s| s.release_date.clone()).min(),
                track_count,
                source_kinds: source_kinds(&sources),
                entries: g.entries.len(),
                also: Vec::new(),
            }
        })
        .collect();
    let mut out_releases = fold_by_title(out_releases);
    out_releases.sort_by(|a, b| {
        b.release_date
            .cmp(&a.release_date)
            .then(a.entry_id.cmp(&b.entry_id))
    });

    let mut plays_by_component: HashMap<i64, i64> = HashMap::new();
    for (id, n) in plays {
        *plays_by_component
            .entry(identity.component_of(id).unwrap_or(id))
            .or_default() += n;
    }
    let mut out_tracks: Vec<TrackRow> = track_groups
        .iter()
        .map(|g| {
            let pairs = group_pairs(g);
            let (main, roles) = credit_of(
                &pairs.iter().collect::<Vec<_>>(),
                &contribs_by_pair,
                &artist_pairs,
            );
            let (title, sources) = titles.get(&g.rep).cloned().unwrap_or_default();
            TrackRow {
                entry_id: g.rep,
                title,
                main,
                roles,
                release_date: g
                    .entries
                    .iter()
                    .flat_map(|e| sources_by_entry.get(e).into_iter().flatten())
                    .filter_map(|s| s.release_date.clone())
                    .min(),
                playable: sources
                    .iter()
                    .any(|s| embed_for(&s.source, &s.identifier).is_some()),
                plays: g
                    .components
                    .iter()
                    .filter_map(|c| plays_by_component.get(c))
                    .sum(),
                source_kinds: source_kinds(&sources),
            }
        })
        .collect();
    out_tracks.sort_by(|a, b| {
        b.release_date
            .cmp(&a.release_date)
            .then(a.entry_id.cmp(&b.entry_id))
    });

    Ok(Discography {
        releases: out_releases,
        tracks: out_tracks,
    })
}

#[cfg(test)]
mod tests {
    use super::{ReleaseRow, fold_by_title, release_kind};

    #[test]
    fn primary_type_wins_over_release_type() {
        assert_eq!(release_kind(["EP"], ["single"], Some(2)), "ep");
    }

    #[test]
    fn release_type_used_without_primary_type() {
        assert_eq!(release_kind([], ["compilation"], Some(20)), "compilation");
        assert_eq!(release_kind([], ["playlist"], None), "playlist");
    }

    #[test]
    fn youtube_playlist_yields_to_another_providers_kind() {
        assert_eq!(release_kind([], ["playlist", "album"], Some(10)), "album");
    }

    #[test]
    fn same_title_releases_fold_into_the_richer_row() {
        let row = |id, title: &str, kinds: &[&str], date: &str| ReleaseRow {
            entry_id: id,
            title: Some(title.to_owned()),
            kind: "album",
            main: true,
            release_date: Some(date.to_owned()),
            track_count: Some(9),
            source_kinds: kinds.iter().map(|k| (*k).to_owned()).collect(),
            entries: 1,
            also: Vec::new(),
        };
        let rows = fold_by_title(vec![
            row(1, "わためのうた vol.1", &["musicbrainz"], "2020-12-29"),
            row(
                2,
                "わためのうた Vol. １",
                &["discogs", "spotify"],
                "2021-05-31",
            ),
            row(3, "わためのうた vol.2", &["musicbrainz"], "2021-12-29"),
        ]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].entry_id, 2);
        assert_eq!(rows[0].also, [1]);
        assert_eq!(rows[0].release_date.as_deref(), Some("2020-12-29"));
        assert_eq!(rows[0].source_kinds, ["discogs", "spotify", "musicbrainz"]);
    }

    #[test]
    fn discogs_status_falls_back_to_track_count() {
        assert_eq!(release_kind([], ["Accepted"], Some(2)), "single");
        assert_eq!(release_kind([], ["Accepted"], Some(5)), "ep");
        assert_eq!(release_kind([], ["Accepted"], Some(13)), "album");
        assert_eq!(release_kind([], ["Accepted"], None), "other");
    }
}
