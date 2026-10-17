//! Shared HTTP response helpers, in the same minimal-hyper style as
//! `src/bin/ytdlp_server.rs` (no framework — see that file for the
//! request-handling loop this binary's `main.rs` mirrors).

use std::collections::HashMap;

use std::convert::Infallible;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use hyper::{Response, StatusCode};
use serde::Serialize;
use serde_json::Value;

/// Boxed so a handler can stream (the player's SSE feed, see `playback`)
/// as well as return a plain buffered body (`full`).
pub type HyperResponse = Response<UnsyncBoxBody<Bytes, Infallible>>;

pub fn full(body: impl Into<Bytes>) -> UnsyncBoxBody<Bytes, Infallible> {
    Full::new(body.into()).boxed_unsync()
}

pub fn json(status: StatusCode, body: Value) -> HyperResponse {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(full(body.to_string()))
        .expect("response builder")
}

pub fn json_typed<T: Serialize>(status: StatusCode, body: &T) -> HyperResponse {
    match serde_json::to_value(body) {
        Ok(v) => json(status, v),
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            &e.to_string(),
        ),
    }
}

/// Error body per store-protocol/specification.md §9: `{"code", "message"}`.
pub fn error(status: StatusCode, code: &str, message: &str) -> HyperResponse {
    json(
        status,
        serde_json::json!({ "code": code, "message": message }),
    )
}

pub fn not_found() -> HyperResponse {
    error(StatusCode::NOT_FOUND, "not_found", "no such resource")
}

/// Percent-decoded `key=value` pairs of a query string (later keys win).
pub fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|p| {
            let mut it = p.splitn(2, '=');
            let k = it.next()?;
            let v = it.next().unwrap_or("");
            Some((
                urlencoding::decode(k).ok()?.into_owned(),
                urlencoding::decode(v).ok()?.into_owned(),
            ))
        })
        .collect()
}

pub fn method_not_allowed() -> HyperResponse {
    error(
        StatusCode::METHOD_NOT_ALLOWED,
        "invalid_input",
        "method not allowed for this resource",
    )
}
