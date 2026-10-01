//! One full "URL in, entry out" ingest run: fetch, flush to the DB, reconcile
//! dedup-barrier tags, and run online soft-dedup. This is exactly the
//! sequence the `import` CLI binary runs; it is factored out here so any
//! caller (the CLI, the `server` binary's store-protocol `ingest` job) can
//! trigger it as a plain function call instead of shelling out.

use std::{collections::HashSet, sync::Arc, time::Duration};

use tracing::warn;

use crate::{
    http::Activity,
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
    softmatch::{SoftMatchConfig, match_new_entries_offloaded},
    state::{ImportProgress, State},
};

/// Once `State`'s estimated buffered size (`State::approx_bytes`) crosses
/// this, the watchdog spawned by `ingest_entry` drains it with a periodic
/// flush. A large artist discography (many tracks, each with its own alias
/// list) can otherwise hold gigabytes of `PairMetadata`/`ChildEdge` in memory
/// for the entire traversal before today's single end-of-import flush ever
/// runs.
const DEFAULT_FLUSH_THRESHOLD_BYTES: usize = 256 * 1024 * 1024;

/// How often the watchdog checks `State::approx_bytes` against the
/// threshold. Cheap (an atomic load), so this can be short without adding
/// meaningful overhead.
const FLUSH_CHECK_INTERVAL: Duration = Duration::from_secs(2);

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

/// Result of ingesting several URLs in one run (see [`ingest_entries`]).
pub struct MultiIngestOutcome {
    /// Per input URL, in input order: the entry it now belongs to (see
    /// [`IngestOutcome::entry_id`]).
    pub entry_ids: Vec<Option<i64>>,
    /// Every entry touched by the run's flush, across all URLs.
    pub touched_ids: HashSet<i64>,
}

/// Ingest `url`: a single-URL [`ingest_entries`].
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
    let outcome = ingest_entries(
        db,
        providers,
        pool,
        options_id,
        dedup_configs,
        merged_dedup,
        soft_cfg,
        vec![url],
        progress,
        None,
    )
    .await?;
    Ok(IngestOutcome {
        entry_id: outcome.entry_ids.into_iter().next().flatten(),
        touched_ids: outcome.touched_ids,
    })
}

/// Ingest every URL in `urls` as one run: fetch via `providers`/`pool` starting at `options_id`, flush
/// into `db`, reconcile barrier tags, and (if `soft_cfg` is given) run online
/// soft-dedup against the newly touched entries.
///
/// All URLs are traversed concurrently against one shared `State`, so their
/// fetches land in the same coalescer batches and an entity reachable from
/// several URLs is fetched once.
///
/// `dedup_configs` is the raw `(source_label, DedupConfig)` list (used for tag
/// reconciliation); `merged_dedup` is `dedup::merge_configs(dedup_configs)`,
/// passed separately since callers that already have it (the CLI binaries do,
/// to avoid recompiling the barrier on every call) shouldn't have to redo it.
///
/// `progress`, if given, is wired straight into the `State` driving this
/// run's `import()` traversal — see `state::ImportProgress`. The CLI passes
/// `None`; the `server` binary uses it to feed its job progress display.
///
/// The whole run is wrapped in an `import_run` (see `MusicDb::start_import_run`):
/// while the traversal is in flight, a background watchdog periodically
/// flushes buffered `State` once it grows past `DEFAULT_FLUSH_THRESHOLD_BYTES`,
/// so a large discography no longer has to be held in memory for the entire
/// import. Every row those periodic flushes write is tagged with the run id,
/// which is what makes this safe: if `ingest_entries` errors out, or the
/// process is killed outright before it gets the chance, the run's rows are
/// purged (immediately on an ordinary error; by `MusicDb::recover_pending_import_runs`
/// on the next startup after a hard crash) rather than left as a
/// half-imported entity sitting in the library. See `pipeline::flush`'s doc
/// comment for why a periodic flush never needs anything more than a plain
/// delete-by-tag to undo.
#[allow(clippy::too_many_arguments)]
pub async fn ingest_entries(
    db: &MusicDb,
    providers: &Arc<Vec<Arc<dyn FetchProvider>>>,
    pool: &Arc<EntryFetchOptionsPool>,
    options_id: crate::providers::types::OptionsId,
    dedup_configs: &[(String, DedupConfig)],
    merged_dedup: &DedupConfig,
    soft_cfg: Option<&SoftMatchConfig>,
    urls: Vec<String>,
    progress: Option<Arc<dyn ImportProgress>>,
    max_fetches: Option<usize>,
) -> anyhow::Result<MultiIngestOutcome> {
    // A run abandoned by a crash in some earlier invocation (this process or
    // another one that has since died) is only safe to clean up before new
    // work starts referencing the same tables; see
    // `MusicDb::recover_pending_import_runs` for why the pid check makes
    // this safe to call unconditionally, including when another process is
    // legitimately mid-import against the same DB file right now.
    match db.recover_pending_import_runs().await {
        Ok(purged) if !purged.is_empty() => {
            warn!(
                "recovered {} abandoned import run(s): {:?}",
                purged.len(),
                purged
            );
        }
        Ok(_) => {}
        Err(e) => warn!("failed to check for abandoned import runs: {e}"),
    }

    // Resolve the canonical (source, identifier) pair for each URL up front so
    // we can look up its entry_id after flush, regardless of whether flush
    // wrote it under the canonical pair or (no provider recognised it) the raw
    // one.
    let lookup_pairs: Vec<(String, String)> = urls
        .iter()
        .map(|url| {
            canonicalize_raw(url)
                .map(|c| (c.canonical_source_key.into_owned(), c.canonical_identifier))
                .unwrap_or_else(|| (StandardProviderKeys::UNKNOWN_URL.to_string(), url.clone()))
        })
        .collect();

    let run_id = db.start_import_run().await?;

    let state = Arc::new(State::with_progress(progress));
    state.set_fetch_budget(max_fetches);
    let result = run_traversal_with_watchdog(
        Arc::clone(&state),
        providers,
        pool,
        urls,
        options_id,
        db,
        merged_dedup,
        run_id,
    )
    .await;

    let touched_ids = match result {
        Ok(touched_ids) => {
            db.commit_import_run(run_id, &state.take_replacements())
                .await?;
            touched_ids
        }
        Err(e) => {
            if let Err(rollback_err) = db.rollback_import_run(run_id).await {
                warn!("failed to roll back import run {run_id}: {rollback_err}");
            }
            return Err(e);
        }
    };

    if !dedup_configs.is_empty() {
        super::dedup::reconcile_tags(providers.as_slice(), db, dedup_configs).await?;
    }
    if let Some(cfg) = soft_cfg {
        match_new_entries_offloaded(
            db.clone(),
            touched_ids.clone(),
            merged_dedup.clone(),
            Arc::clone(providers),
            cfg.clone(),
        )
        .await?;
    }

    let mut entry_ids = Vec::with_capacity(lookup_pairs.len());
    for (source, identifier) in &lookup_pairs {
        entry_ids.push(db.find_entry_id_by_pair(source, identifier).await?);
    }
    Ok(MultiIngestOutcome {
        entry_ids,
        touched_ids,
    })
}

