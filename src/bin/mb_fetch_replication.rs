//! Fetch a MusicBrainz replication packet by sequence number.
//!
//! Usage:
//!   cargo run --bin mb_fetch_replication -- 123456
//!   cargo run --bin mb_fetch_replication -- 123456 -o /tmp/pkt.tar.bz2
//!   cargo run --bin mb_fetch_replication -- 123456 -o - | \
//!     cargo run --bin mb_apply_replication -- -
//!
//! Reads `METABRAINZ_ACCESS_TOKEN` from the environment (or `.env`). Get one
//! at https://metabrainz.org/profile/applications.

use std::{fs::File, io, path::PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;
use tracing::info;

const DEFAULT_BASE_URL: &str = "https://metabrainz.org/api/musicbrainz/";

#[derive(Parser, Debug)]
#[command(about = "Download a MusicBrainz replication packet")]
struct Cli {
    /// Replication sequence number.
    seq: u64,

    /// Output file, or `-` to write to stdout (for piping into mb_apply_replication).
    /// Defaults to `replication-<seq>.tar.bz2` in CWD.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Base URL for the replication-packets endpoint. Override for testing
    /// against a mirror or local mock.
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

    // dbmirror v2 has been the only modern format; the `-v2` suffix is part
    // of the filename.
    let url = format!("{}/replication-{}-v2.tar.bz2", cli.base_url, cli.seq);

    let to_stdout = cli.output.as_deref().is_some_and(|p| p.as_os_str() == "-");
    if !to_stdout {
        let out_path = cli
            .output
            .as_deref()
            .map(|p| p.to_owned())
            .unwrap_or_else(|| PathBuf::from(format!("replication-{}.tar.bz2", cli.seq)));
        info!("fetching packet {} -> {}", cli.seq, out_path.display());
    } else {
        info!("fetching packet {} -> stdout", cli.seq);
    }

    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!(
            "musiclib-rs/",
            env!("CARGO_PKG_VERSION"),
            " (replication-fetcher)"
        ))
        .build()
        .context("building HTTP client")?;

    let authed_url = format!("{url}?token={}", urlencoding::encode(&token));
    let mut resp = client
        .get(&authed_url)
        .send()
        .with_context(|| format!("requesting {url}"))?;

    match resp.status().as_u16() {
        200 => {}
        401 | 403 => bail!(
            "auth failed (HTTP {}); check METABRAINZ_ACCESS_TOKEN",
            resp.status()
        ),
        404 => bail!(
            "packet {} not found (HTTP 404) — sequence may be ahead of the \
             latest published packet, or the URL pattern is wrong",
            cli.seq
        ),
        s => bail!("HTTP {s} fetching packet {}", cli.seq),
    }

    let n = if to_stdout {
        resp.copy_to(&mut io::stdout().lock())
            .context("writing to stdout")?
    } else {
        let out_path = cli
            .output
            .unwrap_or_else(|| PathBuf::from(format!("replication-{}.tar.bz2", cli.seq)));
        let mut file =
            File::create(&out_path).with_context(|| format!("creating {}", out_path.display()))?;
        resp.copy_to(&mut file).context("writing response body")?
    };

    info!("wrote {n} bytes");
    Ok(())
}
