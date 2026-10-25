use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::Context as _;
use clap::Parser;
use musiclib_rs::{
    app_dirs,
    http::{HttpClientConfig, quota::QuotaLedger},
    httpcache,
    musicdb::{LINKS_COMPLETE, MusicDb},
    pipeline::{
        dedup::enforce_barriers, ingest::ingest_entries, progress,
        softmatch::default_soft_match_config,
    },
    providers::{
        fetch_options_yaml::load_from_file,
        listing::{ListingMode, ListingStore, QuotaBudget},
        registry::{RegistryConfig, build_providers_with_listings},
        types::{
            ChildMatcher, ChildMatcherExpr, ChildRule, EntryFetchOptions, EntryFetchOptionsPool,
        },
    },
};
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// URL(s) to import. All are traversed together in one run, so their
    /// fetches share API batches.
    #[arg(required_unless_present_any = ["url_file", "backfill_links"])]
    urls: Vec<String>,
    /// File with more URLs to import, one per line (blank lines and lines
    /// starting with `#` are ignored).
    #[arg(long)]
    url_file: Option<PathBuf>,
    /// Fetch-options YAML file. A bare filename (no path separator) is resolved
    /// relative to <config_dir>/fetch_options/.
    #[arg(long)]
    fetch_options: Option<String>,
    /// Music library database. Defaults to <state_dir>/musiclib.db.
    #[arg(long)]
    db: Option<String>,
    /// Provider credentials config. Defaults to <config_dir>/providers.yaml.
    #[arg(long)]
    registry_config: Option<String>,
    /// HTTP client config. Defaults to <config_dir>/http.yaml.
    #[arg(long)]
    http_config: Option<String>,
    /// Instead of importing URLs, re-import every fetched pair (listing
    /// children, fetching none) to record the identity links each states,
    /// retry the ones that fail once, then mark the DB's links complete so
    /// re-imports can split entries (docs/plan-identity-retraction.md).
    #[arg(long, conflicts_with_all = ["urls", "url_file", "fetch_options"])]
    backfill_links: bool,
    /// Skip the pre-import pass that re-applies violated barriers.
    #[arg(long)]
    skip_dedup: bool,
    /// Skip the post-import online soft-match pass.
    #[arg(long)]
    skip_softmatch: bool,
    /// Safety cap on entity fetches for the whole run. If the traversal
    /// needs more, it stops fetching and the run fails and is rolled back,
    /// rather than committing a partial import (re-running with a higher cap
    /// is cheap: responses are in the HTTP cache).
    #[arg(long)]
    max_fetches: Option<usize>,
    /// Refresh growing listings (uploads, discographies, playlists) instead
    /// of serving their snapshots. `auto` (the bare flag) refreshes YouTube
    /// uploads incrementally and the rest in full; `full` refetches all.
    /// (`--refresh=full` needs the `=`, so `--refresh <url>` reads the URL.)
    #[arg(
        long,
        value_enum,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "auto"
    )]
    refresh: Option<RefreshArg>,
    /// YouTube units refreshes may use today, counting every run's usage
    /// (Pacific-time day). Once spent, listings are served from their
    /// snapshots. With `--refresh`, the rest of the budget goes to full
    /// refreshes of the oldest uploads snapshots after the import. Meant for
    /// the cron job; leave it off for manual refreshes.
    #[arg(long)]
    quota_budget: Option<u64>,
    /// Skip the full-refresh pass after the import. For all but the last of
    /// several runs sharing one day's budget, so an early run's pass doesn't
    /// spend what later runs need for finding new items.
    #[arg(long)]
    no_full_refresh_pass: bool,
    #[command(flatten)]
    dirs: musiclib_rs::app_dirs::DirArgs,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum RefreshArg {
    Auto,
    Full,
}

/// Load YAML config from an explicit path (errors if missing) or a default
/// path (silently falls back to T::default() if missing).
async fn load_config<T: Default + serde::de::DeserializeOwned>(
    explicit: Option<&str>,
    default: &Path,
) -> anyhow::Result<T> {
    let path = match explicit {
        Some(p) => PathBuf::from(p),
        None if default.exists() => default.to_owned(),
        None => return Ok(T::default()),
    };
    let text = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(serde_yaml_ng::from_str(&text)?)
}

fn resolve_fetch_options(s: &str) -> PathBuf {
    let p = Path::new(s);
    if p.is_absolute() || p.components().count() > 1 {
        p.to_owned()
    } else {
        app_dirs::config_dir().join("fetch_options").join(p)
    }
}

