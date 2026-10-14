//! Server side of the NRS Store Protocol (`nrs-org/nrs`'s
//! `store-protocol/specification.md`), for the `music` entry type. This is
//! the interface `nrs-app` (or any other NRS application) talks to, over
//! plain localhost HTTP — never in-process — so musiclib-rs stays a portable
//! service rather than a Cargo-workspace dependency.
//!
//! Implements: capability negotiation, `resolve`, `lookup`, `search`,
//! `ingest` (as a job, per spec §7), the generic job resource, and
//! `suggest_relations`, which proposes `contains` between a release and its
//! tracks, in both directions. Credits are not mapped to NRS relation kinds
//! yet. `ingest` is reported unsupported when the server started without any
//! provider (no credentials).

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Method, Response, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

use musiclib_rs::{
    musicdb::PairFacts, pipeline::ingest::ingest_entry, providers::registry::canonicalize,
};

use crate::{
    entries::{EntrySummary, SourceRef, entry_summaries},
    respond::{self, HyperResponse, parse_query},
    state::AppState,
};

const PREFIX: &str = "/nrs-store/v1";

pub fn handles(path: &str) -> bool {
    path == PREFIX || path.starts_with(&format!("{PREFIX}/"))
}

pub async fn route(
    state: &Arc<AppState>,
    method: &Method,
    path: &str,
    query: &str,
    body: Bytes,
) -> HyperResponse {
    let rest = path.strip_prefix(PREFIX).unwrap_or("");
    match (method, rest) {
        (&Method::GET, "/capabilities") => capabilities(state),
        (&Method::GET, "/lookup") => lookup(state, &parse_query(query)).await,
        (&Method::GET, "/search") => search(state, &parse_query(query)).await,
        (&Method::GET, p) if p.starts_with("/entries/") => {
            let p = &p["/entries/".len()..];
            match p.strip_suffix("/suggested-relations") {
                Some(id) => suggest_relations(state, id, &parse_query(query)).await,
                None => resolve(state, p).await,
            }
        }
        (&Method::POST, "/ingest") => ingest(state, body).await,
        (&Method::GET, p) if p.starts_with("/jobs/") => job_status(state, &p["/jobs/".len()..]),
        (&Method::DELETE, p) if p.starts_with("/jobs/") => job_cancel(state, &p["/jobs/".len()..]),
        (_, "/capabilities" | "/lookup" | "/search" | "/ingest") => respond::method_not_allowed(),
        _ => respond::not_found(),
    }
}

fn capabilities(state: &Arc<AppState>) -> HyperResponse {
    respond::json(
        StatusCode::OK,
        json!({
            "store": "musiclib",
            "protocol_version": "0.5",
            "operations": {
                "resolve": true,
                "lookup": true,
                "search": true,
                "ingest": !state.providers.is_empty(),
                "suggest_relations": true,
            },
            "jobs": {"streaming": false},
        }),
    )
}

fn parse_store_id(store_id: &str) -> Result<i64, HyperResponse> {
    store_id.parse::<i64>().map_err(|_| {
        respond::error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "store_id must be an entry id",
        )
    })
}

fn internal(e: impl std::fmt::Display) -> HyperResponse {
    respond::error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        &e.to_string(),
    )
}

/// The entry a URL already belongs to, without fetching anything: its
/// canonical pair first, then any pair whose identifier is the URL as given.
/// `extra` names the matched pair, which `resolve`'s `extra.sources` lists.
async fn lookup(state: &Arc<AppState>, params: &HashMap<String, String>) -> HyperResponse {
    let Some(query) = params.get("query").filter(|q| !q.is_empty()) else {
        return respond::error(StatusCode::BAD_REQUEST, "invalid_input", "missing query");
    };
    let mut found = None;
    if let Some(c) = canonicalize(query) {
        match state
            .db
            .find_entry_id_by_pair(&c.canonical_source_key, &c.canonical_identifier)
            .await
        {
            Ok(Some(id)) => {
                found = Some((
                    c.canonical_source_key.into_owned(),
                    c.canonical_identifier,
                    id,
                ))
            }
            Ok(None) => {}
            Err(e) => return internal(e),
        }
    }
    if found.is_none() {
        match state.db.find_entry_by_identifier(query).await {
            Ok(Some((source, id))) => found = Some((source, query.clone(), id)),
            Ok(None) => {}
            Err(e) => return internal(e),
        }
    }
    match found {
        Some((source, identifier, id)) => respond::json(
            StatusCode::OK,
            json!({
                "store_id": id.to_string(),
                "extra": {"source": source, "identifier": identifier},
            }),
        ),
        None => respond::not_found(),
    }
}

