//! Optional Last.fm scrobbler. Credentials live beside the DB in owner-only
//! `lastfm.json`; eligible plays and frozen retry payloads live in SQLite.
//! A desktop-style authorization flow works for localhost and remote players
//! without needing a publicly reachable callback URL.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, anyhow, bail};
use bytes::Bytes;
use hyper::{Method, StatusCode};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::{
    lastfm_metadata::{self, Metadata},
    respond::{self, HyperResponse},
    state::AppState,
};

const API_URL: &str = "https://ws.audioscrobbler.com/2.0/";

#[derive(Clone, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    secret: String,
    session_key: Option<String>,
    username: Option<String>,
    /// Start with new plays when first connecting an account, rather than
    /// automatically uploading the user's pre-existing library history.
    #[serde(default)]
    since: i64,
}

#[derive(Default)]
struct Inner {
    saved: Saved,
    pending_token: Option<String>,
    now_playing: Option<String>,
    error: Option<String>,
}

pub struct LastFm {
    path: PathBuf,
    endpoint: String,
    client: reqwest::Client,
    inner: Mutex<Inner>,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct ApiError {
    code: Option<i64>,
    retryable: bool,
    message: String,
}

impl ApiError {
    fn needs_reconnect(&self) -> bool {
        // Credential/signature problems belong to the connection, not to an
        // individual play. Preserve the outbox while the user fixes them.
        matches!(self.code, Some(4 | 9 | 10 | 13 | 26))
    }
}

#[derive(Serialize, Deserialize)]
struct Payload {
    metadata: Metadata,
    started_at: i64,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn signature(params: &BTreeMap<String, String>, secret: &str) -> String {
    let mut hash = Md5::new();
    for (name, value) in params {
        if !matches!(name.as_str(), "format" | "callback" | "api_sig") {
            hash.update(name.as_bytes());
            hash.update(value.as_bytes());
        }
    }
    hash.update(secret.as_bytes());
    format!("{:x}", hash.finalize())
}

fn number(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_str()?.parse().ok())
}

fn track_params(metadata: &Metadata) -> BTreeMap<String, String> {
    let mut params = BTreeMap::from([
        ("artist".into(), metadata.artist.clone()),
        ("track".into(), metadata.track.clone()),
        (
            "duration".into(),
            (metadata.duration_seconds.ceil() as u64).to_string(),
        ),
    ]);
    if let Some(album) = &metadata.album {
        params.insert("album".into(), album.clone());
    }
    if let Some(mbid) = &metadata.mbid {
        params.insert("mbid".into(), mbid.clone());
    }
    params
}

enum Outcome {
    Accepted,
    Ignored(i64, String),
}

fn outcome(value: &Value) -> anyhow::Result<Outcome> {
    let scrobbles = &value["scrobbles"];
    let ignored = &scrobbles["scrobble"]["ignoredMessage"];
    if let Some(code) = number(&ignored["code"]) {
        if code == 0 {
            return Ok(Outcome::Accepted);
        }
        return Ok(Outcome::Ignored(
            code,
            ignored["#text"]
                .as_str()
                .unwrap_or("Last.fm ignored this play")
                .to_owned(),
        ));
    }
    if number(&scrobbles["@attr"]["accepted"]) == Some(1) {
        return Ok(Outcome::Accepted);
    }
    bail!("unrecognized Last.fm scrobble response")
}

impl LastFm {
    pub fn load(path: PathBuf) -> Self {
        Self::with_endpoint(path, API_URL.into())
    }

    fn with_endpoint(path: PathBuf, endpoint: String) -> Self {
        let saved = std::fs::read(&path)
            .ok()
            .and_then(|bytes| {
                serde_json::from_slice(&bytes)
                    .map_err(|e| tracing::warn!("ignoring unreadable {}: {e}", path.display()))
                    .ok()
            })
            .unwrap_or_default();
        Self {
            path,
            endpoint,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .expect("Last.fm HTTP client"),
            inner: Mutex::new(Inner {
                saved,
                ..Default::default()
            }),
        }
    }

