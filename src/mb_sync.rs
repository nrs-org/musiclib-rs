//! Catch the MusicBrainz URL mirror up by applying replication packets,
//! never past the MusicBrainz server the backend queries
//! (`crate::mb_freshness`). Used by `mb_sync_replication` and the library
//! update (`pipeline::update`).
//!
//! Blocking: run it on a blocking thread from async code.

use std::{
    io::BufReader,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use bzip2::read::MultiBzDecoder;
use rusqlite::Connection;
use tracing::{info, warn};

use crate::{
    mb_freshness,
    providers::registry::MusicBrainzConfig,
    replication::{self, PacketInfo},
};

pub struct SyncOptions {
    /// The mirror file; defaults to the backend's (`mirror_db`, else
    /// <data_dir>/mb_mirror.db when it exists).
    pub db: Option<PathBuf>,
    /// Base URL of the replication-packets endpoint.
    pub base_url: String,
    /// Apply packets up to this sequence instead of stopping at a
    /// self-hosted server's own.
    pub until: Option<i64>,
    /// Fail if a self-hosted server is more than this many packets (about
    /// one an hour) behind the newest published one: its own replication
    /// has likely stopped.
    pub max_server_lag: Option<i64>,
    /// Called after each packet is applied, with the mirror's sequence and
    /// the one it's syncing up to (`None` for the newest published).
    pub on_packet: Option<Box<dyn Fn(i64, Option<i64>) + Send>>,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            db: None,
            base_url: mb_freshness::PACKETS_BASE_URL.to_owned(),
            until: None,
            max_server_lag: None,
            on_packet: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct SyncOutcome {
    /// Mirror sequence before and after.
    pub from: i64,
    pub to: i64,
}

/// Sync the mirror. `Ok(None)` when there's no mirror to sync. `token` is a
/// `METABRAINZ_ACCESS_TOKEN`.
pub fn sync(
    mb: Option<&MusicBrainzConfig>,
    token: &str,
    opts: &SyncOptions,
) -> Result<Option<SyncOutcome>> {
    // The server's sequence, `None` for the public server (or a disabled
    // backend). An explicit `until` needs it only for the lag check.
    let server_seq = if opts.until.is_some() && opts.max_server_lag.is_none() {
        None
    } else {
        mb.map(mb_freshness::mirror_cap).transpose()?.flatten()
    };
    if let (Some(max_lag), Some(server)) = (opts.max_server_lag, server_seq) {
        let latest = mb_freshness::latest_published(&opts.base_url, token)?;
        let lag = latest - server;
        if lag > max_lag {
            bail!(
                "the MusicBrainz server is at packet {server}, {lag} behind the newest \
                 ({latest}); is its replication running?"
            );
        }
        info!("server at {server}, {lag} packet(s) behind the newest ({latest})");
    }
    let cap = opts.until.or(server_seq);

    let Some(db) = opts.db.clone().or_else(|| match mb {
        Some(cfg) => cfg.mirror_db_path(),
        None => {
            let default = crate::app_dirs::data_dir().join("mb_mirror.db");
            default.exists().then_some(default)
        }
    }) else {
        info!("no URL mirror configured; nothing to sync");
        return Ok(None);
    };
    let mut conn = Connection::open(&db).with_context(|| format!("opening {}", db.display()))?;

    let (start_seq, db_schema) = read_db_state(&conn)?;
    info!("mirror: replication_sequence={start_seq}, schema_sequence={db_schema}");
    if let Some(cap) = cap
        && start_seq > cap
    {
        bail!(
            "the mirror is at packet {start_seq}, ahead of the server's {cap}; \
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

    let mut current_seq = start_seq;
    let sync_start = Instant::now();
    loop {
        let next_seq = current_seq + 1;
        if cap.is_some_and(|cap| next_seq > cap) {
            info!("mirror at {current_seq}, the server's sequence; stopping there");
            break;
        }
        let url = format!(
            "{}/replication-{}-v2.tar.bz2",
            opts.base_url.trim_end_matches('/'),
            next_seq
        );
        let authed_url = format!("{url}?token={}", urlencoding::encode(token));

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
        replication::exec_ops(
            &mut conn,
            &pkt.ops,
            false,
            pkt.replication_sequence,
            pkt.schema_sequence,
        )
        .with_context(|| format!("applying packet {}", pkt.replication_sequence))?;
        info!(
            "[{next_seq}] {ts} | {n_i}i {n_u}u {n_d}d | {:.1}s",
            pkt_start.elapsed().as_secs_f64()
        );
        current_seq = pkt.replication_sequence;
        if let Some(f) = &opts.on_packet {
            f(current_seq, cap);
        }
    }

    info!(
        "{} packet(s) applied in {:.1}s, mirror at {current_seq}",
        current_seq - start_seq,
        sync_start.elapsed().as_secs_f64()
    );
    Ok(Some(SyncOutcome {
        from: start_seq,
        to: current_seq,
    }))
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
    let decompressed = MultiBzDecoder::new(BufReader::with_capacity(1 << 20, resp));
    replication::stream_packet(decompressed)
        .with_context(|| format!("parsing packet {seq}"))
        .map(Some)
        .map_err(FetchError::Other)
}