/// Sources by how canonical their URLs are for binding, best first; any
/// other source with an http(s) identifier ranks after these.
const URL_PRIORITY: &[&str] = &[
    "musicbrainz",
    "vgmdb",
    "spotify",
    "youtube",
    "nicovideo",
    "soundcloud",
    "discogs",
];

/// What an entry summary carries beyond `EntrySummary`: per entry id, its
/// `subtitle` (`artist_subtitles`) and `edition_group` (`edition_groups`).
struct SummaryExtras {
    subtitles: HashMap<i64, String>,
    edition_groups: HashMap<i64, String>,
}

async fn summary_extras(
    state: &Arc<AppState>,
    entries: &[&EntrySummary],
) -> anyhow::Result<SummaryExtras> {
    Ok(SummaryExtras {
        subtitles: artist_subtitles(state, entries).await?,
        edition_groups: edition_groups(state, entries).await?,
    })
}

/// The protocol's entry summary (spec §5) of `entry`. `title` and `urls`
/// span its soft-identity component, as `entry_summaries` does, so a client
/// recognizes the entry under any duplicate's URL.
fn summary_json(entry: &EntrySummary, extras: &SummaryExtras) -> Value {
    let subtitle = extras.subtitles.get(&entry.entry_id);
    let mut urls: Vec<&SourceRef> = Vec::new();
    for source in &entry.sources {
        if source.identifier.starts_with("http")
            && !urls.iter().any(|u| u.identifier == source.identifier)
        {
            urls.push(source);
        }
    }
    let canonical = urls.iter().min_by_key(|u| {
        let rank = URL_PRIORITY
            .iter()
            .position(|&s| s == u.source)
            .unwrap_or(URL_PRIORITY.len());
        (rank, u.identifier.as_str())
    });
    json!({
        "store_id": entry.entry_id.to_string(),
        "title": entry.title.clone().unwrap_or_default(),
        "kind": entry.entry_type,
        "subtitle": subtitle,
        "release_date": entry.release_date,
        "canonical_url": canonical.map(|u| &u.identifier),
        "urls": urls.iter().map(|u| &u.identifier).collect::<Vec<_>>(),
        "edition_group": extras.edition_groups.get(&entry.entry_id),
    })
}

