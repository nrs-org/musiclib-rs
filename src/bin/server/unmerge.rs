//! Unmerging an entry from the player: splitting off pairs that were merged
//! into it, by storing a dedup barrier (`barrier_group`) that keeps them
//! apart and regrouping the entry right away.
//!
//! - `GET /api/entries/{id}/identity` — the entry's pairs, the identity
//!   links between them (`pair_link`) with what stated each, and the
//!   barriers holding any of its pairs.
//! - `POST /api/entries/{id}/split` `{ "move": [{source, identifier}] }` —
//!   store a barrier with the moved pairs under one anchor and the rest
//!   under another, then regroup: the moved pairs become their own entry.
//! - `DELETE /api/barriers/{id}` — undo a barrier: delete it and regroup
//!   the entries its pairs are in, which merges back whatever links still
//!   join them.
//!
//! An unmerge pins both sides: each side's pairs stay together even if a
//! later re-import drops a link between them, until the barrier is undone.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use hyper::StatusCode;
use musiclib_rs::{
    musicdb::{BarrierGroupRow, LINKS_COMPLETE, MusicDb},
    pipeline::dedup::regroup_entries,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    respond::{self, HyperResponse},
    state::AppState,
};

type Pair = (String, String);

#[derive(Deserialize)]
struct PairRef {
    source: String,
    identifier: String,
}

#[derive(Deserialize)]
struct SplitRequest {
    #[serde(rename = "move")]
    move_pairs: Vec<PairRef>,
}

fn internal(e: impl std::fmt::Display) -> HyperResponse {
    respond::error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        &e.to_string(),
    )
}

fn pair_json(pair: &Pair) -> Value {
    json!({ "source": pair.0, "identifier": pair.1 })
}

/// Each pair's own best name: primary first, then the oldest.
async fn names_of(db: &MusicDb, pairs: &[Pair]) -> anyhow::Result<HashMap<Pair, String>> {
    let mut best: HashMap<Pair, (bool, String)> = HashMap::new();
    for a in db.alias_rows_for_pairs(pairs).await? {
        let key = (a.source, a.identifier);
        if best
            .get(&key)
            .is_none_or(|(primary, _)| a.primary_alias && !primary)
        {
            best.insert(key, (a.primary_alias, a.name));
        }
    }
    Ok(best.into_iter().map(|(k, (_, n))| (k, n)).collect())
}

/// The barriers with a pair in `pairs`, each member with the entry it is in.
async fn barriers_touching(db: &MusicDb, pairs: &HashSet<Pair>) -> anyhow::Result<Vec<Value>> {
    let mut out = Vec::new();
    for group in db.barrier_groups().await? {
        if !group.anchors.values().flatten().any(|p| pairs.contains(p)) {
            continue;
        }
        out.push(barrier_json(db, &group).await?);
    }
    Ok(out)
}

async fn barrier_json(db: &MusicDb, group: &BarrierGroupRow) -> anyhow::Result<Value> {
    let mut anchors = Vec::new();
    for (name, members) in &group.anchors {
        let mut list = Vec::new();
        for pair in members {
            let entry_id = db.find_entry_id_by_pair(&pair.0, &pair.1).await?;
            list.push(json!({ "source": pair.0, "identifier": pair.1, "entry_id": entry_id }));
        }
        anchors.push(json!({ "name": name, "pairs": list }));
    }
    Ok(json!({
        "id": group.id,
        "name": group.name,
        "origin": group.origin,
        "created_at": group.created_at,
        "anchors": anchors,
    }))
}

/// `GET /api/entries/{id}/identity`.
pub async fn identity(state: &Arc<AppState>, entry_id: i64) -> HyperResponse {
    match identity_json(&state.db, entry_id).await {
        Ok(Some(body)) => respond::json(StatusCode::OK, body),
        Ok(None) => respond::not_found(),
        Err(e) => internal(e),
    }
}

async fn identity_json(db: &MusicDb, entry_id: i64) -> anyhow::Result<Option<Value>> {
    let rows = db.source_rows_by_entry_ids(&[entry_id]).await?;
    if rows.is_empty() {
        return Ok(None);
    }
    let pairs: Vec<Pair> = rows
        .iter()
        .map(|r| (r.source.clone(), r.identifier.clone()))
        .collect();
    let names = names_of(db, &pairs).await?;
    let mut links: BTreeMap<(Pair, Pair), Vec<Value>> = BTreeMap::new();
    for row in db.links_among(&pairs).await? {
        links.entry(row.link).or_default().push(json!({
            "source": row.asserted_by.0,
            "identifier": row.asserted_by.1,
            "via": row.via,
        }));
    }
    let pair_set: HashSet<Pair> = pairs.iter().cloned().collect();
    Ok(Some(json!({
        "entry_id": entry_id,
        "links_complete": db.setting(LINKS_COMPLETE).await?.as_deref() == Some("1"),
        "pairs": rows
            .iter()
            .map(|r| {
                let pair = (r.source.clone(), r.identifier.clone());
                json!({
                    "source": r.source,
                    "identifier": r.identifier,
                    "name": names.get(&pair),
                    "fetched": r.fetched_at.is_some(),
                    "gone_at": r.gone_at,
                })
            })
            .collect::<Vec<_>>(),
        "links": links
            .into_iter()
            .map(|((a, b), stated_by)| json!({ "a": pair_json(&a), "b": pair_json(&b), "stated_by": stated_by }))
            .collect::<Vec<_>>(),
        "barriers": barriers_touching(db, &pair_set).await?,
    })))
}

