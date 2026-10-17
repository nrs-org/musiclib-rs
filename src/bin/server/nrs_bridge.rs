//! The player's NRS extension, behind the `nrs` feature: what the
//! now-playing drawer shows and writes, and listening time exported from
//! the play log. NRS is a client of musiclib, never a dependency of it:
//! everything here goes through the NRS app's own HTTP API
//! (`/api/v1/...`), nothing else in the server calls into this module, and
//! an unreachable NRS app only ever makes these endpoints answer
//! `503 nrs_unavailable` (the drawer then hides its NRS parts).
//!
//! - `GET /api/nrs/status`: whether the NRS app answers.
//! - `GET /api/nrs/now/{entry_id}[?release=]`: for the playing edition,
//!   the edition NRS ranks (`EditionProjection::rank_target`), its NRS
//!   entry, score, impacts and profile, its albums (and `release`, the one
//!   it's playing from), and its listening time. The library half is
//!   answered even when NRS isn't.
//! - `POST /api/nrs/score`: binds a library entry to NRS if needed, then
//!   upserts its profile impacts (music quality, visual grade) and adds any
//!   one-off impacts (a response).
//! - `POST /api/nrs/export`: runs a listening-time export now.
//! - `/api/nrs/v1/...`: passed through to the NRS app's `/api/v1/...`, for
//!   the drawer's direct reads and writes (relation suggestions, palette,
//!   media, visual grades).
//!
//! Listening time: `export_listening_loop` periodically adds the play
//! log's unexported time (`MusicDb::plays_pending_export`) to each ranked
//! edition's `music_consumed_progress` impact, as minutes. Only entries
//! already bound in NRS are exported to; the rest stay pending until
//! they're bound (the drawer's first score binds), so listening alone
//! never adds entries to NRS.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context as _, anyhow};
use bytes::Bytes;
use hyper::{Method, StatusCode};
use musiclib_rs::musicdb::{EditionProjection, SoftIdentityProjection};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tracing::{debug, info, warn};

use crate::{
    entries::entry_summaries_with,
    nrs_pool::{self, NrsPool},
    respond::{self, HyperResponse, parse_query},
    state::AppState,
};

/// `play_export.target` for this exporter.
const EXPORT_TARGET: &str = "nrs";
/// The impact kind listening time accrues on (a track's consumed slot).
const CONSUMED_KIND: &str = "music_consumed_progress";
const EXPORT_EVERY: Duration = Duration::from_secs(120);
const FIRST_EXPORT_AFTER: Duration = Duration::from_secs(20);

pub struct BridgeState {
    client: reqwest::Client,
    /// Library entry -> NRS entry, for bindings made here since the pool
    /// was last built (the pool picks them up on its next build).
    adopted: Mutex<HashMap<i64, String>>,
    /// Held while an export runs, so a manual export and the loop's
    /// never add the same time twice.
    exporting: tokio::sync::Mutex<()>,
}

impl Default for BridgeState {
    fn default() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .expect("reqwest client"),
            adopted: Mutex::default(),
            exporting: tokio::sync::Mutex::default(),
        }
    }
}

pub fn handles(path: &str) -> bool {
    path.starts_with("/api/nrs/")
}

pub async fn route(
    state: &Arc<AppState>,
    method: &Method,
    path: &str,
    query: &str,
    body: Bytes,
) -> HyperResponse {
    let rest = &path["/api/nrs".len()..];
    let result = match (method, rest) {
        (&Method::GET, "/status") => Ok(status(state).await),
        (&Method::GET, p) if p.starts_with("/now/") => match p["/now/".len()..].parse::<i64>() {
            Ok(id) => {
                let release = parse_query(query)
                    .get("release")
                    .and_then(|r| r.parse::<i64>().ok());
                now_playing(state, id, release).await
            }
            Err(_) => Err(BridgeError::BadRequest("entry id must be numeric".into())),
        },
        (&Method::POST, "/score") => score(state, &body).await,
        (&Method::POST, "/export") => export_listening(state)
            .await
            .map(|n| json!({ "exported_to": n }))
            .map_err(BridgeError::Nrs),
        (m, p) if p.starts_with("/v1/") => {
            return proxy(state, m, &p["/v1/".len()..], query, body).await;
        }
        _ => return respond::not_found(),
    };
    match result {
        Ok(body) => respond::json(StatusCode::OK, body),
        Err(e) => e.response(),
    }
}