/// Each release's edition group (spec §5): `rg:` and its release group,
/// soft-linked duplicates folded (a Discogs master and its MusicBrainz
/// release group are one), so every pressing and listing of an album
/// shares it; else `ed:` and its edition siblings (`EntrySummary::editions`)
/// when it has any. Other kinds have none.
async fn edition_groups(
    state: &Arc<AppState>,
    entries: &[&EntrySummary],
) -> anyhow::Result<HashMap<i64, String>> {
    let db = &state.db;
    let releases: Vec<&EntrySummary> = entries
        .iter()
        .copied()
        .filter(|e| e.entry_type == "release")
        .collect();
    let pairs: Vec<(String, String)> = releases
        .iter()
        .flat_map(|e| &e.sources)
        .map(|s| (s.source.clone(), s.identifier.clone()))
        .collect();
    let listings = db.child_rows_for_child_pairs(&pairs).await?;
    let parent_pairs: Vec<(String, String)> = listings
        .iter()
        .map(|c| (c.parent_source.clone(), c.parent_identifier.clone()))
        .collect();
    let parent_of_pair: HashMap<(String, String), i64> = db
        .source_rows_for_pairs(&parent_pairs)
        .await?
        .into_iter()
        .map(|s| ((s.source, s.identifier), s.entry_id))
        .collect();
    let parent_ids: Vec<i64> = parent_of_pair.values().copied().collect();
    let group_ids: Vec<i64> = db
        .entry_rows_by_ids(&parent_ids)
        .await?
        .into_iter()
        .filter(|e| e.entry_type == "release_group")
        .map(|e| e.id)
        .collect();
    // A release group's key: the least id of it and its soft-linked twins.
    let groups = entry_summaries(db, &group_ids).await?;
    let group_key = |id: i64| {
        groups.get(&id).map(|g| {
            g.soft_linked_ids
                .iter()
                .copied()
                .chain([id])
                .min()
                .unwrap_or(id)
        })
    };

    let mut out = HashMap::new();
    for release in releases {
        let own: HashSet<(&str, &str)> = release
            .sources
            .iter()
            .map(|s| (s.source.as_str(), s.identifier.as_str()))
            .collect();
        let group = listings
            .iter()
            .filter(|c| own.contains(&(c.child_source.as_str(), c.child_identifier.as_str())))
            .filter_map(|c| {
                parent_of_pair.get(&(c.parent_source.clone(), c.parent_identifier.clone()))
            })
            .filter_map(|&id| group_key(id))
            .min();
        let key = match group {
            Some(group) => format!("rg:{group}"),
            None if release.editions.len() > 1 => {
                let least = release.editions.iter().map(|e| e.entry_id).min();
                format!("ed:{}", least.unwrap_or(release.entry_id))
            }
            None => continue,
        };
        out.insert(release.entry_id, key);
    }
    Ok(out)
}

/// Each non-artist entry's main artists, comma-joined: its `listed_artist`
/// credits, else its other main-artist credits (a YouTube uploader). Only
/// credits naming an artist entry count; some release credits name a
/// recording or release instead.
async fn artist_subtitles(
    state: &Arc<AppState>,
    entries: &[&EntrySummary],
) -> anyhow::Result<HashMap<i64, String>> {
    let db = &state.db;
    let credited: Vec<&EntrySummary> = entries
        .iter()
        .copied()
        .filter(|e| e.entry_type != "artist")
        .collect();
    let pairs: Vec<(String, String)> = credited
        .iter()
        .flat_map(|e| &e.sources)
        .map(|s| (s.source.clone(), s.identifier.clone()))
        .collect();
    let contribs: Vec<_> = db
        .contrib_rows_for_track_pairs(&pairs)
        .await?
        .into_iter()
        .filter(|c| c.main_artist)
        .collect();
    let artist_pairs: Vec<(String, String)> = contribs
        .iter()
        .map(|c| (c.artist_source.clone(), c.artist_identifier.clone()))
        .collect();
    let artist_of_pair: HashMap<(String, String), i64> = db
        .source_rows_for_pairs(&artist_pairs)
        .await?
        .into_iter()
        .map(|s| ((s.source, s.identifier), s.entry_id))
        .collect();
    let candidate_ids: Vec<i64> = artist_of_pair.values().copied().collect();
    let artist_ids: Vec<i64> = db
        .entry_rows_by_ids(&candidate_ids)
        .await?
        .into_iter()
        .filter(|e| e.entry_type == "artist")
        .map(|e| e.id)
        .collect();
    let artists = entry_summaries(db, &artist_ids).await?;

    let mut out = HashMap::new();
    for entry in credited {
        let own: HashSet<(&str, &str)> = entry
            .sources
            .iter()
            .map(|s| (s.source.as_str(), s.identifier.as_str()))
            .collect();
        let names_for = |listed: bool| {
            let mut names: Vec<&str> = Vec::new();
            for c in &contribs {
                if !own.contains(&(c.source.as_str(), c.identifier.as_str()))
                    || (c.role == "listed_artist") != listed
                {
                    continue;
                }
                let name = artist_of_pair
                    .get(&(c.artist_source.clone(), c.artist_identifier.clone()))
                    .and_then(|id| artists.get(id))
                    .and_then(|a| a.title.as_deref());
                if let Some(name) = name.filter(|n| !names.contains(n)) {
                    names.push(name);
                }
            }
            names
        };
        let mut names = names_for(true);
        if names.is_empty() {
            names = names_for(false);
        }
        if !names.is_empty() {
            out.insert(entry.entry_id, names.join(", "));
        }
    }
    Ok(out)
}

