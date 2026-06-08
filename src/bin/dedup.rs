use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use clap::Parser;
use musiclib_rs::{
    http::HttpClientConfig,
    musicdb::MusicDb,
    pipeline::{
        dedup::{DedupConfig, dedup_db},
        progress,
    },
    providers::registry::{RegistryConfig, build_providers},
};
use tracing::info;

/// Re-apply the dedup barrier to an existing music DB by re-importing the
/// entities already in it (shallow) — no new URLs ingested.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    #[arg(long, default_value = "musiclib.db")]
    db: String,
    #[arg(long)]
    registry_config: Option<String>,
    #[arg(long)]
    http_config: Option<String>,
    /// Barrier config file(s). Repeatable; each file is tracked independently.
    #[arg(long)]
    dedup_config: Vec<String>,
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

    dedup_db(providers, &db, &dedup_configs).await?;

    info!(
        "YouTube Data API quota used: {} unit(s)",
        youtube_quota.load(Ordering::Relaxed),
    );
    info!("Done.");
    Ok(())
}
