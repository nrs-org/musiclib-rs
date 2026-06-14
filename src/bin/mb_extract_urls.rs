//! Stream-extract the `url` table from a MusicBrainz `mbdump.tar.bz2`
//! into a local SQLite mirror.
//!
//! With no arguments, downloads the latest full export from MetaBrainz and
//! streams it directly — nothing is written to disk except the SQLite mirror.
//! Can also read from a local file, a pre-untarred directory, or stdin (`-`).
//!
//! Usage:
//!   cargo run --release --bin mb_extract_urls                          # auto-download
//!   cargo run --release --bin mb_extract_urls -- /path/to/mbdump.tar.bz2
//!   cargo run --release --bin mb_extract_urls -- /path/to/untarred-dir
//!   curl -L 'https://.../mbdump.tar.bz2' \
//!     | cargo run --release --bin mb_extract_urls -- -

use std::{
    fs::File,
    io::{self, BufRead, BufReader, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc::sync_channel},
    time::Instant,
};

use anyhow::{Context, Result};
use bzip2::read::MultiBzDecoder;
use clap::Parser;
use musiclib_rs::providers::registry::{self, RegistryConfig};
use rusqlite::{Connection, params};
use tar::Archive;
use tracing::info;

const URL_TABLE_PATH: &str = "mbdump/url";
const DUMP_BASE_URL: &str = "https://data.metabrainz.org/pub/musicbrainz/data/fullexport";

#[derive(Parser, Debug)]
#[command(about = "Extract the `url` table from a MusicBrainz dump into SQLite")]
struct Cli {
    /// Path to `mbdump.tar.bz2`, an already-untarred dump directory, or `-`
    /// for stdin. If omitted, the latest export is downloaded automatically.
    dump: Option<PathBuf>,

    /// SQLite database file. Created if it does not exist.
    /// Defaults to <data_dir>/mb_mirror.db.
    #[arg(short, long)]
    db: Option<PathBuf>,

    /// Provider credentials config. Defaults to <config_dir>/providers.yaml.
    #[arg(long)]
    registry_config: Option<PathBuf>,

    /// Base URL for the MusicBrainz full-export server. Overrides
    /// `dump_base_url` in providers.yaml and the `MUSICBRAINZ_DUMP_BASE_URL`
    /// env var.
    #[arg(long)]
    base_url: Option<String>,
}

const REPLICATION_SEQUENCE_PATH: &str = "REPLICATION_SEQUENCE";
const SCHEMA_SEQUENCE_PATH: &str = "SCHEMA_SEQUENCE";

fn build_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!(
            "musiclib-rs/",
            env!("CARGO_PKG_VERSION"),
            " (mb-extract-urls)"
        ))
        .build()
        .context("building HTTP client")
}

fn latest_dump_date(client: &reqwest::blocking::Client, base_url: &str) -> Result<String> {
    let url = format!("{base_url}/LATEST");
    let text = client
        .get(&url)
        .send()
        .with_context(|| format!("fetching {url}"))?
        .error_for_status()
        .with_context(|| format!("fetching {url}"))?
        .text()
        .context("reading LATEST response")?;
    Ok(text.trim().to_owned())
}

fn load_registry_config(path: Option<&Path>) -> Result<RegistryConfig> {
    let default = musiclib_rs::app_dirs::config_dir().join("providers.yaml");
    let path = match path {
        Some(p) => p.to_owned(),
        None if default.exists() => default,
        None => return Ok(RegistryConfig::default()),
    };
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(serde_yaml_ng::from_str(&text)?)
}