    async fn save(&self, saved: &Saved) -> anyhow::Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&tmp)
            .await
            .context("saving Last.fm credentials")?;
        tokio::io::AsyncWriteExt::write_all(&mut file, &serde_json::to_vec_pretty(saved)?).await?;
        file.sync_all().await?;
        tokio::fs::rename(tmp, &self.path).await?;
        Ok(())
    }

    async fn call(
        &self,
        saved: &Saved,
        method: &str,
        mut params: BTreeMap<String, String>,
        authenticated: bool,
    ) -> Result<Value, ApiError> {
        params.insert("api_key".into(), saved.api_key.clone());
        params.insert("method".into(), method.into());
        if authenticated {
            params.insert("sk".into(), saved.session_key.clone().unwrap_or_default());
        }
        params.insert("api_sig".into(), signature(&params, &saved.secret));
        params.insert("format".into(), "json".into());
        let encoded = params
            .iter()
            .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        let request = if method.starts_with("auth.") {
            self.client.get(format!("{}?{encoded}", self.endpoint))
        } else {
            self.client
                .post(&self.endpoint)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(encoded)
        };
        let transient = || ApiError {
            code: None,
            retryable: true,
            message: "could not reach Last.fm or read its response".into(),
        };
        let response = request.send().await.map_err(|_| transient())?;
        let status = response.status();
        let value: Value = response.json().await.map_err(|_| transient())?;
        if let Some(code) = number(&value["error"]) {
            return Err(ApiError {
                code: Some(code),
                retryable: matches!(code, 11 | 16 | 29),
                message: format!(
                    "Last.fm error {code}: {}",
                    value["message"].as_str().unwrap_or("request rejected")
                ),
            });
        }
        if !status.is_success() {
            return Err(ApiError {
                code: None,
                retryable: status.is_server_error() || status.as_u16() == 429,
                message: format!("Last.fm returned HTTP {status}"),
            });
        }
        Ok(value)
    }

    async fn configure(&self, api_key: &str, secret: &str) -> anyhow::Result<()> {
        if api_key.trim().is_empty() || secret.trim().is_empty() {
            bail!("enter an API key and shared secret");
        }
        let mut inner = self.inner.lock().await;
        let saved = Saved {
            api_key: api_key.trim().into(),
            secret: secret.trim().into(),
            ..Default::default()
        };
        self.save(&saved).await?;
        *inner = Inner {
            saved,
            ..Default::default()
        };
        Ok(())
    }

    async fn authorize(&self) -> anyhow::Result<String> {
        let mut inner = self.inner.lock().await;
        if inner.saved.api_key.is_empty() {
            bail!("configure a Last.fm API key first");
        }
        let response = self
            .call(&inner.saved, "auth.getToken", BTreeMap::new(), false)
            .await?;
        let token = response["token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("Last.fm returned no authorization token"))?;
        let url = format!(
            "https://www.last.fm/api/auth/?api_key={}&token={}",
            urlencoding::encode(&inner.saved.api_key),
            urlencoding::encode(token)
        );
        inner.pending_token = Some(token.into());
        Ok(url)
    }

    async fn connect(&self) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().await;
        let token = inner
            .pending_token
            .clone()
            .ok_or_else(|| anyhow!("authorize musiclib-rs on Last.fm first"))?;
        let response = self
            .call(
                &inner.saved,
                "auth.getSession",
                BTreeMap::from([("token".into(), token)]),
                false,
            )
            .await?;
        let session = response["session"]["key"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("Last.fm returned no session key"))?;
        let username = response["session"]["name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("Last.fm returned no account name"))?;
        let mut saved = inner.saved.clone();
        if !saved
            .username
            .as_deref()
            .is_some_and(|old| old.eq_ignore_ascii_case(username))
        {
            saved.since = now();
        }
        saved.session_key = Some(session.into());
        saved.username = Some(username.to_lowercase());
        self.save(&saved).await?;
        *inner = Inner {
            saved,
            ..Default::default()
        };
        Ok(())
    }

    async fn disconnect(&self) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().await;
        let mut saved = inner.saved.clone();
        saved.session_key = None;
        self.save(&saved).await?;
        *inner = Inner {
            saved,
            ..Default::default()
        };
        Ok(())
    }

    async fn status(&self, db: &musiclib_rs::musicdb::MusicDb) -> anyhow::Result<Value> {
        let inner = self.inner.lock().await;
        let counts = db
            .scrobble_counts(inner.saved.username.as_deref().unwrap_or(""))
            .await?;
        Ok(
            json!({ "configured": !inner.saved.api_key.is_empty(), "connected": inner.saved.session_key.is_some(),
            "username": inner.saved.username, "authorizing": inner.pending_token.is_some(),
            "authorization_url": inner.pending_token.as_ref().map(|token| format!("https://www.last.fm/api/auth/?api_key={}&token={}",
                urlencoding::encode(&inner.saved.api_key), urlencoding::encode(token))),
            "counts": counts, "error": inner.error }),
        )
    }

    async fn export(
        &self,
        db: &musiclib_rs::musicdb::MusicDb,
        snapshot: &Value,
    ) -> anyhow::Result<()> {
        // Also serializes account changes with sends: a queued play can never
        // be submitted with a different account's credentials.
        let mut inner = self.inner.lock().await;
        let saved = inner.saved.clone();
        let (Some(_), Some(account)) = (&saved.session_key, &saved.username) else {
            return Ok(());
        };
        if snapshot["playing"].as_bool() == Some(true)
            && let (Some(key), Some(id)) = (
                snapshot["play_key"].as_str(),
                snapshot["current"]["entry_id"].as_i64(),
            )
            && inner.now_playing.as_deref() != Some(key)
            && let Some(metadata) = lastfm_metadata::resolve(db, id).await?
        {
            inner.now_playing = Some(key.into());
            // Now Playing is best effort and never queued/retried.
            if let Err(error) = self
                .call(
                    &saved,
                    "track.updateNowPlaying",
                    track_params(&metadata),
                    true,
                )
                .await
            {
                inner.error = Some(error.to_string());
                if error.needs_reconnect() {
                    inner.saved.session_key = None;
                    self.save(&inner.saved).await?;
                    return Ok(());
                }
            }
        }
        for play in db.unqueued_scrobbles(account, saved.since).await? {
            let Some(metadata) = lastfm_metadata::resolve(db, play.entry_id).await? else {
                continue;
            };
            if metadata.eligible(play.listened_seconds) {
                db.queue_scrobble(
                    account,
                    play.id,
                    &serde_json::to_string(&Payload {
                        metadata,
                        started_at: play.started_at,
                    })?,
                )
                .await?;
            }
        }
        for submission in db.pending_scrobbles(account, now()).await? {
            let payload: Payload = serde_json::from_str(&submission.payload)?;
            let mut params = track_params(&payload.metadata);
            params.insert("timestamp".into(), payload.started_at.to_string());
            let result = self.call(&saved, "track.scrobble", params, true).await;
            let mut retry = false;
            let (status, error, delay) = match result {
                Ok(value) => match outcome(&value) {
                    Ok(Outcome::Accepted) => ("accepted", None, 0),
                    Ok(Outcome::Ignored(code, message)) => {
                        retry = code == 5;
                        (
                            if retry { "pending" } else { "ignored" },
                            Some(format!("Last.fm ignored this play ({code}): {message}")),
                            3600,
                        )
                    }
                    Err(error) => {
                        retry = true;
                        ("pending", Some(error.to_string()), 60)
                    }
                },
                Err(error) => {
                    if error.needs_reconnect() {
                        inner.saved.session_key = None;
                        self.save(&inner.saved).await?;
                    }
                    retry = error.retryable || error.needs_reconnect();
                    let delay = (30_i64 * (1_i64 << submission.attempts.min(7))).min(3600);
                    (
                        if retry { "pending" } else { "rejected" },
                        Some(error.to_string()),
                        delay,
                    )
                }
            };
            db.finish_scrobble(
                account,
                submission.play_id,
                status,
                error.as_deref(),
                now() + delay,
            )
            .await?;
            inner.error = error;
            if retry {
                break;
            }
        }
        Ok(())
    }
}

