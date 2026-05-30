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
//! Usage:
//!   cargo run --bin mb_sync_replication
//!   cargo run --bin mb_sync_replication -- -d mb_mirror.db

use std::{io::BufReader, path::PathBuf, time::Instant};

use anyhow::{Context, Result, bail};
use bzip2::read::MultiBzDecoder;
use clap::Parser;
use musiclib_rs::replication;
use rusqlite::Connection;
use tracing::info;

const DEFAULT_BASE_URL: &str = "https://metabrainz.org/api/musicbrainz";

#[derive(Parser, Debug)]
#[command(
    about = "Fetch and apply all pending MusicBrainz replication packets to a local SQLite mirror"
)]
struct Cli {
    /// SQLite database file (must already be bootstrapped via mb_extract_urls).
    #[arg(short, long, default_value = "mb_mirror.db")]
    db: PathBuf,

    /// Base URL for the replication-packets endpoint.
    #[arg(long, default_value = DEFAULT_BASE_URL)]
    base_url: String,
}

fn main() -> Result<()> {
    dotenv::dotenv().ok();
    let cli = Cli::parse();

    let token = std::env::var("METABRAINZ_ACCESS_TOKEN").context(
        "METABRAINZ_ACCESS_TOKEN is not set — add it to .env \
         (get one at https://metabrainz.org/profile/applications)",
    )?;

    let mut conn =
        Connection::open(&cli.db).with_context(|| format!("opening {}", cli.db.display()))?;

    let (mut current_seq, db_schema) = read_db_state(&conn)?;
    info!("db: replication_sequence={current_seq}, schema_sequence={db_schema}");

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
        let url = format!(
            "{}/replication-{}-v2.tar.bz2",
            cli.base_url.trim_end_matches('/'),
            next_seq
        );
        let authed_url = format!("{url}?token={}", urlencoding::encode(&token));

        eprint!("[{next_seq}] fetching...");

        let pkt_start = Instant::now();
        let resp = client
            .get(&authed_url)
            .send()
            .with_context(|| format!("requesting {url}"))?;

        match resp.status().as_u16() {
            200 => {}
            404 => {
                info!("[{next_seq}] up to date at {current_seq}");
                break;
            }
            401 | 403 => bail!(
                "auth failed (HTTP {}); check METABRAINZ_ACCESS_TOKEN",
                resp.status()
            ),
            s => bail!("HTTP {s} fetching packet {next_seq}"),
        }

        eprint!("\r[{next_seq}] parsing... ");

        let decompressed = MultiBzDecoder::new(BufReader::with_capacity(1 << 20, resp));
        let pkt = replication::stream_packet(decompressed)
            .with_context(|| format!("parsing packet {next_seq}"))?;

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