/// `POST /api/entries/{id}/split`.
pub async fn split(state: &Arc<AppState>, entry_id: i64, body: &[u8]) -> HyperResponse {
    let req: SplitRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(e) => return respond::error(StatusCode::BAD_REQUEST, "invalid_input", &e.to_string()),
    };
    let db = &state.db;
    let pairs: Vec<Pair> = match db.pairs_by_entry_ids(&[entry_id]).await {
        Ok(pairs) if !pairs.is_empty() => pairs,
        Ok(_) => return respond::not_found(),
        Err(e) => return internal(e),
    };
    let moved: HashSet<Pair> = req
        .move_pairs
        .into_iter()
        .map(|p| (p.source, p.identifier))
        .collect();
    if let Some(stray) = moved.iter().find(|p| !pairs.contains(p)) {
        return respond::error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            &format!("{}:{} is not in entry {entry_id}", stray.0, stray.1),
        );
    }
    let kept: Vec<Pair> = pairs
        .iter()
        .filter(|p| !moved.contains(*p))
        .cloned()
        .collect();
    if moved.is_empty() || kept.is_empty() {
        return respond::error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "move some of the entry's sources, not none or all",
        );
    }
    let mut moved: Vec<Pair> = moved.into_iter().collect();
    moved.sort();
    match split_entry(state, entry_id, kept, moved).await {
        Ok(body) => {
            state.playback.recredit();
            respond::json(StatusCode::OK, body)
        }
        Err(e) => internal(e),
    }
}

async fn split_entry(
    state: &Arc<AppState>,
    entry_id: i64,
    kept: Vec<Pair>,
    moved: Vec<Pair>,
) -> anyhow::Result<Value> {
    let db = &state.db;
    let all: Vec<Pair> = kept.iter().chain(&moved).cloned().collect();
    let names = names_of(db, &all).await?;
    let label = |side: &[Pair]| {
        side.iter()
            .find_map(|p| names.get(p).cloned())
            .unwrap_or_else(|| format!("{}:{}", side[0].0, side[0].1))
    };
    let name = format!("{} / {} (entry {entry_id})", label(&kept), label(&moved));
    let anchors = BTreeMap::from([
        ("kept".to_string(), kept.clone()),
        ("split".to_string(), moved.clone()),
    ]);
    let barrier_id = db.insert_barrier_group(&name, "ui", &anchors).await?;
    regroup_entries(&state.providers, db, &[entry_id], &kept).await?;
    let entry_of = |pair: Pair| async move { db.find_entry_id_by_pair(&pair.0, &pair.1).await };
    Ok(json!({
        "barrier_id": barrier_id,
        "kept_entry_id": entry_of(kept[0].clone()).await?,
        "split_entry_id": entry_of(moved[0].clone()).await?,
    }))
}

/// `DELETE /api/barriers/{id}`.
pub async fn remove_barrier(state: &Arc<AppState>, id: &str) -> HyperResponse {
    let Ok(id) = id.parse::<i64>() else {
        return respond::error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "id must be numeric",
        );
    };
    match remove(state, id).await {
        Ok(Some(body)) => {
            state.playback.recredit();
            respond::json(StatusCode::OK, body)
        }
        Ok(None) => respond::not_found(),
        Err(e) => internal(e),
    }
}

async fn remove(state: &Arc<AppState>, id: i64) -> anyhow::Result<Option<Value>> {
    let db = &state.db;
    let Some(group) = db.delete_barrier_group(id).await? else {
        return Ok(None);
    };
    let mut entries: Vec<i64> = Vec::new();
    for pair in group.anchors.values().flatten() {
        if let Some(entry) = db.find_entry_id_by_pair(&pair.0, &pair.1).await? {
            entries.push(entry);
        }
    }
    entries.sort_unstable();
    entries.dedup();
    regroup_entries(&state.providers, db, &entries, &[]).await?;
    let mut now: Vec<i64> = Vec::new();
    for pair in group.anchors.values().flatten() {
        if let Some(entry) = db.find_entry_id_by_pair(&pair.0, &pair.1).await? {
            now.push(entry);
        }
    }
    now.sort_unstable();
    now.dedup();
    Ok(Some(json!({ "removed": id, "entries": now })))
}
