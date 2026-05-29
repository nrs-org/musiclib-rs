mod flush;
mod importer;
mod state;

use std::{path::Path, sync::Arc};

use clap::Parser;
use musiclib_rs::{
    http::HttpClientConfig,
    musicdb::MusicDb,
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

use flush::flush;
use importer::import;
use state::State;

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
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    let registry_config: RegistryConfig = load_config(args.registry_config.as_deref()).await?;
    let http_config: HttpClientConfig = load_config(args.http_config.as_deref()).await?;

    let http = http_config.build().await?;
    let providers = build_providers(&registry_config, http)?;
    if providers.is_empty() {
        anyhow::bail!("No providers available -- check your credential env vars");
    }
    info!("Loaded {} provider(s)", providers.len());
    let providers = Arc::new(providers);

    let db = MusicDb::new(&format!("sqlite://{}?mode=rwc", args.db)).await?;

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

    flush(state, &db).await?;
    info!("Done.");
    Ok(())
}
