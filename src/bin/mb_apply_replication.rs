//! Apply or undo a MusicBrainz replication packet on a local SQLite URL mirror.
//!
//! Reads the packet produced by `mb_fetch_replication` (or any compatible
//! `replication-<N>-v2.tar.bz2`), filters `mbdump/pending_data` for
//! `musicbrainz.url` rows, normalizes changed URLs via [`url_norm::normalize`],
//! and applies inserts, updates, and deletes to the `mb_url` table in the
//! target SQLite database. The `mb_state` row is updated atomically in the
//! same transaction.
//!
//! Safety checks (apply):
//! - The packet's `REPLICATION_SEQUENCE` must be exactly `db_sequence + 1`.
//! - The packet's `SCHEMA_SEQUENCE` must match the database's `schema_sequence`.
//!
//! Safety checks (undo):
//! - The packet's `REPLICATION_SEQUENCE` must equal `db_sequence` (it must be
//!   the last packet applied). Packets must be undone in reverse order.
//! - The packet's `SCHEMA_SEQUENCE` must match the database's `schema_sequence`.
//!
//! Usage:
//!   cargo run --bin mb_apply_replication -- replication-186266.tar.bz2
//!   cargo run --bin mb_apply_replication -- replication-186266.tar.bz2 -d mb_mirror.db
//!   cargo run --bin mb_apply_replication -- --undo replication-186266.tar.bz2

use std::{
    fs::File,
    io::{BufReader, Read},
    path::PathBuf,
};

use anyhow::{Context, Result, bail};
use bzip2::read::MultiBzDecoder;
use clap::Parser;
use musiclib_rs::replication;
use rusqlite::Connection;
use tracing::info;

#[derive(Parser, Debug)]
#[command(about = "Apply (or undo) a MusicBrainz replication packet on a local SQLite URL mirror")]
struct Cli {
    /// Path to a `replication-<N>-v2.tar.bz2` packet file, or `-` for stdin.
    packet: PathBuf,

    /// SQLite database file (must already be bootstrapped via mb_extract_urls).
    /// Defaults to <data_dir>/mb_mirror.db.
    #[arg(short, long)]
    db: Option<PathBuf>,

    /// Reverse the packet instead of applying it. The packet must be the last
    /// one applied (replication_sequence must equal the db's current value).
    #[arg(long)]
    undo: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let db = cli
        .db
        .unwrap_or_else(|| musiclib_rs::app_dirs::data_dir().join("mb_mirror.db"));
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut conn = Connection::open(&db).with_context(|| format!("opening {}", db.display()))?;

    let (db_replication, db_schema) = read_db_state(&conn)?;
    info!("db state: replication_sequence={db_replication}, schema_sequence={db_schema}");

    let is_stdin = cli.packet.as_os_str() == "-";
    let input: Box<dyn Read> = if is_stdin {
        Box::new(std::io::stdin().lock())
    } else {
        let f =
            File::open(&cli.packet).with_context(|| format!("opening {}", cli.packet.display()))?;
        Box::new(f)
    };

    let decompressed = MultiBzDecoder::new(BufReader::with_capacity(1 << 20, input));
    let pkt = replication::stream_packet(decompressed)?;
    let ts = pkt.timestamp.as_deref().unwrap_or("?");

