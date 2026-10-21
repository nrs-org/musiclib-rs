//! Store-specific player API: search, entry details, playable sources,
//! release tracklists, and library ingest, plus a minimal static frontend.
//! This is deliberately *not* part of the NRS Store Protocol
//! (`store_protocol.rs`) — it's musiclib-rs's own UI-facing surface, per
//! docs/plan-video-player.md Phase 1. Browsing/playback stays read-only
//! (YouTube and Spotify embeds, no persistence/dedup sidebar) except for the
//! `ingest` job below, which the frontend uses as its "add a song" affordance
//! instead of the `import` CLI. It reuses the exact same
//! `pipeline::ingest::ingest_entry` call and `JobManager` machinery as
//! `store_protocol::ingest` — just under `/api` with a response shape that
//! matches this module's other endpoints (numeric `entry_id`, not NRS's
//! string `store_id`) rather than the store-protocol wire format. The
//! frontend owns queue state entirely client-side; this module just tells it
//! which tracks are playable at all (the `playable` flag on
//! `/releases/:id/tracks`).

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::Arc,
};

use bytes::Bytes;
use futures::future::join_all;
use hyper::{Method, StatusCode};
use musiclib_rs::{
    app_dirs,
    musicdb::{
        DERIVED_FROM, Error as DbError, IdentityJudgment, MusicDb, NewDedupFeedback, RowImage,
        SongEdit,
    },
    pipeline::ingest::ingest_entry,
    providers::backends::soundcloud,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    entries::{
        component_members, dedupe_by_edition, entry_aliases, entry_credits, entry_summaries,
        entry_summaries_with,
    },
    fetch_options, playback,
    respond::{self, HyperResponse, parse_query},
    state::AppState,
};

/// `<config_dir>/fetch_options/` — where the import form's depth presets are
/// read from, fresh on every request (see `fetch_options`'s module doc).
fn fetch_options_dir() -> std::path::PathBuf {
    app_dirs::config_dir().join("fetch_options")
}

const FRONTEND_INDEX: &str = include_str!("frontend/index.html");

pub fn handles(path: &str) -> bool {
    path == "/"
        || path == "/graph"
        || path == "/player"
        || path == "/import"
        || path == "/settings"
        || path.starts_with("/api/")
        || path.starts_with("/entry/")
        || path.starts_with("/graph/")
}

pub async fn route(
    state: &Arc<AppState>,
    method: &Method,
    path: &str,
    query: &str,
    body: Bytes,
) -> HyperResponse {
    match path {
        "/" if *method == Method::GET => frontend(),
        // SPA deep links: `/entry/{id}`, `/graph`, `/graph/{id}`, `/player`,
        // and `/import` all serve the exact same shell as `/` — the frontend
        // reads `location.pathname` on load and restores the mode/view
        // client-side. This is the only reason the id isn't validated here;
        // the API calls the page then makes do that.
        p if p.starts_with("/entry/")
            || p == "/graph"
            || p.starts_with("/graph/")
            || p == "/player"
            || p == "/import"
            || p == "/settings" =>
        {
            if *method != Method::GET {
                return respond::method_not_allowed();
            }
            frontend()
        }
        "/api/library/search" if *method == Method::GET => search(state, &parse_query(query)).await,
        "/api/library/shuffle" if *method == Method::GET => {
            shuffle_library(state, &parse_query(query)).await
        }
        #[cfg(feature = "nrs")]
        "/api/library/nrs-pool" if *method == Method::GET => {
            nrs_pool_json(state, &parse_query(query)).await
        }
        "/api/playback" if *method == Method::GET => playback::snapshot(state),
        "/api/playback/events" if *method == Method::GET => {
            playback::events(state, &parse_query(query))
        }
        "/api/playback/command" if *method == Method::POST => playback::command(state, &body).await,
        "/api/playback/report" if *method == Method::POST => playback::report(state, &body),
        "/api/fetch-options/files" if *method == Method::GET => fetch_options_files(),
        "/api/fetch-options/entry-points" if *method == Method::GET => {
            fetch_options_entry_points(&parse_query(query)).await
        }
        "/api/ingest" if *method == Method::POST => ingest(state, &body).await,
        p if p.starts_with("/api/jobs/") => {
            let id = &p["/api/jobs/".len()..];
            match *method {
                Method::GET => job_status(state, id),
                Method::DELETE => job_cancel(state, id),
                _ => respond::method_not_allowed(),
            }
        }
        "/api/relations/link" if *method == Method::POST => {
            recredit_after(state, link_relation(state, &body).await)
        }
        "/api/relations/unlink" if *method == Method::POST => {
            recredit_after(state, unlink_relation(state, &body).await)
        }
        "/api/relations/relate" if *method == Method::POST => {
            recredit_after(state, relate_entries(state, &body).await)
        }
        "/api/relations/unrelate" if *method == Method::POST => {
            recredit_after(state, set_relation_enabled(state, &body, false).await)
        }
        "/api/relations/restore" if *method == Method::POST => {
            recredit_after(state, set_relation_enabled(state, &body, true).await)
        }
        "/api/relations/derive" if *method == Method::POST => {
            recredit_after(state, derive_edition(state, &body).await)
        }
        "/api/songs/edit" if *method == Method::POST => {
            recredit_after(state, edit_song(state, &body).await)
        }
        "/api/relations/rows" if *method == Method::POST => {
            recredit_after(state, restore_relation_rows(state, &body).await)
        }
        p if p.starts_with("/api/entries/") && p.ends_with("/playable-sources") => {
            if *method != Method::GET {
                return respond::method_not_allowed();
            }
            let id = &p["/api/entries/".len()..p.len() - "/playable-sources".len()];
            playable_sources(state, id).await
        }
        p if p.starts_with("/api/entries/") && p.contains("/credits") => {
            let rest = &p["/api/entries/".len()..];
            let parts: Vec<&str> = rest.split('/').collect();
            match (method, parts.as_slice()) {
                (&Method::GET, &[id, "credits"]) => credits(state, id).await,
                (&Method::POST, &[id, "credits", artist, "display-name"]) => {
                    set_credit_display_name(state, id, artist, &body).await
                }
                (_, &[_, "credits"] | &[_, "credits", _, "display-name"]) => {
                    respond::method_not_allowed()
                }
                _ => respond::not_found(),
            }
        }
        "/api/preferred-source" if *method == Method::POST => {
            crate::preferences::set_source(state, &body).await
        }
        "/api/source-privacy" if *method == Method::POST => crate::privacy::set(state, &body).await,
        p if p.starts_with("/api/entries/") && p.ends_with("/preferred-edition") => {
            if *method != Method::POST {
                return respond::method_not_allowed();
            }
            let id = &p["/api/entries/".len()..p.len() - "/preferred-edition".len()];
            crate::preferences::set_edition(state, id, &body).await
        }
        p if p.starts_with("/api/entries/") && p.ends_with("/display-name") => {
            if *method != Method::POST {
                return respond::method_not_allowed();
            }
            let id = &p["/api/entries/".len()..p.len() - "/display-name".len()];
            set_display_name(state, id, &body).await
        }
        p if p.starts_with("/api/entries/") && p.ends_with("/find-sources") => {
            if *method != Method::GET {
                return respond::method_not_allowed();
            }
            match parse_entry_id(&p["/api/entries/".len()..p.len() - "/find-sources".len()]) {
                Ok(id) => {
                    let q = parse_query(query).remove("q").unwrap_or_default();
                    crate::find_source::search(state, id, &q).await
                }
                Err(resp) => resp,
            }
        }
        p if p.starts_with("/api/entries/") && p.ends_with("/attach-source") => {
            if *method != Method::POST {
                return respond::method_not_allowed();
            }
            match parse_entry_id(&p["/api/entries/".len()..p.len() - "/attach-source".len()]) {
                Ok(id) => crate::find_source::attach(state, id, &body).await,
                Err(resp) => resp,
            }
        }
        p if p.starts_with("/api/entries/") && p.ends_with("/cover") => {
            if *method != Method::GET {
                return respond::method_not_allowed();
            }
            match parse_entry_id(&p["/api/entries/".len()..p.len() - "/cover".len()]) {
                Ok(id) => crate::cover::cover(&state.db, id).await,
                Err(resp) => resp,
            }
        }
        p if p.starts_with("/api/entries/") && p.ends_with("/discography") => {
            if *method != Method::GET {
                return respond::method_not_allowed();
            }
            let id = &p["/api/entries/".len()..p.len() - "/discography".len()];
            discography(state, id).await
        }
        p if p.starts_with("/api/entries/") && p.ends_with("/appearances") => {
            if *method != Method::GET {
                return respond::method_not_allowed();
            }
            let id = &p["/api/entries/".len()..p.len() - "/appearances".len()];
            appearances(state, id).await
        }
        p if p.starts_with("/api/entries/") && p.ends_with("/relations") => {
            if *method != Method::GET {
                return respond::method_not_allowed();
            }
            let id = &p["/api/entries/".len()..p.len() - "/relations".len()];
            entry_relations(state, id).await
        }
        p if p.starts_with("/api/entries/") => {
            if *method != Method::GET {
                return respond::method_not_allowed();
            }
            entry_details(state, &p["/api/entries/".len()..]).await
        }
        p if p.starts_with("/api/releases/") && p.ends_with("/tracks") => {
            if *method != Method::GET {
                return respond::method_not_allowed();
            }
            let id = &p["/api/releases/".len()..p.len() - "/tracks".len()];
            release_tracks(state, id).await
        }
        "/"
        | "/api/library/search"
        | "/api/library/shuffle"
        | "/api/library/nrs-pool"
        | "/api/playback"
        | "/api/playback/events"
        | "/api/playback/command"
        | "/api/playback/report"
        | "/api/fetch-options/files"
        | "/api/fetch-options/entry-points"
        | "/api/ingest"
        | "/api/relations/link"
        | "/api/relations/unlink"
        | "/api/relations/relate"
        | "/api/relations/unrelate"
        | "/api/relations/restore"
        | "/api/relations/derive"
        | "/api/songs/edit"
        | "/api/relations/rows"
        | "/api/preferred-source" => respond::method_not_allowed(),
        _ => respond::not_found(),
    }
}

