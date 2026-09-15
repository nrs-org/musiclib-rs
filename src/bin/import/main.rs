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
        dedup::{DedupConfig, dedup_db, merge_configs, reconcile_tags},
        flush::flush,
        importer::import,
        progress,
        softmatch::{SoftMatchConfig, match_new_entries},
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
    /// Barrier config file(s). Repeatable; overrides auto-loading from
    /// <config_dir>/dedup_barriers/*.yaml.
    #[arg(long)]
    dedup_config: Vec<String>,
    /// Skip the pre-import dedup pass.
    #[arg(long)]
    skip_dedup: bool,
    /// Skip the post-import online soft-match pass.
    #[arg(long)]
    skip_softmatch: bool,
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

fn resolve_fetch_options(s: &str) -> PathBuf {
    let p = Path::new(s);
    if p.is_absolute() || p.components().count() > 1 {
        p.to_owned()
    } else {
        app_dirs::config_dir().join("fetch_options").join(p)
    }
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

    if !args.skip_dedup && !dedup_configs.is_empty() {
        dedup_db(Arc::clone(&providers), &db, &dedup_configs).await?;
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
    let touched_ids = flush(state, providers.as_slice(), &db, &merged).await?;
    if !dedup_configs.is_empty() {
        reconcile_tags(providers.as_slice(), &db, &dedup_configs).await?;
    }

    // RELATE writes to `entry_relation` (reversible: tombstoned via `enabled`,
    // never repoints `entry_source`). MERGE stays suggestion-only via
    // `apply_merges: false` — `merge_entries` has no undo path, and its
    // high-precision release gate isn't validated yet (docs/dedup-v2.md).
    let script_path = config_dir.join("match.rhai");
    if !args.skip_softmatch && script_path.exists() {
        let embed_db = app_dirs::data_dir().join("embeddings.db");
        let model_path = config_dir.join("dedup-model.json");
        let persist_suggestions = model_path.exists();
        let soft_cfg = SoftMatchConfig {
            script_path: script_path.display().to_string(),
            model_path: model_path
                .exists()
                .then(|| model_path.display().to_string()),
            persist_suggestions,
            apply_relates: true,
            apply_merges: false,
            csv_path: None,
            embed_db_path: Some(embed_db.display().to_string()),
            embed_model_id: None,
            embed_dim: 256,
            embed_k: 20,
            embed_sim_threshold: 0.45,
            embed_max_pages: 1,
            candidate_max_block: 50,
            candidate_ngram_k: 30,
            verbose_decisions: false,
        };
        match_new_entries(&db, &touched_ids, &merged, providers.as_slice(), &soft_cfg).await?;
    }
    info!(
        "YouTube Data API quota used: {} unit(s)",
        youtube_quota.load(Ordering::Relaxed),
    );
    info!("Done.");
    Ok(())
}