enum BridgeError {
    BadRequest(String),
    NotFound,
    /// The NRS app is unreachable or answered with an error.
    Nrs(anyhow::Error),
    Internal(anyhow::Error),
}

impl BridgeError {
    fn response(self) -> HyperResponse {
        match self {
            Self::BadRequest(m) => respond::error(StatusCode::BAD_REQUEST, "invalid_input", &m),
            Self::NotFound => respond::not_found(),
            Self::Nrs(e) => respond::error(
                StatusCode::SERVICE_UNAVAILABLE,
                "nrs_unavailable",
                &format!("{e:#}"),
            ),
            Self::Internal(e) => respond::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                &format!("{e:#}"),
            ),
        }
    }
}

impl<E: Into<anyhow::Error>> From<E> for BridgeError {
    fn from(e: E) -> Self {
        Self::Internal(e.into())
    }
}

// ---- NRS app client ----

fn nrs_url(state: &AppState, path: &str) -> String {
    format!("{}/api/v1/{path}", state.nrs_url.trim_end_matches('/'))
}

/// One NRS API call: the JSON body, or an error naming NRS's own message.
async fn nrs_call(
    state: &AppState,
    method: reqwest::Method,
    path: &str,
    body: Option<&Value>,
) -> anyhow::Result<Value> {
    let mut request = state
        .nrs_bridge
        .client
        .request(method.clone(), nrs_url(state, path));
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request
        .send()
        .await
        .with_context(|| format!("reaching NRS at {}", state.nrs_url))?;
    let status = response.status();
    let value: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let message = value["error"].as_str().unwrap_or("no message");
        return Err(anyhow!("NRS {method} {path}: {status}: {message}"));
    }
    Ok(value)
}

async fn nrs_get(state: &AppState, path: &str) -> anyhow::Result<Value> {
    nrs_call(state, reqwest::Method::GET, path, None).await
}

fn entry_path(nrs_id: &str, rest: &str) -> String {
    format!("entries/{}{rest}", urlencoding::encode(nrs_id))
}

fn entry_link(state: &AppState, nrs_id: &str) -> String {
    format!(
        "{}/entries/{}",
        state.nrs_url.trim_end_matches('/'),
        urlencoding::encode(nrs_id)
    )
}

async fn status(state: &Arc<AppState>) -> Value {
    let ping = state
        .nrs_bridge
        .client
        .get(nrs_url(state, "entries?limit=1"))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .and_then(|r| r.error_for_status());
    match ping {
        Ok(_) => json!({ "available": true, "nrs_url": state.nrs_url }),
        Err(e) => json!({ "available": false, "nrs_url": state.nrs_url, "error": e.to_string() }),
    }
}

/// `/api/nrs/v1/{rest}` -> the NRS app's `/api/v1/{rest}`, status and body
/// passed through.
async fn proxy(
    state: &Arc<AppState>,
    method: &Method,
    rest: &str,
    query: &str,
    body: Bytes,
) -> HyperResponse {
    let Ok(nrs_method) = reqwest::Method::from_bytes(method.as_str().as_bytes()) else {
        return respond::method_not_allowed();
    };
    let mut url = nrs_url(state, rest);
    if !query.is_empty() {
        url = format!("{url}?{query}");
    }
    let mut request = state.nrs_bridge.client.request(nrs_method, url);
    if !body.is_empty() {
        request = request
            .header("Content-Type", "application/json")
            .body(body.to_vec());
    }
    let response = match request.send().await {
        Ok(r) => r,
        Err(e) => return BridgeError::Nrs(e.into()).response(),
    };
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    // A write may bind entries (approving a suggestion adds its target), so
    // the pool's bindings can't be trusted until it's rebuilt.
    if *method != Method::GET && status.is_success() {
        nrs_pool::invalidate(state);
    }
    match response.bytes().await {
        Ok(bytes) => hyper::Response::builder()
            .status(status)
            .header("Content-Type", "application/json")
            .body(respond::full(bytes))
            .expect("response builder"),
        Err(e) => BridgeError::Nrs(e.into()).response(),
    }
}