/// A relation edit can join or split artist identities, changing the name
/// the playing track's artist line uses: once it succeeds, look it up again.
fn recredit_after(state: &Arc<AppState>, response: HyperResponse) -> HyperResponse {
    if response.status().is_success() {
        state.playback.recredit();
    }
    response
}

/// Debug builds re-read `frontend/index.html` from the source tree on every
/// request, so a frontend edit only needs a browser refresh, not a rebuild.
/// Release builds (and a debug binary moved away from its checkout) serve the
/// copy embedded at compile time.
fn frontend() -> HyperResponse {
    let body = if cfg!(debug_assertions) {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/bin/server/frontend/index.html"
        ))
        .map(Bytes::from)
        .unwrap_or_else(|_| Bytes::from_static(FRONTEND_INDEX.as_bytes()))
    } else {
        Bytes::from_static(FRONTEND_INDEX.as_bytes())
    };
    hyper::Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/html; charset=utf-8")
        .header("Cache-Control", "no-cache")
        .body(respond::full(body))
        .expect("response builder")
}

/// `(embed_url, capabilities)` for a source this player can actually mount an
/// iframe for. `identifier` here is musiclib-rs's canonical form, which is a
/// full URL, not a bare provider id (e.g. `https://youtu.be/{id}` for a
/// video — see `providers::backends::youtube_api::canonicalize::video_url`,
/// `spotify::canonicalize::track_url`) — the `strip_prefix`/`match_track_url`
/// calls below both extract the id and filter to just the playable
/// (video/track) shape, since the same source key also canonicalizes
/// playlists/channels/albums to other URL shapes that aren't directly
/// embeddable here.
///
/// SoundCloud and NicoNico (see docs/plan-video-player.md §"Provider adapter
/// boundary", Phase 3) don't embed from a bare id like YouTube/Spotify do —
/// the frontend drives each through its own JS widget API (`w.soundcloud.com
/// /player/api.js`'s `Widget`, NicoNico's `postMessage`-based `jsapi=1`
/// protocol — see `Player.mountSoundcloud`/`mountNicovideo` in
/// `frontend/index.html`) rather than the plain IFrame Player API YouTube
/// uses, but the embed URL shape is still just a static transform of the
/// canonical identifier, computed here the same as the other two.
pub(crate) fn embed_for(
    source: &str,
    identifier: &str,
) -> Option<(String, &'static [&'static str])> {
    match source {
        "youtube" => {
            let id = identifier.strip_prefix("https://youtu.be/")?;
            Some((
                format!("https://www.youtube.com/embed/{id}"),
                &["play", "pause", "seek", "ended_event"][..],
            ))
        }
        "spotify" => {
            let id = identifier.strip_prefix("https://open.spotify.com/track/")?;
            Some((
                format!("https://open.spotify.com/embed/track/{id}"),
                &["play", "pause"][..],
            ))
        }
        "soundcloud" => {
            // Only the track shape is playable — `identifier` may also be a
            // canonical playlist (`.../sets/...`) or artist URL, both of
            // which share the same `soundcloud.com/...` prefix.
            soundcloud::match_track_url(identifier)?;
            Some((
                format!(
                    "https://w.soundcloud.com/player/?url={}",
                    urlencoding::encode(identifier)
                ),
                &["play", "pause", "seek", "ended_event"][..],
            ))
        }
        "nicovideo" => {
            let id = identifier.strip_prefix("https://www.nicovideo.jp/watch/")?;
            Some((
                format!("https://embed.nicovideo.jp/watch/{id}?jsapi=1"),
                &["play", "pause", "seek", "ended_event"][..],
            ))
        }
        _ => None,
    }
}

/// Where a track (or one of its editions) stands for playback, judged from
/// its sources. `Playable` has a source `embed_for` can mount. `Unlinked`
/// has none yet, but the recording may be on a platform musiclib hasn't
/// imported a link from, so it's worth looking for. `Unavailable` exists
/// only as a local file (a `local` source, e.g. a CTB Girls' Dormitory
/// setlist rip), so there's nothing to look for and nothing to play.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Playability {
    Playable,
    Unlinked,
    Unavailable,
}

impl Playability {
    pub(crate) fn of<'a>(sources: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut local = false;
        for (source, identifier) in sources {
            if embed_for(source, identifier).is_some() {
                return Self::Playable;
            }
            local |= source == "local";
        }
        if local {
            Self::Unavailable
        } else {
            Self::Unlinked
        }
    }
}

async fn search(state: &Arc<AppState>, query: &HashMap<String, String>) -> HyperResponse {
    let q = query.get("q").cloned().unwrap_or_default();
    let limit = query
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(20)
        .min(100);

    let raw_ids = match state.db.search_entry_ids_by_alias(&q, limit).await {
        Ok(ids) => ids,
        Err(e) => {
            return respond::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                &e.to_string(),
            );
        }
    };
    // Fold soft-linked matches, and edition-linked ones (a track and its
    // instrumental), together so the same song doesn't show up as
    // separate-looking rows.
    let ids = match dedupe_by_edition(&state.db, &raw_ids).await {
        Ok(ids) => ids,
        Err(e) => {
            return respond::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                &e.to_string(),
            );
        }
    };
    let summaries = match entry_summaries(&state.db, &ids).await {
        Ok(s) => s,
        Err(e) => {
            return respond::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                &e.to_string(),
            );
        }
    };
    let results: Vec<_> = ids.iter().filter_map(|id| summaries.get(id)).collect();
    respond::json(StatusCode::OK, json!({ "results": results }))
}

