//! The "familiar" side of the player's shuffle mix: every playable library
//! track NRS has an opinion about, with the NRS scores that reach it. A
//! track is reached directly (NRS scored the track), through a release NRS
//! scored (an album: its tracklist, a release group: its releases'
//! tracklists), or through an artist NRS scored (the tracks crediting it).
//! The frontend weighs and samples this pool itself (the mix settings are
//! client-side); this module only answers "which tracks, reached how, with
//! which scores".
//!
//! NRS is read over its local HTTP API (`/api/v1/entries` for each entry's
//! bound URL, `/api/v1/scores` for its value) — the reverse direction of the
//! store protocol, used only here and only on demand, so the server still
//! starts and shuffles without an NRS app running. URLs resolve the same way
//! as the store protocol's `lookup`: canonical pair first, then the URL as
//! a raw identifier.
//!
//! Tracks are folded to one row per edition group, represented by its
//! default edition, the same as `shuffle_library` returns them, so the
//! "new" sampler can exclude the pool by entry id.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Context as _;
use musiclib_rs::providers::registry::canonicalize;
use serde::{Deserialize, Serialize};

use crate::{entries::entry_summaries_with, state::AppState};

/// How long a built pool is reused before the next request rebuilds it.
/// Scores change rarely; the settings page's refresh button skips the wait.
const MAX_AGE: Duration = Duration::from_secs(10 * 60);

/// How an NRS score reaches a track.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    Track,
    Album,
    Artist,
}

/// One NRS entry that resolved to the library.
#[derive(Serialize)]
pub struct PoolSource {
    pub nrs_id: String,
    pub title: String,
    pub via: Via,
    pub score: f64,
    pub entry_id: i64,
}

/// One edition group reached by at least one `PoolSource`.
#[derive(Serialize)]
pub struct PoolTrack {
    pub entry_id: i64,
    pub title: Option<String>,
    /// Indices into `NrsPool::sources`.
    pub direct: Vec<usize>,
    pub albums: Vec<usize>,
    /// `(source index, main artist)`: whether the artist is credited as a
    /// main artist on at least one of the track's pairs, as opposed to only
    /// a secondary role (composer, arranger, ...).
    pub artists: Vec<(usize, bool)>,
}

#[derive(Serialize)]
pub struct NrsPool {
    /// Unix seconds.
    pub built_at: u64,
    pub sources: Vec<PoolSource>,
    pub tracks: Vec<PoolTrack>,
    /// NRS music entries (track/album/artist with a URL) considered.
    pub nrs_entries: usize,
    /// Of those, the ones no library entry holds.
    pub unresolved: Vec<UnresolvedEntry>,
    /// Edition groups reached but dropped for having no playable source.
    pub unplayable: usize,
    #[serde(skip)]
    built: Option<Instant>,
}

#[derive(Serialize)]
pub struct UnresolvedEntry {
    pub nrs_id: String,
    pub title: String,
    pub url: String,
}

impl NrsPool {
    /// Pool entry ids reached through any of `vias`, for excluding them from
    /// the "new" sampler.
    pub fn entry_ids_via(&self, vias: &[Via]) -> HashSet<i64> {
        let via_of = |i: &usize| self.sources[*i].via;
        self.tracks
            .iter()
            .filter(|t| {
                t.direct
                    .iter()
                    .chain(&t.albums)
                    .chain(t.artists.iter().map(|(i, _)| i))
                    .any(|i| vias.contains(&via_of(i)))
            })
            .map(|t| t.entry_id)
            .collect()
    }
}

/// The cached pool, rebuilt when older than `MAX_AGE` or when `refresh`.
/// Concurrent cold requests may both build; the last one wins the cache.
pub async fn get(state: &Arc<AppState>, refresh: bool) -> anyhow::Result<Arc<NrsPool>> {
    if !refresh {
        let cached = state.nrs_pool.lock().expect("pool lock").clone();
        if let Some(pool) = cached.filter(|p| p.built.is_some_and(|b| b.elapsed() < MAX_AGE)) {
            return Ok(pool);
        }
    }
    let pool = Arc::new(build(state).await?);
    *state.nrs_pool.lock().expect("pool lock") = Some(Arc::clone(&pool));
    Ok(pool)
}