// ---- library side: which entry a play ranks ----

/// Every identity member of every edition whose plays count toward
/// `target`: the target itself and the degenerate editions folding into
/// it.
fn folded_members(
    identity: &SoftIdentityProjection,
    editions: &EditionProjection,
    target: i64,
) -> Vec<i64> {
    editions
        .members_of(target)
        .into_iter()
        .filter(|&m| editions.rank_target(m) == target)
        .flat_map(|m| identity_members(identity, m))
        .collect()
}

fn identity_members(identity: &SoftIdentityProjection, id: i64) -> Vec<i64> {
    let canonical = identity.component_of(id).unwrap_or(id);
    identity
        .members_by_component
        .get(&canonical)
        .cloned()
        .unwrap_or_else(|| vec![canonical])
}

/// The NRS entry bound to any of `members` (one identity's entries).
fn bound_nrs_id(state: &AppState, pool: &NrsPool, members: &[i64]) -> Option<String> {
    let adopted = state.nrs_bridge.adopted.lock().expect("adopted lock");
    members.iter().find_map(|m| {
        adopted
            .get(m)
            .cloned()
            .or_else(|| pool.bound.get(m).map(|b| b.nrs_id.clone()))
    })
}

// ---- now playing ----

async fn now_playing(
    state: &Arc<AppState>,
    entry_id: i64,
    release: Option<i64>,
) -> Result<Value, BridgeError> {
    let db = &state.db;
    let seeds: Vec<i64> = std::iter::once(entry_id).chain(release).collect();
    let (identity, editions) = db.display_projections_around(&seeds).await?;
    let canonical = identity.component_of(entry_id).unwrap_or(entry_id);
    let target = editions.rank_target(canonical);
    let mut ids = vec![entry_id, target];
    ids.extend(release);
    let summaries = entry_summaries_with(db, &ids, &identity, &editions).await?;
    let Some(playing) = summaries.get(&entry_id) else {
        return Err(BridgeError::NotFound);
    };
    let target_summary = summaries.get(&target);
    let listening = db
        .play_totals(&folded_members(&identity, &editions, target), EXPORT_TARGET)
        .await?;

    let mut out = json!({
        "playing": { "entry_id": entry_id, "title": playing.title },
        "target": {
            "entry_id": target,
            "title": target_summary.and_then(|s| s.title.clone()),
            "entry_type": target_summary.map(|s| s.entry_type.clone()),
            "folded": target != canonical,
            "transformation": playing
                .editions
                .iter()
                .find(|e| e.is_current)
                .and_then(|e| e.transformation.clone()),
        },
        "listening": listening,
    });
    let release_entry = release.and_then(|r| summaries.get(&r)).map(|s| {
        (
            s.entry_id,
            s.title.clone(),
            identity_members(&identity, s.entry_id),
        )
    });
    out["nrs"] = match nrs_part(state, &identity_members(&identity, target), release_entry).await {
        Ok(v) => v,
        Err(e) => {
            json!({ "available": false, "nrs_url": state.nrs_url, "error": format!("{e:#}") })
        }
    };
    // The MV to grade: NRS's pick when it has one, else the target's own
    // YouTube videos (an entry not in NRS yet has no NRS media at all).
    let no_videos = out["nrs"]["videos"].as_array().is_none_or(|v| v.is_empty());
    if out["nrs"]["available"] == true && no_videos {
        out["nrs"]["videos"] = target_summary
            .into_iter()
            .flat_map(|s| &s.sources)
            .filter(|s| s.source == "youtube")
            .filter_map(|s| s.identifier.strip_prefix("https://youtu.be/"))
            .map(|id| json!({ "youtube": id, "label": "YouTube" }))
            .collect();
    }
    Ok(out)
}