/// `GET /api/library/shuffle?limit=N` (default/cap: 100/500) — a uniformly
/// random, playable sample of the library's rows (one per edition group,
/// as search and the library list show them). There's no playlist concept
/// yet, so this is the closest equivalent to "shuffle play" over everything
/// you've catalogued.
///
/// Rejection sampling, so nothing here scales with the library: draw random
/// track ids, resolve just their edition groups (`display_projections_around`),
/// and keep a draw with probability `1 / group size` — otherwise a group
/// with many editions/soft duplicates would be picked proportionally more
/// often. Unplayable groups (same soft-identity + `embed_for` check
/// `release_tracks` uses) are rejected too, and rounds repeat until `limit`
/// rows are found or `MAX_ROUNDS` runs out on a mostly-unplayable library.
///
/// `exclude=track,album,artist` (any subset) skips rows the last-built NRS
/// pool (`nrs_pool`) reaches that way, so the shuffle mix's "new" share
/// doesn't redraw familiar tracks. No pool built yet means nothing excluded.
/// `skip=1,2,3` skips those entry ids (rows already queued).
async fn shuffle_library(state: &Arc<AppState>, query: &HashMap<String, String>) -> HyperResponse {
    const MAX_ROUNDS: usize = 8;
    let limit = query
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100)
        .clamp(1, 500);
    #[cfg(feature = "nrs")]
    let mut excluded = crate::nrs_pool::excluded_for(state, query.get("exclude"));
    #[cfg(not(feature = "nrs"))]
    let mut excluded: HashSet<i64> = HashSet::new();
    excluded.extend(
        query
            .get("skip")
            .into_iter()
            .flat_map(|s| s.split(','))
            .filter_map(|id| id.parse::<i64>().ok()),
    );

    // Every round pays for the edition groups its draws land in (one huge
    // group gets hit almost every round), so size each round from the
    // yield (kept rows per draw) seen so far, padded, to finish in ~1 round.
    const INITIAL_YIELD: f64 = 0.08;
    const MAX_DRAWS: usize = 10_000;

    let mut seen_groups = HashSet::new();
    let mut tracks = Vec::with_capacity(limit);
    let (mut drawn, mut kept) = (0usize, 0usize);
    for _ in 0..MAX_ROUNDS {
        if tracks.len() >= limit {
            break;
        }
        let rate = if drawn == 0 {
            INITIAL_YIELD
        } else {
            (kept as f64 / drawn as f64).max(0.01)
        };
        let draws = (((limit - tracks.len()) as f64 / rate * 1.5) as usize).clamp(32, MAX_DRAWS);
        match shuffle_round(state, draws, &mut seen_groups, &excluded).await {
            Ok(round) => {
                drawn += draws;
                kept += round.len();
                tracks.extend(round);
            }
            Err(e) => {
                return respond::error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    &e.to_string(),
                );
            }
        }
    }
    tracks.truncate(limit);
    respond::json(StatusCode::OK, json!({ "tracks": tracks }))
}

/// One `shuffle_library` round over `draws` random ids: the accepted,
/// playable rows, in draw order. `seen_groups` (edition-group roots) carries
/// across rounds so a group is never returned twice; `excluded` default
/// editions are never returned.
async fn shuffle_round(
    state: &Arc<AppState>,
    draws: usize,
    seen_groups: &mut HashSet<i64>,
    excluded: &HashSet<i64>,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let candidates = state.db.sample_track_entry_ids(draws).await?;
    let (identity, editions) = state.db.display_projections_around(&candidates).await?;
    let mut accepted = Vec::new();
    for id in candidates {
        let canonical = identity.component_of(id).unwrap_or(id);
        let group_size: usize = editions
            .members_of(canonical)
            .iter()
            .map(|c| identity.members_by_component.get(c).map_or(1, Vec::len))
            .sum();
        if fastrand::usize(..group_size) != 0 {
            continue;
        }
        let default = editions.default_member_of(canonical);
        if !excluded.contains(&default) && seen_groups.insert(editions.group_of(canonical)) {
            accepted.push(default);
        }
    }
    let summaries = entry_summaries_with(&state.db, &accepted, &identity, &editions).await?;
    Ok(accepted
        .iter()
        .filter_map(|id| {
            let summary = summaries.get(id)?;
            let playable = summary
                .sources
                .iter()
                .any(|s| embed_for(&s.source, &s.identifier).is_some());
            playable.then(|| json!({ "entry_id": id, "title": summary.title }))
        })
        .collect())
}

/// `GET /api/library/nrs-pool[?refresh=1]` — the shuffle mix's familiar
/// pool (see `nrs_pool`): `available: false` with the reason when the NRS
/// app can't be read, so the player falls back to a plain shuffle. Absent
/// (404) in a build without the `nrs` feature, which the player treats the
/// same way.
#[cfg(feature = "nrs")]
async fn nrs_pool_json(state: &Arc<AppState>, query: &HashMap<String, String>) -> HyperResponse {
    let refresh = query.get("refresh").is_some_and(|v| v == "1");
    match crate::nrs_pool::get(state, refresh).await {
        Ok(pool) => respond::json(
            StatusCode::OK,
            json!({ "available": true, "nrs_url": state.nrs_url, "pool": &*pool }),
        ),
        Err(e) => respond::json(
            StatusCode::OK,
            json!({ "available": false, "nrs_url": state.nrs_url, "error": format!("{e:#}") }),
        ),
    }
}

/// `GET /api/fetch-options/files` — the import form's file picker: every
/// `.yaml` file the user has dropped into `<config_dir>/fetch_options/`,
/// read fresh from disk on every call (see `fetch_options`'s module doc) —
/// no bundled/hardcoded list, that directory is entirely user-managed.
/// Always includes the built-in `__shallow__` choice first (not a file — see
/// `fetch_options::shallow`) so the form still works before the user has set
/// up any config at all.
fn fetch_options_files() -> HyperResponse {
    let mut files = vec![
        json!({ "id": fetch_options::SHALLOW_ID, "label": "Quick (1 level deep, no config file)" }),
    ];
    files.extend(
        fetch_options::list_files(&fetch_options_dir())
            .into_iter()
            .map(|f| json!({ "id": f.id })),
    );
    respond::json(StatusCode::OK, json!({ "files": files }))
}

/// `GET /api/fetch-options/entry-points?file=<id>` — the entry points one
/// picked file offers, i.e. its YAML document's top-level keys (`main` and
/// whatever other named sets it defines — see `fetch_options`'s module doc
/// on why a file can offer more than one). The frontend only shows its own
/// entry-point picker when this comes back with more than one name.
async fn fetch_options_entry_points(query: &HashMap<String, String>) -> HyperResponse {
    let Some(file_id) = query.get("file") else {
        return respond::error(StatusCode::BAD_REQUEST, "invalid_input", "file is required");
    };
    if file_id == fetch_options::SHALLOW_ID {
        return respond::json(StatusCode::OK, json!({ "entry_points": [] }));
    }
    match fetch_options::entry_points(&fetch_options_dir(), file_id).await {
        Ok(names) => respond::json(StatusCode::OK, json!({ "entry_points": names })),
        Err(message) => respond::error(StatusCode::BAD_REQUEST, "invalid_input", &message),
    }
}

#[derive(Deserialize)]
struct IngestFetchOptions {
    /// A file id from `GET /api/fetch-options/files`.
    file: String,
    /// An entry-point name from `GET /api/fetch-options/entry-points`.
    /// Defaults to `"main"` — see `fetch_options::DEFAULT_ENTRY_POINT`.
    #[serde(default = "default_entry_point")]
    entry_point: String,
}

fn default_entry_point() -> String {
    fetch_options::DEFAULT_ENTRY_POINT.to_string()
}

