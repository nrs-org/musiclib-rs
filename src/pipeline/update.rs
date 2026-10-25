//! The library update (docs/plan-update-roots.md): walk the update roots
//! with listing refresh, within the day's YouTube budget. Run by the
//! player's `POST /api/update` and by `import --update`.
//!
//! 1. Sync the MusicBrainz URL mirror up to the server (never past it), or
//!    refuse to go on if the server's own replication has fallen behind.
//! 2. Import the roots, one run per fetch-options preset, with one listing
//!    store in refresh mode for the whole update: a listing two roots reach
//!    is refreshed once.
//! 3. Full-refresh the oldest snapshots with the budget left.
//! 4. Record each root's outcome on its row.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::Context as _;
use sea_orm::DatabaseConnection;
use serde::Serialize;
use tracing::{info, warn};

use super::{ingest::ingest_entries, softmatch::SoftMatchConfig, state::ImportProgress};
use crate::{
    fetch_presets,
    http::{HttpClient, quota::QuotaLedger},
    mb_freshness,
    mb_sync::{self, SyncOptions, SyncOutcome},
    musicdb::{MusicDb, UpdateRootRow},
    providers::{
        listing::{KindStats, ListingMode, ListingStore, QuotaBudget},
        registry::{RegistryConfig, build_providers_with_listings},
    },
};

/// What an update runs against.
pub struct UpdateContext<'a> {
    pub db: &'a MusicDb,
    pub registry: &'a RegistryConfig,
    /// The client providers are built on.
    pub http: Arc<dyn HttpClient>,
    /// The HTTP cache DB, where listing snapshots live.
    pub listing_db: DatabaseConnection,
    pub ledger: Arc<QuotaLedger>,
    /// The process's YouTube unit counter (`HttpClientConfig`'s).
    pub youtube_quota: Arc<AtomicU64>,
    pub soft_cfg: Option<&'a SoftMatchConfig>,
    pub progress: Option<Arc<dyn ImportProgress>>,
    /// Where fetch-options presets are read from.
    pub fetch_options_dir: PathBuf,
}

pub struct UpdateOptions {
    /// YouTube units the update may use today, across every run.
    pub quota_budget: Option<u64>,
    /// Packets a self-hosted MusicBrainz server may be behind the newest
    /// published one; `None` skips the check (the mirror still stops at the
    /// server).
    pub mb_max_server_lag: Option<i64>,
}