/// Default and maximum `search` result counts.
const SEARCH_LIMIT: (usize, usize) = (20, 100);

/// Entries with an alias matching `q`, best first, optionally of one `kind`
/// (`entry_type`). Soft-linked duplicates are one result, under the
/// best-ranked of their ids; edition siblings (a track and its instrumental)
/// stay separate, unlike the player's search.
async fn search(state: &Arc<AppState>, params: &HashMap<String, String>) -> HyperResponse {
    let q = params.get("q").map(|q| q.trim()).unwrap_or_default();
    let kind = params
        .get("kind")
        .map(String::as_str)
        .filter(|k| !k.is_empty());
    let limit = match params.get("limit") {
        None => SEARCH_LIMIT.0,
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) => n.clamp(1, SEARCH_LIMIT.1),
            Err(_) => {
                return respond::error(StatusCode::BAD_REQUEST, "invalid_input", "bad limit");
            }
        },
    };
    // Over-fetch: duplicates fold away below.
    let raw = match state
        .db
        .search_entry_ids_by_alias_of_type(q, kind, limit * 2)
        .await
    {
        Ok(ids) => ids,
        Err(e) => return internal(e),
    };
    let summaries = match entry_summaries(&state.db, &raw).await {
        Ok(s) => s,
        Err(e) => return internal(e),
    };
    let mut covered = HashSet::new();
    let hits: Vec<&EntrySummary> = raw
        .iter()
        .filter_map(|id| summaries.get(id))
        .filter(|s| {
            let new = !covered.contains(&s.entry_id);
            covered.insert(s.entry_id);
            covered.extend(s.soft_linked_ids.iter().copied());
            new
        })
        .take(limit)
        .collect();
    let extras = match summary_extras(state, &hits).await {
        Ok(extras) => extras,
        Err(e) => return internal(e),
    };
    let results: Vec<Value> = hits.iter().map(|s| summary_json(s, &extras)).collect();
    respond::json(StatusCode::OK, json!({ "results": results }))
}

/// A pair's own best alias: primary first, then the oldest.
fn pair_title(pair: &PairFacts) -> Option<&str> {
    pair.aliases
        .iter()
        .find(|a| a.primary)
        .or_else(|| pair.aliases.first())
        .map(|a| a.name.as_str())
}

/// `resolve`'s `extra`: every pair with its own title and duration, plus the
/// entry's best alias over all of them (primary first, then the oldest).
/// The top-level `title` instead spans the entry's whole soft-identity
/// component, as the player shows it. A release also gets its `tracklist`
/// (see `tracklist_json`).
fn resolve_extra(pairs: &[PairFacts]) -> serde_json::Value {
    let entry_title = pairs
        .iter()
        .flat_map(|p| &p.aliases)
        .min_by_key(|a| (!a.primary, a.id))
        .map(|a| a.name.as_str());
    let sources: Vec<_> = pairs
        .iter()
        .map(|p| {
            json!({
                "source": p.source,
                "identifier": p.identifier,
                "title": pair_title(p),
                "duration_ms": p.duration_ms,
            })
        })
        .collect();
    json!({"entry_title": entry_title, "sources": sources})
}

async fn resolve(state: &Arc<AppState>, store_id: &str) -> HyperResponse {
    let entry_id = match parse_store_id(store_id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    let summaries = match entry_summaries(&state.db, &[entry_id]).await {
        Ok(s) => s,
        Err(e) => {
            return respond::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                &e.to_string(),
            );
        }
    };
    let Some(entry) = summaries.get(&entry_id) else {
        return respond::not_found();
    };
    let pairs = match state.db.entry_pair_facts(entry_id).await {
        Ok(pairs) => pairs,
        Err(e) => return internal(e),
    };
    let mut extra = resolve_extra(&pairs);
    if entry.entry_type == "release" {
        match tracklist_json(state, entry_id).await {
            Ok(tracks) => extra["tracklist"] = tracks,
            Err(e) => return internal(e),
        }
    }
    let extras = match summary_extras(state, &[entry]).await {
        Ok(extras) => extras,
        Err(e) => return internal(e),
    };
    let mut body = summary_json(entry, &extras);
    body["cover_url"] = Value::Null;
    body["extra"] = extra;
    respond::json(StatusCode::OK, body)
}