/// One NRS entry as the drawer shows it: id, link, score, impacts, the
/// profile slots it's expected to fill, and the checks it still fails.
async fn nrs_entry_view(state: &AppState, nrs_id: &str) -> anyhow::Result<Value> {
    let detail = nrs_get(state, &entry_path(nrs_id, "")).await?;
    // Unscored entries still have a score breakdown (V = 0); a failure here
    // shouldn't hide the entry's impacts.
    let score = nrs_get(state, &entry_path(nrs_id, "/score"))
        .await
        .map(|s| {
            json!({
                "value": s["value"],
                "rank": s["rank"],
                "total": s["total"],
                "kind": s["kind"],
                "kind_rank": s["kind_rank"],
                "kind_total": s["kind_total"],
            })
        })
        .unwrap_or(Value::Null);
    Ok(json!({
        "id": nrs_id,
        "title": detail["entry"]["title"],
        "kind": detail["entry"]["kind"],
        "link": entry_link(state, nrs_id),
        "score": score,
        "impacts": detail["impacts"],
        "profile": detail["checks"]["profile"],
        "issues": detail["checks"]["issues"],
    }))
}

async fn nrs_part(
    state: &Arc<AppState>,
    target_members: &[i64],
    release: Option<(i64, Option<String>, Vec<i64>)>,
) -> anyhow::Result<Value> {
    let pool = nrs_pool::get(state, false).await?;
    let nrs_id = bound_nrs_id(state, &pool, target_members);
    let mut entry = Value::Null;
    let mut videos = Value::Array(Vec::new());
    let mut albums: Vec<Value> = Vec::new();
    let mut album_ids: HashSet<String> = HashSet::new();
    if let Some(id) = &nrs_id {
        entry = nrs_entry_view(state, id).await?;
        let media = nrs_get(state, &entry_path(id, "/media")).await?;
        videos = media["videos"].clone();
        for album in media["albums"].as_array().into_iter().flatten() {
            let Some(album_id) = album["id"].as_str() else {
                continue;
            };
            if album_ids.insert(album_id.to_owned()) {
                let mut view = nrs_entry_view(state, album_id).await?;
                view["entry_id"] = Value::Null;
                view["from_release"] = false.into();
                albums.push(view);
            }
        }
    }
    if let Some((release_id, title, members)) = release {
        match bound_nrs_id(state, &pool, &members) {
            Some(album_id) => {
                if let Some(view) = albums
                    .iter_mut()
                    .find(|a| a["id"].as_str() == Some(album_id.as_str()))
                {
                    view["entry_id"] = release_id.into();
                    view["from_release"] = true.into();
                } else {
                    let mut view = nrs_entry_view(state, &album_id).await?;
                    view["entry_id"] = release_id.into();
                    view["from_release"] = true.into();
                    albums.insert(0, view);
                }
            }
            None => albums.insert(
                0,
                json!({ "id": null, "entry_id": release_id, "title": title, "from_release": true }),
            ),
        }
        albums.sort_by_key(|a| !a["from_release"].as_bool().unwrap_or(false));
    }
    Ok(json!({
        "available": true,
        "nrs_url": state.nrs_url,
        "entry": entry,
        "videos": videos,
        "albums": albums,
    }))
}

// ---- scoring ----

#[derive(Deserialize)]
struct ScoreRequest {
    /// The NRS entry to score, when it's already bound.
    nrs_id: Option<String>,
    /// Else the library entry to bind (and score).
    entry_id: Option<i64>,
    /// Profile impacts to upsert: one per kind (`music`, `visual`), the
    /// entry's existing one edited when it has one.
    #[serde(default)]
    set: Vec<Map<String, Value>>,
    /// Impacts to add as new ones (a response such as `nei`).
    #[serde(default)]
    add: Vec<Map<String, Value>>,
}