/// The cached pool if one was built, however old — for the "new" sampler's
/// exclusions, which shouldn't wait on (or fail with) an NRS round trip.
pub fn cached(state: &AppState) -> Option<Arc<NrsPool>> {
    state.nrs_pool.lock().expect("pool lock").clone()
}

#[derive(Deserialize)]
struct EntriesPage {
    entries: Vec<NrsEntry>,
}

#[derive(Deserialize)]
struct NrsEntry {
    id: String,
    title: String,
    store: String,
    store_id: String,
    kind: Option<String>,
}

#[derive(Deserialize)]
struct ScoresPage {
    rows: Vec<ScoreRow>,
    total: usize,
}

#[derive(Deserialize)]
struct ScoreRow {
    id: String,
    value: f64,
}

/// NRS entries that could be music in this library, with their scores:
/// URL-bound, and of a music kind (or no kind yet — the library's own entry
/// type decides below).
async fn fetch_nrs(base: &str) -> anyhow::Result<Vec<(NrsEntry, f64)>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let base = base.trim_end_matches('/');
    let entries: EntriesPage = client
        .get(format!("{base}/api/v1/entries?limit=10000"))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("reading NRS entries from {base}"))?
        .json()
        .await?;
    let mut scores = HashMap::new();
    let mut offset = 0;
    loop {
        let page: ScoresPage = client
            .get(format!("{base}/api/v1/scores?limit=1000&offset={offset}"))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .with_context(|| format!("reading NRS scores from {base}"))?
            .json()
            .await?;
        let n = page.rows.len();
        scores.extend(page.rows.into_iter().map(|r| (r.id, r.value)));
        offset += n;
        if n == 0 || offset >= page.total {
            break;
        }
    }
    Ok(entries
        .entries
        .into_iter()
        .filter(|e| e.store.starts_with("url") && e.store_id.contains("://"))
        .filter(|e| {
            e.kind
                .as_deref()
                .is_none_or(|k| matches!(k, "track" | "album" | "artist"))
        })
        .filter_map(|e| {
            let score = *scores.get(&e.id)?;
            Some((e, score))
        })
        .collect())
}

