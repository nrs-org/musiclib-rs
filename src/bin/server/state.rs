use std::sync::Arc;

use musiclib_rs::{
    musicdb::MusicDb,
    pipeline::{dedup::DedupConfig, softmatch::SoftMatchConfig},
    providers::{
        FetchProvider,
        types::{EntryFetchOptionsPool, OptionsId},
    },
};

use crate::jobs::JobManager;

/// Shared server state, handed to every request handler behind one `Arc`.
/// Both route groups (`store_protocol`, `player`) read from the same
/// `MusicDb`/provider set so there's exactly one process writing to
/// `musiclib.db`.
pub struct AppState {
    pub db: MusicDb,
    pub providers: Arc<Vec<Arc<dyn FetchProvider>>>,
    /// Fetch options used by the store-protocol `ingest` job, and by the
    /// player UI's `/api/ingest` when the request doesn't name a preset.
    /// Named presets the import form can choose *instead* are resolved
    /// fresh per request by `fetch_options::list`/`resolve` — not stored
    /// here — so new preset files show up without a server restart; see
    /// that module's doc comment.
    pub pool: Arc<EntryFetchOptionsPool>,
    pub root_id: OptionsId,
    pub dedup_configs: Vec<(String, DedupConfig)>,
    pub merged_dedup: DedupConfig,
    /// `None` disables online soft-dedup after an ingest (no
    /// `<config_dir>/match.rhai`), matching the `import` CLI's behavior.
    pub soft_cfg: Option<SoftMatchConfig>,
    pub jobs: Arc<JobManager>,
}