fn main() -> Result<()> {
    dotenv::dotenv().ok();
    let cli = Cli::parse();

    let registry_config = load_registry_config(cli.registry_config.as_deref())?;

    let base_url = cli
        .base_url
        .or_else(|| {
            registry_config
                .musicbrainz
                .as_ref()
                .and_then(|mb| mb.dump_base_url.as_ref())
                .and_then(|c| c.resolve())
        })
        .unwrap_or_else(|| DUMP_BASE_URL.to_owned());

    let db = cli
        .db
        .unwrap_or_else(|| musiclib_rs::app_dirs::data_dir().join("mb_mirror.db"));
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut conn = Connection::open(&db).with_context(|| format!("opening {}", db.display()))?;
    init_db(&conn)?;

    match cli.dump.as_deref() {
        Some(p) if p.as_os_str() != "-" && p.is_dir() => run_from_dir(p, &mut conn),
        Some(p) => {
            let input: Box<dyn Read> = if p.as_os_str() == "-" {
                Box::new(io::stdin().lock())
            } else {
                Box::new(File::open(p).with_context(|| format!("opening {}", p.display()))?)
            };
            let decompressed = MultiBzDecoder::new(BufReader::with_capacity(1 << 20, input));
            run(decompressed, &mut conn)
        }
        None => {
            let client = build_client()?;
            let date = latest_dump_date(&client, &base_url)?;
            let url = format!("{base_url}/{date}/mbdump.tar.bz2");
            info!("downloading {url}");
            let resp = client
                .get(&url)
                .send()
                .with_context(|| format!("fetching {url}"))?
                .error_for_status()
                .with_context(|| format!("fetching {url}"))?;
            let total = resp.content_length();
            let pb = indicatif::ProgressBar::new(total.unwrap_or(0));
            pb.set_style(
                indicatif::ProgressStyle::with_template(
                    "{spinner} [{elapsed_precise}] [{bar:40}] {bytes}/{total_bytes} ({eta})",
                )
                .unwrap()
                .progress_chars("=>-"),
            );
            let decompressed =
                MultiBzDecoder::new(BufReader::with_capacity(1 << 20, pb.wrap_read(resp)));
            let result = run(decompressed, &mut conn);
            pb.finish_and_clear();
            result
        }
    }
}

/// Load directly from an untarred dump directory. Expects the same layout
/// the tarball produces: `REPLICATION_SEQUENCE`, `SCHEMA_SEQUENCE`, and
/// `mbdump/url` all relative to `dir`.
fn run_from_dir(dir: &Path, conn: &mut Connection) -> Result<()> {
    let r_path = dir.join(REPLICATION_SEQUENCE_PATH);
    let s_path = dir.join(SCHEMA_SEQUENCE_PATH);
    let url_path = dir.join(URL_TABLE_PATH);

    let r = read_seq(
        &mut File::open(&r_path).with_context(|| format!("opening {}", r_path.display()))?,
    )
    .with_context(|| format!("reading {}", r_path.display()))?;
    info!("REPLICATION_SEQUENCE = {r}");

    let s = read_seq(
        &mut File::open(&s_path).with_context(|| format!("opening {}", s_path.display()))?,
    )
    .with_context(|| format!("reading {}", s_path.display()))?;
    info!("SCHEMA_SEQUENCE = {s}");

    info!("loading urls from {}...", url_path.display());
    let url_file =
        File::open(&url_path).with_context(|| format!("opening {}", url_path.display()))?;
    let count = load_urls(url_file, conn)?;
    info!("inserted {count} urls, building index...");
    build_index(conn)?;

    set_state(conn, r, s)?;
    info!("wrote mb_state: replication_sequence={r}, schema_sequence={s}");
    info!("done: {count} urls indexed");
    Ok(())
}