async fn build(state: &Arc<AppState>) -> anyhow::Result<NrsPool> {
    let db = &state.db;
    let nrs = fetch_nrs(&state.nrs_url).await?;
    let nrs_entries = nrs.len();

    // URL -> library entry, batched over canonical pairs; raw-identifier
    // fallback one by one for the rest (local files, unknown URL shapes).
    let canonical: Vec<Option<(String, String)>> = nrs
        .iter()
        .map(|(e, _)| {
            canonicalize(&e.store_id)
                .map(|c| (c.canonical_source_key.into_owned(), c.canonical_identifier))
        })
        .collect();
    let pairs: Vec<(String, String)> = canonical.iter().flatten().cloned().collect();
    let by_pair: HashMap<(String, String), i64> = db
        .source_rows_for_pairs(&pairs)
        .await?
        .into_iter()
        .map(|s| ((s.source, s.identifier), s.entry_id))
        .collect();
    let mut resolved: Vec<(usize, i64)> = Vec::new();
    let mut unresolved = Vec::new();
    for (i, ((entry, _), pair)) in nrs.iter().zip(&canonical).enumerate() {
        let mut id = pair.as_ref().and_then(|p| by_pair.get(p).copied());
        if id.is_none() {
            id = db
                .find_entry_by_identifier(&entry.store_id)
                .await?
                .map(|(_, id)| id);
        }
        match id {
            Some(id) => resolved.push((i, id)),
            None => unresolved.push(UnresolvedEntry {
                nrs_id: entry.id.clone(),
                title: entry.title.clone(),
                url: entry.store_id.clone(),
            }),
        }
    }

    // Each resolved entry stands for its whole soft-identity component:
    // a release's tracklist or an artist's credits may hang off any member.
    let ids: Vec<i64> = resolved.iter().map(|&(_, id)| id).collect();
    let (identity, _) = db.display_projections_around(&ids).await?;
    let members = |id: i64| -> Vec<i64> {
        let c = identity.component_of(id).unwrap_or(id);
        identity
            .members_by_component
            .get(&c)
            .cloned()
            .unwrap_or_else(|| vec![id])
    };
    let types: HashMap<i64, String> = db
        .entry_rows_by_ids(&ids)
        .await?
        .into_iter()
        .map(|e| (e.id, e.entry_type))
        .collect();

    let mut sources = Vec::new();
    // Per pool source, its member entries' pairs.
    let mut release_pairs: Vec<(usize, (String, String))> = Vec::new();
    let mut artist_pairs: Vec<(usize, (String, String))> = Vec::new();
    // (track entry id, source index, main artist)
    let mut reached: Vec<(i64, usize, bool)> = Vec::new();
    let mut by_member: Vec<(usize, Vec<i64>)> = Vec::new();
    for &(i, id) in &resolved {
        let (entry, score) = &nrs[i];
        let via = match types.get(&id).map(String::as_str) {
            Some("track") => Via::Track,
            Some("release" | "release_group") => Via::Album,
            Some("artist") => Via::Artist,
            _ => continue,
        };
        let index = sources.len();
        sources.push(PoolSource {
            nrs_id: entry.id.clone(),
            title: entry.title.clone(),
            via,
            score: *score,
            entry_id: id,
        });
        match via {
            Via::Track => reached.push((id, index, false)),
            Via::Album | Via::Artist => by_member.push((index, members(id))),
        }
    }
    let member_ids: Vec<i64> = by_member
        .iter()
        .flat_map(|(_, m)| m.iter().copied())
        .collect();
    let mut index_of_member: HashMap<i64, Vec<usize>> = HashMap::new();
    for (index, ms) in &by_member {
        for m in ms {
            index_of_member.entry(*m).or_default().push(*index);
        }
    }
    for row in db.source_rows_by_entry_ids(&member_ids).await? {
        for &index in index_of_member.get(&row.entry_id).into_iter().flatten() {
            let pair = (row.source.clone(), row.identifier.clone());
            match sources[index].via {
                Via::Album => release_pairs.push((index, pair)),
                Via::Artist => artist_pairs.push((index, pair)),
                Via::Track => {}
            }
        }
    }

    // Releases: their tracks; release groups: their releases' tracks (one
    // more hop through the same edges).
    let mut frontier = release_pairs;
    for _ in 0..2 {
        if frontier.is_empty() {
            break;
        }
        let parents: Vec<(String, String)> = frontier.iter().map(|(_, p)| p.clone()).collect();
        let mut sources_of_parent: HashMap<(String, String), Vec<usize>> = HashMap::new();
        for (index, pair) in frontier {
            sources_of_parent.entry(pair).or_default().push(index);
        }
        let children = db.child_rows_for_parent_pairs(&parents).await?;
        let child_pairs: Vec<(String, String)> = children
            .iter()
            .map(|c| (c.child_source.clone(), c.child_identifier.clone()))
            .collect();
        let child_entries = pair_entries(state, &child_pairs).await?;
        let mut next = Vec::new();
        for c in &children {
            let parent = (c.parent_source.clone(), c.parent_identifier.clone());
            let child = (c.child_source.clone(), c.child_identifier.clone());
            let Some((entry_id, entry_type)) = child_entries.get(&child) else {
                continue;
            };
            for &index in sources_of_parent.get(&parent).into_iter().flatten() {
                match entry_type.as_str() {
                    "track" => reached.push((*entry_id, index, false)),
                    "release" => next.push((index, child.clone())),
                    _ => {}
                }
            }
        }
        frontier = next;
    }

    // Artists: the tracks crediting them.
    let artist_keys: Vec<(String, String)> = artist_pairs.iter().map(|(_, p)| p.clone()).collect();
    let mut sources_of_artist: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for (index, pair) in artist_pairs {
        sources_of_artist.entry(pair).or_default().push(index);
    }
    let contribs = db.contrib_rows_for_artist_pairs(&artist_keys).await?;
    let credited: Vec<(String, String)> = contribs
        .iter()
        .map(|c| (c.source.clone(), c.identifier.clone()))
        .collect();
    let credited_entries = pair_entries(state, &credited).await?;
    for c in &contribs {
        let artist = (c.artist_source.clone(), c.artist_identifier.clone());
        let work = (c.source.clone(), c.identifier.clone());
        let Some((entry_id, entry_type)) = credited_entries.get(&work) else {
            continue;
        };
        if entry_type != "track" {
            continue;
        }
        for &index in sources_of_artist.get(&artist).into_iter().flatten() {
            reached.push((*entry_id, index, c.main_artist));
        }
    }

    // Fold to edition groups (default edition), then keep the playable ones.
    let track_ids: Vec<i64> = reached
        .iter()
        .map(|&(id, _, _)| id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let (identity, editions) = db.display_projections_around(&track_ids).await?;
    let mut groups: BTreeMap<i64, (BTreeMap<usize, bool>, HashSet<usize>)> = BTreeMap::new();
    for (id, index, main) in reached {
        let canonical = identity.component_of(id).unwrap_or(id);
        let default = editions.default_member_of(canonical);
        let (artists, others) = groups.entry(default).or_default();
        if sources[index].via == Via::Artist {
            *artists.entry(index).or_default() |= main;
        } else {
            others.insert(index);
        }
    }
    let defaults: Vec<i64> = groups.keys().copied().collect();
    let summaries = entry_summaries_with(db, &defaults, &identity, &editions).await?;
    let mut unplayable = 0;
    let mut tracks = Vec::new();
    for (entry_id, (artists, others)) in groups {
        let Some(summary) = summaries.get(&entry_id).filter(|s| {
            s.sources
                .iter()
                .any(|s| crate::player::embed_for(&s.source, &s.identifier).is_some())
        }) else {
            unplayable += 1;
            continue;
        };
        let mut direct: Vec<usize> = Vec::new();
        let mut albums: Vec<usize> = Vec::new();
        for index in others {
            match sources[index].via {
                Via::Track => direct.push(index),
                _ => albums.push(index),
            }
        }
        direct.sort_unstable();
        albums.sort_unstable();
        tracks.push(PoolTrack {
            entry_id,
            title: summary.title.clone(),
            direct,
            albums,
            artists: artists.into_iter().collect(),
        });
    }

    Ok(NrsPool {
        built_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        sources,
        tracks,
        nrs_entries,
        unresolved,
        unplayable,
        built: Some(Instant::now()),
    })
}

/// `pair -> (entry id, entry type)` for those of `pairs` the library holds.
async fn pair_entries(
    state: &Arc<AppState>,
    pairs: &[(String, String)],
) -> anyhow::Result<HashMap<(String, String), (i64, String)>> {
    let rows = state.db.source_rows_for_pairs(pairs).await?;
    let ids: Vec<i64> = rows
        .iter()
        .map(|r| r.entry_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let types: HashMap<i64, String> = state
        .db
        .entry_rows_by_ids(&ids)
        .await?
        .into_iter()
        .map(|e| (e.id, e.entry_type))
        .collect();
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let t = types.get(&r.entry_id)?.clone();
            Some(((r.source, r.identifier), (r.entry_id, t)))
        })
        .collect())
}
