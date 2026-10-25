//! Continuously fetch and apply MusicBrainz replication packets until the
//! mirror is caught up with the latest published packet.
//!
//! Reads the current `replication_sequence` from the SQLite mirror, then
//! fetches and applies packets starting from `sequence + 1`, streaming each
//! HTTP response directly into the apply pipeline (no temp files). Stops when
//! the next packet returns HTTP 404 (nothing left to apply).
//!
//! Reads `METABRAINZ_ACCESS_TOKEN` from the environment (or `.env`). Get one
//! at https://metabrainz.org/profile/applications.
//!
//! The mirror never gets ahead of the MusicBrainz server the backend queries
//! (`musiclib_rs::mb_freshness`): with a self-hosted server, syncing stops at
//! the server's own replication sequence. The loop is `musiclib_rs::mb_sync`,
//! which the library update runs too.
//!
//! Usage:
//!   cargo run --bin mb_sync_replication
//!   cargo run --bin mb_sync_replication -- -d mb_mirror.db
//!   cargo run --bin mb_sync_replication -- --max-server-lag 36

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use musiclib_rs::{
    mb_freshness::PACKETS_BASE_URL,
    mb_sync::{self, SyncOptions},
    providers::registry::RegistryConfig,
};

#[derive(Parser, Debug)]
#[command(
    about = "Fetch and apply all pending MusicBrainz replication packets to a local SQLite mirror"
)]
struct Cli {
    /// SQLite database file (must already be bootstrapped via mb_extract_urls).
    /// Defaults to the mirror the MusicBrainz backend uses (`mirror_db`, else
    /// <data_dir>/mb_mirror.db).
    #[arg(short, long)]
    db: Option<PathBuf>,

    /// Base URL for the replication-packets endpoint.
    #[arg(long, default_value = PACKETS_BASE_URL)]
    base_url: String,

    /// providers.yaml with the MusicBrainz server settings. Defaults to
    /// <config_dir>/providers.yaml.
    #[arg(long)]
    registry_config: Option<PathBuf>,

    /// Apply packets up to this sequence instead of stopping at a self-hosted
    /// server's own.
    #[arg(long)]
    until: Option<i64>,

    /// Fail if a self-hosted server is more than this many packets (about one
    /// an hour) behind the newest published one: its own replication has
    /// likely stopped.
    #[arg(long)]
    max_server_lag: Option<i64>,
    #[command(flatten)]
    dirs: musiclib_rs::app_dirs::DirArgs,
}

fn main() -> Result<()> {
    dotenv::dotenv().ok();
    let cli = Cli::parse();
    cli.dirs.apply();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let token = std::env::var("METABRAINZ_ACCESS_TOKEN").context(
        "METABRAINZ_ACCESS_TOKEN is not set — add it to .env \
         (get one at https://metabrainz.org/profile/applications)",
    )?;
    let registry = RegistryConfig::load(cli.registry_config.as_deref())?;
    mb_sync::sync(
        registry.musicbrainz.as_ref(),
        &token,
        &SyncOptions {
            db: cli.db,
            base_url: cli.base_url,
            until: cli.until,
            max_server_lag: cli.max_server_lag,
            on_packet: None,
        },
    )?;
    Ok(())
}