#[derive(Deserialize)]
struct IngestRequest {
    query: String,
    /// Omitted falls back to the server's process-wide default
    /// (`--fetch-options`, or the built-in 1-level-deep fallback) — same as
    /// before this field existed.
    #[serde(default)]
    fetch_options: Option<IngestFetchOptions>,
}

/// `POST /api/ingest` — the webapp's "add a song" affordance: runs
/// `pipeline::ingest::ingest_entry` (the same fetch/flush/dedup sequence the
/// `import` CLI and `store_protocol::ingest` use) as a background job against
/// the shared `AppState`, so it competes for the same `MusicDb` connection
/// and provider HTTP budget as everything else this process is doing — see
/// `job_status` for the write-during-read question this raises for concurrent
/// browsing. Returns `202` with a `job_id`; poll `GET /api/jobs/:id` for
/// progress and the eventual `entry_id`.
async fn ingest(state: &Arc<AppState>, body: &[u8]) -> HyperResponse {
    let req: IngestRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return respond::error(StatusCode::BAD_REQUEST, "invalid_input", &e.to_string()),
    };
    let query = req.query.trim().to_string();
    if query.is_empty() {
        return respond::error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "query must not be empty",
        );
    }
    let (pool, options_id) = match &req.fetch_options {
        Some(fo) => {
            match fetch_options::resolve(&fetch_options_dir(), &fo.file, &fo.entry_point).await {
                Some(resolved) => resolved,
                None => {
                    return respond::error(
                        StatusCode::BAD_REQUEST,
                        "invalid_input",
                        &format!(
                            "unknown fetch_options file/entry_point {:?}/{:?}",
                            fo.file, fo.entry_point
                        ),
                    );
                }
            }
        }
        None => (Arc::clone(&state.pool), state.root_id),
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
            &pool,
            options_id,
            &job_state.dedup_configs,
            &job_state.merged_dedup,
            job_state.soft_cfg.as_ref(),
            query,
            Some(sink),
        )
        .await;
        match result {
            Ok(outcome) => match outcome.entry_id {
                Some(id) => handle.succeed(json!({ "entry_id": id })),
                None => handle.fail("not_found", "no provider recognised the query"),
            },
            Err(e) => handle.fail("provider_error", e.to_string()),
        }
    });

    respond::json(
        StatusCode::ACCEPTED,
        json!({ "job_id": job_id, "op": "ingest", "status": "pending" }),
    )
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

async fn entry_details(state: &Arc<AppState>, id: &str) -> HyperResponse {
    let Ok(entry_id) = id.parse::<i64>() else {
        return respond::error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "id must be numeric",
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
    let Some(summary) = summaries.get(&entry_id) else {
        return respond::not_found();
    };
    let mut summary = summary.clone();
    if summary.editions.len() > 1 {
        let lines = join_all(
            summary
                .editions
                .iter()
                .map(|ed| crate::playback::artist_line(&state.db, ed.entry_id, None)),
        )
        .await;
        for (ed, line) in summary.editions.iter_mut().zip(lines) {
            // A missing artist line just leaves the edition unlabelled —
            // never worth failing the whole entry page over.
            ed.artists = line
                .map(|artists| artists.into_iter().map(|a| a.name).collect())
                .unwrap_or_default();
        }
        let ids: Vec<i64> = summary.editions.iter().map(|ed| ed.entry_id).collect();
        match crate::preferences::edition_weights(&state.db, &ids).await {
            Ok(weights) => {
                for ed in &mut summary.editions {
                    ed.preferred_weight = weights.get(&ed.entry_id).copied().unwrap_or(0);
                }
            }
            Err(e) => tracing::warn!("couldn't read preferred editions of {entry_id}: {e:#}"),
        }
    }
    let aliases = match entry_aliases(&state.db, entry_id).await {
        Ok(a) => a,
        Err(e) => {
            return respond::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                &e.to_string(),
            );
        }
    };
    let pairs: Vec<(String, String)> = summary
        .sources
        .iter()
        .map(|s| (s.source.clone(), s.identifier.clone()))
        .collect();
    let privacy = crate::privacy::for_entry(&state.db, &pairs).await;
    let mut body = json!(summary);
    body["aliases"] = json!(aliases);
    body["privacy"] = privacy;
    respond::json(StatusCode::OK, body)
}

/// `GET /api/entries/{id}/discography` — an artist's releases and tracks
/// (`discography::discography`).
async fn discography(state: &Arc<AppState>, id: &str) -> HyperResponse {
    let entry_id = match parse_entry_id(id) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    match crate::discography::discography(&state.db, entry_id).await {
        Ok(d) => respond::json_typed(StatusCode::OK, &d),
        Err(e) => respond::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            &e.to_string(),
        ),
    }
}

/// `GET /api/entries/{id}/appearances` — the releases listing a track,
/// with its position on each (`discography::appearances`).
async fn appearances(state: &Arc<AppState>, id: &str) -> HyperResponse {
    let entry_id = match parse_entry_id(id) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    match crate::discography::appearances(&state.db, entry_id).await {
        Ok(a) => respond::json_typed(StatusCode::OK, &a),
        Err(e) => respond::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            &e.to_string(),
        ),
    }
}

#[derive(Deserialize)]
struct DisplayNameRequest {
    name: Option<String>,
}

/// `POST /api/entries/{id}/display-name` — `{"name": "…"}` shows the entry
/// (its whole soft-identity component) by that name everywhere instead of
/// the one its aliases give it; `{"name": null}` or a blank name goes back
/// to the aliases.
async fn set_display_name(state: &Arc<AppState>, id: &str, body: &[u8]) -> HyperResponse {
    let entry_id = match parse_entry_id(id) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let req: DisplayNameRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(e) => return respond::error(StatusCode::BAD_REQUEST, "invalid_input", &e.to_string()),
    };
    let name = req.name.as_deref().map(str::trim).filter(|n| !n.is_empty());
    let members = match component_members(&state.db, entry_id).await {
        Ok(m) => m,
        Err(e) => {
            return respond::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                &e.to_string(),
            );
        }
    };
    if let Err(e) = state.db.set_display_name(entry_id, &members, name).await {
        return db_error_response(e);
    }
    // Queued copies of a track carry its title; the playing track's artist
    // line may use this name.
    if let Ok(summaries) = entry_summaries(&state.db, &[entry_id]).await
        && let Some(title) = summaries.get(&entry_id).and_then(|s| s.title.clone())
    {
        state.playback.retitle(&members, &title);
    }
    state.playback.recredit();
    entry_details(state, id).await
}

/// `GET /api/entries/{id}/credits` — the artists the entry credits, each
/// with the name it credits them as (`entries::entry_credits`).
async fn credits(state: &Arc<AppState>, id: &str) -> HyperResponse {
    let entry_id = match parse_entry_id(id) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    match entry_credits(&state.db, entry_id).await {
        Ok(credits) => respond::json(StatusCode::OK, json!({ "credits": credits })),
        Err(e) => respond::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            &e.to_string(),
        ),
    }
}

/// `POST /api/entries/{id}/credits/{artist}/display-name` — `{"name": "…"}`
/// credits the artist by that name on this entry only; `{"name": null}` or
/// a blank name goes back to the artist's own name. Answers with the
/// entry's credits.
async fn set_credit_display_name(
    state: &Arc<AppState>,
    id: &str,
    artist: &str,
    body: &[u8],
) -> HyperResponse {
    let (entry_id, artist_id) = match (parse_entry_id(id), parse_entry_id(artist)) {
        (Ok(e), Ok(a)) => (e, a),
        (Err(resp), _) | (_, Err(resp)) => return resp,
    };
    let req: DisplayNameRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(e) => return respond::error(StatusCode::BAD_REQUEST, "invalid_input", &e.to_string()),
    };
    let name = req.name.as_deref().map(str::trim).filter(|n| !n.is_empty());
    let members = match (
        component_members(&state.db, entry_id).await,
        component_members(&state.db, artist_id).await,
    ) {
        (Ok(e), Ok(a)) => (e, a),
        (Err(e), _) | (_, Err(e)) => {
            return respond::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                &e.to_string(),
            );
        }
    };
    if let Err(e) = state
        .db
        .set_credit_display_name(entry_id, &members.0, artist_id, &members.1, name)
        .await
    {
        return db_error_response(e);
    }
    state.playback.recredit();
    credits(state, id).await
}

