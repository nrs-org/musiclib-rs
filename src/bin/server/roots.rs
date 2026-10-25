//! Update roots (`musicdb`'s `update_root`): the URLs the library update
//! walks, each with its fetch options. See docs/plan-update-roots.md.
//!
//! - `GET /api/roots`: `{"roots": [...]}` in the order added, each with the
//!   library entry its URL resolved to (`entry_id`, `title`; `null` until
//!   it's been imported) and the outcome of its last update.
//! - `POST /api/roots` `{"url", "fetch_options": {"file", "entry_point"?},
//!   "label"?}` adds one: the new root. The URL is canonicalized; a second
//!   root for the same canonical URL is refused.
//! - `PATCH /api/roots/{id}` `{"fetch_options"?, "label"?, "enabled"?}`
//!   edits (`"label": null` clears it); `DELETE` removes. Removing a root
//!   only stops updating it: what it imported stays in the library.
//!
//! - `POST /api/roots/preview` `{"url", "fetch_options"}`: what those fetch
//!   options decide for each of the URL's direct children (fetched, skipped,
//!   undecided), without importing anything: `{"root", "existing_root_id",
//!   "rules", "listings", "errors"}`. `rules` are the entry point's
//!   `child_rules` as written; each child's `rule` is a 1-based index into
//!   them (`pipeline::update::preview_root`).
//!
//! Fetch options are checked when saved, by loading them; a file broken
//! later shows up when an update runs the root.

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use hyper::{Method, StatusCode};
use musiclib_rs::{
    musicdb::{Error as DbError, NewUpdateRoot, UpdateRootEdit, UpdateRootRow},
    pipeline::update::preview_root,
    providers::{fetch_options_yaml::YamlFetchDocument, registry::canonicalize},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    entries::entry_summaries,
    fetch_options,
    player::fetch_options_dir,
    respond::{self, HyperResponse},
    state::AppState,
};

const PREFIX: &str = "/api/roots";

pub fn handles(path: &str) -> bool {
    path == PREFIX || path.starts_with("/api/roots/")
}

pub async fn route(
    state: &Arc<AppState>,
    method: &Method,
    path: &str,
    body: Bytes,
) -> HyperResponse {
    let rest = &path[PREFIX.len()..];
    let segments: Vec<&str> = rest.split('/').skip(1).collect();
    match (method, segments.as_slice()) {
        (&Method::GET, []) => list(state).await,
        (&Method::POST, []) => create(state, &body).await,
        (_, []) => respond::method_not_allowed(),
        (&Method::POST, ["preview"]) => preview(state, &body).await,
        (_, ["preview"]) => respond::method_not_allowed(),
        (_, [id]) => {
            let Ok(id) = id.parse::<i64>() else {
                return bad_request("root id must be numeric");
            };
            match *method {
                Method::PATCH => edit(state, id, &body).await,
                Method::DELETE => delete(state, id).await,
                _ => respond::method_not_allowed(),
            }
        }
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

fn internal(message: &str) -> HyperResponse {
    respond::error(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
}

fn db_error(e: DbError) -> HyperResponse {
    match e {
        DbError::InvalidInput(msg) | DbError::Blocked(msg) => bad_request(&msg),
        DbError::Database(_) => internal(&e.to_string()),
    }
}

#[derive(Deserialize)]
struct FetchOptionsBody {
    file: String,
    #[serde(default = "default_entry_point")]
    entry_point: String,
}

fn default_entry_point() -> String {
    fetch_options::DEFAULT_ENTRY_POINT.to_string()
}

/// `fetch_options` as a `(file, entry_point)` pair, or the response to send
/// when they don't load.
async fn checked_fetch_options(fo: FetchOptionsBody) -> Result<(String, String), HyperResponse> {
    match fetch_options::resolve(&fetch_options_dir(), &fo.file, &fo.entry_point).await {
        Some(_) => Ok((fo.file, fo.entry_point)),
        None => Err(bad_request(&format!(
            "unknown fetch_options file/entry_point {:?}/{:?}",
            fo.file, fo.entry_point
        ))),
    }
}

fn root_json(r: &UpdateRootRow, title: Option<&str>) -> Value {
    let last_stats = r
        .last_stats
        .as_deref()
        .and_then(|s| serde_json::from_str::<Value>(s).ok());
    json!({
        "id": r.id,
        "url": r.url,
        "source": r.source,
        "identifier": r.identifier,
        "fetch_options": { "file": r.fetch_options_file, "entry_point": r.entry_point },
        "label": r.label,
        "enabled": r.enabled,
        "added_at": r.added_at,
        "entry_id": r.entry_id,
        "title": title,
        "last_run": r.last_run_at.map(|at| json!({
            "at": at,
            "status": r.last_status,
            "error": r.last_error,
            "stats": last_stats,
        })),
    })
}

/// `roots` as JSON, with their entries' titles.
async fn roots_json(
    state: &Arc<AppState>,
    roots: &[UpdateRootRow],
) -> Result<Vec<Value>, HyperResponse> {
    let ids: Vec<i64> = roots.iter().filter_map(|r| r.entry_id).collect();
    let summaries = entry_summaries(&state.db, &ids)
        .await
        .map_err(|e| internal(&e.to_string()))?;
    Ok(roots
        .iter()
        .map(|r| {
            let title = r
                .entry_id
                .and_then(|id| summaries.get(&id))
                .and_then(|s| s.title.as_deref());
            root_json(r, title)
        })
        .collect())
}

async fn one_json(state: &Arc<AppState>, id: i64, status: StatusCode) -> HyperResponse {
    match state.db.update_root(id).await {
        Ok(Some(root)) => match roots_json(state, std::slice::from_ref(&root)).await {
            Ok(mut out) => respond::json(status, out.remove(0)),
            Err(response) => response,
        },
        Ok(None) => respond::not_found(),
        Err(e) => db_error(e),
    }
}

async fn list(state: &Arc<AppState>) -> HyperResponse {
    let roots = match state.db.update_roots().await {
        Ok(roots) => roots,
        Err(e) => return db_error(e),
    };
    match roots_json(state, &roots).await {
        Ok(roots) => respond::json(StatusCode::OK, json!({ "roots": roots })),
        Err(response) => response,
    }
}

#[derive(Deserialize)]
struct CreateBody {
    url: String,
    fetch_options: FetchOptionsBody,
    #[serde(default)]
    label: Option<String>,
}

async fn create(state: &Arc<AppState>, body: &[u8]) -> HyperResponse {
    let req: CreateBody = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return bad_request(&e.to_string()),
    };
    let url = req.url.trim().to_string();
    let Some(canonical) = canonicalize(&url) else {
        return bad_request(&format!("no provider recognises {url:?}"));
    };
    let (fetch_options_file, entry_point) = match checked_fetch_options(req.fetch_options).await {
        Ok(fo) => fo,
        Err(response) => return response,
    };
    let root = NewUpdateRoot {
        url,
        source: canonical.canonical_source_key.into_owned(),
        identifier: canonical.canonical_identifier,
        fetch_options_file,
        entry_point,
        label: req.label,
    };
    match state.db.add_update_root(&root, now()).await {
        Ok(id) => one_json(state, id, StatusCode::CREATED).await,
        Err(e) => db_error(e),
    }
}

/// A field that can be absent (leave it), `null` (clear it) or a value.
fn double_option<'de, D, T>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(de).map(Some)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditBody {
    #[serde(default)]
    fetch_options: Option<FetchOptionsBody>,
    #[serde(default, deserialize_with = "double_option")]
    label: Option<Option<String>>,
    #[serde(default)]
    enabled: Option<bool>,
}

async fn edit(state: &Arc<AppState>, id: i64, body: &[u8]) -> HyperResponse {
    let req: EditBody = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return bad_request(&e.to_string()),
    };
    let fetch_options = match req.fetch_options {
        Some(fo) => match checked_fetch_options(fo).await {
            Ok(fo) => Some(fo),
            Err(response) => return response,
        },
        None => None,
    };
    let edit = UpdateRootEdit {
        fetch_options,
        label: req.label,
        enabled: req.enabled,
    };
    match state.db.edit_update_root(id, &edit).await {
        Ok(true) => one_json(state, id, StatusCode::OK).await,
        Ok(false) => respond::not_found(),
        Err(e) => db_error(e),
    }
}

