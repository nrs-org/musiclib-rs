//! Shared logic for reading and applying MusicBrainz dbmirror v2 replication
//! packets to a local SQLite URL mirror.

use std::io::{BufRead, BufReader, Read};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use serde::Deserialize;
use tar::Archive;
use uuid::Uuid;

use crate::providers::registry;

pub const PENDING_DATA_PATH: &str = "mbdump/pending_data";
pub const REPLICATION_SEQUENCE_PATH: &str = "REPLICATION_SEQUENCE";
pub const SCHEMA_SEQUENCE_PATH: &str = "SCHEMA_SEQUENCE";
pub const TIMESTAMP_PATH: &str = "TIMESTAMP";
pub const URL_TABLE: &str = "musicbrainz.url";

#[derive(Debug)]
pub enum UrlOp {
    Insert {
        id: i64,
        gid: Uuid,
        new_url: String,
    },
    Update {
        id: i64,
        gid: Uuid,
        old_url: String,
        new_url: String,
    },
    Delete {
        id: i64,
        gid: Uuid,
        old_url: String,
    },
}

/// Parsed contents of a replication packet.
pub struct PacketInfo {
    pub replication_sequence: i64,
    pub schema_sequence: i64,
    /// Content of the `TIMESTAMP` file, trimmed. Present in all modern packets.
    pub timestamp: Option<String>,
    pub ops: Vec<UrlOp>,
}

#[derive(Deserialize)]
struct UrlRow {
    id: i64,
    gid: Option<String>,
    url: Option<String>,
}

/// Stream a replication packet tarball and return a [`PacketInfo`]. Stops
/// reading the archive as soon as all required entries have been seen.
pub fn stream_packet<R: Read>(input: R) -> Result<PacketInfo> {
    let mut archive = Archive::new(input);

    let mut pkt_replication: Option<i64> = None;
    let mut pkt_schema: Option<i64> = None;
    let mut pkt_timestamp: Option<String> = None;
    let mut ops: Option<Vec<UrlOp>> = None;

    for entry in archive.entries().context("reading tar entries")? {
        let mut entry = entry.context("reading tar entry")?;
        let path_str: String = {
            let path = entry.path().context("decoding entry path")?;
            match path.to_str() {
                Some(s) => s.to_owned(),
                None => continue,
            }
        };

        match path_str.as_str() {
            REPLICATION_SEQUENCE_PATH => {
                let n = read_seq(&mut entry).context("reading REPLICATION_SEQUENCE")?;
                pkt_replication = Some(n);
            }
            SCHEMA_SEQUENCE_PATH => {
                let n = read_seq(&mut entry).context("reading SCHEMA_SEQUENCE")?;
                pkt_schema = Some(n);
            }
            TIMESTAMP_PATH if pkt_timestamp.is_none() => {
                let mut s = String::new();
                entry.read_to_string(&mut s).context("reading TIMESTAMP")?;
                pkt_timestamp = Some(s.trim().to_owned());
            }
            PENDING_DATA_PATH if ops.is_none() => {
                ops = Some(parse_url_ops(entry)?);
            }
            _ => {}
        }

        if pkt_replication.is_some()
            && pkt_schema.is_some()
            && pkt_timestamp.is_some()
            && ops.is_some()
        {
            break;
        }
    }

    let pkt_replication = pkt_replication
        .with_context(|| format!("{REPLICATION_SEQUENCE_PATH} not found in packet"))?;
    let pkt_schema =
        pkt_schema.with_context(|| format!("{SCHEMA_SEQUENCE_PATH} not found in packet"))?;
    let ops = ops.with_context(|| format!("{PENDING_DATA_PATH} not found in packet"))?;

    Ok(PacketInfo {
        replication_sequence: pkt_replication,
        schema_sequence: pkt_schema,
        timestamp: pkt_timestamp,
        ops,
    })
}

