//! Server side of the NRS Store Protocol (`nrs-org/nrs`'s
//! `store-protocol/specification.md`), for the `music` entry type. This is
//! the interface `nrs-app` (or any other NRS application) talks to, over
//! plain localhost HTTP — never in-process — so musiclib-rs stays a portable
//! service rather than a Cargo-workspace dependency.
//!
//! Implements: capability negotiation, `resolve`, `lookup`, `ingest` (as a
//! job, per spec §7), the generic job resource, and `suggest_relations`,
//! which proposes `contains` for a release's tracklist. Credits are not
//! mapped to NRS relation kinds yet. `ingest` is reported unsupported when
//! the server started without any provider (no credentials).

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Method, Response, StatusCode};
use serde::Deserialize;
use serde_json::json;

use musiclib_rs::{
    musicdb::PairFacts, pipeline::ingest::ingest_entry, providers::registry::canonicalize,
};

use crate::{
    entries::entry_summaries,
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
        (&Method::GET, p) if p.starts_with("/entries/") => {
            let p = &p["/entries/".len()..];
            match p.strip_suffix("/suggested-relations") {
                Some(id) => suggest_relations(state, id).await,
                None => resolve(state, p).await,
            }
        }
        (&Method::POST, "/ingest") => ingest(state, body).await,
        (&Method::GET, p) if p.starts_with("/jobs/") => job_status(state, &p["/jobs/".len()..]),
        (&Method::DELETE, p) if p.starts_with("/jobs/") => job_cancel(state, &p["/jobs/".len()..]),
        (_, "/capabilities" | "/lookup" | "/ingest") => respond::method_not_allowed(),
        _ => respond::not_found(),
    }
}

fn capabilities(state: &Arc<AppState>) -> HyperResponse {
    respond::json(
        StatusCode::OK,
        json!({
            "store": "musiclib",
            "protocol_version": "0.2",
            "operations": {
                "resolve": true,
                "lookup": true,
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
    respond::json(
        StatusCode::OK,
        json!({
            "store_id": entry_id.to_string(),
            "title": entry.title,
            "kind": entry.entry_type,
            "cover_url": null,
            "release_date": entry.release_date,
            "extra": extra,
        }),
    )
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

/// A release's tracklist as `contains` suggestions: every track entry one of
/// its pairs lists as a child, first position first. Other kinds suggest
/// nothing.
async fn suggest_relations(state: &Arc<AppState>, store_id: &str) -> HyperResponse {
    let entry_id = match parse_store_id(store_id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    let suggestions = match tracklist(state, entry_id).await {
        Ok(Some(tracks)) => tracks,
        Ok(None) => return respond::not_found(),
        Err(e) => return internal(e),
    };
    let suggestions: Vec<_> = suggestions
        .into_iter()
        .map(|t| {
            let evidence = match t.position {
                (Some(d), Some(t)) => format!("tracklist disc {d} track {t}"),
                (None, Some(t)) => format!("tracklist track {t}"),
                _ => "tracklist".to_string(),
            };
            json!({
                "kind": "contains",
                "target_store_id": t.entry_id.to_string(),
                "confidence": 1.0,
                "evidence": evidence,
            })
        })
        .collect();
    respond::json(StatusCode::OK, json!({ "suggestions": suggestions }))
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
