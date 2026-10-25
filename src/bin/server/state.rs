#[cfg(feature = "nrs")]
use std::sync::Mutex;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64},
};

use musiclib_rs::{
    http::quota::QuotaLedger,
    musicdb::MusicDb,
    pipeline::{progress::HttpActivityClient, softmatch::SoftMatchConfig},
    providers::{
        FetchProvider,
        registry::RegistryConfig,
        types::{EntryFetchOptionsPool, OptionsId},
    },
};
use sea_orm::DatabaseConnection;

use crate::jobs::JobManager;
#[cfg(feature = "player")]
use crate::playback::PlaybackHub;

/// Shared server state, handed to every request handler behind one `Arc`.
/// Both route groups (`store_protocol`, `player`) read from the same
/// `MusicDb`/provider set so there's exactly one process writing to
/// `musiclib.db`.
pub struct AppState {
    pub db: MusicDb,
    /// Built over a listing store that serves snapshots (`ListingMode::Cached`):
    /// an ingest sees what the last update saw. A job that refreshes
    /// listings (an update, an ingest asking to) builds its own from
    /// `registry` and `activity`, over a store in refresh mode.
    pub providers: Arc<Vec<Arc<dyn FetchProvider>>>,
    pub registry: Arc<RegistryConfig>,
    /// The HTTP cache DB: listing snapshots and the YouTube quota ledger.
    pub listing_db: DatabaseConnection,
    pub ledger: Arc<QuotaLedger>,
    /// Set while the library update runs; only one runs at a time.
    pub updating: AtomicBool,
    /// Fetch options used by the store-protocol `ingest` job, and by the
    /// player UI's `/api/ingest` when the request doesn't name a preset.
    /// Named presets the import form can choose *instead* are resolved
    /// fresh per request by `fetch_options::list`/`resolve` — not stored
    /// here — so new preset files show up without a server restart; see
    /// that module's doc comment.
    pub pool: Arc<EntryFetchOptionsPool>,
    pub root_id: OptionsId,
    /// `None` disables online soft-dedup after an ingest (no
    /// `<config_dir>/match.rhai`), matching the `import` CLI's behavior.
    pub soft_cfg: Option<SoftMatchConfig>,
    pub jobs: Arc<JobManager>,
    /// Process-wide HTTP request counts per domain, wrapped around the
    /// shared client every provider was built with — see
    /// `ingest_progress::JobProgressSink`, which embeds a snapshot of this
    /// in each running `ingest` job's progress payload.
    pub activity: Arc<HttpActivityClient>,
    /// Cumulative YouTube Data API quota units spent since the process
    /// started (every provider shares one counter, same as `activity`) —
    /// mirrors the `import` CLI's end-of-run "quota used" line, but kept
    /// running and surfaced live through `ingest_progress::JobProgressSink`.
    pub youtube_quota: Arc<AtomicU64>,
    /// The NRS app the player's extension talks to (see `nrs_pool`,
    /// `nrs_bridge`).
    #[cfg(feature = "nrs")]
    pub nrs_url: String,
    /// Where browsers reach the NRS app: the base of the links the bridge
    /// hands out (`nrs_url` unless configured otherwise).
    #[cfg(feature = "nrs")]
    pub nrs_public_url: String,
    #[cfg(feature = "nrs")]
    pub nrs_pool: Mutex<Option<Arc<crate::nrs_pool::NrsPool>>>,
    /// The player's one playback session (queue, devices, position) — see
    /// `playback`.
    #[cfg(feature = "player")]
    pub playback: Arc<PlaybackHub>,
    #[cfg(feature = "nrs")]
    pub nrs_bridge: crate::nrs_bridge::BridgeState,
    /// The player's Spotify login, shared by every device — see
    /// `spotify_auth`.
    #[cfg(feature = "player")]
    pub spotify: crate::spotify_auth::SpotifyAuth,
    #[cfg(feature = "player")]
    pub lastfm: crate::lastfm::LastFm,
}