async fn score(state: &Arc<AppState>, body: &[u8]) -> Result<Value, BridgeError> {
    let request: ScoreRequest = serde_json::from_slice(body)
        .map_err(|e| BridgeError::BadRequest(format!("bad score request: {e}")))?;
    for impact in request.set.iter().chain(&request.add) {
        if !impact.get("kind").is_some_and(Value::is_string) {
            return Err(BridgeError::BadRequest("every impact needs a kind".into()));
        }
    }
    let nrs_id = match (request.nrs_id, request.entry_id) {
        (Some(id), _) => id,
        (None, Some(entry_id)) => bind(state, entry_id).await.map_err(BridgeError::Nrs)?,
        (None, None) => {
            return Err(BridgeError::BadRequest("give nrs_id or entry_id".into()));
        }
    };
    let detail = nrs_get(state, &entry_path(&nrs_id, ""))
        .await
        .map_err(BridgeError::Nrs)?;
    for fields in &request.set {
        let kind = fields["kind"].as_str().expect("checked above");
        let existing = own_impact(&detail, kind);
        let result = match existing {
            Some(impact) => {
                let mut merged = string_fields(&impact["params"]);
                merged.extend(string_fields(&Value::Object(fields.clone())));
                put_impact(state, &nrs_id, &impact, &merged).await
            }
            None => post_impact(state, &nrs_id, &detail, fields).await,
        };
        result.map_err(BridgeError::Nrs)?;
    }
    for fields in &request.add {
        post_impact(state, &nrs_id, &detail, fields)
            .await
            .map_err(BridgeError::Nrs)?;
    }
    Ok(json!({ "nrs_id": nrs_id, "link": entry_link(state, &nrs_id) }))
}

/// Binds library entry `entry_id` to NRS (NRS resolves it through the store
/// protocol and finds any entry already bound to one of its URLs).
async fn bind(state: &Arc<AppState>, entry_id: i64) -> anyhow::Result<String> {
    let response = nrs_call(
        state,
        reqwest::Method::POST,
        "entries/adopt",
        Some(&json!({ "store": "music", "store_id": entry_id.to_string() })),
    )
    .await?;
    let nrs_id = response["id"]
        .as_str()
        .ok_or_else(|| anyhow!("NRS adopt returned no id"))?
        .to_owned();
    state
        .nrs_bridge
        .adopted
        .lock()
        .expect("adopted lock")
        .insert(entry_id, nrs_id.clone());
    nrs_pool::invalidate(state);
    info!("bound library entry {entry_id} to NRS entry {nrs_id}");
    Ok(nrs_id)
}

/// The entry's impact of `kind` to edit: its own one first, else a shared
/// one (edited for every member, as NRS's own form does).
fn own_impact(detail: &Value, kind: &str) -> Option<Value> {
    let of_kind: Vec<&Value> = detail["impacts"]
        .as_array()?
        .iter()
        .filter(|i| i["kind"].as_str() == Some(kind))
        .collect();
    of_kind
        .iter()
        .find(|i| i["shared"].is_null())
        .or(of_kind.first())
        .map(|i| (*i).clone())
}

/// NRS impact forms are string maps; stored params may hold numbers.
fn string_fields(value: &Value) -> BTreeMap<String, String> {
    value
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| {
            let s = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => return None,
            };
            Some((k.clone(), s))
        })
        .collect()
}

/// The profile slot's defaults for `kind` (e.g. a track's `visual_kind`),
/// so a new impact starts the way NRS's own form starts it.
fn slot_defaults(detail: &Value, kind: &str) -> BTreeMap<String, String> {
    detail["checks"]["profile"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|slot| slot["kind"].as_str() == Some(kind))
        .flat_map(|slot| slot["defaults"].as_array().cloned().unwrap_or_default())
        .filter_map(|pair| Some((pair[0].as_str()?.to_owned(), pair[1].as_str()?.to_owned())))
        .collect()
}

async fn post_impact(
    state: &AppState,
    nrs_id: &str,
    detail: &Value,
    fields: &Map<String, Value>,
) -> anyhow::Result<()> {
    let kind = fields["kind"].as_str().unwrap_or_default();
    let mut body = slot_defaults(detail, kind);
    body.extend(string_fields(&Value::Object(fields.clone())));
    nrs_call(
        state,
        reqwest::Method::POST,
        &entry_path(nrs_id, "/impacts"),
        Some(&json!(body)),
    )
    .await?;
    Ok(())
}