/// Stream the tar archive, capturing the three things we care about:
/// `REPLICATION_SEQUENCE`, `SCHEMA_SEQUENCE`, and `mbdump/url`. Exits the
/// iteration as soon as all three are seen, so we don't waste time decoding
/// the trailing portion of the archive.
fn run<R: Read>(input: R, conn: &mut Connection) -> Result<()> {
    let mut archive = Archive::new(input);

    let mut replication_sequence: Option<i64> = None;
    let mut schema_sequence: Option<i64> = None;
    let mut url_count: Option<u64> = None;

    for entry in archive.entries().context("reading tar entries")? {
        let mut entry = entry.context("reading tar entry")?;
        let path_str: String = {
            let path = entry.path().context("decoding tar entry path")?;
            match path.to_str() {
                Some(s) => s.to_owned(),
                None => continue,
            }
        };

        match path_str.as_str() {
            REPLICATION_SEQUENCE_PATH => {
                let n = read_seq(&mut entry).context("reading REPLICATION_SEQUENCE")?;
                info!("REPLICATION_SEQUENCE = {n}");
                replication_sequence = Some(n);
            }
            SCHEMA_SEQUENCE_PATH => {
                let n = read_seq(&mut entry).context("reading SCHEMA_SEQUENCE")?;
                info!("SCHEMA_SEQUENCE = {n}");
                schema_sequence = Some(n);
            }
            URL_TABLE_PATH if url_count.is_none() => {
                info!("found {URL_TABLE_PATH}, loading...");
                let count = load_urls(entry, conn)?;
                info!("inserted {count} urls, building index...");
                build_index(conn)?;
                url_count = Some(count);
            }
            _ => {}
        }

        if url_count.is_some() && replication_sequence.is_some() && schema_sequence.is_some() {
            break;
        }
    }

    let count = url_count.context(format!("{URL_TABLE_PATH} not found in archive"))?;
    let r = replication_sequence
        .context(format!("{REPLICATION_SEQUENCE_PATH} not found in archive"))?;
    let s = schema_sequence.context(format!("{SCHEMA_SEQUENCE_PATH} not found in archive"))?;

    set_state(conn, r, s)?;
    info!("wrote mb_state: replication_sequence={r}, schema_sequence={s}");
    info!("done: {count} urls indexed");
    Ok(())
}

/// Read an MB sequence file (a single integer plus trailing newline).
fn read_seq<R: Read>(reader: &mut R) -> Result<i64> {
    let mut s = String::new();
    reader
        .read_to_string(&mut s)
        .context("reading sequence file")?;
    let trimmed = s.trim();
    trimmed
        .parse::<i64>()
        .with_context(|| format!("parsing {trimmed:?} as i64"))
}

/// Apply bulk-load PRAGMAs and ensure the target tables exist. The lookup
/// index on `url_norm` is created *after* bulk insert in [`build_index`] —
/// much faster than maintaining it row-by-row.
///
/// `mb_state` is a single-row table (enforced by `CHECK (id = 1)`) holding the
/// two sequence ints that drive the replication consumer.
fn init_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous  = NORMAL;
         PRAGMA temp_store   = MEMORY;
         PRAGMA cache_size   = -262144;
         CREATE TABLE IF NOT EXISTS mb_url (
             id       INTEGER PRIMARY KEY,
             url_norm TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS mb_state (
             id                   INTEGER PRIMARY KEY CHECK (id = 1),
             replication_sequence INTEGER NOT NULL,
             schema_sequence      INTEGER NOT NULL
         );",
    )
    .context("initializing sqlite")?;
    Ok(())
}

/// Non-unique because multiple raw URLs can normalize to the same form, and
/// replication identifies rows by `id` — collapsing them would break updates.
fn build_index(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE INDEX IF NOT EXISTS mb_url_url_norm ON mb_url(url_norm);")
        .context("creating url_norm index")?;
    Ok(())
}

/// Upsert the single `mb_state` row.
fn set_state(conn: &Connection, replication_sequence: i64, schema_sequence: i64) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO mb_state(id, replication_sequence, schema_sequence) \
         VALUES (1, ?1, ?2)",
        params![replication_sequence, schema_sequence],
    )
    .context("writing mb_state")?;
    Ok(())
}