    if cli.undo {
        if pkt.replication_sequence != db_replication {
            bail!(
                "undo sequence mismatch: db has {db_replication}, \
                 packet is {} (can only undo the last applied packet)",
                pkt.replication_sequence
            );
        }
        if pkt.schema_sequence != db_schema {
            bail!(
                "schema mismatch: db has schema_sequence={db_schema}, \
                 packet has {} — re-bootstrap from a fresh full dump",
                pkt.schema_sequence
            );
        }
        let (n_i, n_u, n_d) = replication::count_ops(&pkt.ops);
        info!(
            "undoing packet {} ({ts}): {n_i}i {n_u}u {n_d}d",
            pkt.replication_sequence
        );
        replication::exec_ops(
            &mut conn,
            &pkt.ops,
            true,
            pkt.replication_sequence - 1,
            pkt.schema_sequence,
        )?;
        info!("done: undid packet {}", pkt.replication_sequence);
    } else {
        if pkt.replication_sequence != db_replication + 1 {
            bail!(
                "sequence gap: db has {db_replication}, packet is {} (expected {})",
                pkt.replication_sequence,
                db_replication + 1
            );
        }
        if pkt.schema_sequence != db_schema {
            bail!(
                "schema mismatch: db has schema_sequence={db_schema}, \
                 packet has {} — re-bootstrap from a fresh full dump",
                pkt.schema_sequence
            );
        }
        let (n_i, n_u, n_d) = replication::count_ops(&pkt.ops);
        info!(
            "applying packet {} ({ts}): {n_i}i {n_u}u {n_d}d",
            pkt.replication_sequence
        );
        replication::exec_ops(
            &mut conn,
            &pkt.ops,
            false,
            pkt.replication_sequence,
            pkt.schema_sequence,
        )?;
        info!("done: applied packet {}", pkt.replication_sequence);
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use musiclib_rs::replication::{
        PENDING_DATA_PATH, REPLICATION_SEQUENCE_PATH, SCHEMA_SEQUENCE_PATH, URL_TABLE,
    };
    use rusqlite::params;
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

    fn init_db(conn: &Connection) {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
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
        .unwrap();
    }

    fn seed_state(conn: &Connection, replication: i64, schema: i64) {
        conn.execute(
            "INSERT OR REPLACE INTO mb_state(id, replication_sequence, schema_sequence) \
             VALUES (1, ?1, ?2)",
            params![replication, schema],
        )
        .unwrap();
    }

    fn seed_url(conn: &Connection, id: i64, url_norm: &str) {
        conn.execute(
            "INSERT OR REPLACE INTO mb_url(id, url_norm) VALUES (?1, ?2)",
            params![id, url_norm],
        )
        .unwrap();
    }

    fn get_seq(conn: &Connection) -> i64 {
        conn.query_row("SELECT replication_sequence FROM mb_state", [], |r| {
            r.get(0)
        })
        .unwrap()
    }

    fn get_url(conn: &Connection, id: i64) -> Option<String> {
        conn.query_row(
            "SELECT url_norm FROM mb_url WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .ok()
    }

    fn build_packet(replication: &[u8], schema: &[u8], pending_data: &[u8]) -> Vec<u8> {
        let mut b = Builder::new(Vec::new());
        append_file(&mut b, REPLICATION_SEQUENCE_PATH, replication);
        append_file(&mut b, SCHEMA_SEQUENCE_PATH, schema);
        append_file(&mut b, PENDING_DATA_PATH, pending_data);
        b.into_inner().unwrap()
    }

    fn pending_row(seqnum: u64, table: &str, op: &str, old: &str, new: &str) -> String {
        format!("{seqnum}\t{table}\t{op}\t99\t{old}\t{new}\t\\N\t\\N\n")
    }

    fn run_apply(tar: Vec<u8>, conn: &mut Connection, db_rep: i64, db_schema: i64) -> Result<()> {
        let pkt = replication::stream_packet(Cursor::new(tar))?;
        if pkt.replication_sequence != db_rep + 1 {
            bail!(
                "sequence gap: db has {db_rep}, packet is {}",
                pkt.replication_sequence
            );
        }
        if pkt.schema_sequence != db_schema {
            bail!("schema mismatch");
        }
        replication::exec_ops(
            conn,
            &pkt.ops,
            false,
            pkt.replication_sequence,
            pkt.schema_sequence,
        )
    }

    fn run_undo(tar: Vec<u8>, conn: &mut Connection, db_rep: i64, db_schema: i64) -> Result<()> {
        let pkt = replication::stream_packet(Cursor::new(tar))?;
        if pkt.replication_sequence != db_rep {
            bail!(
                "undo sequence mismatch: db has {db_rep}, packet is {}",
                pkt.replication_sequence
            );
        }
        if pkt.schema_sequence != db_schema {
            bail!("schema mismatch");
        }
        replication::exec_ops(
            conn,
            &pkt.ops,
            true,
            pkt.replication_sequence - 1,
            pkt.schema_sequence,
        )
    }

    #[test]
    fn apply_insert() {
        let pending = pending_row(
            1,
            URL_TABLE,
            "i",
            r"\N",
            r#"{"id":42,"gid":"aaa","url":"https://www.youtube.com/watch?v=dQw4w9WgXcQ","edits_pending":0,"last_updated":""}"#,
        );
        let tar = build_packet(b"101\n", b"31\n", pending.as_bytes());
        let mut conn = Connection::open_in_memory().unwrap();
        init_db(&conn);
        seed_state(&conn, 100, 31);

        run_apply(tar, &mut conn, 100, 31).unwrap();

        assert_eq!(get_url(&conn, 42).unwrap(), "https://youtu.be/dQw4w9WgXcQ");
        assert_eq!(get_seq(&conn), 101);
    }

    #[test]
    fn apply_update() {
        let pending = pending_row(
            1,
            URL_TABLE,
            "u",
            r#"{"id":7,"gid":"bbb","url":"http://youtu.be/abc","edits_pending":0,"last_updated":""}"#,
            r#"{"id":7,"gid":"bbb","url":"https://youtu.be/abc","edits_pending":0,"last_updated":""}"#,
        );
        let tar = build_packet(b"101\n", b"31\n", pending.as_bytes());
        let mut conn = Connection::open_in_memory().unwrap();
        init_db(&conn);
        seed_state(&conn, 100, 31);
        seed_url(&conn, 7, "http://youtu.be/abc");

        run_apply(tar, &mut conn, 100, 31).unwrap();

        assert_eq!(get_url(&conn, 7).unwrap(), "https://youtu.be/abc");
    }

    #[test]
    fn apply_delete() {
        let pending = pending_row(
            1,
            URL_TABLE,
            "d",
            r#"{"id":99,"gid":"ccc","url":"https://example.com","edits_pending":0,"last_updated":""}"#,
            r"\N",
        );
        let tar = build_packet(b"101\n", b"31\n", pending.as_bytes());
        let mut conn = Connection::open_in_memory().unwrap();
        init_db(&conn);
        seed_state(&conn, 100, 31);
        seed_url(&conn, 99, "https://example.com");

        run_apply(tar, &mut conn, 100, 31).unwrap();

        assert!(get_url(&conn, 99).is_none());
    }

    #[test]
    fn apply_sequence_gap_rejected() {
        let tar = build_packet(b"103\n", b"31\n", b"");
        let mut conn = Connection::open_in_memory().unwrap();
        init_db(&conn);
        seed_state(&conn, 100, 31);

        let err = run_apply(tar, &mut conn, 100, 31).unwrap_err();
        assert!(format!("{err}").contains("sequence gap"));
    }

    #[test]
    fn apply_schema_mismatch_rejected() {
        let tar = build_packet(b"101\n", b"32\n", b"");
        let mut conn = Connection::open_in_memory().unwrap();
        init_db(&conn);
        seed_state(&conn, 100, 31);

        let err = run_apply(tar, &mut conn, 100, 31).unwrap_err();
        assert!(format!("{err}").contains("schema mismatch"));
    }

    #[test]
    fn undo_insert_removes_url() {
        let pending = pending_row(
            1,
            URL_TABLE,
            "i",
            r"\N",
            r#"{"id":42,"gid":"aaa","url":"https://example.com","edits_pending":0,"last_updated":""}"#,
        );
        let tar = build_packet(b"101\n", b"31\n", pending.as_bytes());
        let mut conn = Connection::open_in_memory().unwrap();
        init_db(&conn);
        seed_state(&conn, 101, 31);
        seed_url(&conn, 42, "https://example.com");

        run_undo(tar, &mut conn, 101, 31).unwrap();

        assert!(get_url(&conn, 42).is_none());
        assert_eq!(get_seq(&conn), 100);
    }

    #[test]
    fn undo_delete_restores_url() {
        let pending = pending_row(
            1,
            URL_TABLE,
            "d",
            r#"{"id":99,"gid":"ccc","url":"https://example.com","edits_pending":0,"last_updated":""}"#,
            r"\N",
        );
        let tar = build_packet(b"101\n", b"31\n", pending.as_bytes());
        let mut conn = Connection::open_in_memory().unwrap();
        init_db(&conn);
        seed_state(&conn, 101, 31);

        run_undo(tar, &mut conn, 101, 31).unwrap();

        assert_eq!(get_url(&conn, 99).unwrap(), "https://example.com");
        assert_eq!(get_seq(&conn), 100);
    }

    #[test]
    fn undo_update_restores_old_url() {
        let pending = pending_row(
            1,
            URL_TABLE,
            "u",
            r#"{"id":7,"gid":"bbb","url":"https://example.com/old","edits_pending":0,"last_updated":""}"#,
            r#"{"id":7,"gid":"bbb","url":"https://example.com/new","edits_pending":0,"last_updated":""}"#,
        );
        let tar = build_packet(b"101\n", b"31\n", pending.as_bytes());
        let mut conn = Connection::open_in_memory().unwrap();
        init_db(&conn);
        seed_state(&conn, 101, 31);
        seed_url(&conn, 7, "https://example.com/new");

        run_undo(tar, &mut conn, 101, 31).unwrap();

        assert_eq!(get_url(&conn, 7).unwrap(), "https://example.com/old");
        assert_eq!(get_seq(&conn), 100);
    }

    #[test]
    fn undo_wrong_sequence_rejected() {
        let tar = build_packet(b"100\n", b"31\n", b"");
        let mut conn = Connection::open_in_memory().unwrap();
        init_db(&conn);
        seed_state(&conn, 101, 31);

        let err = run_undo(tar, &mut conn, 101, 31).unwrap_err();
        assert!(format!("{err}").contains("undo sequence mismatch"));
    }

    #[test]
    fn apply_then_undo_roundtrip() {
        let old_json = r#"{"id":7,"gid":"bbb","url":"https://example.com/old","edits_pending":0,"last_updated":""}"#;
        let new_json = r#"{"id":7,"gid":"bbb","url":"https://example.com/new","edits_pending":0,"last_updated":""}"#;
        let pending = pending_row(1, URL_TABLE, "u", old_json, new_json);
        let tar_bytes = build_packet(b"101\n", b"31\n", pending.as_bytes());

        let mut conn = Connection::open_in_memory().unwrap();
        init_db(&conn);
        seed_state(&conn, 100, 31);
        seed_url(&conn, 7, "https://example.com/old");

        run_apply(tar_bytes.clone(), &mut conn, 100, 31).unwrap();
        assert_eq!(get_url(&conn, 7).unwrap(), "https://example.com/new");
        assert_eq!(get_seq(&conn), 101);

        run_undo(tar_bytes, &mut conn, 101, 31).unwrap();
        assert_eq!(get_url(&conn, 7).unwrap(), "https://example.com/old");
        assert_eq!(get_seq(&conn), 100);
    }
}