async fn put_impact(
    state: &AppState,
    nrs_id: &str,
    impact: &Value,
    fields: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    let impact_id = impact["id"]
        .as_str()
        .ok_or_else(|| anyhow!("NRS impact without an id"))?;
    nrs_call(
        state,
        reqwest::Method::PUT,
        &entry_path(
            nrs_id,
            &format!("/impacts/{}", urlencoding::encode(impact_id)),
        ),
        Some(&json!(fields)),
    )
    .await?;
    Ok(())
}

// ---- listening time ----

/// Runs `export_listening` every `EXPORT_EVERY`, for the server's lifetime.
/// An unreachable NRS app is the normal case when it isn't running, so it's
/// only logged at debug.
pub async fn export_listening_loop(state: Arc<AppState>) {
    tokio::time::sleep(FIRST_EXPORT_AFTER).await;
    loop {
        match export_listening(&state).await {
            Ok(0) => {}
            Ok(n) => info!("exported listening time to {n} NRS entr(y/ies)"),
            Err(e) => debug!("listening-time export skipped: {e:#}"),
        }
        tokio::time::sleep(EXPORT_EVERY).await;
    }
}

/// Adds each ranked edition's unexported listening time to its NRS
/// `music_consumed_progress` impact. Returns how many NRS entries got
/// time. Plays of unbound editions stay pending.
async fn export_listening(state: &Arc<AppState>) -> anyhow::Result<usize> {
    let _running = state.nrs_bridge.exporting.lock().await;
    let db = &state.db;
    let pending = db.plays_pending_export(EXPORT_TARGET).await?;
    if pending.is_empty() {
        return Ok(0);
    }
    let pool = nrs_pool::get(state, false).await?;
    let ids: Vec<i64> = pending
        .iter()
        .map(|p| p.entry_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let (identity, editions) = db.display_projections_around(&ids).await?;
    let mut by_target: BTreeMap<i64, Vec<&musiclib_rs::musicdb::PendingExport>> = BTreeMap::new();
    for play in &pending {
        let canonical = identity
            .component_of(play.entry_id)
            .unwrap_or(play.entry_id);
        by_target
            .entry(editions.rank_target(canonical))
            .or_default()
            .push(play);
    }
    let mut exported_to = 0;
    for (target, plays) in by_target {
        let Some(nrs_id) = bound_nrs_id(state, &pool, &identity_members(&identity, target)) else {
            continue;
        };
        let seconds: f64 = plays
            .iter()
            .map(|p| p.listened_seconds - p.exported_seconds)
            .sum();
        if let Err(e) = add_consumed_minutes(state, &nrs_id, seconds / 60.0).await {
            warn!("adding listening time to NRS entry {nrs_id}: {e:#}");
            continue;
        }
        let marks: Vec<(i64, f64)> = plays
            .iter()
            .map(|p| (p.play_id, p.listened_seconds))
            .collect();
        db.record_play_exports(EXPORT_TARGET, &marks).await?;
        exported_to += 1;
    }
    Ok(exported_to)
}

/// Adds `minutes` to the entry's consumed impact: its existing one edited
/// (a track has one consumed slot, and consumed time isn't additive across
/// impacts), else a new one.
async fn add_consumed_minutes(state: &AppState, nrs_id: &str, minutes: f64) -> anyhow::Result<()> {
    let detail = nrs_get(state, &entry_path(nrs_id, "")).await?;
    match own_impact(&detail, CONSUMED_KIND) {
        Some(impact) => {
            let mut fields = string_fields(&impact["params"]);
            let current: f64 = fields
                .get("duration_minutes")
                .and_then(|m| m.parse().ok())
                .unwrap_or(0.0);
            fields.insert("kind".into(), CONSUMED_KIND.into());
            fields.insert(
                "duration_minutes".into(),
                format!("{:.2}", current + minutes),
            );
            put_impact(state, nrs_id, &impact, &fields).await
        }
        None => {
            let mut fields = Map::new();
            fields.insert("kind".into(), CONSUMED_KIND.into());
            fields.insert("duration_minutes".into(), format!("{minutes:.2}").into());
            post_impact(state, nrs_id, &detail, &fields).await
        }
    }
}
