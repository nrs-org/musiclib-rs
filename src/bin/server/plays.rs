//! The player's listening history (`musicdb`'s `play_event`), reported by
//! the frontend as tracks play:
//!
//! - `POST /api/plays` `{"entry_id"}` starts a play: `{"play_id", "started_at"}`.
//! - `POST /api/plays/{id}` `{"listened_seconds"}` reports how much of it has
//!   been heard so far (POST rather than PATCH so `navigator.sendBeacon` can
//!   send the last report as the page closes).
//! - `GET /api/plays/last?since=` is each entry's latest counted play (skips
//!   under `MIN_COUNTED_PLAY_SECONDS` aren't plays), for the shuffle
//!   cooldown: `{"plays": [[entry_id, started_at], ...]}`, unix seconds.
//!
//! Nothing here knows about NRS; the `nrs` extension reads the log on its
//! own schedule (`nrs_bridge::export_listening_loop`).

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use hyper::{Method, StatusCode};
use musiclib_rs::musicdb::Error as DbError;
use serde::Deserialize;
use serde_json::json;

use crate::{
    respond::{self, HyperResponse, parse_query},
    state::AppState,
};

const PREFIX: &str = "/api/plays";

pub fn handles(path: &str) -> bool {
    path == PREFIX || path.starts_with("/api/plays/")
}

pub async fn route(
    state: &Arc<AppState>,
    method: &Method,
    path: &str,
    query: &str,
    body: Bytes,
) -> HyperResponse {
    match (method, &path[PREFIX.len()..]) {
        (&Method::POST, "") => start(state, &body).await,
        (&Method::GET, "/last") => last(state, query).await,
        (&Method::POST, rest) if rest.starts_with('/') => progress(state, &rest[1..], &body).await,
        (_, "" | "/last") => respond::method_not_allowed(),
        _ => respond::not_found(),
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn bad_request(message: &str) -> HyperResponse {
    respond::error(StatusCode::BAD_REQUEST, "invalid_input", message)
}

fn db_error(e: DbError) -> HyperResponse {
    match e {
        DbError::InvalidInput(msg) => bad_request(&msg),
        DbError::Database(_) => respond::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            &e.to_string(),
        ),
    }
}

#[derive(Deserialize)]
struct StartBody {
    entry_id: i64,
}

async fn start(state: &Arc<AppState>, body: &[u8]) -> HyperResponse {
    let Ok(StartBody { entry_id }) = serde_json::from_slice(body) else {
        return bad_request("expected {\"entry_id\": <number>}");
    };
    match state.db.entry_rows_by_ids(&[entry_id]).await {
        Ok(rows) if rows.is_empty() => return respond::not_found(),
        Ok(_) => {}
        Err(e) => return db_error(e),
    }
    let started_at = now();
    match state.db.start_play(entry_id, started_at).await {
        Ok(play_id) => respond::json(
            StatusCode::OK,
            json!({ "play_id": play_id, "started_at": started_at }),
        ),
        Err(e) => db_error(e),
    }
}

#[derive(Deserialize)]
struct ProgressBody {
    listened_seconds: f64,
}

async fn progress(state: &Arc<AppState>, id: &str, body: &[u8]) -> HyperResponse {
    let Ok(play_id) = id.parse::<i64>() else {
        return bad_request("play id must be numeric");
    };
    let Ok(ProgressBody { listened_seconds }) = serde_json::from_slice(body) else {
        return bad_request("expected {\"listened_seconds\": <number>}");
    };
    match state.db.set_play_listened(play_id, listened_seconds).await {
        Ok(true) => respond::json(StatusCode::OK, json!({ "ok": true })),
        Ok(false) => respond::not_found(),
        Err(e) => db_error(e),
    }
}

async fn last(state: &Arc<AppState>, query: &str) -> HyperResponse {
    let since = parse_query(query)
        .get("since")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    match state.db.last_counted_plays(since).await {
        Ok(plays) => respond::json(StatusCode::OK, json!({ "plays": plays })),
        Err(e) => db_error(e),
    }
}