/// A release's tracks in order, each as the release lists it: the listed
/// pair's own title (else the track entry's display title) and duration.
async fn tracklist_json(state: &Arc<AppState>, entry_id: i64) -> anyhow::Result<serde_json::Value> {
    let tracks = tracklist(state, entry_id).await?.unwrap_or_default();
    let listed: Vec<(String, String)> = tracks.iter().map(|t| t.pair.clone()).collect();
    let titles = state.db.pair_titles(&listed).await?;
    let untitled: Vec<i64> = tracks
        .iter()
        .filter(|t| !titles.contains_key(&t.pair))
        .map(|t| t.entry_id)
        .collect();
    let fallback = entry_summaries(&state.db, &untitled).await?;
    let rows: Vec<_> = tracks
        .iter()
        .map(|t| {
            let title = titles
                .get(&t.pair)
                .cloned()
                .or_else(|| fallback.get(&t.entry_id).and_then(|s| s.title.clone()));
            json!({
                "store_id": t.entry_id.to_string(),
                "disc": t.position.0,
                "track": t.position.1,
                "title": title,
                "duration_ms": t.duration_ms,
            })
        })
        .collect();
    Ok(json!(rows))
}

/// `contains` suggestions: a release's tracks, first position first
/// (`outgoing`), and with `directions=both` also the releases listing a
/// track (`incoming`, see `appearances`). Other kinds suggest nothing.
/// Each embeds its target's entry summary.
async fn suggest_relations(
    state: &Arc<AppState>,
    store_id: &str,
    params: &HashMap<String, String>,
) -> HyperResponse {
    let entry_id = match parse_store_id(store_id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    let incoming = match params.get("directions").map(String::as_str) {
        None | Some("outgoing") => false,
        Some("both") => true,
        Some(_) => {
            return respond::error(
                StatusCode::BAD_REQUEST,
                "invalid_input",
                "directions must be outgoing or both",
            );
        }
    };
    let tracks = match tracklist(state, entry_id).await {
        Ok(Some(tracks)) => tracks,
        Ok(None) => return respond::not_found(),
        Err(e) => return internal(e),
    };
    let mut found: Vec<(&str, i64, TrackPosition)> = tracks
        .iter()
        .map(|t| ("outgoing", t.entry_id, t.position))
        .collect();
    if incoming {
        match appearances(state, entry_id).await {
            Ok(releases) => found.extend(
                releases
                    .into_iter()
                    .map(|(id, position)| ("incoming", id, position)),
            ),
            Err(e) => return internal(e),
        }
    }

    let target_ids: Vec<i64> = found.iter().map(|&(_, id, _)| id).collect();
    let targets = match entry_summaries(&state.db, &target_ids).await {
        Ok(s) => s,
        Err(e) => return internal(e),
    };
    let target_refs: Vec<&EntrySummary> = targets.values().collect();
    let extras = match summary_extras(state, &target_refs).await {
        Ok(extras) => extras,
        Err(e) => return internal(e),
    };
    let suggestions: Vec<_> = found
        .into_iter()
        .map(|(direction, id, position)| {
            let evidence = match position {
                (Some(d), Some(t)) => format!("tracklist disc {d} track {t}"),
                (None, Some(t)) => format!("tracklist track {t}"),
                _ => "tracklist".to_string(),
            };
            json!({
                "kind": "contains",
                "direction": direction,
                "target_store_id": id.to_string(),
                "target": targets.get(&id).map(|t| summary_json(t, &extras)),
                "confidence": 1.0,
                "evidence": evidence,
            })
        })
        .collect();
    respond::json(StatusCode::OK, json!({ "suggestions": suggestions }))
}

/// The releases listing a track, each at its earliest position there (see
/// `position_order`), oldest release first. The track's soft-linked
/// duplicates count as the track, and soft-linked duplicate releases are one
/// release, under the oldest. Empty for a non-track.
async fn appearances(
    state: &Arc<AppState>,
    entry_id: i64,
) -> anyhow::Result<Vec<(i64, TrackPosition)>> {
    let db = &state.db;
    let summaries = entry_summaries(db, &[entry_id]).await?;
    let Some(track) = summaries.get(&entry_id).filter(|e| e.entry_type == "track") else {
        return Ok(Vec::new());
    };
    let pairs: Vec<(String, String)> = track
        .sources
        .iter()
        .map(|s| (s.source.clone(), s.identifier.clone()))
        .collect();
    let listings = db.child_rows_for_child_pairs(&pairs).await?;
    let parent_pairs: Vec<(String, String)> = listings
        .iter()
        .map(|c| (c.parent_source.clone(), c.parent_identifier.clone()))
        .collect();
    let parent_of_pair: HashMap<(String, String), i64> = db
        .source_rows_for_pairs(&parent_pairs)
        .await?
        .into_iter()
        .map(|s| ((s.source, s.identifier), s.entry_id))
        .collect();
    let parent_ids: Vec<i64> = parent_of_pair.values().copied().collect();
    let releases: HashSet<i64> = db
        .entry_rows_by_ids(&parent_ids)
        .await?
        .into_iter()
        .filter(|e| e.entry_type == "release")
        .map(|e| e.id)
        .collect();

    let mut earliest: BTreeMap<i64, TrackPosition> = BTreeMap::new();
    for c in &listings {
        let pair = (c.parent_source.clone(), c.parent_identifier.clone());
        let Some(&id) = parent_of_pair.get(&pair).filter(|id| releases.contains(id)) else {
            continue;
        };
        let position = (c.disc_no, c.track_no);
        if earliest
            .get(&id)
            .is_none_or(|&p| position_order(position) < position_order(p))
        {
            earliest.insert(id, position);
        }
    }
    let ids: Vec<i64> = earliest.keys().copied().collect();
    let summaries = entry_summaries(db, &ids).await?;
    let mut by_date: Vec<&EntrySummary> = ids.iter().filter_map(|id| summaries.get(id)).collect();
    // Undated last.
    by_date.sort_by_key(|s| (s.release_date.is_none(), s.release_date.clone(), s.entry_id));
    let mut covered = HashSet::new();
    Ok(by_date
        .into_iter()
        .filter(|s| {
            let new = !covered.contains(&s.entry_id);
            covered.insert(s.entry_id);
            covered.extend(s.soft_linked_ids.iter().copied());
            new
        })
        .map(|s| (s.entry_id, earliest[&s.entry_id]))
        .collect())
}

/// `(disc, track)`.
type TrackPosition = (Option<i32>, Option<i32>);

/// Tracklist order: numbered positions first (a listing without numbers,
/// such as a YouTube playlist, says nothing about order), a missing disc
/// counting as disc 1.
fn position_order((disc, track): TrackPosition) -> (bool, i32, Option<i32>) {
    (track.is_none(), disc.unwrap_or(1), track)
}

/// One track entry of a release, at the earliest position (see
/// `position_order`) any of the release's pairs lists it.
struct Track {
    position: TrackPosition,
    entry_id: i64,
    /// The pair listed at `position`.
    pair: (String, String),
    duration_ms: Option<i64>,
}

/// `None` when `entry_id` doesn't exist; otherwise the track entries a
/// release lists, in order (empty for a non-release).
async fn tracklist(
    state: &Arc<AppState>,
    entry_id: i64,
) -> Result<Option<Vec<Track>>, musiclib_rs::musicdb::Error> {
    let db = &state.db;
    let Some(entry) = db.entry_rows_by_ids(&[entry_id]).await?.into_iter().next() else {
        return Ok(None);
    };
    if entry.entry_type != "release" {
        return Ok(Some(Vec::new()));
    }
    let pairs = db.pairs_by_entry_ids(&[entry_id]).await?;
    let children = db.child_rows_for_parent_pairs(&pairs).await?;
    let child_pairs: Vec<(String, String)> = children
        .iter()
        .map(|c| (c.child_source.clone(), c.child_identifier.clone()))
        .collect();
    let child_rows: HashMap<(String, String), (i64, Option<i64>)> = db
        .source_rows_for_pairs(&child_pairs)
        .await?
        .into_iter()
        .map(|s| {
            let duration = s.duration_ms.first().copied();
            ((s.source, s.identifier), (s.entry_id, duration))
        })
        .collect();
    let child_ids: Vec<i64> = child_rows.values().map(|&(id, _)| id).collect();
    let tracks: std::collections::HashSet<i64> = db
        .entry_rows_by_ids(&child_ids)
        .await?
        .into_iter()
        .filter(|e| e.entry_type == "track")
        .map(|e| e.id)
        .collect();
    let mut earliest: BTreeMap<i64, Track> = BTreeMap::new();
    for c in &children {
        let pair = (c.child_source.clone(), c.child_identifier.clone());
        let Some(&(id, duration_ms)) = child_rows.get(&pair) else {
            continue;
        };
        if !tracks.contains(&id) {
            continue;
        }
        let position = (c.disc_no, c.track_no);
        if earliest
            .get(&id)
            .is_some_and(|t| position_order(t.position) <= position_order(position))
        {
            continue;
        }
        earliest.insert(
            id,
            Track {
                position,
                entry_id: id,
                pair,
                duration_ms,
            },
        );
    }
    let mut out: Vec<Track> = earliest.into_values().collect();
    out.sort_by_key(|t| (position_order(t.position), t.entry_id));
    Ok(Some(out))
}

#[derive(Deserialize)]
struct IngestRequest {
    query: String,
}

async fn ingest(state: &Arc<AppState>, body: Bytes) -> HyperResponse {
    if state.providers.is_empty() {
        return respond::error(
            StatusCode::NOT_IMPLEMENTED,
            "unsupported",
            "no providers configured; ingest is disabled",
        );
    }
    let req: IngestRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return respond::error(StatusCode::BAD_REQUEST, "invalid_input", &e.to_string());
        }
    };

    let job_state = Arc::clone(state);
    let job_id = state.jobs.spawn("ingest", move |handle| async move {
        handle.progress("fetching", "running import pipeline");
        let sink = crate::ingest_progress::JobProgressSink::new(
            handle.clone(),
            Arc::clone(&job_state.activity),
            Arc::clone(&job_state.youtube_quota),
        );
        let result = ingest_entry(
            &job_state.db,
            &job_state.providers,
            &job_state.pool,
            job_state.root_id,
            &job_state.dedup_configs,
            &job_state.merged_dedup,
            job_state.soft_cfg.as_ref(),
            req.query,
            Some(sink),
        )
        .await;
        match result {
            Ok(outcome) => match outcome.entry_id {
                Some(id) => handle.succeed(json!({ "store_id": id.to_string() })),
                None => handle.fail("not_found", "no provider recognised the query"),
            },
            Err(e) => handle.fail("provider_error", e.to_string()),
        }
    });

    let body = json!({ "job_id": job_id, "op": "ingest", "status": "pending" });
    Response::builder()
        .status(StatusCode::ACCEPTED)
        .header("Content-Type", "application/json")
        .header("Location", format!("{PREFIX}/jobs/{job_id}"))
        .body(Full::new(Bytes::from(body.to_string())))
        .expect("response builder")
}

fn job_status(state: &Arc<AppState>, id: &str) -> HyperResponse {
    match state.jobs.view(id) {
        Some(view) => respond::json_typed(StatusCode::OK, &view),
        None => respond::not_found(),
    }
}

fn job_cancel(state: &Arc<AppState>, id: &str) -> HyperResponse {
    if state.jobs.cancel(id) {
        job_status(state, id)
    } else {
        respond::not_found()
    }
}
