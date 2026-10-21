//! The player's playlists (`musicdb`'s `playlist`, `playlist_item`): named,
//! ordered lists of tracks. Playing one is the frontend's business — it
//! sends the items as an ordinary `set_queue` command — so nothing here
//! touches the playback session.
//!
//! - `GET /api/playlists`: `{"playlists": [{id, name, item_count, ...}]}`,
//!   most recently changed first.
//! - `POST /api/playlists` `{"name"}` creates one: the new playlist.
//! - `GET /api/playlists/{id}`: the playlist with its `items`, each with the
//!   entry it resolves to now (`null` once its anchor is gone), title,
//!   artists, and whether it's playable.
//! - `PATCH /api/playlists/{id}` `{"name"}` renames; `DELETE` deletes.
//! - `POST /api/playlists/{id}/items` `{"entry_ids", "allow_duplicates"?}`
//!   appends: `{"added": [item_id], "duplicates": [entry_id],
//!   "missing": [entry_id]}`. Entries already in the playlist are skipped
//!   unless `allow_duplicates`; `missing` are unknown or sourceless
//!   entries, and anything that isn't a track.
//! - `DELETE /api/playlists/{id}/items/{item_id}` removes one item.
//! - `PUT /api/playlists/{id}/order` `{"item_ids"}` reorders: every item,
//!   once, in the new order.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use futures::future::join_all;
use hyper::{Method, StatusCode};
use musiclib_rs::musicdb::{Error as DbError, NewPlaylistItem, PlaylistRow};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    entries::entry_summaries,
    player::embed_for,
    respond::{self, HyperResponse},
    state::AppState,
};

const PREFIX: &str = "/api/playlists";

