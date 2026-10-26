//! The library update (docs/plan-update-roots.md): walk the update roots
//! with listing refresh, within the day's YouTube budget. Run by the
//! player's `POST /api/update` and by `import --update`.
//!
//! 1. Sync the MusicBrainz URL mirror up to the server (never past it), or
//!    refuse to go on if the server's own replication has fallen behind.
//! 2. Import the roots, one run per fetch-options preset, with one listing
//!    store in refresh mode for the whole update: a listing two roots reach
//!    is refreshed once. A preset's walk is pruned (what earlier runs
//!    fetched is skipped, `State::prunes`) unless one of its roots is due a
//!    full walk: never walked in full, preset changed since, or the last
//!    full walk is older than `UpdateOptions::full_walk_every`.
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
    /// Told where the update is, as it moves on.
    pub on_step: Option<Arc<dyn Fn(UpdateStep) + Send + Sync>>,
    /// Where fetch-options presets are read from.
    pub fetch_options_dir: PathBuf,
}

/// Where an update is (`UpdateContext::on_step`).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum UpdateStep {
    /// Syncing the MusicBrainz mirror: applied up to `at`, syncing up to
    /// `target` (`None` for the newest published packet).
    Mirror {
        at: Option<i64>,
        target: Option<i64>,
    },
    /// Importing the roots of one preset: the `index`th of `of` (1-based).
    Preset {
        index: usize,
        of: usize,
        file: String,
        entry_point: String,
        roots: usize,
    },
    /// A root's run ended.
    RootDone(RootOutcome),
    /// Spending the leftover budget on the oldest snapshots.
    FullRefresh,
}

pub struct UpdateOptions {
    /// YouTube units the update may use today, across every run.
    pub quota_budget: Option<u64>,
    /// Packets a self-hosted MusicBrainz server may be behind the newest
    /// published one; `None` skips the check (the mirror still stops at the
    /// server).
    pub mb_max_server_lag: Option<i64>,
    /// Walk every root in full.
    pub full: bool,
    /// A root is walked in full again once its last full walk is this old
    /// (seconds).
    pub full_walk_every: i64,
}

