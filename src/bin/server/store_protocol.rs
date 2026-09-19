//! Server side of the NRS Store Protocol (`nrs-org/nrs`'s
//! `store-protocol/specification.md`), for the `music` entry type. This is
//! the interface `nrs-app` (or any other NRS application) talks to, over
//! plain localhost HTTP — never in-process — so musiclib-rs stays a portable
//! service rather than a Cargo-workspace dependency.
//!
//! Implements: capability negotiation, `resolve`, `ingest` (as a job, per
//! spec §7), and the generic job resource. `suggest_relations` is reported
//! unsupported (`operations.suggest_relations: false`) — nothing in
//! musiclib-rs yet maps credits/tracklists to NRS relation kinds.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Method, Response, StatusCode};
use serde::Deserialize;
use serde_json::json;

use musiclib_rs::pipeline::ingest::ingest_entry;

use crate::{
    entries::entry_summaries,
    respond::{self, HyperResponse},
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
    body: Bytes,
) -> HyperResponse {
    let rest = path.strip_prefix(PREFIX).unwrap_or("");
    match (method, rest) {
        (&Method::GET, "/capabilities") => capabilities(),
        (&Method::GET, p) if p.starts_with("/entries/") => {
            resolve(state, &p["/entries/".len()..]).await
        }
        (&Method::POST, "/ingest") => ingest(state, body).await,
        (&Method::GET, p) if p.starts_with("/jobs/") => job_status(state, &p["/jobs/".len()..]),
        (&Method::DELETE, p) if p.starts_with("/jobs/") => job_cancel(state, &p["/jobs/".len()..]),
        (_, "/capabilities" | "/ingest") => respond::method_not_allowed(),
        _ => respond::not_found(),
    }
}

fn capabilities() -> HyperResponse {
    respond::json(
        StatusCode::OK,
        json!({
            "store": "musiclib",
            "protocol_version": "0.1",
            "operations": {
                "resolve": true,
                "ingest": true,
                "suggest_relations": false,
            },
            "jobs": {"streaming": false},
        }),
    )
}

async fn resolve(state: &Arc<AppState>, store_id: &str) -> HyperResponse {
    let Ok(entry_id) = store_id.parse::<i64>() else {
        return respond::error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "store_id must be an entry id",
        );
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
    respond::json(
        StatusCode::OK,
        json!({
            "store_id": entry_id.to_string(),
            "title": entry.title,
            "kind": entry.entry_type,
            "cover_url": null,
            "release_date": entry.release_date,
            "extra": {},
        }),
    )
}

#[derive(Deserialize)]
struct IngestRequest {
    query: String,
}

async fn ingest(state: &Arc<AppState>, body: Bytes) -> HyperResponse {
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