async fn delete(state: &Arc<AppState>, id: i64) -> HyperResponse {
    match state.db.delete_update_root(id).await {
        Ok(true) => respond::json(StatusCode::OK, json!({ "deleted": id })),
        Ok(false) => respond::not_found(),
        Err(e) => db_error(e),
    }
}

#[derive(Deserialize)]
struct PreviewBody {
    url: String,
    fetch_options: FetchOptionsBody,
}

/// The entry point's rules as written in its file (the built-in preset's
/// as generated from its pool), each with an explicit `"fetch"`: an omitted
/// one parses as `fetch: null`, a skip.
async fn rules_json(file: &str, entry_point: &str) -> Value {
    let mut rules = written_rules(file, entry_point).await;
    if let Value::Array(rules) = &mut rules {
        for rule in rules {
            if let Value::Object(rule) = rule {
                rule.entry("fetch").or_insert(Value::Null);
            }
        }
    }
    rules
}

async fn written_rules(file: &str, entry_point: &str) -> Value {
    if file == fetch_options::SHALLOW_ID {
        let (pool, root_id) = fetch_options::shallow();
        let doc = YamlFetchDocument::from_pool(&pool, root_id);
        return serde_json::to_value(&doc.0["main"].child_rules).unwrap_or_default();
    }
    let path = fetch_options_dir().join(format!("{file}.yaml"));
    let Ok(text) = tokio::fs::read_to_string(&path).await else {
        return Value::Null;
    };
    serde_yaml_ng::from_str::<YamlFetchDocument>(&text)
        .ok()
        .and_then(|doc| {
            doc.0
                .get(entry_point)
                .and_then(|o| serde_json::to_value(&o.child_rules).ok())
        })
        .unwrap_or(Value::Null)
}

async fn preview(state: &Arc<AppState>, body: &[u8]) -> HyperResponse {
    let req: PreviewBody = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return bad_request(&e.to_string()),
    };
    let url = req.url.trim().to_string();
    let (file, entry_point) = (req.fetch_options.file, req.fetch_options.entry_point);
    let Some((pool, root_id)) =
        fetch_options::resolve(&fetch_options_dir(), &file, &entry_point).await
    else {
        return bad_request(&format!(
            "unknown fetch_options file/entry_point {file:?}/{entry_point:?}"
        ));
    };
    let report = match preview_root(&state.providers, &url, pool, root_id).await {
        Ok(report) => report,
        Err(e) => return bad_request(&format!("{e:#}")),
    };
    let existing_root_id = match state.db.update_roots().await {
        Ok(roots) => roots
            .iter()
            .find(|r| r.source == report.source && r.identifier == report.identifier)
            .map(|r| r.id),
        Err(e) => return db_error(e),
    };
    respond::json(
        StatusCode::OK,
        json!({
            "root": {
                "source": report.source,
                "identifier": report.identifier,
                "entry_type": report.entry_type,
                "name": report.name,
            },
            "existing_root_id": existing_root_id,
            "rules": rules_json(&file, &entry_point).await,
            "listings": report.listings,
            "errors": report.errors,
        }),
    )
}