/// `--backfill-links`: see its doc on `Args`.
async fn backfill_links(
    db: &MusicDb,
    providers: &Arc<Vec<Arc<dyn musiclib_rs::providers::FetchProvider>>>,
) -> anyhow::Result<()> {
    /// Pairs per import run: each commits on its own, so an interrupted
    /// backfill keeps what it finished.
    const RUN_SIZE: usize = 1000;
    let mut pool = EntryFetchOptionsPool::default();
    let root_id = pool.insert(EntryFetchOptions {
        child_rules: vec![],
    });
    let pool = Arc::new(pool);
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let mut pending = db.live_fetched_pairs(None).await?;
    info!(
        "Backfilling links: re-importing {} fetched pair(s)",
        pending.len()
    );
    for pass in 1..=2 {
        let urls: Vec<String> = pending.iter().map(|(_, id)| id.clone()).collect();
        let runs = urls.len().div_ceil(RUN_SIZE);
        for (n, chunk) in urls.chunks(RUN_SIZE).enumerate() {
            info!("Pass {pass}, run {}/{runs}", n + 1);
            ingest_entries(
                db,
                providers,
                &pool,
                root_id,
                None,
                chunk.to_vec(),
                None,
                None,
            )
            .await?;
        }
        pending = db.live_fetched_pairs(Some(started)).await?;
        if pending.is_empty() {
            break;
        }
        warn!(
            "{} pair(s) failed to re-fetch{}: {:?}",
            pending.len(),
            if pass == 1 {
                "; retrying them once"
            } else {
                ""
            },
            pending.iter().take(20).collect::<Vec<_>>(),
        );
    }
    db.set_setting(LINKS_COMPLETE, "1").await?;
    info!(
        "Links backfilled ({} pair(s) never re-fetched); re-imports now split entries a removed link disconnects",
        pending.len()
    );
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let res = run().await;
    // Responses still queued for the HTTP cache would be lost on exit.
    musiclib_rs::httpcache::db::flush_all().await;
    res
}