fn parse_entry_id(id: &str) -> Result<i64, HyperResponse> {
    id.parse::<i64>().map_err(|_| {
        respond::error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "id must be numeric",
        )
    })
}

#[derive(Deserialize)]
struct RelationRequest {
    a: i64,
    b: i64,
    /// Only meaningful for unlink: a plain unlink downgrades the edge to a
    /// disabled "suggestion" (still shown, one click from restoring). Force
    /// asserts `different_identity` instead — a hard block the automatic
    /// dedup pipeline won't cross either, one click from being softened
    /// back rather than trivially restored. Ignored by link.
    #[serde(default)]
    force: bool,
}

fn parse_relation_body(body: &[u8]) -> Result<RelationRequest, HyperResponse> {
    serde_json::from_slice(body)
        .map_err(|e| respond::error(StatusCode::BAD_REQUEST, "invalid_input", &e.to_string()))
}

/// `entry_id`'s direct structural neighbors via `entry_child` — e.g. a
/// release's tracks, or a track's release. Read-only display data, distinct
/// from the editable soft-identity graph: `entry_child` is importer-owned
/// (there's no feedback/edit path for it), so this only ever *adds* nodes
/// and edges, never something `entry_relations` needs to let the UI change.
/// Scoped to one hop each direction from `entry_id` itself — not expanded
/// recursively for every soft-identity sibling, to keep the response bounded.
async fn structural_edges_around(
    db: &MusicDb,
    entry_id: i64,
) -> Result<(Vec<(i64, i64, Option<i32>, Option<i32>)>, Vec<i64>), DbError> {
    let pairs = db.pairs_by_entry_ids(&[entry_id]).await?;

    let children = db.child_rows_for_parent_pairs(&pairs).await?;
    let child_pairs: Vec<(String, String)> = children
        .iter()
        .map(|c| (c.child_source.clone(), c.child_identifier.clone()))
        .collect();
    let child_entry_by_pair: HashMap<(String, String), i64> = db
        .source_rows_for_pairs(&child_pairs)
        .await?
        .into_iter()
        .map(|s| ((s.source, s.identifier), s.entry_id))
        .collect();

    let parents = db.child_rows_for_child_pairs(&pairs).await?;
    let parent_pairs: Vec<(String, String)> = parents
        .iter()
        .map(|c| (c.parent_source.clone(), c.parent_identifier.clone()))
        .collect();
    let parent_entry_by_pair: HashMap<(String, String), i64> = db
        .source_rows_for_pairs(&parent_pairs)
        .await?
        .into_iter()
        .map(|s| ((s.source, s.identifier), s.entry_id))
        .collect();

    let mut edges: Vec<(i64, i64, Option<i32>, Option<i32>)> = Vec::new();
    for c in &children {
        let key = (c.child_source.clone(), c.child_identifier.clone());
        if let Some(&child_id) = child_entry_by_pair.get(&key)
            && child_id != entry_id
        {
            edges.push((entry_id, child_id, c.disc_no, c.track_no));
        }
    }
    for c in &parents {
        let key = (c.parent_source.clone(), c.parent_identifier.clone());
        if let Some(&parent_id) = parent_entry_by_pair.get(&key)
            && parent_id != entry_id
        {
            edges.push((parent_id, entry_id, c.disc_no, c.track_no));
        }
    }
    // The same two entries can be linked by more than one underlying pair
    // (e.g. both a MusicBrainz and a Discogs release->track pairing) — fold
    // those down to a single displayed edge.
    edges.sort_by_key(|&(p, c, _, _)| (p, c));
    edges.dedup_by_key(|&mut (p, c, _, _)| (p, c));

    let mut new_ids: Vec<i64> = edges
        .iter()
        .flat_map(|&(p, c, _, _)| [p, c])
        .filter(|&id| id != entry_id)
        .collect();
    new_ids.sort_unstable();
    new_ids.dedup();

    Ok((edges, new_ids))
}

/// The relations graph around `id`'s edition group: every relation row
/// (enabled or not) touching an edition or a soft-identity member, and the
/// rows between the entries those reach (`MusicDb::relation_graph_around`),
/// plus a display node — own sources only, not the component-wide union —
/// for every entry any of those edges references. A disabled edge still
/// needs its far endpoint rendered as a node even though that entry dropped
/// out of the *live* component, or a plain "Unlink" would make the
/// downgraded edge (and the undo it's supposed to offer) vanish outright.
/// Each node carries its edition role (`edition_group`, `edition_size`,
/// `default_edition`, `folds_into` — see `EditionProjection`). Also folds
/// in `id`'s direct structural (`entry_child`) neighbors as read-only
/// nodes/edges — see `structural_edges_around`.
async fn entry_relations(state: &Arc<AppState>, id: &str) -> HyperResponse {
    let entry_id = match parse_entry_id(id) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let edges = match state.db.relation_graph_around(entry_id).await {
        Ok(e) => e,
        Err(e) => {
            return respond::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                &e.to_string(),
            );
        }
    };
    let (structural_edges, structural_ids) =
        match structural_edges_around(&state.db, entry_id).await {
            Ok(v) => v,
            Err(e) => {
                return respond::error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    &e.to_string(),
                );
            }
        };
    let mut node_ids: Vec<i64> = vec![entry_id];
    for r in &edges {
        node_ids.push(r.entry_a);
        node_ids.push(r.entry_b);
    }
    node_ids.extend(structural_ids);
    node_ids.sort_unstable();
    node_ids.dedup();
    let (identity, editions) = match state.db.display_projections_around(&node_ids).await {
        Ok(v) => v,
        Err(e) => return db_error_response(e),
    };
    let summaries = match entry_summaries_with(&state.db, &node_ids, &identity, &editions).await {
        Ok(s) => s,
        Err(e) => {
            return respond::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                &e.to_string(),
            );
        }
    };
    // What tells versions with one title apart: who performs them, when,
    // and how long they run. Artists are per identity, so asked once each.
    let track_identities: Vec<i64> = node_ids
        .iter()
        .filter(|nid| summaries.get(nid).is_some_and(|s| s.entry_type == "track"))
        .map(|&nid| identity.component_of(nid).unwrap_or(nid))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let artist_lines = join_all(
        track_identities
            .iter()
            .map(|&id| crate::playback::artist_line(&state.db, id, None)),
    )
    .await;
    let artists: HashMap<i64, Vec<String>> = track_identities
        .into_iter()
        .zip(artist_lines)
        .map(|(id, line)| {
            let names = line
                .map(|artists| artists.into_iter().map(|a| a.name).collect())
                .unwrap_or_default();
            (id, names)
        })
        .collect();
    let mut durations: HashMap<i64, i64> = HashMap::new();
    match state.db.source_rows_by_entry_ids(&node_ids).await {
        Ok(rows) => {
            for row in rows {
                if let Some(&ms) = row.duration_ms.first() {
                    durations.entry(row.entry_id).or_insert(ms);
                }
            }
        }
        Err(e) => return db_error_response(e),
    }
    let nodes: Vec<_> = node_ids
        .iter()
        .map(|&nid| {
            let summary = summaries.get(&nid);
            let own_member = summary.and_then(|s| s.members.iter().find(|m| m.entry_id == nid));
            let canonical = identity.component_of(nid).unwrap_or(nid);
            let rank_target = editions.rank_target(canonical);
            let original = editions.original_of(canonical);
            let parent = editions.parent_of(canonical);
            // What this version is, relative to what it was made from.
            let song_kind = parent
                .or(original)
                .filter(|&to| to != canonical)
                .and_then(|to| editions.edge_label(canonical, to))
                .map(|(kind, transformation)| json!({ "kind": kind, "transformation": transformation }));
            json!({
                "entry_id": nid,
                // This node's own title, not the soft-dedup-merged
                // `summary.title` — two soft-linked entries (e.g. a track
                // and its instrumental) can have genuinely different
                // titles, and the graph needs to tell them apart.
                "title": own_member.and_then(|m| m.title.clone()),
                "entry_type": summary.map(|s| s.entry_type.clone()),
                "sources": own_member.map(|m| m.sources.clone()).unwrap_or_default(),
                "edition_group": editions.group_of(canonical),
                "edition_size": editions.members_of(canonical).len(),
                "default_edition": editions.default_member_of(canonical) == canonical,
                "folds_into": (rank_target != canonical).then_some(rank_target),
                "identity": canonical,
                "identity_size": identity
                    .members_by_component
                    .get(&canonical)
                    .map_or(1, Vec::len),
                "artists": artists.get(&canonical).cloned().unwrap_or_default(),
                "release_date": summary.and_then(|s| s.release_date.clone()),
                "duration_ms": durations.get(&nid),
                "song_original": original,
                "song_parent": parent,
                "song_kind": song_kind,
            })
        })
        .collect();
    let edges: Vec<_> = edges
        .iter()
        .map(|r| {
            // `extra` holds the reason text and, for dedup-v2 primitive-relation
            // kinds, the `relate(...)` script's metadata map (`transformation`,
            // `derived_side`, …) — parse it so the frontend can render fields
            // directly instead of a raw JSON string. Malformed/absent payloads
            // just render as no metadata, never an error.
            let extra = r
                .extra
                .as_deref()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
            json!({
                "entry_a": r.entry_a,
                "entry_b": r.entry_b,
                "kind": r.kind,
                "origin": r.origin,
                "confidence": r.confidence,
                "enabled": r.enabled,
                "extra": extra,
            })
        })
        .collect();
    let structural_edges: Vec<_> = structural_edges
        .into_iter()
        .map(|(parent_entry_id, child_entry_id, disc_no, track_no)| {
            json!({
                "parent_entry_id": parent_entry_id,
                "child_entry_id": child_entry_id,
                "disc_no": disc_no,
                "track_no": track_no,
            })
        })
        .collect();
    respond::json(
        StatusCode::OK,
        json!({
            "entry_id": entry_id,
            "nodes": nodes,
            "edges": edges,
            "structural_edges": structural_edges,
        }),
    )
}

