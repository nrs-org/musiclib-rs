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
//! the server's own replication sequence.
//!
//! Usage:
//!   cargo run --bin mb_sync_replication
//!   cargo run --bin mb_sync_replication -- -d mb_mirror.db
//!   cargo run --bin mb_sync_replication -- --max-server-lag 36   # daily update

use std::{
    io::BufReader,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use bzip2::read::MultiBzDecoder;
use clap::Parser;
use musiclib_rs::{
    mb_freshness::{self, PACKETS_BASE_URL},
    providers::registry::RegistryConfig,
    replication::{self, PacketInfo},
};
use rusqlite::Connection;
use tracing::{info, warn};

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
    /// likely stopped. For the daily update.
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
    let mb = registry.musicbrainz.as_ref();

    // The server's sequence, `None` for the public server (or a disabled
    // backend). An explicit `--until` needs it only for the lag check.
    let server_seq = if cli.until.is_some() && cli.max_server_lag.is_none() {
        None
    } else {
        mb.map(mb_freshness::mirror_cap).transpose()?.flatten()
    };
    if let (Some(max_lag), Some(server)) = (cli.max_server_lag, server_seq) {
        let latest = mb_freshness::latest_published(&cli.base_url, &token)?;
        let lag = latest - server;
        if lag > max_lag {
            bail!(
                "the MusicBrainz server is at packet {server}, {lag} behind the newest \
                 ({latest}); is its replication running?"
            );
        }
        info!("server at {server}, {lag} packet(s) behind the newest ({latest})");
    }
    let cap = cli.until.or(server_seq);

    let Some(db) = cli.db.or_else(|| match mb {
        Some(cfg) => cfg.mirror_db_path(),
        None => {
            let default = musiclib_rs::app_dirs::data_dir().join("mb_mirror.db");
            default.exists().then_some(default)
        }
    }) else {
        info!("no URL mirror configured; nothing to sync");
        return Ok(());
    };
    let mut conn = Connection::open(&db).with_context(|| format!("opening {}", db.display()))?;

    let (mut current_seq, db_schema) = read_db_state(&conn)?;
    info!("db: replication_sequence={current_seq}, schema_sequence={db_schema}");
    if let Some(cap) = cap
        && current_seq > cap
    {
        bail!(
            "the mirror is at packet {current_seq}, ahead of the server's {cap}; \
             undo the extra packets with `mb_apply_replication --undo`"
        );
    }

    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!(
            "musiclib-rs/",
            env!("CARGO_PKG_VERSION"),
            " (replication-sync)"
        ))
        .build()
        .context("building HTTP client")?;

    let mut packets_applied = 0u64;
    let sync_start = Instant::now();

    loop {
        let next_seq = current_seq + 1;
        if cap.is_some_and(|cap| next_seq > cap) {
            info!("mirror at {current_seq}, the server's sequence; stopping there");
            break;
        }
        let url = format!(
            "{}/replication-{}-v2.tar.bz2",
            cli.base_url.trim_end_matches('/'),
            next_seq
        );
        let authed_url = format!("{url}?token={}", urlencoding::encode(&token));

        let pkt_start = Instant::now();
        let Some(pkt) = fetch_with_retry(&client, &url, &authed_url, next_seq)? else {
            info!("[{next_seq}] up to date at {current_seq}");
            break;
        };

        if pkt.replication_sequence != next_seq {
            bail!(
                "sequence mismatch: expected packet {next_seq}, got {}",
                pkt.replication_sequence
            );
        }
        if pkt.schema_sequence != db_schema {
            bail!(
                "schema mismatch at packet {}: db has schema_sequence={db_schema}, \
                 packet has {} — re-bootstrap from a fresh full dump",
                pkt.replication_sequence,
                pkt.schema_sequence
            );
        }

        let (n_i, n_u, n_d) = replication::count_ops(&pkt.ops);
        let ts = pkt.timestamp.as_deref().unwrap_or("?");

        eprint!("\r[{next_seq}] applying ({ts})...");

        replication::exec_ops(
            &mut conn,
            &pkt.ops,
            false,
            pkt.replication_sequence,
            pkt.schema_sequence,
        )
        .with_context(|| format!("applying packet {}", pkt.replication_sequence))?;

        let elapsed = pkt_start.elapsed();
        info!(
            "[{next_seq}] {ts} | {n_i}i {n_u}u {n_d}d | {:.1}s",
            elapsed.as_secs_f64()
        );

        current_seq = pkt.replication_sequence;
        packets_applied += 1;
    }

    let total = sync_start.elapsed();
    info!(
        "{packets_applied} packet(s) applied in {:.1}s, mirror at {current_seq}",
        total.as_secs_f64()
    );
    Ok(())
}

fn read_db_state(conn: &Connection) -> Result<(i64, i64)> {
    conn.query_row(
        "SELECT replication_sequence, schema_sequence FROM mb_state",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .context("reading mb_state — has the database been bootstrapped with mb_extract_urls?")
}

/// Attempts per packet: a dropped download shouldn't fail the daily update.
const FETCH_ATTEMPTS: u32 = 3;

/// `fetch_packet`, retried with backoff. Bad credentials aren't retried.
fn fetch_with_retry(
    client: &reqwest::blocking::Client,
    url: &str,
    authed_url: &str,
    seq: i64,
) -> Result<Option<PacketInfo>> {
    let mut attempt = 1;
    loop {
        match fetch_packet(client, url, authed_url, seq) {
            Err(FetchError::Auth(status)) => {
                bail!("auth failed (HTTP {status}); check METABRAINZ_ACCESS_TOKEN")
            }
            Err(FetchError::Other(e)) if attempt < FETCH_ATTEMPTS => {
                let wait = Duration::from_secs(5 * 3u64.pow(attempt - 1));
                warn!("[{seq}] {e:#}; retrying in {}s", wait.as_secs());
                std::thread::sleep(wait);
                attempt += 1;
            }
            Err(FetchError::Other(e)) => return Err(e),
            Ok(pkt) => return Ok(pkt),
        }
    }
}

enum FetchError {
    Auth(reqwest::StatusCode),
    Other(anyhow::Error),
}

/// Download and parse packet `seq`, streaming the body into the parser.
/// `None` when it isn't published yet.
fn fetch_packet(
    client: &reqwest::blocking::Client,
    url: &str,
    authed_url: &str,
    seq: i64,
) -> Result<Option<PacketInfo>, FetchError> {
    eprint!("[{seq}] fetching...");
    let resp = client
        .get(authed_url)
        .send()
        .with_context(|| format!("requesting {url}"))
        .map_err(FetchError::Other)?;
    match resp.status().as_u16() {
        200 => {}
        404 => return Ok(None),
        401 | 403 => return Err(FetchError::Auth(resp.status())),
        s => {
            return Err(FetchError::Other(anyhow::anyhow!(
                "HTTP {s} fetching packet {seq}"
            )));
        }
    }
    eprint!("\r[{seq}] parsing... ");
    let decompressed = MultiBzDecoder::new(BufReader::with_capacity(1 << 20, resp));
    replication::stream_packet(decompressed)
        .with_context(|| format!("parsing packet {seq}"))
        .map(Some)
        .map_err(FetchError::Other)
}