/// Parse `mbdump/pending_data`, returning only `musicbrainz.url` ops.
pub fn parse_url_ops<R: Read>(reader: R) -> Result<Vec<UrlOp>> {
    let buf = BufReader::with_capacity(1 << 20, reader);
    let mut ops = Vec::new();

    for (line_no, line) in buf.lines().enumerate() {
        let line = line.with_context(|| format!("reading pending_data line {line_no}"))?;
        let mut fields = line.splitn(8, '\t');

        let _seqnum = fields.next();
        let table = match fields.next() {
            Some(t) => t,
            None => continue,
        };
        if table != URL_TABLE {
            continue;
        }

        let op = fields.next().unwrap_or("");
        let _txn_id = fields.next();
        let old_json = fields.next().unwrap_or(r"\N");
        let new_json = fields.next().unwrap_or(r"\N");

        let op = match op {
            "i" => {
                let row: UrlRow = parse_json_col(new_json, line_no, "new_json")?;
                let gid = parse_gid(row.gid, line_no, row.id)?;
                let new_url = row.url.with_context(|| {
                    format!("line {line_no}: insert has null url for id={}", row.id)
                })?;
                UrlOp::Insert {
                    id: row.id,
                    gid,
                    new_url,
                }
            }
            "u" => {
                let old: UrlRow = parse_json_col(old_json, line_no, "old_json")?;
                let new: UrlRow = parse_json_col(new_json, line_no, "new_json")?;
                // gid is immutable once assigned; either row carries the same value.
                let gid = parse_gid(new.gid.or(old.gid), line_no, new.id)?;
                let old_url = old.url.with_context(|| {
                    format!(
                        "line {line_no}: update old_json has null url for id={}",
                        old.id
                    )
                })?;
                let new_url = new.url.with_context(|| {
                    format!(
                        "line {line_no}: update new_json has null url for id={}",
                        new.id
                    )
                })?;
                UrlOp::Update {
                    id: new.id,
                    gid,
                    old_url,
                    new_url,
                }
            }
            "d" => {
                let row: UrlRow = parse_json_col(old_json, line_no, "old_json")?;
                let gid = parse_gid(row.gid, line_no, row.id)?;
                let old_url = row.url.with_context(|| {
                    format!("line {line_no}: delete has null url for id={}", row.id)
                })?;
                UrlOp::Delete {
                    id: row.id,
                    gid,
                    old_url,
                }
            }
            other => bail!("line {line_no}: unknown op {other:?}"),
        };
        ops.push(op);
    }

    Ok(ops)
}

/// Execute URL ops in a single transaction and update `mb_state`.
/// `reverse=true` inverts each op for undo: inserts become deletes, deletes
/// become inserts, and updates restore the old URL.
pub fn exec_ops(
    conn: &mut Connection,
    ops: &[UrlOp],
    reverse: bool,
    new_replication: i64,
    new_schema: i64,
) -> Result<()> {
    let tx = conn.transaction().context("beginning transaction")?;

    {
        let mut upsert =
            tx.prepare("INSERT OR REPLACE INTO mb_url(id, gid, url_norm) VALUES (?1, ?2, ?3)")?;
        let mut delete = tx.prepare("DELETE FROM mb_url WHERE id = ?1")?;

        for op in ops {
            match (op, reverse) {
                (UrlOp::Insert { id, gid, new_url }, false) => {
                    upsert.execute(params![id, gid, registry::normalize(new_url)])?;
                }
                (UrlOp::Insert { id, .. }, true) => {
                    delete.execute(params![id])?;
                }
                (
                    UrlOp::Update {
                        id, gid, new_url, ..
                    },
                    false,
                ) => {
                    upsert.execute(params![id, gid, registry::normalize(new_url)])?;
                }
                (
                    UrlOp::Update {
                        id, gid, old_url, ..
                    },
                    true,
                ) => {
                    upsert.execute(params![id, gid, registry::normalize(old_url)])?;
                }
                (UrlOp::Delete { id, .. }, false) => {
                    delete.execute(params![id])?;
                }
                (UrlOp::Delete { id, gid, old_url }, true) => {
                    upsert.execute(params![id, gid, registry::normalize(old_url)])?;
                }
            }
        }
    }

    tx.execute(
        "INSERT OR REPLACE INTO mb_state(id, replication_sequence, schema_sequence) \
         VALUES (1, ?1, ?2)",
        params![new_replication, new_schema],
    )
    .context("updating mb_state")?;

    tx.commit().context("committing transaction")?;
    Ok(())
}

pub fn count_ops(ops: &[UrlOp]) -> (u64, u64, u64) {
    ops.iter().fold((0, 0, 0), |(i, u, d), op| match op {
        UrlOp::Insert { .. } => (i + 1, u, d),
        UrlOp::Update { .. } => (i, u + 1, d),
        UrlOp::Delete { .. } => (i, u, d + 1),
    })
}

pub fn read_seq<R: Read>(reader: &mut R) -> Result<i64> {
    let mut s = String::new();
    reader
        .read_to_string(&mut s)
        .context("reading sequence file")?;
    let trimmed = s.trim();
    trimmed
        .parse::<i64>()
        .with_context(|| format!("parsing {trimmed:?} as i64"))
}

fn parse_json_col<T: serde::de::DeserializeOwned>(
    col: &str,
    line_no: usize,
    col_name: &str,
) -> Result<T> {
    if col == r"\N" {
        bail!("line {line_no}: expected JSON in {col_name} but got \\N");
    }
    serde_json::from_str(col).with_context(|| format!("line {line_no}: parsing {col_name} JSON"))
}

fn parse_gid(gid: Option<String>, line_no: usize, id: i64) -> Result<Uuid> {
    let gid = gid.with_context(|| format!("line {line_no}: null gid for id={id}"))?;
    Uuid::parse_str(&gid)
        .with_context(|| format!("line {line_no}: parsing gid {gid:?} for id={id}"))
}
