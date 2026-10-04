//! Per-pair facts for match scripts: the `musiclib-pair-facts/1` contract
//! (docs/plan-v15-runtime.md §2b).
//!
//! Scripts read these lazily through the `pair_facts_json(...)` Rhai host
//! function instead of `EntryInfo` growing a field per consumer. Facts are keyed
//! by `(source, identifier)`, so they stay valid when entries merge, and they
//! mirror the pair-centric DB rather than any one script's needs.
//!
//! Reads go through their own read-only SQLite connection: Rhai host functions
//! are synchronous, and the facts are plain lookups on indexed columns.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::Context as _;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};

pub const SCHEMA: &str = "musiclib-pair-facts/1";
/// Items listed per artist pair in `credited` (as in the training reference).
const CREDITED_CAP: i64 = 500;

pub struct PairFactsSource {
    conn: Mutex<Connection>,
    cache: Mutex<HashMap<(String, String), Value>>,
}

/// Integral numbers as JSON integers (the reference writes Python ints).
fn number(x: f64) -> Value {
    if x.fract() == 0.0 && x.abs() < 9.0e15 {
        json!(x as i64)
    } else {
        json!(x)
    }
}

impl PairFactsSource {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )
        .with_context(|| format!("opening {} read-only for pair facts", path.display()))?;
        Ok(Self {
            conn: Mutex::new(conn),
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// Facts for one pair (cached for the life of this source).
    pub fn facts(&self, source: &str, identifier: &str) -> anyhow::Result<Value> {
        let key = (source.to_owned(), identifier.to_owned());
        if let Some(v) = self.cache.lock().unwrap().get(&key) {
            return Ok(v.clone());
        }
        let v = build(&self.conn.lock().unwrap(), source, identifier)?;
        self.cache.lock().unwrap().insert(key, v.clone());
        Ok(v)
    }
}

fn entry_of(c: &Connection, s: &str, i: &str) -> rusqlite::Result<Option<i64>> {
    c.prepare_cached("SELECT entry_id FROM entry_source WHERE source = ?1 AND identifier = ?2")?
        .query_row(params![s, i], |r| r.get(0))
        .optional()
}

/// `(name, primary)` in the reference order: primary first, then alias id.
fn names(c: &Connection, s: &str, i: &str) -> rusqlite::Result<Vec<(String, bool)>> {
    c.prepare_cached(
        r#"SELECT name, "primary" FROM entry_alias WHERE source = ?1 AND identifier = ?2 ORDER BY "primary" DESC, id"#,
    )?
    .query_map(params![s, i], |r| Ok((r.get(0)?, r.get::<_, Option<bool>>(1)?.unwrap_or(false))))?
    .collect()
}

/// The pair's own first name; else the first name among its entry's pairs in
/// `(source, identifier)` order.
fn best_name(c: &Connection, s: &str, i: &str) -> rusqlite::Result<Option<String>> {
    if let Some((n, _)) = names(c, s, i)?.into_iter().next() {
        return Ok(Some(n));
    }
    let Some(e) = entry_of(c, s, i)? else {
        return Ok(None);
    };
    let pairs: Vec<(String, String)> = c
        .prepare_cached("SELECT source, identifier FROM entry_source WHERE entry_id = ?1 ORDER BY source, identifier")?
        .query_map(params![e], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (ps, pi) in pairs {
        if let Some((n, _)) = names(c, &ps, &pi)?.into_iter().next() {
            return Ok(Some(n));
        }
    }
    Ok(None)
}

/// A pair's first name in `names` order, as a correlated subquery over the
/// pair columns `{s}`/`{i}` of the enclosing query.
fn first_name_sql(s: &str, i: &str) -> String {
    format!(
        r#"(SELECT a.name FROM entry_alias a WHERE a.source = {s} AND a.identifier = {i}
            ORDER BY a."primary" DESC, a.id LIMIT 1)"#
    )
}

// Related pairs (artists, parents, children, credited items) are resolved to
// entry ids, types and names with joins in the listing query itself rather
// than a query per related pair: an artist lists up to CREDITED_CAP items.
fn build(c: &Connection, s: &str, i: &str) -> anyhow::Result<Value> {
    type SourceRow = (
        Option<i64>,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let row: Option<SourceRow> = c
        .prepare_cached(
            "SELECT es.entry_id, es.release_date, es.duration_ms, es.duration_ms_all, es.release_type, \
             es.primary_type, e.entry_type \
             FROM entry_source es LEFT JOIN entry e ON e.id = es.entry_id \
             WHERE es.source = ?1 AND es.identifier = ?2",
        )?
        .query_row(params![s, i], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
        })
        .optional()?;
    let (entry_id, release_date, duration, duration_all, release_type, primary_type, entry_type) =
        row.unwrap_or((None, None, None, None, None, None, None));

    let mut durations: Vec<f64> = Vec::new();
    if let Some(d) = duration.filter(|&d| d != 0) {
        durations.push(d as f64);
    }
    if let Some(Ok(Value::Array(all))) = duration_all.as_deref().map(serde_json::from_str::<Value>)
    {
        durations.extend(all.iter().filter_map(Value::as_f64));
    }
    durations.sort_by(f64::total_cmp);
    durations.dedup();

    type ContributionRow = (
        String,
        String,
        Option<String>,
        bool,
        Option<i64>,
        Option<String>,
    );
    let contributions: Vec<ContributionRow> = c
        .prepare_cached(&format!(
            "SELECT c.artist_source, c.artist_identifier, c.role, c.main_artist, es.entry_id, {} \
             FROM contribution c \
             LEFT JOIN entry_source es ON es.source = c.artist_source AND es.identifier = c.artist_identifier \
             WHERE c.source = ?1 AND c.identifier = ?2 ORDER BY c.id",
            first_name_sql("c.artist_source", "c.artist_identifier"),
        ))?
        .query_map(params![s, i], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get::<_, Option<bool>>(3)?.unwrap_or(false),
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let contributions: Vec<Value> = contributions
        .into_iter()
        .map(|(as_, ai, role, main, artist_entry, own_name)| {
            // An artist pair without names of its own falls back to its
            // entry's other pairs (rare: one query chain, only then).
            let artist_name = match own_name {
                Some(n) => Some(n),
                None => best_name(c, &as_, &ai)?,
            };
            Ok(json!({
                "artist": [as_, ai],
                "artist_entry_id": artist_entry,
                "artist_name": artist_name,
                "role": role,
                "main": main,
            }))
        })
        .collect::<rusqlite::Result<_>>()?;

    type ParentRow = (
        Option<i64>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<String>,
    );
    let parents: Vec<Value> = c
        .prepare_cached(&format!(
            "SELECT es.entry_id, e.entry_type, ch.disc_no, ch.track_no, {} FROM entry_child ch \
             LEFT JOIN entry_source es ON es.source = ch.parent_source AND es.identifier = ch.parent_identifier \
             LEFT JOIN entry e ON e.id = es.entry_id \
             WHERE ch.child_source = ?1 AND ch.child_identifier = ?2",
            first_name_sql("ch.parent_source", "ch.parent_identifier"),
        ))?
        .query_map(params![s, i], |r| {
            let (e, t, disc, track, name): ParentRow =
                (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?);
            Ok(json!({"entry_id": e, "entry_type": t, "disc": disc, "track": track, "name": name}))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let children: Vec<Value> = c
        .prepare_cached(&format!(
            "SELECT es.entry_id, e.entry_type, {} FROM entry_child ch \
             LEFT JOIN entry_source es ON es.source = ch.child_source AND es.identifier = ch.child_identifier \
             LEFT JOIN entry e ON e.id = es.entry_id \
             WHERE ch.parent_source = ?1 AND ch.parent_identifier = ?2",
            first_name_sql("ch.child_source", "ch.child_identifier"),
        ))?
        .query_map(params![s, i], |r| {
            let (e, t, name): (Option<i64>, Option<String>, Option<String>) = (r.get(0)?, r.get(1)?, r.get(2)?);
            Ok(json!({"entry_id": e, "entry_type": t, "name": name}))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let credited: Vec<(Option<i64>, Option<String>)> = c
        .prepare_cached(&format!(
            "SELECT es.entry_id, {} FROM contribution c \
             LEFT JOIN entry_source es ON es.source = c.source AND es.identifier = c.identifier \
             WHERE c.artist_source = ?1 AND c.artist_identifier = ?2 ORDER BY c.id LIMIT ?3",
            first_name_sql("c.source", "c.identifier"),
        ))?
        .query_map(params![s, i, CREDITED_CAP], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let (credited, credited_names): (Vec<_>, Vec<_>) = credited.into_iter().unzip();

    let names: Vec<Value> = names(c, s, i)?
        .into_iter()
        .map(|(n, p)| json!([n, p]))
        .collect();
    Ok(json!({
        "source": s,
        "identifier": i,
        "entry_id": entry_id,
        "entry_type": entry_type,
        "names": names,
        "durations": durations.into_iter().map(number).collect::<Vec<_>>(),
        "release_date": release_date,
        "release_type": release_type,
        "primary_type": primary_type,
        "contributions": contributions,
        "parents": parents,
        "children": children,
        "credited": credited,
        "credited_names": credited_names,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden check against the Python reference facts
    /// (`train/learned-matcher/export_parity.py`), when the gitignored
    /// snapshot and fixture are present.
    #[test]
    fn matches_reference_facts() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("data");
        let db = root.join("eval/live-2026-10-03.db");
        let fixture = root.join("learned-matcher/parity/v17/facts.jsonl");
        if !db.exists() || !fixture.exists() {
            eprintln!(
                "skipping: {} or {} missing",
                db.display(),
                fixture.display()
            );
            return;
        }
        let src = PairFactsSource::open(&db).unwrap();
        let mut n = 0;
        for line in std::fs::read_to_string(fixture).unwrap().lines() {
            let want: Value = serde_json::from_str(line).unwrap();
            let got = src
                .facts(
                    want["source"].as_str().unwrap(),
                    want["identifier"].as_str().unwrap(),
                )
                .unwrap();
            // Parents and children are unordered in the contract. Parent
            // names and `credited_names` are additions the reference predates.
            let mut g = got.clone();
            let mut w = want.clone();
            g.as_object_mut().unwrap().remove("credited_names");
            for p in g["parents"].as_array_mut().unwrap() {
                p.as_object_mut().unwrap().remove("name");
            }
            for k in ["parents", "children"] {
                for v in [&mut g, &mut w] {
                    v[k].as_array_mut().unwrap().sort_by_key(|x| x.to_string());
                }
            }
            assert_eq!(g, w, "pair {} {}", want["source"], want["identifier"]);
            n += 1;
        }
        eprintln!("{n} pairs match the reference facts");
    }
}
