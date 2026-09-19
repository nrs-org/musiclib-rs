//! One full "URL in, entry out" ingest run: fetch, flush to the DB, reconcile
//! dedup-barrier tags, and run online soft-dedup. This is exactly the
//! sequence the `import` CLI binary runs; it is factored out here so any
//! caller (the CLI, the `server` binary's store-protocol `ingest` job) can
//! trigger it as a plain function call instead of shelling out.

use std::{collections::HashSet, sync::Arc};

use crate::{
    musicdb::MusicDb,
    providers::{
        FetchProvider, registry::canonicalize as canonicalize_raw,
        std_values::StandardProviderKeys, types::EntryFetchOptionsPool,
    },
};

use super::{
    dedup::DedupConfig,
    flush::flush,
    importer::import,
    softmatch::{SoftMatchConfig, match_new_entries},
    state::{ImportProgress, State},
};

/// Result of ingesting one URL.
pub struct IngestOutcome {
    /// The entry the input URL now belongs to, if any provider recognised it.
    /// `None` means every provider rejected the URL outright (nothing was
    /// fetched or written).
    pub entry_id: Option<i64>,
    /// Every entry touched by the flush (the ingested entry plus anything
    /// that got merged into or cross-linked with it). Useful to callers that
    /// want to look past the single `entry_id`, e.g. for audit logging.
    pub touched_ids: HashSet<i64>,
}

/// Ingest `url`: fetch via `providers`/`pool` starting at `options_id`, flush
/// into `db`, reconcile barrier tags, and (if `soft_cfg` is given) run online
/// soft-dedup against the newly touched entries.
///
/// `dedup_configs` is the raw `(source_label, DedupConfig)` list (used for tag
/// reconciliation); `merged_dedup` is `dedup::merge_configs(dedup_configs)`,
/// passed separately since callers that already have it (the CLI binaries do,
/// to avoid recompiling the barrier on every call) shouldn't have to redo it.
///
/// `progress`, if given, is wired straight into the `State` driving this
/// run's `import()` traversal — see `state::ImportProgress`. The CLI passes
/// `None`; the `server` binary uses it to feed its job progress display.
#[allow(clippy::too_many_arguments)]
pub async fn ingest_entry(
    db: &MusicDb,
    providers: &Arc<Vec<Arc<dyn FetchProvider>>>,
    pool: &Arc<EntryFetchOptionsPool>,
    options_id: crate::providers::types::OptionsId,
    dedup_configs: &[(String, DedupConfig)],
    merged_dedup: &DedupConfig,
    soft_cfg: Option<&SoftMatchConfig>,
    url: String,
    progress: Option<Arc<dyn ImportProgress>>,
) -> anyhow::Result<IngestOutcome> {
    // Resolve the canonical (source, identifier) pair for `url` up front so we
    // can look up its entry_id after flush, regardless of whether flush wrote
    // it under the canonical pair or (no provider recognised it) the raw one.
    let canonical_pair = canonicalize_raw(&url)
        .map(|c| (c.canonical_source_key.into_owned(), c.canonical_identifier));
    let lookup_pair = canonical_pair
        .clone()
        .unwrap_or_else(|| (StandardProviderKeys::UNKNOWN_URL.to_string(), url.clone()));

    let state = Arc::new(State::with_progress(progress));
    import(
        Arc::clone(&state),
        Arc::clone(providers),
        Arc::clone(pool),
        (StandardProviderKeys::UNKNOWN_URL.to_string(), url),
        options_id,
    )
    .await;

    let touched_ids = flush(state, providers.as_slice(), db, merged_dedup).await?;
    if !dedup_configs.is_empty() {
        super::dedup::reconcile_tags(providers.as_slice(), db, dedup_configs).await?;
    }
    if let Some(cfg) = soft_cfg {
        match_new_entries(db, &touched_ids, merged_dedup, providers.as_slice(), cfg).await?;
    }

    let entry_id = db
        .find_entry_id_by_pair(&lookup_pair.0, &lookup_pair.1)
        .await?;
    Ok(IngestOutcome {
        entry_id,
        touched_ids,
    })
}