/// `POST /api/relations/link` — asserts a direct `same_identity` edge
/// between `a` and `b` via `MusicDb::record_identity_feedback`, the same
/// reversible, append-only soft-identity path the automatic dedup pipeline
/// uses, so this is real calibration data (`dedup_feedback`), not a
/// UI-only toggle. `entry_summaries` already unions sources across a
/// component regardless of which member id is requested, and
/// `dedupe_by_edition` already folds search results down to one
/// (default-edition) row per component, so both merge and hide fall out of
/// existing read paths for free — nothing else needs updating here.
async fn link_relation(state: &Arc<AppState>, body: &[u8]) -> HyperResponse {
    let req = match parse_relation_body(body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let feedback = NewDedupFeedback {
        entry_a: req.a,
        entry_b: req.b,
        judgment: IdentityJudgment::Same,
        origin: "manual".to_string(),
        model_version: None,
        probability: None,
        candidate_channels: None,
        features: None,
        evidence: None,
        note: Some("linked from the player UI".to_string()),
        supersedes_id: None,
    };
    if let Err(e) = state.db.record_identity_feedback(feedback).await {
        return db_error_response(e);
    }
    respond::json(StatusCode::OK, json!({ "ok": true }))
}

/// `POST /api/relations/unlink` — plain (`force: false`, the default)
/// downgrades whatever relation(s) exist between exactly `a` and `b` to
/// disabled via `record_identity_feedback(Unsure)`: the edge drops out of
/// the live component but the row survives, visible as a "suggestion" one
/// click from being restored (`link`). `force: true` instead asserts
/// `different_identity` — a hard block the automatic dedup pipeline won't
/// cross either, shown distinctly, one click from being softened back to a
/// suggestion rather than trivially restored — the "shift-delete" version.
/// Operating on the literal edge (rather than "detach this entry from
/// wherever it's attached") is what makes either correct: components form
/// transitively, so the pair a UI shows as "linked" often isn't the pair
/// holding the actual relation row — the graph view always passes the real
/// edge endpoints here, never a guessed one.
async fn unlink_relation(state: &Arc<AppState>, body: &[u8]) -> HyperResponse {
    let req = match parse_relation_body(body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let (judgment, note) = if req.force {
        (
            IdentityJudgment::Different,
            "force-unlinked (marked different) from the player UI",
        )
    } else {
        (IdentityJudgment::Unsure, "unlinked from the player UI")
    };
    let feedback = NewDedupFeedback {
        entry_a: req.a,
        entry_b: req.b,
        judgment,
        origin: "manual".to_string(),
        model_version: None,
        probability: None,
        candidate_channels: None,
        features: None,
        evidence: None,
        note: Some(note.to_string()),
        supersedes_id: None,
    };
    if let Err(e) = state.db.record_identity_feedback(feedback).await {
        return db_error_response(e);
    }
    respond::json(StatusCode::OK, json!({ "ok": true }))
}

/// The RELATE-tier version-kind taxonomy the dedup pipeline itself can
/// emit (`docs/CONFIG_REFERENCE.md`'s `relate(kind, conf, reason)`
/// constructor) — "related but not the same entry", distinct from the
/// `same_identity`/`different_identity` pair `link`/`unlink` own. Kept as
/// an explicit allowlist so a typo'd or malicious `kind` can't land an
/// arbitrary string in `entry_relation.kind`; also excludes that pair's own
/// kinds (and the pipeline's `"variant"` default, which
/// `soft_identity_projection_for_display` treats as a same-identity signal)
/// so this endpoint can't be used to sidestep the ledger-tracked link/unlink
/// path for those.
///
/// Deliberately does **not** include the dedup-v2 primitive-relation
/// predicates (`musicdb::{MEMBER_OF, DERIVED_FROM, PARTICIPATES_IN,
/// FACET_OF}`, docs/dedup-v2.md) — those are pipeline-only assertions for
/// now (e.g. `decide_track` relating a track to its instrumental via
/// `derived_from`), read-only in the player UI (`entry_relations` still
/// returns them; see `PRIMITIVE_RELATION_KIND_SET` in the frontend). Add a
/// kind here only once manual curation for it has actually been designed.
/// `derived_from` can still be removed and restored (`TOGGLE_KINDS`).
const RELATE_KINDS: &[&str] = &[
    "alt_version",
    "live",
    "remix",
    "instrumental",
    "cover",
    "medley",
    "release_variant",
    "in_release_group",
    "same_artist",
];

/// Primitive-relation kinds a person may tombstone and re-enable, though
/// not create. A tombstoned `derived_from` row stays down: softmatch skips
/// pairs with a rejected edition row (`KnownRelations::already_related`),
/// and the edition/identity projections read enabled rows only. New
/// `derived_from` rows come from `/api/relations/derive`, which records
/// their direction.
const TOGGLE_KINDS: &[&str] = &[DERIVED_FROM];

#[derive(Deserialize)]
struct RelateRequest {
    a: i64,
    b: i64,
    kind: String,
}

fn parse_relate_body(body: &[u8], toggle: bool) -> Result<RelateRequest, HyperResponse> {
    let req: RelateRequest = serde_json::from_slice(body)
        .map_err(|e| respond::error(StatusCode::BAD_REQUEST, "invalid_input", &e.to_string()))?;
    if req.a == req.b {
        return Err(respond::error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "relation endpoints must differ",
        ));
    }
    let kind = req.kind.as_str();
    let allowed = RELATE_KINDS.contains(&kind) || (toggle && TOGGLE_KINDS.contains(&kind));
    if !allowed {
        return Err(respond::error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            &format!("unknown relation kind {:?}", req.kind),
        ));
    }
    Ok(req)
}