pub async fn run(state: Arc<AppState>) {
    let mut tick = tokio::time::interval(Duration::from_secs(15));
    let mut snapshots = state.playback.subscribe();
    loop {
        tick.tick().await;
        let snapshot =
            serde_json::from_str::<Value>(&snapshots.borrow_and_update()).unwrap_or_default();
        if let Err(error) = state.lastfm.export(&state.db, &snapshot).await {
            tracing::warn!("Last.fm scrobbling: {error:#}");
            state.lastfm.inner.lock().await.error = Some(error.to_string());
        }
    }
}

pub fn handles(path: &str) -> bool {
    path.starts_with("/api/lastfm/")
}

pub async fn route(state: &AppState, method: &Method, path: &str, body: Bytes) -> HyperResponse {
    let result: anyhow::Result<Value> = match (method, &path["/api/lastfm/".len()..]) {
        (&Method::GET, "status") => state.lastfm.status(&state.db).await,
        (&Method::PUT, "config") => {
            #[derive(Deserialize)]
            struct Config {
                api_key: String,
                secret: String,
            }
            let request: Config = match serde_json::from_slice(&body) {
                Ok(request) => request,
                Err(_) => {
                    return respond::error(
                        StatusCode::BAD_REQUEST,
                        "invalid_input",
                        "expected api_key and secret",
                    );
                }
            };
            state
                .lastfm
                .configure(&request.api_key, &request.secret)
                .await
                .map(|()| json!({"ok": true}))
        }
        (&Method::POST, "authorize") => state
            .lastfm
            .authorize()
            .await
            .map(|url| json!({"url": url})),
        (&Method::POST, "connect") => state.lastfm.connect().await.map(|()| json!({"ok": true})),
        (&Method::DELETE, "session") => state
            .lastfm
            .disconnect()
            .await
            .map(|()| json!({"ok": true})),
        (_, "status" | "config" | "authorize" | "connect" | "session") => {
            return respond::method_not_allowed();
        }
        _ => return respond::not_found(),
    };
    let mut response = match result {
        Ok(value) => respond::json(StatusCode::OK, value),
        Err(error) => respond::error(StatusCode::BAD_GATEWAY, "lastfm", &error.to_string()),
    };
    response.headers_mut().insert(
        hyper::header::CACHE_CONTROL,
        hyper::header::HeaderValue::from_static("no-store"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use musiclib_rs::{
        musicdb::MusicDb,
        providers::types::{Alias, Contribution, EntrySpecificData, EntryType},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    type Requests = Arc<std::sync::Mutex<Vec<(String, BTreeMap<String, String>)>>>;

    async fn fake_lastfm(replies: Vec<(u16, Value)>) -> (String, Requests) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/2.0/", listener.local_addr().unwrap());
        let requests: Requests = Default::default();
        let seen = requests.clone();
        tokio::spawn(async move {
            for (status, value) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0; 4096];
                loop {
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&bytes);
                    let Some(end) = text.find("\r\n\r\n") else {
                        continue;
                    };
                    let length = text[..end]
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|n| n.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if bytes.len() < end + 4 + length {
                        continue;
                    }
                    let first = text.lines().next().unwrap();
                    let method = first.split_whitespace().next().unwrap().to_owned();
                    let params = if method == "GET" {
                        first
                            .split_whitespace()
                            .nth(1)
                            .unwrap()
                            .split_once('?')
                            .unwrap()
                            .1
                    } else {
                        &text[end + 4..]
                    };
                    seen.lock()
                        .unwrap()
                        .push((method, respond::parse_query(params).into_iter().collect()));
                    break;
                }
                let body = value.to_string();
                let response = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (endpoint, requests)
    }

    fn accepted() -> Value {
        json!({"scrobbles":{"scrobble":{"ignoredMessage":{"code":"0"}}, "@attr":{"accepted":"1", "ignored":"0"}}})
    }

    fn metadata() -> Metadata {
        Metadata {
            artist: "角巻わため & guests".into(),
            track: "Song".into(),
            album: Some("Album".into()),
            mbid: Some("1c75e623-32dc-4e2d-82d2-72d966cbe82b".into()),
            duration_seconds: 180.0,
            source: "spotify".into(),
            identifier: "spotify-track".into(),
        }
    }

    async fn logged_in(path: PathBuf, endpoint: String) -> LastFm {
        let auth = LastFm::with_endpoint(path, endpoint);
        auth.configure("api-key", "shared-secret").await.unwrap();
        {
            let mut inner = auth.inner.lock().await;
            inner.saved.session_key = Some("session-key".into());
            inner.saved.username = Some("alice".into());
            inner.saved.since = now() - 3600;
            auth.save(&inner.saved).await.unwrap();
        }
        auth
    }

    async fn queued(db: &MusicDb, started_at: i64) -> i64 {
        let id = db.start_play(7, started_at).await.unwrap();
        db.queue_scrobble(
            "alice",
            id,
            &serde_json::to_string(&Payload {
                metadata: metadata(),
                started_at,
            })
            .unwrap(),
        )
        .await
        .unwrap();
        id
    }

    #[tokio::test]
    async fn authenticates_scrobbles_only_eligible_new_plays_and_keeps_secrets_on_server() {
        let dir = tempfile::tempdir().unwrap();
        let db = MusicDb::new("sqlite::memory:").await.unwrap();
        let (endpoint, seen) = fake_lastfm(vec![
            (200, json!({"token":"request-token"})),
            (
                200,
                json!({"session":{"name":"Alice", "key":"session-key"}}),
            ),
            (200, json!({"nowplaying":{"ignoredMessage":{"code":"0"}}})),
            (200, accepted()),
        ])
        .await;
        let path = dir.path().join("lastfm.json");
        let auth = LastFm::with_endpoint(path.clone(), endpoint.clone());
        auth.configure("api-key", "shared-secret").await.unwrap();
        assert!(
            auth.authorize()
                .await
                .unwrap()
                .contains("token=request-token")
        );
        auth.connect().await.unwrap();
        let entry = db.insert_entry(Some(EntryType::Track)).await.unwrap();
        db.upsert_pair(
            "musicbrainz",
            "https://musicbrainz.org/recording/1c75e623-32dc-4e2d-82d2-72d966cbe82b",
            entry,
            None,
            &EntrySpecificData::Track {
                duration_ms: vec![],
                positions: Default::default(),
                songs: vec![],
            },
            None,
        )
        .await
        .unwrap();
        db.upsert_pair(
            "spotify",
            "track",
            entry,
            None,
            &EntrySpecificData::Track {
                duration_ms: vec![180000],
                positions: Default::default(),
                songs: vec![],
            },
            None,
        )
        .await
        .unwrap();
        db.insert_aliases_for_pair(
            "spotify",
            "track",
            &[Alias {
                name: "Canonical Song".into(),
                primary: true,
                ..Default::default()
            }],
            None,
        )
        .await
        .unwrap();
        let artist = db.insert_entry(Some(EntryType::Artist)).await.unwrap();
        db.upsert_pair(
            "spotify",
            "artist",
            artist,
            None,
            &EntrySpecificData::Artist,
            None,
        )
        .await
        .unwrap();
        db.insert_aliases_for_pair(
            "spotify",
            "artist",
            &[Alias {
                name: "Singer".into(),
                primary: true,
                ..Default::default()
            }],
            None,
        )
        .await
        .unwrap();
        db.insert_contribution(
            "spotify",
            "track",
            "spotify",
            "artist",
            &Contribution {
                role: "listed_artist".into(),
                main_artist: true,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
        let old = db.start_play(entry, now() - 60).await.unwrap();
        db.set_play_listened(old, 180.0).await.unwrap();
        let skipped = db.start_play(entry, now()).await.unwrap();
        db.set_play_listened(skipped, 89.0).await.unwrap();
        let play = db.start_play(entry, now()).await.unwrap();
        db.set_play_listened(play, 90.0).await.unwrap();
        let snapshot = json!({"playing":true, "play_key":"play", "current":{"entry_id":entry}});
        auth.export(&db, &snapshot).await.unwrap();
        db.set_play_listened(play, 180.0).await.unwrap();
        auth.export(&db, &snapshot).await.unwrap();
        assert_eq!(seen.lock().unwrap().len(), 4);
        let requests = seen.lock().unwrap().clone();
        assert_eq!(requests[0].0, "GET");
        assert_eq!(requests[2].0, "POST");
        assert_eq!(requests[2].1["method"], "track.updateNowPlaying");
        assert_eq!(requests[3].1["method"], "track.scrobble");
        for request in &requests[2..] {
            assert_eq!(request.1["mbid"], "1c75e623-32dc-4e2d-82d2-72d966cbe82b");
            assert_eq!(request.1["track"], "Canonical Song");
            assert_eq!(request.1["artist"], "Singer");
            assert_eq!(request.1["api_sig"], signature(&request.1, "shared-secret"));
        }
        assert_eq!(
            requests[3].1["timestamp"],
            db.unqueued_scrobbles("bob", 0)
                .await
                .unwrap()
                .iter()
                .find(|p| p.id == play)
                .unwrap()
                .started_at
                .to_string()
        );
        let status = auth.status(&db).await.unwrap().to_string();
        for secret in ["api-key", "shared-secret", "session-key"] {
            assert!(!status.contains(secret));
        }
        assert_eq!(auth.status(&db).await.unwrap()["counts"]["accepted"], 1);
        let restored = LastFm::with_endpoint(path.clone(), endpoint);
        assert_eq!(restored.status(&db).await.unwrap()["connected"], true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn retries_frozen_payload_after_restart_before_newer_plays() {
        let dir = tempfile::tempdir().unwrap();
        let db = MusicDb::new("sqlite::memory:").await.unwrap();
        let (endpoint, seen) = fake_lastfm(vec![
            (503, json!({"error":11, "message":"offline"})),
            (200, accepted()),
            (200, accepted()),
        ])
        .await;
        let path = dir.path().join("lastfm.json");
        let auth = logged_in(path.clone(), endpoint.clone()).await;
        let first = queued(&db, now() - 120).await;
        let second = queued(&db, now()).await;
        auth.export(&db, &json!({})).await.unwrap();
        auth.export(&db, &json!({})).await.unwrap();
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "backoff blocks newer plays too"
        );
        db.finish_scrobble("alice", first, "pending", None, 0)
            .await
            .unwrap();
        drop(auth);
        let restored = LastFm::with_endpoint(path, endpoint);
        restored.export(&db, &json!({})).await.unwrap();
        let requests = seen.lock().unwrap().clone();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[0].1["mbid"],
            "1c75e623-32dc-4e2d-82d2-72d966cbe82b"
        );
        assert_eq!(
            requests[0].1, requests[1].1,
            "retry keeps original Unicode metadata, timestamp and signature"
        );
        assert_eq!(db.scrobble_counts("alice").await.unwrap()["accepted"], 2);
        assert!(
            db.pending_scrobbles("alice", now())
                .await
                .unwrap()
                .is_empty()
        );
        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn revoked_session_pauses_queue_and_http_success_can_be_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let db = MusicDb::new("sqlite::memory:").await.unwrap();
        let (endpoint, seen) =
            fake_lastfm(vec![(200, json!({"error":9, "message":"invalid session"}))]).await;
        let path = dir.path().join("lastfm.json");
        let auth = logged_in(path.clone(), endpoint).await;
        let play = queued(&db, now()).await;
        auth.export(&db, &json!({})).await.unwrap();
        assert_eq!(auth.status(&db).await.unwrap()["connected"], false);
        auth.export(&db, &json!({})).await.unwrap();
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(db.scrobble_counts("alice").await.unwrap()["pending"], 1);
        let (endpoint, _) = fake_lastfm(vec![(200, json!({"scrobbles":{"scrobble":{"ignoredMessage":{"code":"1", "#text":"artist filtered"}}}}))]).await;
        let reconnected = logged_in(path, endpoint).await;
        db.finish_scrobble("alice", play, "pending", None, 0)
            .await
            .unwrap();
        reconnected.export(&db, &json!({})).await.unwrap();
        assert_eq!(db.scrobble_counts("alice").await.unwrap()["ignored"], 1);
    }

    #[test]
    fn signatures_sort_utf8_parameters_and_exclude_format() {
        let mut params = BTreeMap::from([
            ("method".into(), "auth.getSession".into()),
            ("api_key".into(), "xxxxxxxx".into()),
            ("token".into(), "xxxxxxx".into()),
        ]);
        let expected = format!(
            "{:x}",
            Md5::digest(b"api_keyxxxxxxxxmethodauth.getSessiontokenxxxxxxxmysecret")
        );
        assert_eq!(signature(&params, "mysecret"), expected);
        params.insert("format".into(), "json".into());
        assert_eq!(signature(&params, "mysecret"), expected);
        params.insert("artist".into(), "角巻わため & guests".into());
        let expected = format!(
            "{:x}",
            Md5::digest(
                "api_keyxxxxxxxxartist角巻わため & guestsmethodauth.getSessiontokenxxxxxxxmysecret"
                    .as_bytes()
            )
        );
        assert_eq!(signature(&params, "mysecret"), expected);
    }

    #[test]
    fn successful_http_can_contain_ignored_scrobble() {
        assert!(matches!(outcome(&json!({"scrobbles":{"scrobble":{"ignoredMessage":{"code":"3", "#text":"too old"}}}})).unwrap(), Outcome::Ignored(3, _)));
        assert!(matches!(
            outcome(&json!({"scrobbles":{"scrobble":{"ignoredMessage":{"code":"0"}}}})).unwrap(),
            Outcome::Accepted
        ));
        assert!(outcome(&json!({})).is_err());
    }

    #[test]
    fn old_outbox_payloads_remain_readable_and_omit_missing_mbid() {
        let payload: Payload = serde_json::from_value(json!({
            "started_at": 100,
            "metadata": {"artist":"Singer", "track":"Song", "album":null,
                "duration_seconds":180.0, "source":"spotify", "identifier":"track"}
        }))
        .unwrap();
        assert_eq!(payload.metadata.mbid, None);
        let params = track_params(&payload.metadata);
        assert!(!params.contains_key("mbid"));
        assert_eq!(params["artist"], "Singer");
        assert_eq!(params["track"], "Song");
    }
}
