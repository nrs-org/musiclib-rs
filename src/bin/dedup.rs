use std::{
    ffi::OsStr,
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
    /// Music library database. Defaults to <state_dir>/musiclib.db.
    #[arg(long)]
    db: Option<String>,
    /// Provider credentials config. Defaults to <config_dir>/providers.yaml.
    #[arg(long)]
    registry_config: Option<String>,
    /// HTTP client config. Defaults to <config_dir>/http.yaml.
    #[arg(long)]
    http_config: Option<String>,
    /// Barrier config file(s). Repeatable; overrides auto-loading from
    /// <config_dir>/dedup_barriers/*.yaml.
    #[arg(long)]
    dedup_config: Vec<String>,
}

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

fn dedup_paths_from_dir(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == OsStr::new("yaml")))
        .collect();
    paths.sort();
    Ok(paths)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();

    let args = Args::parse();
    let config_dir = app_dirs::config_dir();

    let registry_config: RegistryConfig = load_config(
        args.registry_config.as_deref(),
        &config_dir.join("providers.yaml"),
    )
    .await?;

    let dedup_paths: Vec<PathBuf> = if args.dedup_config.is_empty() {
        dedup_paths_from_dir(&config_dir.join("dedup_barriers"))?
    } else {
        args.dedup_config.iter().map(PathBuf::from).collect()
    };
    let mut dedup_configs: Vec<(String, DedupConfig)> = Vec::new();
    for path in &dedup_paths {
        let text = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: DedupConfig = serde_yaml_ng::from_str(&text)?;
        dedup_configs.push((path.display().to_string(), cfg));
    }

    let mut http_config: HttpClientConfig =
        load_config(args.http_config.as_deref(), &config_dir.join("http.yaml")).await?;
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

    let db_path = args
        .db
        .map(PathBuf::from)
        .unwrap_or_else(|| app_dirs::data_dir().join("musiclib.db"));
    tokio::fs::create_dir_all(db_path.parent().unwrap()).await?;
    let db = MusicDb::new(&format!("sqlite://{}?mode=rwc", db_path.display())).await?;

    dedup_db(providers, &db, &dedup_configs).await?;

    info!(
        "YouTube Data API quota used: {} unit(s)",
        youtube_quota.load(Ordering::Relaxed),
    );
    info!("Done.");
    Ok(())
}