/// Stream `mbdump/url` row-by-row and INSERT each `(id, url_norm)` into
/// `mb_url`. URLs are normalized inline via [`registry::normalize`] so both
/// the bootstrap and lookup paths agree on the canonical form. One enclosing
/// transaction so commit cost amortizes over the full load.
///
/// Pipeline: the reader (this thread) parses TSV and batches raw rows into a
/// bounded channel; a pool of worker threads pulls batches and normalizes URLs
/// in parallel; a single writer thread drains the normalized batches and
/// executes the SQLite inserts. SQLite stays single-writer; the parallelism
/// comes from `normalize`, which is the CPU-heavy step (regex chain across
/// every backend).
fn load_urls<R: Read>(reader: R, conn: &mut Connection) -> Result<u64> {
    const BATCH_SIZE: usize = 2048;
    const CHANNEL_CAP: usize = 16;

    // Leave a core for the reader and one for the writer; the rest normalize.
    let n_workers = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(2);

    let (raw_tx, raw_rx) = sync_channel::<Vec<(i64, String)>>(CHANNEL_CAP);
    let raw_rx = Arc::new(Mutex::new(raw_rx));
    let (norm_tx, norm_rx) = sync_channel::<Vec<(i64, String)>>(CHANNEL_CAP);

    std::thread::scope(|s| -> Result<u64> {
        for _ in 0..n_workers {
            let rx = Arc::clone(&raw_rx);
            let tx = norm_tx.clone();
            s.spawn(move || {
                loop {
                    let batch = {
                        let guard = rx.lock().expect("raw_rx mutex poisoned");
                        match guard.recv() {
                            Ok(b) => b,
                            Err(_) => break,
                        }
                    };
                    let normalized: Vec<(i64, String)> = batch
                        .into_iter()
                        .map(|(id, raw)| (id, registry::normalize(&raw)))
                        .collect();
                    if tx.send(normalized).is_err() {
                        break;
                    }
                }
            });
        }
        // Only the worker clones of norm_tx remain; this lets norm_rx terminate
        // once every worker exits.
        drop(norm_tx);

        let writer = s.spawn(move || -> Result<u64> {
            let dbtx = conn.transaction()?;
            let mut count = 0u64;
            let mut last_log = Instant::now();
            {
                let mut stmt =
                    dbtx.prepare("INSERT OR REPLACE INTO mb_url(id, url_norm) VALUES (?1, ?2)")?;
                while let Ok(batch) = norm_rx.recv() {
                    for (id, norm) in batch {
                        stmt.execute(params![id, norm])?;
                        count += 1;
                        if count.is_multiple_of(100_000) && last_log.elapsed().as_secs() >= 2 {
                            info!("  {count} urls inserted...");
                            last_log = Instant::now();
                        }
                    }
                }
            }
            dbtx.commit()?;
            Ok(count)
        });

        let mut batch: Vec<(i64, String)> = Vec::with_capacity(BATCH_SIZE);
        let read_res: Result<()> = (|| {
            for_each_url(reader, |id, url| {
                batch.push((id, url));
                if batch.len() >= BATCH_SIZE {
                    let full = std::mem::replace(&mut batch, Vec::with_capacity(BATCH_SIZE));
                    if raw_tx.send(full).is_err() {
                        return Err(anyhow::anyhow!("worker pool exited early"));
                    }
                }
                Ok(())
            })?;
            if !batch.is_empty() && raw_tx.send(batch).is_err() {
                return Err(anyhow::anyhow!("worker pool exited early"));
            }
            Ok(())
        })();
        // Close the raw channel so workers drain and exit; do this even on
        // reader error so the workers don't deadlock.
        drop(raw_tx);
        read_res?;

        writer
            .join()
            .map_err(|_| anyhow::anyhow!("writer thread panicked"))?
    })
}

