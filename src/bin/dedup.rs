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
        dedup::{enforce_barriers, import_yaml_barriers},
        progress,
        softmatch::backfill_block_key_index,
    },
    providers::registry::{RegistryConfig, build_providers},
};
use tracing::info;

/// Re-apply the stored dedup barriers an existing music DB violates — no new
/// URLs ingested — after importing any YAML barrier files given.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Music library database. Defaults to <data_dir>/musiclib.db.
    #[arg(long)]
    db: Option<String>,
    /// Provider credentials config. Defaults to <config_dir>/providers.yaml.
    #[arg(long)]
    registry_config: Option<String>,
    /// HTTP client config. Defaults to <config_dir>/http.yaml.
    #[arg(long)]
    http_config: Option<String>,
    /// Store the barriers of these YAML files (or every `*.yaml` in these
    /// directories) in the DB, in the old `dedup_barriers/*.yaml` format.
    /// Importing a file twice adds nothing.
    #[arg(long, num_args = 1..)]
    import_yaml: Vec<PathBuf>,
    /// Print the stored barriers and exit.
    #[arg(long)]
    list: bool,
    #[command(flatten)]
    dirs: musiclib_rs::app_dirs::DirArgs,
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

fn yaml_paths_in_dir(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
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

    let mut yaml_paths: Vec<PathBuf> = Vec::new();
    for path in &args.import_yaml {
        if path.is_dir() {
            yaml_paths.extend(yaml_paths_in_dir(path)?);
        } else {
            yaml_paths.push(path.clone());
        }
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

    if args.list {
        for g in db.barrier_groups().await? {
            println!("#{} {} ({})", g.id, g.name, g.origin);
            for (anchor, pairs) in &g.anchors {
                println!("  {anchor}:");
                for (source, identifier) in pairs {
                    println!("    {source}:{identifier}");
                }
            }
        }
        return Ok(());
    }
    if !yaml_paths.is_empty() {
        let paths: Vec<&Path> = yaml_paths.iter().map(PathBuf::as_path).collect();
        let added = import_yaml_barriers(&providers, &db, &paths).await?;
        info!(
            "Imported {added} barrier group(s) from {} file(s)",
            paths.len()
        );
    }

    enforce_barriers(&providers, &db).await?;

    let backfilled = backfill_block_key_index(&db).await?;
    if backfilled > 0 {
        info!("Block-key index backfill indexed {backfilled} entr(y/ies)");
    }

    info!(
        "YouTube Data API quota used: {} unit(s)",
        youtube_quota.load(Ordering::Relaxed),
    );
    info!("Done.");
    Ok(())
}