async fn run() -> anyhow::Result<()> {
    dotenv::dotenv().ok();

    let args = Args::parse();
    args.dirs.apply();
    let config_dir = app_dirs::config_dir();

    let registry_config: RegistryConfig = load_config(
        args.registry_config.as_deref(),
        &config_dir.join("providers.yaml"),
    )
    .await?;

    let mut http_config: HttpClientConfig =
        load_config(args.http_config.as_deref(), &config_dir.join("http.yaml")).await?;
    http_config.coalescer_rules = musiclib_rs::providers::registry::coalesce_rules();

    let youtube_quota = Arc::new(AtomicU64::new(0));
    http_config.youtube_quota_counter = Some(Arc::clone(&youtube_quota));

    // Listing snapshots and the quota ledger live in the HTTP cache DB
    // (in memory when there's none: every listing is then a snapshot miss).
    let cache_db_url = http_config
        .db_cache
        .as_ref()
        .map_or_else(|| "sqlite::memory:".to_owned(), |c| c.path.clone());
    let cache_db = httpcache::db::connect(cache_db_url).await?;
    let ledger = Arc::new(QuotaLedger::new(cache_db.clone(), Arc::clone(&youtube_quota)).await?);
    let mode = match args.refresh {
        None => ListingMode::Cached,
        Some(RefreshArg::Auto) => ListingMode::Auto,
        Some(RefreshArg::Full) => ListingMode::Full,
    };
    let mut listings = ListingStore::new(cache_db, mode)
        .await?
        .with_kind_modes(registry_config.listings.clone());
    if let Some(units) = args.quota_budget {
        listings = listings.with_budget(QuotaBudget {
            units,
            ledger: Arc::clone(&ledger),
        });
    }
    let listings = Arc::new(listings);

    // The quota total is otherwise only logged once, at clean exit — a killed
    // process (SIGTERM during a batch, common when driving many imports and
    // culling stuck/unwanted ones) leaves no record at all of what it really
    // consumed, even though the counter itself was tracking correctly the
    // whole time. Log it periodically too so a killed run's log still has a
    // recent, accurate figure instead of nothing.
    // The ledger is flushed on the same tick, so a killed run's units still
    // count toward the day.
    {
        let youtube_quota = Arc::clone(&youtube_quota);
        let ledger = Arc::clone(&ledger);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            interval.tick().await; // skip the immediate first tick
            loop {
                interval.tick().await;
                info!(
                    "YouTube Data API quota used so far: {} unit(s)",
                    youtube_quota.load(Ordering::Relaxed),
                );
                if let Err(e) = ledger.flush().await {
                    warn!("recording YouTube quota use failed: {e}");
                }
            }
        });
    }

    let http = http_config.build().await?;
    // Cloned before `http` is wrapped/consumed below — see softmatch.rs's
    // analogous clone for why the script-facing `http_call(...)` primitive
    // gets the raw (non-progress-wrapped) client.
    let http_for_script = Arc::clone(&http);
    let http = progress::ProgressHttpClient::new(http);

    tracing_subscriber::fmt()
        .with_writer(progress::MultiProgressMakeWriter::new(Arc::clone(
            &http.multi,
        )))
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // A MusicBrainz URL mirror ahead of its self-hosted server would send
    // lookups for links the server doesn't have yet, and cache the 404s.
    if let Some(mb) = registry_config.musicbrainz.clone() {
        tokio::task::spawn_blocking(move || musiclib_rs::mb_freshness::check_before_import(&mb))
            .await??;
    }

    let providers =
        build_providers_with_listings(&registry_config, http, Some(Arc::clone(&listings)))?;
    if providers.is_empty() {
        anyhow::bail!("No providers available -- check your credential env vars");
    }
    info!("Loaded {} provider(s)", providers.len());
    let providers = Arc::new(providers);

    let db_path = args
        .db
        .map(PathBuf::from)
        .unwrap_or_else(|| app_dirs::data_dir().join("musiclib.db"));
    tokio::fs::create_dir_all(db_path.parent().unwrap()).await?;
    let db = MusicDb::new(&format!("sqlite://{}?mode=rwc", db_path.display())).await?;

    if !args.skip_dedup {
        enforce_barriers(&providers, &db).await?;
    }

    if args.backfill_links {
        backfill_links(&db, &providers).await?;
        info!(
            "YouTube Data API quota used: {} unit(s)",
            youtube_quota.load(Ordering::Relaxed),
        );
        return Ok(());
    }

    let (pool, root_id) = match args.fetch_options.as_deref() {
        Some(s) => {
            let path = resolve_fetch_options(s);
            let (pool, root_id, _hash) = load_from_file(&path).await?;
            info!("Fetch options: {}", path.display());
            (pool, root_id)
        }
        None => {
            let mut pool = EntryFetchOptionsPool::default();
            let root_id = pool.insert(EntryFetchOptions {
                child_rules: vec![ChildRule {
                    matcher: ChildMatcherExpr::Matcher(ChildMatcher::Always),
                    options_id: Some(EntryFetchOptionsPool::DEFAULT_ID),
                }],
            });
            info!("Fetch options: default (1 level deep)");
            (Arc::new(pool), root_id)
        }
    };

    let mut soft_cfg = (!args.skip_softmatch)
        .then(default_soft_match_config)
        .flatten();
    if let Some(cfg) = soft_cfg.as_mut() {
        cfg.http_client = Some(Arc::clone(&http_for_script));
    }
    let mut urls = args.urls;
    if let Some(path) = &args.url_file {
        let text = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        urls.extend(
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(str::to_owned),
        );
    }
    if urls.is_empty() {
        anyhow::bail!("no URLs to import");
    }
    info!("Importing {} URL(s)", urls.len());

    let outcome = ingest_entries(
        &db,
        &providers,
        &pool,
        root_id,
        soft_cfg.as_ref(),
        urls.clone(),
        None,
        args.max_fetches,
    )
    .await?;
    for (url, entry_id) in urls.iter().zip(&outcome.entry_ids) {
        match entry_id {
            Some(id) => info!(entry_id = id, url, "Ingested"),
            None => info!(
                url,
                "No provider recognised the input URL; nothing ingested"
            ),
        }
    }

    // Full refreshes of the oldest uploads snapshots, with whatever budget
    // the import left. Items they find are imported by the next run.
    if args.refresh.is_some() && !args.no_full_refresh_pass {
        listings.full_refresh_pass().await?;
    }
    listings.log_summary().await;

    info!(
        "YouTube Data API quota used: {} unit(s)",
        youtube_quota.load(Ordering::Relaxed),
    );
    match ledger.used_today().await {
        Ok(units) => info!("YouTube Data API quota used today, all runs: {units} unit(s)"),
        Err(e) => warn!("recording YouTube quota use failed: {e}"),
    }
    info!("Done.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Args, RefreshArg};

    /// A bare `--refresh` is `auto` and leaves a following URL alone; only
    /// `--refresh=full` takes a value.
    #[test]
    fn refresh_flag_parsing() {
        let args = Args::try_parse_from(["import", "--refresh", "https://youtu.be/x"]).unwrap();
        assert!(matches!(args.refresh, Some(RefreshArg::Auto)));
        assert_eq!(args.urls, ["https://youtu.be/x"]);

        let args = Args::try_parse_from(["import", "--refresh=full", "u"]).unwrap();
        assert!(matches!(args.refresh, Some(RefreshArg::Full)));

        let args = Args::try_parse_from(["import", "u"]).unwrap();
        assert!(args.refresh.is_none());
        assert!(Args::try_parse_from(["import", "--refresh=bogus", "u"]).is_err());
    }
}
