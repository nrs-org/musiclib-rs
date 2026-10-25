//! The jobs list and its live feed, for the player's Jobs view
//! (docs/plan-jobs-ui.md). One job at a time is `GET`/`DELETE
//! /api/jobs/{id}` (`player.rs`).
//!
//! - `GET /api/jobs?limit=&before=`: `{"running": [...], "finished": [...]}`.
//!   `running` is every pending or running job, oldest first; `finished` the
//!   newest `limit` (default 50, at most 200) recorded jobs created before
//!   `before` (unix seconds), for paging back through history.
//! - `GET /api/jobs/events` (SSE): first `{"type": "snapshot", "running"}`,
//!   then `{"type": "job", "job"}` whenever a job starts, reports progress
//!   (at most about once a second) or ends. A client that falls behind gets
//!   a fresh snapshot.

use std::{collections::HashMap, convert::Infallible, sync::Arc, time::Duration};

use bytes::Bytes;
use futures::StreamExt as _;
use http_body_util::{BodyExt as _, StreamBody};
use hyper::{Method, Response, StatusCode, body::Frame};
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::{
    respond::{self, HyperResponse},
    state::AppState,
};

const KEEPALIVE: Duration = Duration::from_secs(15);

pub fn handles(path: &str) -> bool {
    path == "/api/jobs" || path == "/api/jobs/events"
}

pub async fn route(
    state: &Arc<AppState>,
    method: &Method,
    path: &str,
    query: &HashMap<String, String>,
) -> HyperResponse {
    match (method, path) {
        (&Method::GET, "/api/jobs") => list(state, query).await,
        (&Method::GET, "/api/jobs/events") => events(state),
        _ => respond::method_not_allowed(),
    }
}

async fn list(state: &Arc<AppState>, query: &HashMap<String, String>) -> HyperResponse {
    let limit = query
        .get("limit")
        .and_then(|l| l.parse::<u64>().ok())
        .unwrap_or(50)
        .clamp(1, 200);
    let before = query.get("before").and_then(|b| b.parse::<i64>().ok());
    respond::json(
        StatusCode::OK,
        json!({
            "running": state.jobs.running(),
            "finished": state.jobs.finished(limit, before).await,
        }),
    )
}

fn snapshot(state: &AppState) -> Bytes {
    let body = json!({ "type": "snapshot", "running": state.jobs.running() });
    Bytes::from(format!("data: {body}\n\n"))
}

fn events(state: &Arc<AppState>) -> HyperResponse {
    let rx = state.jobs.subscribe();
    let first = snapshot(state);
    let state = Arc::clone(state);
    let stream = futures::stream::unfold((rx, Some(first)), move |(mut rx, first)| {
        let state = Arc::clone(&state);
        async move {
            if let Some(first) = first {
                let mut chunk = b"retry: 2000\n".to_vec();
                chunk.extend_from_slice(&first);
                return Some((Bytes::from(chunk), (rx, None)));
            }
            let chunk = tokio::select! {
                event = rx.recv() => match event {
                    Ok(job) => {
                        let body = json!({ "type": "job", "job": job });
                        Bytes::from(format!("data: {body}\n\n"))
                    }
                    // Fell behind: start over from the current state.
                    Err(RecvError::Lagged(_)) => snapshot(&state),
                    Err(RecvError::Closed) => return None,
                },
                _ = tokio::time::sleep(KEEPALIVE) => Bytes::from_static(b": keepalive\n\n"),
            };
            Some((chunk, (rx, None)))
        }
    });
    let body = StreamBody::new(stream.map(|b| Ok::<_, Infallible>(Frame::data(b))));
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(body.boxed_unsync())
        .expect("response builder")
}