impl Default for UpdateOptions {
    fn default() -> Self {
        Self {
            quota_budget: Some(8000),
            // About a packet an hour, and the server replicates daily.
            mb_max_server_lag: Some(36),
            full: false,
            full_walk_every: 7 * 24 * 60 * 60,
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
    /// Walked in full, not pruned.
    pub full_walk: bool,
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
    let step = |s: UpdateStep| {
        if let Some(f) = &ctx.on_step {
            f(s);
        }
    };
    step(UpdateStep::Mirror {
        at: None,
        target: None,
    });
    let mirror = sync_mirror(ctx.registry, opts.mb_max_server_lag, ctx.on_step.clone()).await?;

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
    let preset_count = groups.len();
    for (index, ((file, entry_point), members)) in groups.into_iter().enumerate() {
        info!("== {file}::{entry_point}: {} root(s)", members.len());
        step(UpdateStep::Preset {
            index: index + 1,
            of: preset_count,
            file: file.to_owned(),
            entry_point: entry_point.to_owned(),
            roots: members.len(),
        });
        let preset =
            fetch_presets::resolve_with_hash(&ctx.fetch_options_dir, file, entry_point).await;
        let full = opts.full
            || preset.as_ref().is_none_or(|(_, _, hash)| {
                members
                    .iter()
                    .any(|r| full_walk_due(r, hash, now, opts.full_walk_every))
            });
        info!(
            "{file}::{entry_point}: {} walk",
            if full { "full" } else { "pruned" }
        );
        let result = match preset {
            None => Err(format!(
                "fetch options {file}/{entry_point} don't load (missing or invalid)"
            )),
            Some((pool, root_id, hash)) => {
                let result = ingest_entries(
                    ctx.db,
                    &providers,
                    &pool,
                    root_id,
                    ctx.soft_cfg,
                    members.iter().map(|r| r.url.clone()).collect(),
                    ctx.progress.clone(),
                    None,
                    !full,
                )
                .await
                .map(|outcome| outcome.entry_ids)
                .map_err(|e| format!("{e:#}"));
                if full && result.is_ok() {
                    let ids: Vec<i64> = members.iter().map(|r| r.id).collect();
                    if let Err(e) = ctx.db.record_full_walk(&ids, now, &hash).await {
                        warn!("recording {file}::{entry_point}'s full walk failed: {e}");
                    }
                }
                result
            }
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
                    full_walk: full,
                },
                Err(e) => RootOutcome {
                    id: root.id,
                    url: root.url.clone(),
                    entry_id: None,
                    error: Some(e.clone()),
                    full_walk: full,
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
            step(UpdateStep::RootDone(outcome.clone()));
            outcomes.push(outcome);
        }
    }

    // Full refreshes of the oldest snapshots with what the walk left.
    step(UpdateStep::FullRefresh);
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

/// Whether `root` is due a full walk with the preset hashing to `hash`.
fn full_walk_due(root: &UpdateRootRow, hash: &str, now: i64, every: i64) -> bool {
    match (root.full_walk_at, &root.full_walk_hash) {
        (Some(at), Some(last)) => last != hash || now - at >= every,
        _ => true,
    }
}

/// Step 1. Without `METABRAINZ_ACCESS_TOKEN` the mirror can't be synced; it
/// is then only checked not to be ahead of the server.
async fn sync_mirror(
    registry: &RegistryConfig,
    max_server_lag: Option<i64>,
    on_step: Option<Arc<dyn Fn(UpdateStep) + Send + Sync>>,
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
                on_packet: on_step.map(|f| {
                    Box::new(move |at, target| {
                        f(UpdateStep::Mirror {
                            at: Some(at),
                            target,
                        })
                    }) as mb_sync::PacketHook
                }),
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

/// What `preview_root` found.
#[derive(Debug, Clone, Serialize)]
pub struct RootPreview {
    pub source: String,
    pub identifier: String,
    pub entry_type: crate::providers::types::EntryType,
    pub name: Option<String>,
    pub listings: Vec<crate::providers::matcher::PreviewListing>,
    /// Listings that failed partway; their children so far are still listed.
    pub errors: Vec<String>,
}

/// Fetch `url` with the fetch options `pool`/`root_id` and report what they
/// decide for each of its direct children (`matcher::preview`): fetched,
/// skipped, or undecided because the deciding rule needs the child's own
/// data. Nothing is written to the library. Without `fetch_children` no
/// child is fetched; with it, the children those rules need are (about a
/// YouTube unit per 50 videos). Reading the root's listings costs what it
/// costs (a few units for a channel); every response lands in the HTTP
/// cache for the real import.
pub async fn preview_root(
    providers: &[Arc<dyn crate::providers::FetchProvider>],
    url: &str,
    pool: Arc<crate::providers::types::EntryFetchOptionsPool>,
    root_id: crate::providers::types::OptionsId,
    fetch_children: bool,
) -> anyhow::Result<RootPreview> {
    use crate::providers::{matcher::preview, std_values::StandardProviderKeys, types::child_next};
    let mut found = None;
    for p in providers {
        if let Some(c) = p.canonicalize(StandardProviderKeys::UNKNOWN_URL, url).await {
            found = Some((Arc::clone(p), c));
            break;
        }
    }
    let Some((provider, canonical)) = found else {
        anyhow::bail!("no provider recognises {url:?}");
    };
    let source = canonical.canonical_source_key.into_owned();
    let identifier = canonical.canonical_identifier;
    let (result, listings) = preview(
        async {
            let entity = Arc::clone(&provider)
                .fetch_entry(&source, &identifier, pool, root_id)
                .await?;
            let mut errors = Vec::new();
            for listing in &entity.children {
                let mut cursor = listing.cursor();
                loop {
                    match child_next(&mut cursor).await {
                        Ok(Some(_)) => {}
                        Ok(None) => break,
                        Err(e) => {
                            errors.push(e.to_string());
                            break;
                        }
                    }
                }
            }
            Ok::<_, crate::providers::types::Error>((entity, errors))
        },
        fetch_children,
    )
    .await;
    let (entity, errors) = result?;
    Ok(RootPreview {
        source,
        identifier,
        entry_type: entity.specific_data.entry_type(),
        name: entity.aliases.first().map(|a| a.name.clone()),
        listings,
        errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(full_walk_at: Option<i64>, full_walk_hash: Option<&str>) -> UpdateRootRow {
        UpdateRootRow {
            id: 1,
            url: "u".into(),
            source: "youtube".into(),
            identifier: "u".into(),
            fetch_options_file: "f".into(),
            entry_point: "main".into(),
            label: None,
            enabled: true,
            added_at: 0,
            last_run_at: None,
            last_status: None,
            last_error: None,
            last_stats: None,
            full_walk_at,
            full_walk_hash: full_walk_hash.map(str::to_owned),
            entry_id: None,
        }
    }

    /// Due when never walked in full, when the preset changed, and once
    /// the last full walk is `every` old.
    #[test]
    fn full_walk_due_when_new_changed_or_old() {
        assert!(full_walk_due(&root(None, None), "h", 100, 50));
        assert!(full_walk_due(&root(Some(90), Some("old")), "h", 100, 50));
        assert!(!full_walk_due(&root(Some(90), Some("h")), "h", 100, 50));
        assert!(full_walk_due(&root(Some(50), Some("h")), "h", 100, 50));
    }
}