impl Default for UpdateOptions {
    fn default() -> Self {
        Self {
            quota_budget: Some(8000),
            // About a packet an hour, and the server replicates daily.
            mb_max_server_lag: Some(36),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RootOutcome {
    pub id: i64,
    pub url: String,
    /// The entry the root's URL belongs to now.
    pub entry_id: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateReport {
    /// `None` when there's no mirror, or it wasn't synced (no token).
    pub mirror: Option<SyncOutcome>,
    pub roots: Vec<RootOutcome>,
    pub listings: BTreeMap<&'static str, KindStats>,
    /// Units this update used, and today's total across every run.
    pub youtube_units: u64,
    pub youtube_units_today: Option<u64>,
}

/// Run the update over `roots` (the caller picks them: every enabled one,
/// or the ones asked for). Fails only before importing anything (the
/// MusicBrainz check); a root that fails is recorded and the rest go on.
pub async fn run_update(
    ctx: UpdateContext<'_>,
    roots: &[UpdateRootRow],
    opts: &UpdateOptions,
    now: i64,
) -> anyhow::Result<UpdateReport> {
    let units_before = ctx.youtube_quota.load(Ordering::Relaxed);
    let mirror = sync_mirror(ctx.registry, opts.mb_max_server_lag).await?;

    let mut listings = ListingStore::new(ctx.listing_db.clone(), ListingMode::Auto)
        .await?
        .with_kind_modes(ctx.registry.listings.clone());
    if let Some(units) = opts.quota_budget {
        listings = listings.with_budget(QuotaBudget {
            units,
            ledger: Arc::clone(&ctx.ledger),
        });
    }
    let listings = Arc::new(listings);
    let providers = Arc::new(build_providers_with_listings(
        ctx.registry,
        Arc::clone(&ctx.http),
        Some(Arc::clone(&listings)),
    )?);
    if providers.is_empty() {
        anyhow::bail!("no providers available; check the credential env vars");
    }

    // One run per preset, in the order presets first appear.
    let mut groups: Vec<((&str, &str), Vec<&UpdateRootRow>)> = Vec::new();
    for root in roots {
        let key = (root.fetch_options_file.as_str(), root.entry_point.as_str());
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, members)) => members.push(root),
            None => groups.push((key, vec![root])),
        }
    }

    let mut outcomes = Vec::with_capacity(roots.len());
    for ((file, entry_point), members) in groups {
        info!("== {file}::{entry_point}: {} root(s)", members.len());
        let result = match fetch_presets::resolve(&ctx.fetch_options_dir, file, entry_point).await {
            None => Err(format!(
                "fetch options {file}/{entry_point} don't load (missing or invalid)"
            )),
            Some((pool, root_id)) => ingest_entries(
                ctx.db,
                &providers,
                &pool,
                root_id,
                ctx.soft_cfg,
                members.iter().map(|r| r.url.clone()).collect(),
                ctx.progress.clone(),
                None,
            )
            .await
            .map(|outcome| outcome.entry_ids)
            .map_err(|e| format!("{e:#}")),
        };
        if let Err(e) = &result {
            warn!("{file}::{entry_point} failed: {e}");
        }
        for (i, root) in members.into_iter().enumerate() {
            let outcome = match &result {
                Ok(entry_ids) => RootOutcome {
                    id: root.id,
                    url: root.url.clone(),
                    entry_id: entry_ids.get(i).copied().flatten(),
                    error: None,
                },
                Err(e) => RootOutcome {
                    id: root.id,
                    url: root.url.clone(),
                    entry_id: None,
                    error: Some(e.clone()),
                },
            };
            let stats = serde_json::json!({ "entry_id": outcome.entry_id }).to_string();
            let recorded = match &outcome.error {
                None => {
                    ctx.db
                        .record_update_root_run(root.id, now, Ok(&stats))
                        .await
                }
                Some(e) => ctx.db.record_update_root_run(root.id, now, Err(e)).await,
            };
            if let Err(e) = recorded {
                warn!("recording root {}'s outcome failed: {e}", root.id);
            }
            outcomes.push(outcome);
        }
    }

    // Full refreshes of the oldest snapshots with what the walk left.
    listings
        .full_refresh_pass()
        .await
        .context("full-refresh pass")?;
    listings.log_summary().await;

    let youtube_units = ctx.youtube_quota.load(Ordering::Relaxed) - units_before;
    let youtube_units_today = match ctx.ledger.used_today().await {
        Ok(units) => Some(units),
        Err(e) => {
            warn!("reading the YouTube quota ledger failed: {e}");
            None
        }
    };
    info!(
        "update done: {} root(s), {} failed, {youtube_units} YouTube unit(s)",
        outcomes.len(),
        outcomes.iter().filter(|o| o.error.is_some()).count()
    );
    Ok(UpdateReport {
        mirror,
        roots: outcomes,
        listings: listings.stats(),
        youtube_units,
        youtube_units_today,
    })
}

/// Step 1. Without `METABRAINZ_ACCESS_TOKEN` the mirror can't be synced; it
/// is then only checked not to be ahead of the server.
async fn sync_mirror(
    registry: &RegistryConfig,
    max_server_lag: Option<i64>,
) -> anyhow::Result<Option<SyncOutcome>> {
    let Some(mb) = registry.musicbrainz.clone() else {
        return Ok(None);
    };
    let token = std::env::var("METABRAINZ_ACCESS_TOKEN").ok();
    tokio::task::spawn_blocking(move || match token {
        Some(token) => mb_sync::sync(
            Some(&mb),
            &token,
            &SyncOptions {
                max_server_lag,
                ..SyncOptions::default()
            },
        ),
        None => {
            warn!("METABRAINZ_ACCESS_TOKEN is not set; not syncing the MusicBrainz mirror");
            mb_freshness::check_before_import(&mb).map(|()| None)
        }
    })
    .await?
    .context("MusicBrainz mirror")
}
