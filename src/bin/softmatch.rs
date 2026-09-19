use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context as _;
use clap::Parser;
use musiclib_rs::{
    app_dirs,
    http::HttpClientConfig,
    musicdb::MusicDb,
    pipeline::{
        dedup::{DedupConfig, merge_configs},
        progress,
        softmatch::{SoftMatchConfig, match_db},
    },
    providers::registry::{RegistryConfig, build_providers},
};

/// Heuristic soft-dedup: score candidate entry pairs with a Rhai script (or,
/// per --jev-types, TypeSafe's Jev model) and report or apply the results.
/// Dry-run by default; pass --apply to persist RELATE and soft MERGE
/// (same_identity) decisions to the database. No verdict from any backend
/// ever destructively collapses two entries — that stays an import-time
/// operation.
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
    /// Barrier config file(s). Repeatable; overrides auto-loading from
    /// <config_dir>/dedup_barriers/*.yaml.
    #[arg(long)]
    dedup_config: Vec<String>,
    /// Rhai match script. Defaults to <config_dir>/match.rhai.
    #[arg(long)]
    script: Option<String>,
    /// Versioned learned scorer JSON. When present, replaces Rhai verdict
    /// scoring while retaining the script's local embedding hook.
    #[arg(long)]
    model: Option<String>,
    /// Persist learned DEFER candidates in the database for review.
    #[arg(long)]
    persist_suggestions: bool,
    /// Persist RELATE and soft MERGE (same_identity) decisions to the database.
    #[arg(long)]
    apply: bool,
    /// Entry types scored with TypeSafe's Jev model instead of the Rhai
    /// script/learned model, e.g. "track" or "track,release". Requires
    /// TYPESAFE_API_KEY. Empty by default (every type uses the existing
    /// heuristic).
    #[arg(long, value_delimiter = ',')]
    jev_types: Vec<String>,
    /// Write all candidate pairs (including DISTINCT and BARRIER) to a CSV
    /// file for manual quality review.
    #[arg(long)]
    csv: Option<String>,
    /// SQLite file for the embedding cache. Defaults to <data_dir>/embeddings.db.
    /// Requires the Rhai script to define embed(text) -> array.
    #[arg(long)]
    embed_db: Option<String>,
    /// Stable identity of the model behind the local Rhai embed() hook. Must
    /// match the learned scorer artifact's embedding contract.
    #[arg(long)]
    embedding_model_id: Option<String>,
    /// Disable semantic (embedding-based) blocking even if embed() is defined.
    #[arg(long)]
    no_embed: bool,
    /// Embedding vector dimension. Must match the model used in embed().
    /// [default: 256; use 384 with `inference --features minilm`]
    #[arg(long, default_value_t = 256)]
    embed_dim: usize,
    /// Number of KNN neighbours per entry for semantic blocking. [default: 20]
    #[arg(long, default_value_t = 20)]
    embed_k: usize,
    /// Minimum cosine similarity to treat a KNN pair as a blocking candidate. [default: 0.45]
    #[arg(long, default_value_t = 0.45)]
    embed_threshold: f64,
    /// Max KNN pages per type. Values above one are recall experiments. [default: 1]
    #[arg(long, default_value_t = 1)]
    embed_max_pages: usize,
    /// Largest lexical/structural posting list expanded into pairs. [default: 50]
    #[arg(long, default_value_t = 50)]
    candidate_max_block: usize,
    /// Character-trigram neighbors retained per entry. [default: 30]
    #[arg(long, default_value_t = 30)]
    candidate_ngram_k: usize,
    /// Print every suggested merge/relation and its sources to stdout.
    #[arg(long)]
    verbose_decisions: bool,
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

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

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
    let merged_dedup = merge_configs(&dedup_configs);

    let script_path = args
        .script
        .map(PathBuf::from)
        .unwrap_or_else(|| config_dir.join("match.rhai"));
    if !script_path.exists() {
        anyhow::bail!(
            "Match script not found: {}  (copy config/match.example.rhai there to get started)",
            script_path.display()
        );
    }
    let model_path = args.model.map(PathBuf::from).or_else(|| {
        let default = config_dir.join("dedup-model.json");
        default.exists().then_some(default)
    });

    let mut http_config: HttpClientConfig =
        load_config(args.http_config.as_deref(), &config_dir.join("http.yaml")).await?;
    http_config.coalescer_rules = musiclib_rs::providers::registry::coalesce_rules();

    let http = http_config.build().await?;
    // Cloned before `http` is wrapped/consumed below — this raw client (no
    // progress-bar wrapping) is what the script-facing `http_call(...)`
    // primitive uses, so its console output isn't misattributed as a
    // provider fetch.
    let http_for_script = Arc::clone(&http);
    let http = progress::ProgressHttpClient::new(http);

    let providers = build_providers(&registry_config, http)?;
    let providers = Arc::new(providers);

    let db_path = args
        .db
        .map(PathBuf::from)
        .unwrap_or_else(|| app_dirs::data_dir().join("musiclib.db"));
    let db = MusicDb::new(&format!("sqlite://{}?mode=rwc", db_path.display())).await?;

    let embed_db_path: Option<String> = if args.no_embed {
        None
    } else {
        Some(args.embed_db.unwrap_or_else(|| {
            app_dirs::data_dir()
                .join("embeddings.db")
                .display()
                .to_string()
        }))
    };

    let jev_entry_types: std::collections::HashSet<String> = args.jev_types.into_iter().collect();
    if !jev_entry_types.is_empty() && std::env::var("TYPESAFE_API_KEY").is_err() {
        anyhow::bail!(
            "--jev-types {:?} was passed but TYPESAFE_API_KEY is not set",
            jev_entry_types
        );
    }

    let soft_cfg = SoftMatchConfig {
        script_path: script_path.display().to_string(),
        model_path: model_path.map(|path| path.display().to_string()),
        persist_suggestions: args.persist_suggestions,
        apply_relates: args.apply,
        jev_entry_types,
        csv_path: args.csv,
        embed_db_path,
        embed_model_id: args.embedding_model_id,
        embed_dim: args.embed_dim,
        embed_k: args.embed_k,
        embed_sim_threshold: args.embed_threshold,
        embed_max_pages: args.embed_max_pages,
        candidate_max_block: args.candidate_max_block,
        candidate_ngram_k: args.candidate_ngram_k,
        verbose_decisions: args.verbose_decisions,
        http_client: Some(http_for_script),
    };

    match_db(&db, &merged_dedup, providers.as_slice(), &soft_cfg).await?;

    Ok(())
}