pub fn handles(path: &str) -> bool {
    path == PREFIX || path.starts_with("/api/playlists/")
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
        (_, [""]) => respond::not_found(),
        (_, [id, tail @ ..]) => {
            let Ok(id) = id.parse::<i64>() else {
                return bad_request("playlist id must be numeric");
            };
            match (method, tail) {
                (&Method::GET, []) => show(state, id).await,
                (&Method::PATCH, []) => rename(state, id, &body).await,
                (&Method::DELETE, []) => delete(state, id).await,
                (&Method::POST, ["items"]) => add_items(state, id, &body).await,
                (&Method::DELETE, ["items", item_id]) => match item_id.parse::<i64>() {
                    Ok(item_id) => remove_item(state, id, item_id).await,
                    Err(_) => bad_request("item id must be numeric"),
                },
                (&Method::PUT, ["order"]) => reorder(state, id, &body).await,
                (_, [] | ["items"] | ["items", _] | ["order"]) => respond::method_not_allowed(),
                _ => respond::not_found(),
            }
        }
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

fn playlist_json(p: &PlaylistRow) -> Value {
    json!({
        "id": p.id,
        "name": p.name,
        "item_count": p.item_count,
        "created_at": p.created_at,
        "updated_at": p.updated_at,
    })
}

/// The playlist as it is now, or the response to send instead.
async fn current(state: &Arc<AppState>, id: i64) -> Result<PlaylistRow, HyperResponse> {
    match state.db.playlist(id).await {
        Ok(Some(p)) => Ok(p),
        Ok(None) => Err(respond::not_found()),
        Err(e) => Err(db_error(e)),
    }
}

async fn list(state: &Arc<AppState>) -> HyperResponse {
    match state.db.playlists().await {
        Ok(rows) => respond::json(
            StatusCode::OK,
            json!({ "playlists": rows.iter().map(playlist_json).collect::<Vec<_>>() }),
        ),
        Err(e) => db_error(e),
    }
}

#[derive(Deserialize)]
struct NameBody {
    name: String,
}

async fn create(state: &Arc<AppState>, body: &[u8]) -> HyperResponse {
    let Ok(NameBody { name }) = serde_json::from_slice(body) else {
        return bad_request("expected {\"name\": <string>}");
    };
    let id = match state.db.create_playlist(&name, now()).await {
        Ok(id) => id,
        Err(e) => return db_error(e),
    };
    match current(state, id).await {
        Ok(p) => respond::json(StatusCode::CREATED, playlist_json(&p)),
        Err(response) => response,
    }
}

async fn rename(state: &Arc<AppState>, id: i64, body: &[u8]) -> HyperResponse {
    let Ok(NameBody { name }) = serde_json::from_slice(body) else {
        return bad_request("expected {\"name\": <string>}");
    };
    match state.db.rename_playlist(id, &name, now()).await {
        Ok(true) => match current(state, id).await {
            Ok(p) => respond::json(StatusCode::OK, playlist_json(&p)),
            Err(response) => response,
        },
        Ok(false) => respond::not_found(),
        Err(e) => db_error(e),
    }
}

async fn delete(state: &Arc<AppState>, id: i64) -> HyperResponse {
    match state.db.delete_playlist(id).await {
        Ok(true) => respond::json(StatusCode::OK, json!({ "ok": true })),
        Ok(false) => respond::not_found(),
        Err(e) => db_error(e),
    }
}

async fn show(state: &Arc<AppState>, id: i64) -> HyperResponse {
    let playlist = match current(state, id).await {
        Ok(p) => p,
        Err(response) => return response,
    };
    let items = match state.db.playlist_items(id).await {
        Ok(items) => items,
        Err(e) => return db_error(e),
    };
    let entry_ids: Vec<i64> = items
        .iter()
        .filter_map(|i| i.entry_id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let summaries = match entry_summaries(&state.db, &entry_ids).await {
        Ok(s) => s,
        Err(e) => return internal(&e.to_string()),
    };
    // Credits are per soft-identity, like the graph's: one line per entry.
    let lines = join_all(
        entry_ids
            .iter()
            .map(|&id| crate::playback::artist_line(&state.db, id, None)),
    )
    .await;
    let artists: HashMap<i64, Vec<String>> = entry_ids
        .iter()
        .zip(lines)
        .map(|(&id, line)| {
            let names = line
                .map(|artists| artists.into_iter().map(|a| a.name).collect())
                .unwrap_or_default();
            (id, names)
        })
        .collect();
    let items: Vec<Value> = items
        .iter()
        .map(|item| {
            let summary = item.entry_id.and_then(|id| summaries.get(&id));
            let playable = summary.is_some_and(|s| {
                s.sources
                    .iter()
                    .any(|src| embed_for(&src.source, &src.identifier).is_some())
            });
            json!({
                "item_id": item.id,
                "entry_id": item.entry_id,
                "title": summary.and_then(|s| s.title.clone()).or_else(|| item.title.clone()),
                "artists": item.entry_id.and_then(|id| artists.get(&id)).cloned().unwrap_or_default(),
                "playable": playable,
                "added_at": item.added_at,
            })
        })
        .collect();
    let mut body = playlist_json(&playlist);
    body["items"] = Value::Array(items);
    respond::json(StatusCode::OK, body)
}

#[derive(Deserialize)]
struct AddBody {
    entry_ids: Vec<i64>,
    #[serde(default)]
    allow_duplicates: bool,
}

async fn add_items(state: &Arc<AppState>, id: i64, body: &[u8]) -> HyperResponse {
    let Ok(AddBody {
        entry_ids,
        allow_duplicates,
    }) = serde_json::from_slice(body)
    else {
        return bad_request("expected {\"entry_ids\": [<number>, ...]}");
    };
    if let Err(response) = current(state, id).await {
        return response;
    }
    let existing = match state.db.playlist_items(id).await {
        Ok(items) => items,
        Err(e) => return db_error(e),
    };
    let existing_ids: Vec<i64> = existing.iter().filter_map(|i| i.entry_id).collect();
    let mut wanted: Vec<i64> = entry_ids.clone();
    wanted.extend(&existing_ids);
    wanted.sort_unstable();
    wanted.dedup();
    let summaries = match entry_summaries(&state.db, &wanted).await {
        Ok(s) => s,
        Err(e) => return internal(&e.to_string()),
    };
    // What's in the playlist already, soft-linked copies included: the
    // same track under another entry is still the same track.
    let mut present: HashSet<i64> = HashSet::new();
    for id in existing_ids {
        present.insert(id);
        if let Some(s) = summaries.get(&id) {
            present.extend(&s.soft_linked_ids);
        }
    }

    let mut new_items = Vec::new();
    let (mut duplicates, mut missing) = (Vec::new(), Vec::new());
    for entry_id in entry_ids {
        if summaries
            .get(&entry_id)
            .is_none_or(|s| s.entry_type != "track")
        {
            missing.push(entry_id);
            continue;
        }
        if !allow_duplicates && present.contains(&entry_id) {
            duplicates.push(entry_id);
            continue;
        }
        let anchor = match state.db.playlist_anchor(entry_id).await {
            Ok(anchor) => anchor,
            Err(e) => return db_error(e),
        };
        let Some((source, identifier)) = anchor else {
            missing.push(entry_id);
            continue;
        };
        if !allow_duplicates {
            present.insert(entry_id);
            if let Some(s) = summaries.get(&entry_id) {
                present.extend(&s.soft_linked_ids);
            }
        }
        new_items.push(NewPlaylistItem {
            source,
            identifier,
            title: summaries.get(&entry_id).and_then(|s| s.title.clone()),
        });
    }
    let added = if new_items.is_empty() {
        Vec::new()
    } else {
        match state.db.add_playlist_items(id, &new_items, now()).await {
            Ok(Some(ids)) => ids,
            Ok(None) => return respond::not_found(),
            Err(e) => return db_error(e),
        }
    };
    respond::json(
        StatusCode::OK,
        json!({ "added": added, "duplicates": duplicates, "missing": missing }),
    )
}

async fn remove_item(state: &Arc<AppState>, id: i64, item_id: i64) -> HyperResponse {
    match state.db.remove_playlist_item(id, item_id, now()).await {
        Ok(true) => respond::json(StatusCode::OK, json!({ "ok": true })),
        Ok(false) => respond::not_found(),
        Err(e) => db_error(e),
    }
}

#[derive(Deserialize)]
struct OrderBody {
    item_ids: Vec<i64>,
}

async fn reorder(state: &Arc<AppState>, id: i64, body: &[u8]) -> HyperResponse {
    let Ok(OrderBody { item_ids }) = serde_json::from_slice(body) else {
        return bad_request("expected {\"item_ids\": [<number>, ...]}");
    };
    match state.db.reorder_playlist(id, &item_ids, now()).await {
        Ok(true) => respond::json(StatusCode::OK, json!({ "ok": true })),
        Ok(false) => respond::not_found(),
        Err(e) => db_error(e),
    }
}
