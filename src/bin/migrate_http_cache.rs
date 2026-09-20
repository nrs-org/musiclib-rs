//! Recompress the persistent HTTP cache and reclaim SQLite free pages.
//!
//! Stop any process using the cache before running this command. It rewrites
//! existing response bodies in place and then runs VACUUM.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use musiclib_rs::httpcache::DbHttpCache;

#[derive(Debug, Parser)]
#[command(about = "Recompress and compact the persistent HTTP cache")]
struct Cli {
    /// SQLite HTTP cache database.
    /// Defaults to <cache_dir>/http_cache.db.
    #[arg(short, long)]
    db: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    let cli = Cli::parse();
    let db = cli
        .db
        .unwrap_or_else(|| musiclib_rs::app_dirs::cache_dir().join("http_cache.db"));

    if !db.exists() {
        anyhow::bail!("HTTP cache database does not exist: {}", db.display());
    }

    let db_url = format!("sqlite://{}", db.display());
    let cache = DbHttpCache::new(db_url)
        .await
        .with_context(|| format!("opening HTTP cache {}", db.display()))?;
    let stats = cache
        .recompress_existing()
        .await
        .context("recompressing HTTP cache entries")?;
    cache.vacuum().await.context("vacuuming HTTP cache")?;

    println!(
        "scanned {}; compressed {}; unchanged {}; saved {} bytes",
        stats.scanned, stats.compressed, stats.unchanged, stats.saved_bytes
    );
    Ok(())
}
