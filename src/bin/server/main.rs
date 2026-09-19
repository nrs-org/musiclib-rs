//! Local HTTP server for musiclib-rs. Two independent route groups, each
//! behind its own Cargo feature (both on by default):
//!
//! - `store_protocol`: server side of the NRS Store Protocol
//!   (`nrs-org/nrs`'s `store-protocol/specification.md`) for the `music`
//!   entry type — this is what an `nrs-app` talks to.
//! - `player`: musiclib-rs's own read-only library/playback API plus a
//!   minimal static frontend — docs/plan-video-player.md Phase 1/2.
//!
//! Both share one `MusicDb` connection so exactly one process owns writes to
//! `musiclib.db`; hand-rolled hyper (no framework), matching the existing
//! `src/bin/ytdlp_server.rs`.

mod entries;
mod fetch_options;
mod ingest_progress;
mod jobs;
#[cfg(feature = "player")]
mod player;
mod respond;
mod state;
#[cfg(feature = "store_protocol")]
mod store_protocol;

use std::{
    ffi::OsStr,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicU64},
};

use anyhow::Context as _;
use clap::Parser;
use http_body_util::BodyExt;
use hyper::{Request, body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use musiclib_rs::{
    app_dirs,
    http::HttpClientConfig,
    musicdb::MusicDb,
    pipeline::{
        dedup::{DedupConfig, merge_configs},
        progress::HttpActivityClient,
        softmatch::default_soft_match_config,
    },
    providers::{
        fetch_options_yaml::load_from_file,
        registry::{RegistryConfig, build_providers},
    },
};
use tracing::info;

use jobs::JobManager;
use respond::HyperResponse;
use state::AppState;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

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
    /// Fetch-options YAML used by the store-protocol `ingest` job. A bare
    /// filename resolves relative to <config_dir>/fetch_options/. Defaults to
    /// a single level deep, matching the `import` CLI's fallback.
    #[arg(long)]
    fetch_options: Option<String>,
    /// Listen address.
    #[arg(long, default_value = "127.0.0.1:4600")]
    listen: String,
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

fn resolve_fetch_options(s: &str) -> PathBuf {
    let p = Path::new(s);
    if p.is_absolute() || p.components().count() > 1 {
        p.to_owned()
    } else {
        app_dirs::config_dir().join("fetch_options").join(p)
    }
}

async fn handle(state: Arc<AppState>, req: Request<Incoming>) -> Result<HyperResponse, BoxError> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let body = req.into_body().collect().await?.to_bytes();

    #[cfg(feature = "store_protocol")]
    if store_protocol::handles(&path) {
        return Ok(store_protocol::route(&state, &method, &path, body).await);
    }
    #[cfg(feature = "player")]
    if player::handles(&path) {
        return Ok(player::route(&state, &method, &path, &query, body).await);
    }
    let _ = (&method, &path, &query, &body);
    Ok(respond::not_found())
}

/// Single-threaded runtime under a `LocalSet`: connection handling and job
/// tasks use `spawn_local`, not `tokio::spawn`, because the dedup scoring
/// pipeline's `rhai::Engine` isn't `Sync` (see `jobs::JobManager::spawn`).
/// Fine for a local, single-user server — concurrency here is about not
/// blocking on network I/O, not using multiple cores.
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();
    // `EnvFilter::from_default_env()` shows nothing at all when `RUST_LOG`
    // isn't set — fine for the `import`/`dedup` CLIs, whose scripts always
    // set it explicitly, but this is a long-running background process
    // nobody launches with a `RUST_LOG=` prefix every time. Without a
    // fallback, import failures (`warn!` in `pipeline::importer`) and even
    // the startup/listening lines below were silently going nowhere — the
    // only reason the `@ShirakamiFubuki` panic surfaced at all was that a
    // Rust panic bypasses `tracing` and prints unconditionally. `RUST_LOG`
    // still overrides this when set.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,server=info")),
        )
        .init();

    let args = Args::parse();
    let config_dir = app_dirs::config_dir();

    let registry_config: RegistryConfig = load_config(
        args.registry_config.as_deref(),
        &config_dir.join("providers.yaml"),
    )
    .await?;

    let dedup_paths: Vec<PathBuf> = if args.dedup_config.is_empty() {
        yaml_paths_in_dir(&config_dir.join("dedup_barriers"))?
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
    let merged_dedup = merge_configs(&dedup_configs);

    let mut http_config: HttpClientConfig =
        load_config(args.http_config.as_deref(), &config_dir.join("http.yaml")).await?;
    http_config.coalescer_rules = musiclib_rs::providers::registry::coalesce_rules();
    let youtube_quota = Arc::new(AtomicU64::new(0));
    http_config.youtube_quota_counter = Some(Arc::clone(&youtube_quota));
    let http = http_config.build().await?;
    let activity = HttpActivityClient::new(http);

    let providers = build_providers(&registry_config, activity.clone())?;
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

    let (pool, root_id) = match args.fetch_options.as_deref() {
        Some(s) => {
            let path = resolve_fetch_options(s);
            let (pool, root_id, _hash) = load_from_file(&path).await?;
            info!("Ingest fetch options: {}", path.display());
            (pool, root_id)
        }
        None => {
            info!("Ingest fetch options: default (1 level deep)");
            fetch_options::shallow()
        }
    };
    info!(
        "Import UI fetch-options presets: read fresh from {} on every request",
        config_dir.join("fetch_options").display()
    );

    let soft_cfg = default_soft_match_config();
    if soft_cfg.is_none() {
        info!("No <config_dir>/match.rhai; online soft-dedup disabled for ingest jobs");
    }

    let state = Arc::new(AppState {
        db,
        providers,
        pool,
        root_id,
        dedup_configs,
        merged_dedup,
        soft_cfg,
        jobs: JobManager::new(),
        activity,
        youtube_quota,
    });

    let addr: SocketAddr = args.listen.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("server listening on {addr}");
    #[cfg(feature = "store_protocol")]
    info!("store protocol mounted at /nrs-store/v1");
    #[cfg(feature = "player")]
    info!("player API mounted at /api (frontend at /)");

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            loop {
                let (stream, _) = listener.accept().await?;
                let state = Arc::clone(&state);
                tokio::task::spawn_local(async move {
                    let io = TokioIo::new(stream);
                    if let Err(e) = http1::Builder::new()
                        .serve_connection(
                            io,
                            service_fn(move |req| handle(Arc::clone(&state), req)),
                        )
                        .await
                    {
                        tracing::error!("connection error: {e}");
                    }
                });
            }
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        })
        .await
}
