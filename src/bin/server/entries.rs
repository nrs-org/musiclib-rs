//! Entry-lookup helpers shared by both route groups. Deliberately not
//! `pipeline::softmatch::EntryInfo` — that type (and the title-merging logic
//! around it) is coupled to the dedup-scoring engine; the server just needs a
//! cheap display title and source list per entry.
//!
//! These helpers resolve through *soft* dedup (`entry_relation` same_identity
//! links) in addition to *hard* dedup (physical `entry_source` grouping) —
//! two entries the dedup pipeline has linked but not physically merged are
//! presented as one, with sources unioned across every linked member. Hard
//! dedup needs nothing extra here: it already collapsed to a single
//! `entry_id` at import time, so it's just an ordinary entry.

use std::collections::{HashMap, HashSet};

use musiclib_rs::musicdb::MusicDb;
use serde::Serialize;

#[derive(Clone, Serialize)]
pub struct SourceRef {
    pub source: String,
    pub identifier: String,
}

#[derive(Clone, Serialize)]
pub struct EntrySummary {
    pub entry_id: i64,
    pub entry_type: String,
    pub title: Option<String>,
    pub release_date: Option<String>,
    /// Union of sources across this entry and every entry it's soft-linked
    /// with (see `soft_linked_ids`) — not just this one row's own sources.
    pub sources: Vec<SourceRef>,
    /// Other entry ids soft dedup has linked to this one (`entry_relation`
    /// `same_identity`, enabled, not physically merged). Empty for an entry
    /// with no soft links. The UI uses this to show that what's displayed
    /// spans more than one underlying catalog row, distinct from a
    /// genuinely single entry.
    pub soft_linked_ids: Vec<i64>,
    /// Every component member (this entry included) with *only its own*
    /// sources, not the union above. `title`/`sources` are identical no
    /// matter which member you look at, so this is the only thing that
    /// actually tells soft-linked duplicates apart in a merge UI.
    pub members: Vec<MemberSources>,
    /// Other identities in this entry's *edition group* (`musicdb::EDITION_KINDS`
    /// — `derived_from` plus the version-ish `RELATE_KINDS`), this entry's own
    /// identity included. A track and its instrumental are two distinct
    /// identities (unlike `members`, which are the same identity) presented
    /// as one library row with a picker — the same way the player already
    /// treats multiple provider links to one identity as alternate playback
    /// sources. Exactly one entry (`entry_id`) per underlying identity, even
    /// though a caller may pass any member of that identity's own soft-identity
    /// component. Length 1 (just this entry) for an entry with no edition links.
    pub editions: Vec<EditionRef>,
}