/// `POST /api/relations/relate` — asserts a version-kind relation (e.g.
/// `cover`, `remix`, `same_artist` — see `RELATE_KINDS`) directly via
/// `MusicDb::upsert_relation`, the same path the automatic dedup pipeline's
/// own RELATE verdicts use (`pipeline::softmatch`), confidence 1.0 and
/// origin "manual". Deliberately not routed through
/// `record_identity_feedback`/`dedup_feedback` like `link`/`unlink`: that
/// ledger exists to calibrate the same/different identity boundary, and a
/// version-kind annotation isn't an identity judgment — it doesn't affect
/// `soft_identity_projection` (only `same_identity` and, for display,
/// `"variant"` do).
async fn relate_entries(state: &Arc<AppState>, body: &[u8]) -> HyperResponse {
    let req = match parse_relate_body(body, false) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    if let Err(e) = state
        .db
        .upsert_relation(req.a, req.b, &req.kind, 1.0, "manual", None)
        .await
    {
        return db_error_response(e);
    }
    respond::json(StatusCode::OK, json!({ "ok": true }))
}

/// `POST /api/relations/unrelate` — the inverse of `relate`: tombstones
/// (disables, doesn't delete) the one exact `(a, b, kind)` row via
/// `MusicDb::set_relation_enabled`, mirroring how `unlink` never deletes
/// provenance either. `POST /api/relations/restore` re-enables it, keeping
/// its origin and metadata (a `derived_from` row's direction). Both also
/// take `TOGGLE_KINDS`. A pair with no such row is a no-op (`ok: true`
/// either way) rather than an error — the UI only ever offers these for a
/// relation it just showed, so a mismatch here would mean the graph view
/// and the DB had already drifted apart.
async fn set_relation_enabled(state: &Arc<AppState>, body: &[u8], enabled: bool) -> HyperResponse {
    let req = match parse_relate_body(body, true) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    if let Err(e) = state
        .db
        .set_relation_enabled(req.a, req.b, &req.kind, enabled)
        .await
    {
        return db_error_response(e);
    }
    respond::json(StatusCode::OK, json!({ "ok": true }))
}

#[derive(Deserialize)]
struct DeriveRequest {
    original: i64,
    derived: i64,
    transformation: Option<String>,
}

/// `POST /api/relations/derive` `{"original", "derived", "transformation"}`
/// — records that `derived` is an edition of `original` made by
/// `transformation` (`"instrumental"`, `"live"`, ...; null for an
/// unspecified alternate version), replacing whatever the pair's
/// `derived_from` edge said (see `MusicDb::set_edition_derivation`). This is
/// how the now-playing drawer fixes which editions fold into their original
/// (`EditionProjection::rank_target`).
async fn derive_edition(state: &Arc<AppState>, body: &[u8]) -> HyperResponse {
    let req: DeriveRequest = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return respond::error(StatusCode::BAD_REQUEST, "invalid_input", &e.to_string()),
    };
    if let Err(e) = state
        .db
        .set_edition_derivation(req.original, req.derived, req.transformation.as_deref())
        .await
    {
        return db_error_response(e);
    }
    respond::json(StatusCode::OK, json!({ "ok": true }))
}

/// `POST /api/songs/edit` `{"op": "detach" | "set_original" | "set_parent"
/// | "move" | "derive" | "relate" | "disable" | "enable", ...}` — edits a version of a song as a whole (see
/// `musicdb::SongEdit`) and returns `{"undo": [...]}`: the rows it changed
/// as they were, for `/api/relations/rows`. A `different_song` block in
/// the way is a 409 `blocked`, which `set_parent` with `"unblock": true`
/// lifts.
async fn edit_song(state: &Arc<AppState>, body: &[u8]) -> HyperResponse {
    let edit: SongEdit = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return respond::error(StatusCode::BAD_REQUEST, "invalid_input", &e.to_string()),
    };
    if let SongEdit::Relate { kind, .. } = &edit
        && !RELATE_KINDS.contains(&kind.as_str())
    {
        return respond::error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            &format!("unknown relation kind {kind:?}"),
        );
    }
    match state.db.edit_song(edit).await {
        Ok(undo) => respond::json(StatusCode::OK, json!({ "undo": undo })),
        Err(e) => db_error_response(e),
    }
}

#[derive(Deserialize)]
struct RowsRequest {
    rows: Vec<RowImage>,
}

/// `POST /api/relations/rows` `{"rows": [...]}` — writes relation rows back
/// as an earlier edit's `undo` recorded them, and returns `{"redo": [...]}`,
/// the rows it replaced, which this same endpoint takes to redo.
async fn restore_relation_rows(state: &Arc<AppState>, body: &[u8]) -> HyperResponse {
    let req: RowsRequest = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return respond::error(StatusCode::BAD_REQUEST, "invalid_input", &e.to_string()),
    };
    match state.db.restore_relation_rows(req.rows).await {
        Ok(redo) => respond::json(StatusCode::OK, json!({ "redo": redo })),
        Err(e) => db_error_response(e),
    }
}

fn db_error_response(e: DbError) -> HyperResponse {
    match e {
        DbError::InvalidInput(msg) => {
            respond::error(StatusCode::BAD_REQUEST, "invalid_input", &msg)
        }
        DbError::Blocked(msg) => respond::error(StatusCode::CONFLICT, "blocked", &msg),
        DbError::Database(_) => respond::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            &e.to_string(),
        ),
    }
}

async fn playable_sources(state: &Arc<AppState>, id: &str) -> HyperResponse {
    let Ok(entry_id) = id.parse::<i64>() else {
        return respond::error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "id must be numeric",
        );
    };
    // Sources are pulled from every soft-linked member of this entry's
    // component, not just its own row — a track only playable via a
    // soft-linked sibling's Spotify source should still be playable here.
    let members = match component_members(&state.db, entry_id).await {
        Ok(m) => m,
        Err(e) => {
            return respond::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                &e.to_string(),
            );
        }
    };
    let source_rows = match state.db.source_rows_by_entry_ids(&members).await {
        Ok(rows) => rows,
        Err(e) => {
            return respond::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                &e.to_string(),
            );
        }
    };
    let pairs: Vec<(String, String)> = source_rows
        .iter()
        .map(|s| (s.source.clone(), s.identifier.clone()))
        .collect();
    // The weights only label the switcher; playing doesn't need them here.
    let weights = state
        .db
        .preferred_source_weights(&pairs)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("couldn't read preferred sources of {entry_id}: {e:#}");
            HashMap::new()
        });
    let sources: Vec<_> = source_rows
        .iter()
        .zip(&pairs)
        .filter_map(|(s, pair)| {
            let (embed_url, capabilities) = embed_for(&s.source, &s.identifier)?;
            Some(json!({
                "provider": s.source,
                "identifier": s.identifier,
                "embed_url": embed_url,
                "capabilities": capabilities,
                "preferred_weight": weights.get(pair).copied().unwrap_or(0),
            }))
        })
        .collect();
    respond::json(
        StatusCode::OK,
        json!({ "entry_id": entry_id, "sources": sources }),
    )
}

