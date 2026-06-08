use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use clap::Parser;
use musiclib_rs::{
    http::HttpClientConfig,
    musicdb::MusicDb,
    pipeline::{
        dedup::{DedupConfig, dedup_db, merge_configs, reconcile_tags},
        flush::flush,
        importer::import,
        progress,
        state::State,
    },
    providers::{
        fetch_options_yaml::load_from_file,
        registry::{RegistryConfig, build_providers},
        std_values::StandardProviderKeys,
        types::{
            ChildMatcher, ChildMatcherExpr, ChildRule, EntryFetchOptions, EntryFetchOptionsPool,
        },
    },
};
use tracing::info;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    url: String,
    #[arg(long)]
    fetch_options: Option<String>,
    #[arg(long, default_value = "musiclib.db")]
    db: String,
    #[arg(long)]
    registry_config: Option<String>,
    #[arg(long)]
    http_config: Option<String>,
    /// Barrier config file(s). Repeatable; each file is tracked independently.
    #[arg(long)]
    dedup_config: Vec<String>,
    /// Skip the pre-import dedup pass (which re-applies the barrier to the
    /// existing DB before importing the new URL).
    #[arg(long)]
    skip_dedup: bool,
}

async fn load_config<T: Default + serde::de::DeserializeOwned>(
    path: Option<&str>,
) -> anyhow::Result<T> {
    match path {
        Some(p) => Ok(serde_yaml_ng::from_str(
            &tokio::fs::read_to_string(p).await?,
        )?),
        None => Ok(T::default()),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();

    let args = Args::parse();

    let registry_config: RegistryConfig = load_config(args.registry_config.as_deref()).await?;
    let mut dedup_configs: Vec<(String, DedupConfig)> = Vec::new();
    for path in &args.dedup_config {
        let cfg: DedupConfig = load_config(Some(path)).await?;
        dedup_configs.push((path.clone(), cfg));
    }
    let mut http_config: HttpClientConfig = load_config(args.http_config.as_deref()).await?;
    http_config.coalescer_rules = musiclib_rs::providers::registry::coalesce_rules();

    let youtube_quota = Arc::new(AtomicU64::new(0));
    http_config.youtube_quota_counter = Some(Arc::clone(&youtube_quota));

    let http = http_config.build().await?;
    let http = progress::ProgressHttpClient::new(http);

    tracing_subscriber::fmt()
        .with_writer(progress::MultiProgressMakeWriter::new(Arc::clone(
            &http.multi,
        )))
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let providers = build_providers(&registry_config, http)?;
    if providers.is_empty() {
        anyhow::bail!("No providers available -- check your credential env vars");
    }
    info!("Loaded {} provider(s)", providers.len());
    let providers = Arc::new(providers);

    let db = MusicDb::new(&format!("sqlite://{}?mode=rwc", args.db)).await?;

    // Re-apply the dedup barrier to the existing DB before importing, so the new
    // content merges into an already-clean library. Skipped when no barriers are
    // declared (nothing to do) or via --skip-dedup.
    if !args.skip_dedup && !dedup_configs.is_empty() {
        dedup_db(Arc::clone(&providers), &db, &dedup_configs).await?;
    }

    let (pool, root_id) = match args.fetch_options.as_deref() {
        Some(path) => {
            let (pool, root_id, _hash) = load_from_file(Path::new(path)).await?;
            info!("Fetch options: {path}");
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

    let state = Arc::new(State::new());

    import(
        Arc::clone(&state),
        Arc::clone(&providers),
        Arc::clone(&pool),
        (StandardProviderKeys::UNKNOWN_URL.to_string(), args.url),
        root_id,
    )
    .await;

    let merged = merge_configs(&dedup_configs);
    flush(state, providers.as_slice(), &db, &merged).await?;
    // The ingest may have grown/merged entries containing anchors; refresh the
    // tags so the next dedup run sees the current grouping.
    if !dedup_configs.is_empty() {
        reconcile_tags(providers.as_slice(), &db, &dedup_configs).await?;
    }
    info!(
        "YouTube Data API quota used: {} unit(s)",
        youtube_quota.load(Ordering::Relaxed),
    );
    info!("Done.");
    Ok(())
}