#[derive(Clone, Serialize)]
pub struct MemberSources {
    pub entry_id: i64,
    pub sources: Vec<SourceRef>,
    /// This member's own title — picked the same way as `EntrySummary::title`
    /// but scoped to just this member's own sources, not the component-wide
    /// union. Soft-linked members can still differ slightly in title text
    /// (alias capitalization/romanization across providers), so callers that
    /// render one member at a time (the relations graph, a node dropped into
    /// it) need this instead of the merged `EntrySummary.title` or every
    /// member renders identically. A track and its instrumental are *not*
    /// soft-identity members of each other any more — see `EntrySummary::editions`.
    pub title: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct EditionRef {
    pub entry_id: i64,
    /// This edition's own display title — same derivation as `MemberSources::title`.
    pub title: Option<String>,
    /// The `entry_relation.kind` connecting this edition to the default one
    /// (`None` for the default edition itself, or when it's related only
    /// transitively through a third edition — see `EditionProjection::edge_label`).
    pub kind: Option<String>,
    /// `extra.transformation` off that same edge, when present (e.g.
    /// `"instrumental"`, `"live"`) — a more specific label than `kind` alone.
    pub transformation: Option<String>,
    pub is_default: bool,
    /// True for the one edition matching the requested entry's own identity
    /// (`EntrySummary::entry_id`'s soft-identity component) — lets a switcher
    /// highlight "you are here" even when `entry_id` isn't itself the
    /// identity's canonical id.
    pub is_current: bool,
}

/// Edition switcher order: the original first, then full alternate versions,
/// then degenerate ones (`musicdb::DEGENERATE_MARKERS` — the same ranking
/// `EditionProjection::default_member_of` uses to pick the playback default
/// until per-user preference exists, so the switcher's left-to-right order
/// matches what actually plays). `transformation` may hold several
/// space-separated markers at once (e.g. `"instrumental named:long"`); one
/// degenerate token is enough to sort the whole edition last.
fn edition_sort_rank(ed: &EditionRef) -> u8 {
    if ed.is_default {
        return 0;
    }
    let degenerate = ed.transformation.as_deref().is_some_and(|t| {
        t.split_whitespace()
            .any(|tok| musiclib_rs::musicdb::DEGENERATE_MARKERS.contains(&tok))
    });
    if degenerate { 2 } else { 1 }
}

/// Every member of `entry_id`'s soft-identity component, `entry_id` included
/// — a solo (no soft links) entry is a one-member component of itself.
pub async fn component_members(db: &MusicDb, entry_id: i64) -> anyhow::Result<Vec<i64>> {
    let projection = db.soft_identity_projection_for_display().await?;
    let canonical = projection.component_of(entry_id).unwrap_or(entry_id);
    Ok(projection
        .members_by_component
        .get(&canonical)
        .cloned()
        .unwrap_or_else(|| vec![entry_id]))
}

/// Fold `ids` down to one id per *edition group* (`musicdb::EDITION_KINDS`,
/// layered on top of soft-identity components), so a caller presenting a
/// list — search results in particular — doesn't show the same song, or a
/// different edition of it (a track and its instrumental), more than once.
/// Each group is represented by its designated default edition
/// (`EditionProjection::default_member_of`), not necessarily whichever
/// member of `ids` triggered the fold.
pub async fn dedupe_by_edition(db: &MusicDb, ids: &[i64]) -> anyhow::Result<Vec<i64>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let identity = db.soft_identity_projection_for_display().await?;
    let editions = db.edition_projection_for_display().await?;
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(ids.len());
    for &id in ids {
        let canonical = identity.component_of(id).unwrap_or(id);
        let group = editions.group_of(canonical);
        if seen.insert(group) {
            out.push(editions.default_member_of(canonical));
        }
    }
    Ok(out)
}

/// Picks a display title for a set of sources: a primary alias on any of
/// them if one exists, else the first alias found, else falls back to
/// `source:identifier` on the first source. Shared by the component-wide
/// title (all members' sources) and each member's own title (just its own).
///
/// YouTube sources are tried last (stable sort, so relative order among the
/// rest is unchanged): YouTube video titles routinely bundle the artist name,
/// a "Music Video"/"Official Audio" suffix, etc., so a clean title from any
/// other source (Spotify, MusicBrainz, ...) should win whenever one exists.
fn pick_title(
    sources: &[&musiclib_rs::musicdb::SourceRow],
    aliases_by_pair: &HashMap<(String, String), Vec<&musiclib_rs::musicdb::AliasRow>>,
) -> Option<String> {
    let mut ordered: Vec<&musiclib_rs::musicdb::SourceRow> = sources.to_vec();
    ordered.sort_by_key(|s| s.source == "youtube");

    let mut title = None;
    for s in &ordered {
        let Some(names) = aliases_by_pair.get(&(s.source.clone(), s.identifier.clone())) else {
            continue;
        };
        if let Some(primary) = names.iter().find(|a| a.primary_alias) {
            return Some(primary.name.clone());
        }
        if title.is_none() {
            title = names.first().map(|a| a.name.clone());
        }
    }
    title.or_else(|| {
        ordered
            .first()
            .map(|s| format!("{}:{}", s.source, s.identifier))
    })
}