/// Parse the `url` table TSV and yield `(id, raw_url)` for each row. The
/// `String` is passed owned so callers can move it across a thread boundary
/// without an extra clone.
///
/// Schema: `id, gid, url, edits_pending, last_updated` in Postgres COPY TEXT
/// format (tab-separated, `\N` NULL, backslash escapes).
fn for_each_url<R: Read>(reader: R, mut f: impl FnMut(i64, String) -> Result<()>) -> Result<u64> {
    let buf = BufReader::with_capacity(1 << 20, reader);
    let mut count: u64 = 0;

    for line in buf.lines() {
        let line = line.context("reading url table line")?;
        let mut fields = line.split('\t');
        let id_raw = fields.next().context("missing id column")?;
        let _gid = fields.next().context("missing gid column")?;
        let url_raw = fields.next().context("missing url column")?;

        if url_raw == r"\N" {
            // url is NOT NULL in the schema, but tolerate it
            continue;
        }
        let id: i64 = id_raw
            .parse()
            .with_context(|| format!("parsing id {id_raw:?}"))?;
        let url = unescape_copy(url_raw);
        f(id, url)?;
        count += 1;
    }
    Ok(count)
}

/// Decode Postgres COPY TEXT backslash escapes.
fn unescape_copy(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('b') => out.push('\x08'),
            Some('f') => out.push('\x0c'),
            Some('v') => out.push('\x0b'),
            // Pass through anything we don't recognize (incl. `\N`, which the
            // caller handles before getting here).
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tar::{Builder, Header};

    fn append_file(builder: &mut Builder<Vec<u8>>, name: &str, contents: &[u8]) {
        let mut header = Header::new_gnu();
        header.set_path(name).unwrap();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, contents).unwrap();
    }

    fn build_test_tar(replication: &[u8], schema: &[u8], url_tsv: &[u8]) -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());
        append_file(&mut builder, REPLICATION_SEQUENCE_PATH, replication);
        append_file(&mut builder, SCHEMA_SEQUENCE_PATH, schema);
        append_file(&mut builder, URL_TABLE_PATH, url_tsv);
        builder.into_inner().unwrap()
    }

    #[test]
    fn unescape_passthrough() {
        assert_eq!(
            unescape_copy("https://example.com/foo"),
            "https://example.com/foo"
        );
    }

    #[test]
    fn unescape_known_escapes() {
        assert_eq!(unescape_copy(r"a\\b\tc\nd"), "a\\b\tc\nd");
    }

    #[test]
    fn for_each_url_basic() {
        let tsv = "1\t00000000-0000-0000-0000-000000000001\thttps://a.example\t0\t2024-01-01\n\
                   2\t00000000-0000-0000-0000-000000000002\thttps://b.example/x\t0\t2024-01-02\n";
        let mut got: Vec<(i64, String)> = Vec::new();
        let count = for_each_url(Cursor::new(tsv), |id, url| {
            got.push((id, url));
            Ok(())
        })
        .unwrap();
        assert_eq!(count, 2);
        assert_eq!(got[0], (1, "https://a.example".to_owned()));
        assert_eq!(got[1], (2, "https://b.example/x".to_owned()));
    }

    #[test]
    fn run_extracts_state_and_urls_from_tar() {
        let url_tsv = b"7\t00000000-0000-0000-0000-000000000007\thttps://www.youtube.com/watch?v=dQw4w9WgXcQ\t0\t2024-01-01\n" as &[u8];
        let tar_bytes = build_test_tar(b"123456\n", b"27\n", url_tsv);

        let mut conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();
        run(Cursor::new(tar_bytes), &mut conn).unwrap();

        let (r, s): (i64, i64) = conn
            .query_row(
                "SELECT replication_sequence, schema_sequence FROM mb_state",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((r, s), (123456, 27));

        let url: String = conn
            .query_row("SELECT url_norm FROM mb_url WHERE id = 7", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(url, "https://youtu.be/dQw4w9WgXcQ");
    }

    #[test]
    fn run_errors_if_url_table_missing() {
        let mut builder = Builder::new(Vec::new());
        append_file(&mut builder, REPLICATION_SEQUENCE_PATH, b"1\n");
        append_file(&mut builder, SCHEMA_SEQUENCE_PATH, b"27\n");
        let tar_bytes = builder.into_inner().unwrap();

        let mut conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();
        let err = run(Cursor::new(tar_bytes), &mut conn).unwrap_err();
        assert!(format!("{err}").contains(URL_TABLE_PATH));
    }

    #[test]
    fn run_errors_if_sequence_files_missing() {
        let mut builder = Builder::new(Vec::new());
        append_file(
            &mut builder,
            URL_TABLE_PATH,
            b"1\t00000000-0000-0000-0000-000000000001\thttps://example.com\t0\t2024-01-01\n",
        );
        let tar_bytes = builder.into_inner().unwrap();

        let mut conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();
        let err = run(Cursor::new(tar_bytes), &mut conn).unwrap_err();
        assert!(format!("{err}").contains(REPLICATION_SEQUENCE_PATH));
    }

    #[test]
    fn run_from_dir_loads_from_filesystem() {
        use std::fs;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::write(dir.join(REPLICATION_SEQUENCE_PATH), b"999\n").unwrap();
        fs::write(dir.join(SCHEMA_SEQUENCE_PATH), b"27\n").unwrap();
        fs::create_dir_all(dir.join("mbdump")).unwrap();
        fs::write(
            dir.join(URL_TABLE_PATH),
            b"5\t00000000-0000-0000-0000-000000000005\thttps://youtu.be/dQw4w9WgXcQ\t0\t2024-01-01\n",
        )
        .unwrap();

        let mut conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();
        run_from_dir(dir, &mut conn).unwrap();

        let (r, s): (i64, i64) = conn
            .query_row(
                "SELECT replication_sequence, schema_sequence FROM mb_state",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((r, s), (999, 27));

        let url: String = conn
            .query_row("SELECT url_norm FROM mb_url WHERE id = 5", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(url, "https://youtu.be/dQw4w9WgXcQ");
    }

    #[test]
    fn read_seq_parses_trimmed_int() {
        assert_eq!(read_seq(&mut Cursor::new(b"42\n")).unwrap(), 42);
        assert_eq!(read_seq(&mut Cursor::new(b"  100  \n")).unwrap(), 100);
        assert!(read_seq(&mut Cursor::new(b"not-a-number")).is_err());
    }

    #[test]
    fn mb_state_is_single_row_and_upsert() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();

        set_state(&conn, 100, 27).unwrap();
        let (r, s): (i64, i64) = conn
            .query_row(
                "SELECT replication_sequence, schema_sequence FROM mb_state",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((r, s), (100, 27));

        // Second call upserts the same row (CHECK (id = 1) keeps it single).
        set_state(&conn, 101, 27).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM mb_state", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let r2: i64 = conn
            .query_row("SELECT replication_sequence FROM mb_state", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(r2, 101);
    }

    #[test]
    fn load_urls_normalizes_and_writes_to_sqlite() {
        // Row 11's URL is a youtube.com/watch form with junk tail params; the
        // YT canonicalize provider collapses it to https://youtu.be/<id>.
        let tsv = "10\t00000000-0000-0000-0000-00000000000a\thttps://example.com/page\t0\t2024-01-01\n\
                   11\t00000000-0000-0000-0000-00000000000b\thttps://www.youtube.com/watch?v=dQw4w9WgXcQ&t=42s&feature=share\t0\t2024-01-02\n";
        let mut conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();
        let n = load_urls(Cursor::new(tsv), &mut conn).unwrap();
        assert_eq!(n, 2);

        let norm: String = conn
            .query_row("SELECT url_norm FROM mb_url WHERE id = 11", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(norm, "https://youtu.be/dQw4w9WgXcQ");

        build_index(&conn).unwrap();
        let found: i64 = conn
            .query_row(
                "SELECT id FROM mb_url WHERE url_norm = ?1",
                params!["https://youtu.be/dQw4w9WgXcQ"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(found, 11);
    }
}