/// `(disc_no, track_no, track entry)`.
type TrackPos = (Option<i32>, Option<i32>, i64);

/// A `TrackPos` for each track `entry_id` lists, in
/// order. Only `track` children count: a release group's children are its
/// releases (and, from some providers, its artist), so a release group with
/// no tracks of its own shows the longest tracklist among its releases.
async fn tracklist(db: &MusicDb, entry_id: i64) -> anyhow::Result<Vec<TrackPos>> {
    let (mut tracks, releases) = direct_children(db, entry_id).await?;
    if tracks.is_empty() {
        for release in releases {
            let (release_tracks, _) = direct_children(db, release).await?;
            if release_tracks.len() > tracks.len() {
                tracks = release_tracks;
            }
        }
    }
    // One provider can list two entries soft dedup has since linked: keep
    // one per soft-identity component.
    let ids: Vec<i64> = tracks.iter().map(|(_, _, id)| *id).collect();
    let (identity, _) = db.display_projections_around(&ids).await?;
    let mut seen = HashSet::new();
    tracks.retain(|(_, _, id)| seen.insert(identity.component_of(*id).unwrap_or(*id)));
    Ok(tracks)
}

/// `entry_id`'s track children (ordered as in `tracklist`) and its release
/// children.
async fn direct_children(db: &MusicDb, entry_id: i64) -> anyhow::Result<(Vec<TrackPos>, Vec<i64>)> {
    let pairs = db.pairs_by_entry_ids(&[entry_id]).await?;
    let children = db.child_rows_for_parent_pairs(&pairs).await?;
    let child_pairs: Vec<(String, String)> = children
        .iter()
        .map(|c| (c.child_source.clone(), c.child_identifier.clone()))
        .collect();
    let pair_to_entry: HashMap<(String, String), i64> = db
        .source_rows_for_pairs(&child_pairs)
        .await?
        .into_iter()
        .map(|s| ((s.source, s.identifier), s.entry_id))
        .collect();
    let child_ids: Vec<i64> = pair_to_entry.values().copied().collect();
    let type_of: HashMap<i64, String> = db
        .entry_rows_by_ids(&child_ids)
        .await?
        .into_iter()
        .map(|e| (e.id, e.entry_type))
        .collect();
    let is = |id: &i64, t: &str| type_of.get(id).is_some_and(|et| et == t);

    // Each of the entry's pairs is one provider's tracklist, listing that
    // provider's own track entries — which dedup may not have linked to the
    // other providers'. Show one provider's list, not their union: the
    // longest, preferring one that numbers its tracks.
    let mut by_pair: HashMap<(&str, &str), Vec<TrackPos>> = HashMap::new();
    for c in &children {
        let key = (c.child_source.clone(), c.child_identifier.clone());
        if let Some(&eid) = pair_to_entry.get(&key).filter(|id| is(id, "track")) {
            by_pair
                .entry((c.parent_source.as_str(), c.parent_identifier.as_str()))
                .or_default()
                .push((c.disc_no, c.track_no, eid));
        }
    }
    let mut tracks = by_pair
        .into_iter()
        .max_by_key(|(pair, list)| {
            let numbered = list.iter().all(|(_, track, _)| track.is_some());
            (list.len(), numbered, std::cmp::Reverse(*pair))
        })
        .map(|(_, list)| list)
        .unwrap_or_default();
    tracks.sort_by_key(|(disc, track, _)| (disc.unwrap_or(0), track.unwrap_or(i32::MAX)));
    let mut releases: Vec<i64> = child_ids
        .into_iter()
        .filter(|id| is(id, "release"))
        .collect();
    releases.sort_unstable();
    releases.dedup();
    Ok((tracks, releases))
}

async fn release_tracks(state: &Arc<AppState>, id: &str) -> HyperResponse {
    let Ok(entry_id) = id.parse::<i64>() else {
        return respond::error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "id must be numeric",
        );
    };
    let ordered = match tracklist(&state.db, entry_id).await {
        Ok(t) => t,
        Err(e) => {
            return respond::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                &e.to_string(),
            );
        }
    };

    let ids: Vec<i64> = ordered.iter().map(|(_, _, id)| *id).collect();
    let summaries = match entry_summaries(&state.db, &ids).await {
        Ok(s) => s,
        Err(e) => {
            return respond::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                &e.to_string(),
            );
        }
    };

    // Whether a track is embeddable via any of its own sources *or* a
    // soft-linked sibling's, so the client can skip/dim entries a playlist
    // queue could never actually play instead of discovering that one at a
    // time via a playable-sources round trip each. `summaries` above already
    // did the soft-identity resolution for titles; reuse its `sources` here
    // too rather than resolving components a second time.
    let mut playable_by_entry: HashMap<i64, bool> = HashMap::new();
    for &eid in &ids {
        let Some(summary) = summaries.get(&eid) else {
            continue;
        };
        let playable = summary
            .sources
            .iter()
            .any(|s| embed_for(&s.source, &s.identifier).is_some());
        playable_by_entry.insert(eid, playable);
    }

    let tracks: Vec<_> = ordered
        .iter()
        .map(|(disc, track, eid)| {
            let title = summaries.get(eid).and_then(|s| s.title.clone());
            json!({
                "entry_id": eid,
                "disc_no": disc,
                "track_no": track,
                "title": title,
                "playable": playable_by_entry.get(eid).copied().unwrap_or(false),
            })
        })
        .collect();
    respond::json(
        StatusCode::OK,
        json!({ "entry_id": entry_id, "tracks": tracks }),
    )
}

#[cfg(test)]
mod tests {
    use super::{Playability, embed_for};

    #[test]
    fn playability_prefers_any_embeddable_source() {
        let local = ("local", "local://ctbgd/track/x");
        let video = ("youtube", "https://youtu.be/dQw4w9WgXcQ");
        let mb = ("musicbrainz", "https://musicbrainz.org/recording/x");
        assert_eq!(Playability::of([local, video]), Playability::Playable);
        assert_eq!(Playability::of([mb, local]), Playability::Unavailable);
        assert_eq!(Playability::of([mb]), Playability::Unlinked);
        assert_eq!(Playability::of([]), Playability::Unlinked);
        // A channel URL isn't a video: still nothing to mount.
        assert_eq!(
            Playability::of([("youtube", "https://www.youtube.com/@x")]),
            Playability::Unlinked
        );
    }

    #[test]
    fn soundcloud_track_embeds() {
        let (url, caps) = embed_for("soundcloud", "https://soundcloud.com/laserimouto/prismatix")
            .expect("track URL should embed");
        assert_eq!(
            url,
            "https://w.soundcloud.com/player/?url=https%3A%2F%2Fsoundcloud.com%2Flaserimouto%2Fprismatix"
        );
        assert_eq!(caps, ["play", "pause", "seek", "ended_event"]);
    }

    #[test]
    fn soundcloud_playlist_and_artist_urls_are_not_embeddable() {
        assert!(
            embed_for(
                "soundcloud",
                "https://soundcloud.com/laserimouto/sets/anime"
            )
            .is_none()
        );
        assert!(embed_for("soundcloud", "https://soundcloud.com/laserimouto").is_none());
    }

    #[test]
    fn nicovideo_watch_url_embeds() {
        let (url, caps) = embed_for("nicovideo", "https://www.nicovideo.jp/watch/sm46158033")
            .expect("watch URL should embed");
        assert_eq!(url, "https://embed.nicovideo.jp/watch/sm46158033?jsapi=1");
        assert_eq!(caps, ["play", "pause", "seek", "ended_event"]);
    }

    #[test]
    fn nicovideo_non_video_urls_are_not_embeddable() {
        assert!(embed_for("nicovideo", "https://www.nicovideo.jp/user/12345").is_none());
        assert!(embed_for("nicovideo", "https://www.nicovideo.jp/mylist/12345").is_none());
    }
}