/// Resolve `ids` to display-ready summaries in a handful of batch queries
/// (not one round trip per id, nor one per soft-identity component). Title
/// picks a primary alias if any member's source has one, else the first
/// alias found across the component, else falls back to `source:identifier`.
pub async fn entry_summaries(
    db: &MusicDb,
    ids: &[i64],
) -> anyhow::Result<HashMap<i64, EntrySummary>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }

    let projection = db.soft_identity_projection_for_display().await?;
    let editions = db.edition_projection_for_display().await?;

    // Resolve every requested id to its full soft-identity component (a
    // solo entry is its own one-member component), keyed by canonical id so
    // repeated requests for members of the same component don't redo work.
    // Also resolve every edition sibling's own component the same way —
    // `editions` only needs their *own* title (never a component-wide
    // union across editions), but that still means fetching their sources.
    let mut components: HashMap<i64, Vec<i64>> = HashMap::new();
    let fetch_component = |canonical: i64, components: &mut HashMap<i64, Vec<i64>>| {
        components.entry(canonical).or_insert_with(|| {
            projection
                .members_by_component
                .get(&canonical)
                .cloned()
                .unwrap_or_else(|| vec![canonical])
        });
    };
    for &id in ids {
        let canonical = projection.component_of(id).unwrap_or(id);
        fetch_component(canonical, &mut components);
        for sibling in editions.members_of(canonical) {
            fetch_component(sibling, &mut components);
        }
    }
    let expanded_ids: Vec<i64> = components.values().flatten().copied().collect();

    let entry_rows = db.entry_rows_by_ids(&expanded_ids).await?;
    let source_rows = db.source_rows_by_entry_ids(&expanded_ids).await?;
    let pairs: Vec<(String, String)> = source_rows
        .iter()
        .map(|s| (s.source.clone(), s.identifier.clone()))
        .collect();
    let alias_rows = db.alias_rows_for_pairs(&pairs).await?;

    let mut aliases_by_pair: HashMap<(String, String), Vec<&musiclib_rs::musicdb::AliasRow>> =
        HashMap::new();
    for a in &alias_rows {
        aliases_by_pair
            .entry((a.source.clone(), a.identifier.clone()))
            .or_default()
            .push(a);
    }
    let mut sources_by_entry: HashMap<i64, Vec<&musiclib_rs::musicdb::SourceRow>> = HashMap::new();
    for s in &source_rows {
        sources_by_entry.entry(s.entry_id).or_default().push(s);
    }
    let entry_type_by_id: HashMap<i64, &str> = entry_rows
        .iter()
        .map(|e| (e.id, e.entry_type.as_str()))
        .collect();

    let mut out = HashMap::with_capacity(ids.len());
    for &id in ids {
        // Skip ids that don't actually exist — callers rely on absence from
        // the map to mean "not found" (404), not a synthesized empty entry.
        let Some(&entry_type) = entry_type_by_id.get(&id) else {
            continue;
        };
        let canonical = projection.component_of(id).unwrap_or(id);
        let Some(members) = components.get(&canonical) else {
            continue;
        };
        let member_sources: Vec<&musiclib_rs::musicdb::SourceRow> = members
            .iter()
            .flat_map(|m| sources_by_entry.get(m).cloned().unwrap_or_default())
            .collect();

        let title = pick_title(&member_sources, &aliases_by_pair);
        let release_date = member_sources.iter().find_map(|s| s.release_date.clone());

        let default_canonical = editions.default_member_of(canonical);
        let mut edition_list: Vec<EditionRef> = editions
            .members_of(canonical)
            .into_iter()
            .map(|sib| {
                let sib_sources: Vec<&musiclib_rs::musicdb::SourceRow> = components
                    .get(&sib)
                    .cloned()
                    .unwrap_or_else(|| vec![sib])
                    .iter()
                    .flat_map(|m| sources_by_entry.get(m).cloned().unwrap_or_default())
                    .collect();
                let (kind, transformation) = editions
                    .edge_label(sib, default_canonical)
                    .map(|(kind, transformation)| {
                        (Some(kind.to_string()), transformation.map(str::to_string))
                    })
                    .unwrap_or((None, None));
                EditionRef {
                    entry_id: sib,
                    title: pick_title(&sib_sources, &aliases_by_pair),
                    kind,
                    transformation,
                    is_default: sib == default_canonical,
                    is_current: sib == canonical,
                }
            })
            .collect();
        edition_list.sort_by_key(|ed| (edition_sort_rank(ed), ed.entry_id));

        out.insert(
            id,
            EntrySummary {
                entry_id: id,
                entry_type: entry_type.to_string(),
                title,
                release_date,
                sources: member_sources
                    .iter()
                    .map(|s| SourceRef {
                        source: s.source.clone(),
                        identifier: s.identifier.clone(),
                    })
                    .collect(),
                soft_linked_ids: members.iter().copied().filter(|&m| m != id).collect(),
                members: members
                    .iter()
                    .map(|&m| {
                        let own_sources = sources_by_entry.get(&m).cloned().unwrap_or_default();
                        MemberSources {
                            entry_id: m,
                            title: pick_title(&own_sources, &aliases_by_pair),
                            sources: own_sources
                                .iter()
                                .map(|s| SourceRef {
                                    source: s.source.clone(),
                                    identifier: s.identifier.clone(),
                                })
                                .collect(),
                        }
                    })
                    .collect(),
                editions: edition_list,
            },
        );
    }
    Ok(out)
}