/// Run the `import()` traversal to completion while a background watchdog
/// periodically drains `state` (tagged with `run_id`) once it grows past
/// `DEFAULT_FLUSH_THRESHOLD_BYTES`, then performs the run's single final
/// (`final_flush = true`) flush, which -- unlike the periodic ones -- writes
/// every remaining class regardless of whether it touches a pre-existing
/// entry. Returns the touched entry ids from that final flush.
#[allow(clippy::too_many_arguments)]
async fn run_traversal_with_watchdog(
    state: Arc<State>,
    providers: &Arc<Vec<Arc<dyn FetchProvider>>>,
    pool: &Arc<EntryFetchOptionsPool>,
    urls: Vec<String>,
    options_id: crate::providers::types::OptionsId,
    db: &MusicDb,
    merged_dedup: &DedupConfig,
    run_id: i64,
) -> anyhow::Result<HashSet<i64>> {
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
    let watchdog = tokio::spawn({
        let state = Arc::clone(&state);
        let providers = Arc::clone(providers);
        let db = db.clone();
        let merged_dedup = merged_dedup.clone();
        async move {
            let mut interval = tokio::time::interval(FLUSH_CHECK_INTERVAL);
            interval.tick().await; // skip the immediate first tick
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if state.approx_bytes() >= DEFAULT_FLUSH_THRESHOLD_BYTES
                            && let Err(e) = flush(
                                Arc::clone(&state),
                                providers.as_slice(),
                                &db,
                                &merged_dedup,
                                Some(run_id),
                                false,
                            )
                            .await
                        {
                            warn!("periodic flush failed (run {run_id}): {e}");
                        }
                    }
                    _ = stop_rx.changed() => break,
                }
            }
        }
    });

    // Tracked so the coalescer knows the traversal's CPU work between awaits
    // is still running (and may yet enqueue more ids) — see `http::activity`.
    let roots = urls.into_iter().map(|url| {
        import(
            Arc::clone(&state),
            Arc::clone(providers),
            Arc::clone(pool),
            (StandardProviderKeys::UNKNOWN_URL.to_string(), url),
            options_id,
        )
    });
    Activity::global()
        .track(futures::future::join_all(roots))
        .await;

    // Stop the watchdog and wait for it to actually exit before the final
    // flush below, so the two never run concurrently against the same state.
    let _ = stop_tx.send(true);
    if let Err(e) = watchdog.await {
        warn!("periodic flush watchdog panicked (run {run_id}): {e}");
    }

    // Over the fetch budget, the traversal stopped fetching partway, so what
    // it collected is incomplete. Fail instead of flushing; the caller rolls
    // the run back (including anything periodic flushes already wrote).
    let (fetches, over_budget) = state.fetch_count();
    tracing::info!("run {run_id}: {fetches} fetch(es)");
    if over_budget {
        anyhow::bail!(
            "fetch budget exceeded ({fetches} fetches attempted); run {run_id} rolled back"
        );
    }

    flush(
        state,
        providers.as_slice(),
        db,
        merged_dedup,
        Some(run_id),
        true,
    )
    .await
}
