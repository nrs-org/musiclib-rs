//! The player's source finder: search YouTube Music and Spotify for a track
//! and attach the result the user picks. It's how a track gets a playable
//! source when it has none yet (an `Unlinked` edition — see
//! `player::Playability`), and how one whose sources all fail at play time
//! (a geoblocked upload, an embed-blocked video) gets another.
//!
//! - `GET /api/entries/{id}/find-sources?q=` — candidates from every
//!   configured provider, each under its own timeout, so one slow or failing
//!   provider only drops its own results (listed in `errors`), and the
//!   track's known length to compare them against.
//! - `POST /api/entries/{id}/attach-source` `{ "url" }` — a job: import the
//!   URL through the normal pipeline (unless it's in the library already),
//!   then link it as the same recording as `{id}`. Its result names the new
//!   source, so the player can mount it straight away.
//!
//! YouTube Music goes through the ytmusicapi helper (`YTMUSICAPI_SERVER_URL`,
//! no Data API quota); Spotify through the client-credentials search API
//! (`SPOTIFY_CLIENT_ID`/`SPOTIFY_CLIENT_SECRET`). A provider without its
//! configuration is skipped and reported in `errors`.

use std::{
    collections::HashSet,
    sync::{Arc, OnceLock},
    time::Duration,
};

use futures::future::join;
use hyper::StatusCode;
use musiclib_rs::{
    musicdb::{IdentityJudgment, NewDedupFeedback},
    pipeline::ingest::ingest_entry,
    providers::{
        backends::spotify::{client::SpotifyClient, types::Paging},
        types::Error as ProviderError,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    entries::component_members,
    player::embed_for,
    respond::{self, HyperResponse},
    state::AppState,
};

/// Per provider: past this, its results are dropped and it's listed in
/// `errors` rather than holding up the others.
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(10);
/// Per provider (ytmusicapi applies it to songs and to videos each).
const LIMIT: usize = 8;

struct Finder {
    http: reqwest::Client,
    ytmusicapi: Option<String>,
    spotify: Option<SpotifyClient>,
}

fn finder() -> &'static Finder {
    static FINDER: OnceLock<Finder> = OnceLock::new();
    FINDER.get_or_init(|| {
        let env = |k| std::env::var(k).ok().filter(|v: &String| !v.is_empty());
        Finder {
            http: reqwest::Client::new(),
            ytmusicapi: env("YTMUSICAPI_SERVER_URL").map(|u| u.trim_end_matches('/').to_string()),
            spotify: env("SPOTIFY_CLIENT_ID")
                .zip(env("SPOTIFY_CLIENT_SECRET"))
                .map(|(id, secret)| SpotifyClient::new(&id, &secret)),
        }
    })
}

/// One search result, in the shape the player's finder lists.
#[derive(Serialize)]
struct Candidate {
    provider: &'static str,
    /// musiclib-rs's canonical identifier — what `embed_for` mounts and what
    /// `attach-source` takes.
    identifier: String,
    /// `"song"` (a YouTube Music art track), `"video"`, or `"track"` (Spotify).
    kind: &'static str,
    title: Option<String>,
    artists: Vec<String>,
    album: Option<String>,
    duration_ms: Option<i64>,
    thumbnail: Option<String>,
    /// The library entry this source already belongs to, if any.
    entry_id: Option<i64>,
    /// That entry is this track (its soft-identity component) — usually the
    /// very source that just failed.
    linked: bool,
}

#[derive(Deserialize)]
struct YtmResult {
    video_id: String,
    kind: String,
    title: Option<String>,
    #[serde(default)]
    artists: Vec<String>,
    album: Option<String>,
    duration_seconds: Option<i64>,
    thumbnail: Option<String>,
}

async fn search_ytmusic(f: &Finder, q: &str) -> Result<Vec<Candidate>, String> {
    let base = f
        .ytmusicapi
        .as_deref()
        .ok_or("YTMUSICAPI_SERVER_URL isn't set")?;
    let resp = f
        .http
        .get(format!(
            "{base}/search?q={}&limit={LIMIT}",
            urlencoding::encode(q)
        ))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("ytmusicapi helper returned {}", resp.status()));
    }
    let rows: Vec<YtmResult> = resp.json().await.map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(|r| Candidate {
            provider: "youtube",
            identifier: format!("https://youtu.be/{}", r.video_id),
            kind: if r.kind == "song" { "song" } else { "video" },
            title: r.title,
            artists: r.artists,
            album: r.album,
            duration_ms: r.duration_seconds.map(|s| s * 1000),
            thumbnail: r.thumbnail,
            entry_id: None,
            linked: false,
        })
        .collect())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SpotifySearch {
    tracks: Paging<SpotifyTrack>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SpotifyTrack {
    id: String,
    name: String,
    #[serde(default)]
    artists: Vec<SpotifyName>,
    album: Option<SpotifyAlbum>,
    duration_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SpotifyName {
    name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SpotifyAlbum {
    name: String,
    #[serde(default)]
    images: Vec<SpotifyImage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SpotifyImage {
    url: String,
}

async fn search_spotify(f: &Finder, q: &str) -> Result<Vec<Candidate>, String> {
    let client = f
        .spotify
        .as_ref()
        .ok_or("Spotify client credentials aren't set")?;
    // Spotify caps search at 10 in development mode.
    let limit = LIMIT.min(10).to_string();
    client
        .get::<SpotifySearch, _, _, ProviderError, _>(
            "search",
            &[("q", q), ("type", "track"), ("limit", &limit)],
            |resp| {
                let rows: Vec<Candidate> = resp
                    .tracks
                    .items
                    .iter()
                    .map(|t| Candidate {
                        provider: "spotify",
                        identifier: format!("https://open.spotify.com/track/{}", t.id),
                        kind: "track",
                        title: Some(t.name.clone()),
                        artists: t.artists.iter().map(|a| a.name.clone()).collect(),
                        album: t.album.as_ref().map(|a| a.name.clone()),
                        duration_ms: t.duration_ms,
                        // Spotify lists the largest image first.
                        thumbnail: t
                            .album
                            .as_ref()
                            .and_then(|a| a.images.last())
                            .map(|i| i.url.clone()),
                        entry_id: None,
                        linked: false,
                    })
                    .collect();
                async move { Ok(rows) }
            },
        )
        .await
        .map_err(|e| e.to_string())
}

async fn timed(
    fut: impl Future<Output = Result<Vec<Candidate>, String>>,
) -> Result<Vec<Candidate>, String> {
    tokio::time::timeout(PROVIDER_TIMEOUT, fut)
        .await
        .unwrap_or_else(|_| Err(format!("no answer in {}s", PROVIDER_TIMEOUT.as_secs())))
}

pub async fn search(state: &Arc<AppState>, entry_id: i64, q: &str) -> HyperResponse {
    let q = q.trim();
    if q.is_empty() {
        return respond::error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "q must not be empty",
        );
    }
    let f = finder();
    let (ytm, spotify) = join(timed(search_ytmusic(f, q)), timed(search_spotify(f, q))).await;
    let members: Vec<i64> = component_members(&state.db, entry_id)
        .await
        .unwrap_or_default();
    // The track's own length, to tell a result of the right cut from an
    // instrumental, a TV size or an MV with an intro: the median of every
    // length its sources report.
    let mut lengths: Vec<i64> = state
        .db
        .source_rows_by_entry_ids(&members)
        .await
        .unwrap_or_default()
        .into_iter()
        .flat_map(|s| s.duration_ms)
        .collect();
    lengths.sort_unstable();
    let duration_ms = lengths.get(lengths.len() / 2).copied();
    let members: HashSet<i64> = members.into_iter().collect();

    let mut results = Vec::new();
    let mut errors = Vec::new();
    for (provider, outcome) in [("youtube", ytm), ("spotify", spotify)] {
        match outcome {
            Ok(rows) => results.extend(rows),
            Err(message) => errors.push(json!({ "provider": provider, "message": message })),
        }
    }
    let mut seen = HashSet::new();
    results.retain(|c| seen.insert(c.identifier.clone()));
    for c in &mut results {
        c.entry_id = state
            .db
            .find_entry_id_by_pair(c.provider, &c.identifier)
            .await
            .ok()
            .flatten();
        c.linked = c.entry_id.is_some_and(|id| members.contains(&id));
    }
    respond::json(
        StatusCode::OK,
        json!({ "duration_ms": duration_ms, "results": results, "errors": errors }),
    )
}

#[derive(Deserialize)]
struct AttachRequest {
    url: String,
}

pub async fn attach(state: &Arc<AppState>, entry_id: i64, body: &[u8]) -> HyperResponse {
    let req: AttachRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return respond::error(StatusCode::BAD_REQUEST, "invalid_input", &e.to_string()),
    };
    let url = req.url.trim().to_string();
    let Some(provider) = ["youtube", "spotify"]
        .into_iter()
        .find(|p| embed_for(p, &url).is_some())
    else {
        return respond::error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "url must be a YouTube video or Spotify track the player can mount",
        );
    };

    let job_state = Arc::clone(state);
    let job_id = state.jobs.spawn("attach_source", move |handle| async move {
        let state = job_state;
        let existing = match state.db.find_entry_id_by_pair(provider, &url).await {
            Ok(id) => id,
            Err(e) => return handle.fail("internal", e.to_string()),
        };
        let found = match existing {
            Some(id) => id,
            None => {
                handle.progress("fetching", "importing the picked source");
                let sink = crate::ingest_progress::JobProgressSink::new(
                    handle.clone(),
                    Arc::clone(&state.activity),
                    Arc::clone(&state.youtube_quota),
                );
                match ingest_entry(
                    &state.db,
                    &state.providers,
                    &state.pool,
                    state.root_id,
                    state.soft_cfg.as_ref(),
                    url.clone(),
                    Some(sink),
                )
                .await
                {
                    Ok(outcome) => match outcome.entry_id {
                        Some(id) => id,
                        None => return handle.fail("not_found", "no provider recognised the URL"),
                    },
                    Err(e) => return handle.fail("provider_error", e.to_string()),
                }
            }
        };
        let members = match component_members(&state.db, entry_id).await {
            Ok(m) => m,
            Err(e) => return handle.fail("internal", e.to_string()),
        };
        if !members.contains(&found) {
            handle.progress("linking", "linking it to the track");
            let feedback = NewDedupFeedback {
                entry_a: entry_id,
                entry_b: found,
                judgment: IdentityJudgment::Same,
                origin: "manual".to_string(),
                model_version: None,
                probability: None,
                candidate_channels: None,
                features: None,
                evidence: None,
                note: Some("attached from the player's source finder".to_string()),
                supersedes_id: None,
            };
            if let Err(e) = state.db.record_identity_feedback(feedback).await {
                return handle.fail("internal", e.to_string());
            }
            state.playback.recredit();
        }
        handle.succeed(json!({
            "entry_id": entry_id,
            "attached_entry_id": found,
            "provider": provider,
            "identifier": url,
        }));
    });
    respond::json(
        StatusCode::ACCEPTED,
        json!({ "job_id": job_id, "op": "attach_source", "status": "pending" }),
    )
}
