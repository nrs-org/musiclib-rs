//! One-shot repair for `docs/plan-multi-work-recordings.md` (part A5).
//!
//! The cover-original import used to link a cover to the first 20 untagged
//! performances of each work it performs, and did so even for recordings that
//! perform several works (karaoke streams, medleys). This re-fetches every
//! MusicBrainz recording that has original-recording children (the HTTP
//! cache and the MB mirror make this cheap), and rewrites the DB to what the
//! current importer would produce:
//!
//! 1. the recording's songs and song count (`track_extra`, `num_tracks`);
//! 2. its original-recording `entry_child` rows: one anchor per work, none
//!    for a multi-song recording (an anchor not yet in the DB becomes a stub);
//! 3. the provider-asserted cover/arrangement/remix `entry_relation` rows of
//!    its entry, to match, and none at all for an entry whose pairs perform
//!    several songs between them (a stream video merged with per-song
//!    recordings);
//! 4. GC of original stubs nothing references any more;
//! 5. deletion of heuristic/Jev edition rows that break softmatch's
//!    `edition_guard`;
//! 6. the `entry_relation.extra` rows stored as a JSON list instead of an
//!    object.
//!
//! Backs the DB up with `VACUUM INTO` first. `--dry-run` reports without
//! writing.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context as _;
use clap::Parser;
use futures::StreamExt as _;
use musiclib_rs::{
    app_dirs,
    http::HttpClientConfig,
    musicdb::{MusicDb, merge_songs, songs_from_track_extra},
    pipeline::{progress::HttpActivityClient, softmatch::edition_guard_violations},
    providers::{
        FetchProvider,
        registry::{RegistryConfig, build_providers},
        types::{EntryFetchOptionsPool, EntrySpecificData, child_next},
    },
};
use rusqlite::{Connection, OptionalExtension as _, Transaction, params};
use tracing::{info, warn};

const MB: &str = "musicbrainz";
const RECORDING_LIKE: &str = "%musicbrainz.org/recording/%";

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Repair multi-song recordings and cover-original fan-out"
)]
struct Args {
    /// Music library database. Defaults to <data_dir>/musiclib.db.
    #[arg(long)]
    db: Option<String>,
    /// Provider credentials config. Defaults to <config_dir>/providers.yaml.
    #[arg(long)]
    registry_config: Option<String>,
    /// HTTP client config. Defaults to <config_dir>/http.yaml.
    #[arg(long)]
    http_config: Option<String>,
    /// Report what would change, write nothing.
    #[arg(long)]
    dry_run: bool,
    /// Concurrent recording fetches.
    #[arg(long, default_value_t = 16)]
    concurrency: usize,
}

async fn load_config<T: Default + serde::de::DeserializeOwned>(
    explicit: Option<&str>,
    default: &Path,
) -> anyhow::Result<T> {
    let path = match explicit {
        Some(p) => PathBuf::from(p),
        None if default.exists() => default.to_owned(),
        None => return Ok(T::default()),
    };
    let text = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(serde_yaml_ng::from_str(&text)?)
}

/// What the current importer says about one recording.
struct Refetched {
    songs: Vec<Vec<String>>,
    /// `(original recording URL, relation kind, listing name)`.
    originals: Vec<(String, String, Option<String>)>,
}

async fn refetch(provider: Arc<dyn FetchProvider>, url: String) -> anyhow::Result<Refetched> {
    let pool = Arc::new(EntryFetchOptionsPool::default());
    let result = provider
        .fetch_entry(MB, &url, pool, EntryFetchOptionsPool::DEFAULT_ID)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let songs = match &result.specific_data {
        EntrySpecificData::Track { songs, .. } => songs.clone(),
        _ => anyhow::bail!("{url} is not a track"),
    };
    let mut originals = Vec::new();
    for source in &result.children {
        let mut cursor = source.owned_cursor();
        while let Some((child, _)) = child_next(&mut cursor)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
        {
            let Some(kind) = &child.original_relation_kind else {
                continue;
            };
            for id in child.sources.get(MB).into_iter().flatten() {
                originals.push((id.clone(), kind.to_string(), child.name.clone()));
            }
        }
    }
    Ok(Refetched { songs, originals })
}

fn entry_of(tx: &Transaction, identifier: &str) -> rusqlite::Result<Option<i64>> {
    tx.query_row(
        "SELECT entry_id FROM entry_source WHERE source = ?1 AND identifier = ?2",
        params![MB, identifier],
        |r| r.get(0),
    )
    .optional()
}

/// The entry for an original recording, creating a stub (as `flush` would
/// for a listed-but-unfetched child) when the DB doesn't have it yet.
fn ensure_stub(tx: &Transaction, identifier: &str, name: Option<&str>) -> rusqlite::Result<i64> {
    if let Some(id) = entry_of(tx, identifier)? {
        return Ok(id);
    }
    tx.execute("INSERT INTO entry (entry_type) VALUES ('track')", [])?;
    let id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO entry_source (source, identifier, entry_id) VALUES (?1, ?2, ?3)",
        params![MB, identifier, id],
    )?;
    if let Some(name) = name {
        tx.execute(
            "INSERT INTO entry_alias (source, identifier, name, locale, extra, \"primary\") \
             VALUES (?1, ?2, ?3, NULL, 'null', 0)",
            params![MB, identifier, name],
        )?;
    }
    Ok(id)
}

/// `(derived_entry, source_entry)` objects in a relation's `extra`, which is
/// normally one object but can be a list (see `fix_list_extras`).
fn derived_sides(extra: Option<&str>) -> Vec<(i64, i64)> {
    let Some(value) = extra.and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
    else {
        return vec![];
    };
    let objects = match value {
        serde_json::Value::Array(items) => items,
        other => vec![other],
    };
    objects
        .iter()
        .filter_map(|o| {
            Some((
                o.get("derived_entry")?.as_i64()?,
                o.get("source_entry")?.as_i64()?,
            ))
        })
        .collect()
}

fn confidence(kind: &str) -> f64 {
    // Mirrors `flush::confidence_for_original_relation`.
    if kind == "cover" { 0.85 } else { 0.9 }
}

#[derive(Default, Debug)]
struct Report {
    fetch_failures: usize,
    multi_song_entries: usize,
    child_edges_removed: usize,
    child_edges_added: usize,
    stubs_created: usize,
    relations_removed: usize,
    relations_added: usize,
    entries_gc: usize,
    guard_violations: usize,
    list_extras_fixed: usize,
}

fn rewrite(
    tx: &Transaction,
    fetched: &HashMap<String, Refetched>,
    failed: &HashSet<String>,
    report: &mut Report,
) -> anyhow::Result<()> {
    let stubs_before: i64 = tx.query_row("SELECT COUNT(*) FROM entry", [], |r| r.get(0))?;
    let mut unlinked: BTreeSet<String> = BTreeSet::new();
    // Desired provider relations per derived entry: (original entry, kind).
    let mut desired: HashMap<i64, HashSet<(i64, String)>> = HashMap::new();
    let mut skip_entries: HashSet<i64> = HashSet::new();

    for url in failed {
        if let Some(entry) = entry_of(tx, url)? {
            skip_entries.insert(entry);
        }
    }

    for (url, refetched) in fetched {
        let Some(entry) = entry_of(tx, url)? else {
            continue;
        };
        let (num_tracks, track_extra) = if refetched.songs.is_empty() {
            (None, None)
        } else {
            (
                Some(refetched.songs.len() as i64),
                Some(serde_json::json!({ "songs": refetched.songs }).to_string()),
            )
        };
        tx.execute(
            "UPDATE entry_source SET num_tracks = ?1, track_extra = ?2 \
             WHERE source = ?3 AND identifier = ?4",
            params![num_tracks, track_extra, MB, url],
        )?;

        let old: HashSet<String> = tx
            .prepare_cached(
                "SELECT child_identifier FROM entry_child \
                 WHERE parent_source = ?1 AND parent_identifier = ?2 \
                   AND child_source = ?1 AND child_identifier LIKE ?3",
            )?
            .query_map(params![MB, url, RECORDING_LIKE], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let new: HashMap<&str, Option<&str>> = refetched
            .originals
            .iter()
            .map(|(id, _, name)| (id.as_str(), name.as_deref()))
            .collect();
        for child in old.iter().filter(|c| !new.contains_key(c.as_str())) {
            tx.execute(
                "DELETE FROM entry_child WHERE parent_source = ?1 AND parent_identifier = ?2 \
                 AND child_source = ?1 AND child_identifier = ?3",
                params![MB, url, child],
            )?;
            report.child_edges_removed += 1;
            unlinked.insert(child.clone());
        }
        for (child, name) in new.iter().filter(|(c, _)| !old.contains(**c)) {
            ensure_stub(tx, child, *name)?;
            tx.execute(
                "INSERT OR IGNORE INTO entry_child \
                 (parent_source, parent_identifier, child_source, child_identifier) \
                 VALUES (?1, ?2, ?1, ?3)",
                params![MB, url, child],
            )?;
            report.child_edges_added += 1;
        }

        let wanted = desired.entry(entry).or_default();
        for (child, kind, _) in &refetched.originals {
            if let Some(original) = entry_of(tx, child)?
                && original != entry
            {
                wanted.insert((original, kind.clone()));
            }
        }
    }
    let stubs_after: i64 = tx.query_row("SELECT COUNT(*) FROM entry", [], |r| r.get(0))?;
    report.stubs_created = (stubs_after - stubs_before) as usize;

    for (&derived, wanted) in &mut desired {
        if skip_entries.contains(&derived) {
            continue;
        }
        let groups: Vec<Vec<String>> = tx
            .prepare_cached(
                "SELECT track_extra FROM entry_source \
                 WHERE entry_id = ?1 AND track_extra IS NOT NULL",
            )?
            .query_map(params![derived], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .flat_map(|raw| songs_from_track_extra(Some(raw)))
            .collect();
        if merge_songs(groups).len() > 1 {
            report.multi_song_entries += 1;
            wanted.clear();
        }
        let rows: Vec<(i64, i64, String, Option<String>)> = tx
            .prepare_cached(
                "SELECT entry_a, entry_b, kind, extra FROM entry_relation \
                 WHERE origin = 'provider' AND kind IN ('cover', 'arrangement', 'remix') \
                   AND (entry_a = ?1 OR entry_b = ?1)",
            )?
            .query_map(params![derived], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut existing: HashSet<(i64, String)> = HashSet::new();
        for (a, b, kind, extra) in rows {
            if !derived_sides(extra.as_deref())
                .iter()
                .any(|&(d, _)| d == derived)
            {
                continue; // `derived` is the original side of this row
            }
            let original = if a == derived { b } else { a };
            if wanted.contains(&(original, kind.clone())) {
                existing.insert((original, kind));
            } else {
                tx.execute(
                    "DELETE FROM entry_relation WHERE entry_a = ?1 AND entry_b = ?2 AND kind = ?3",
                    params![a, b, kind],
                )?;
                report.relations_removed += 1;
            }
        }
        for (original, kind) in wanted.difference(&existing) {
            let extra = serde_json::json!({ "derived_entry": derived, "source_entry": original });
            let added = tx.execute(
                "INSERT INTO entry_relation (entry_a, entry_b, kind, confidence, origin, enabled, extra) \
                 VALUES (?1, ?2, ?3, ?4, 'provider', 1, ?5) \
                 ON CONFLICT (entry_a, entry_b, kind) DO NOTHING",
                params![
                    derived.min(*original),
                    derived.max(*original),
                    kind,
                    confidence(kind),
                    extra.to_string()
                ],
            )?;
            report.relations_added += added;
        }
    }

    for child in &unlinked {
        if let Some(entry) = entry_of(tx, child)?
            && gc_if_unreferenced(tx, entry)?
        {
            report.entries_gc += 1;
        }
    }
    Ok(())
}

/// Delete `entry` if it is an MB-only recording that only the cover-original
/// import ever pointed at: no parent lists it, no provider/manual relation or
/// manual identity judgment touches it, and no dedup barrier names it.
fn gc_if_unreferenced(tx: &Transaction, entry: i64) -> rusqlite::Result<bool> {
    let pairs: Vec<(String, String)> = tx
        .prepare_cached("SELECT source, identifier FROM entry_source WHERE entry_id = ?1")?
        .query_map(params![entry], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mb_recordings_only = pairs
        .iter()
        .all(|(s, i)| s == MB && i.contains("musicbrainz.org/recording/"));
    if pairs.is_empty() || !mb_recordings_only {
        return Ok(false);
    }
    let referenced: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM entry_child c JOIN entry_source s \
                          ON c.child_source = s.source AND c.child_identifier = s.identifier \
                        WHERE s.entry_id = ?1) \
             OR EXISTS (SELECT 1 FROM entry_relation \
                        WHERE (entry_a = ?1 OR entry_b = ?1) \
                          AND origin NOT IN ('heuristic', 'jev')) \
             OR EXISTS (SELECT 1 FROM dedup_feedback \
                        WHERE (entry_a = ?1 OR entry_b = ?1) \
                          AND origin NOT IN ('heuristic', 'jev')) \
             OR EXISTS (SELECT 1 FROM entry_dedup WHERE entry_id = ?1)",
        params![entry],
        |r| r.get(0),
    )?;
    if referenced {
        return Ok(false);
    }
    for (source, identifier) in &pairs {
        // `entry_alias_fts` is external-content: tell it about each removal.
        tx.execute(
            "INSERT INTO entry_alias_fts (entry_alias_fts, rowid, name) \
             SELECT 'delete', id, name FROM entry_alias WHERE source = ?1 AND identifier = ?2",
            params![source, identifier],
        )?;
        for sql in [
            "DELETE FROM entry_alias WHERE source = ?1 AND identifier = ?2",
            "DELETE FROM contribution WHERE source = ?1 AND identifier = ?2",
            "DELETE FROM entry_child WHERE parent_source = ?1 AND parent_identifier = ?2",
            "DELETE FROM entry_source WHERE source = ?1 AND identifier = ?2",
        ] {
            tx.execute(sql, params![source, identifier])?;
        }
    }
    for sql in [
        "DELETE FROM entry_relation WHERE entry_a = ?1 OR entry_b = ?1",
        "DELETE FROM dedup_feedback WHERE entry_a = ?1 OR entry_b = ?1",
        "DELETE FROM dedup_block_key WHERE entry_id = ?1",
        "DELETE FROM entry WHERE id = ?1",
    ] {
        tx.execute(sql, params![entry])?;
    }
    Ok(true)
}

/// Rewrite `extra` values stored as a JSON list of `{derived_entry,
/// source_entry}` objects into the single object naming the row's own
/// endpoints (else the first).
fn fix_list_extras(tx: &Transaction) -> rusqlite::Result<usize> {
    let rows: Vec<(i64, i64, String, String)> = tx
        .prepare("SELECT entry_a, entry_b, kind, extra FROM entry_relation WHERE extra LIKE '[%'")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut fixed = 0;
    for (a, b, kind, extra) in rows {
        let Ok(serde_json::Value::Array(items)) = serde_json::from_str(&extra) else {
            continue;
        };
        let endpoints = [a, b];
        let pick = items
            .iter()
            .find(|o| {
                ["derived_entry", "source_entry"].iter().all(|k| {
                    o.get(k)
                        .and_then(|v| v.as_i64())
                        .is_some_and(|id| endpoints.contains(&id))
                })
            })
            .or(items.first());
        if let Some(object) = pick {
            fixed += tx.execute(
                "UPDATE entry_relation SET extra = ?1 WHERE entry_a = ?2 AND entry_b = ?3 AND kind = ?4",
                params![object.to_string(), a, b, kind],
            )?;
        }
    }
    Ok(fixed)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("warn,repair_multi_work=info")
            }),
        )
        .init();

    let args = Args::parse();
    let config_dir = app_dirs::config_dir();
    let registry_config: RegistryConfig = load_config(
        args.registry_config.as_deref(),
        &config_dir.join("providers.yaml"),
    )
    .await?;
    let mut http_config: HttpClientConfig =
        load_config(args.http_config.as_deref(), &config_dir.join("http.yaml")).await?;
    http_config.coalescer_rules = musiclib_rs::providers::registry::coalesce_rules();
    let http = HttpActivityClient::new(http_config.build().await?);
    let providers = build_providers(&registry_config, http)?;

    let db_path = args
        .db
        .map(PathBuf::from)
        .unwrap_or_else(|| app_dirs::data_dir().join("musiclib.db"));
    let db_url = format!("sqlite://{}?mode=rw", db_path.display());
    // Schema sync: adds `entry_source.track_extra` on a DB from before it.
    drop(MusicDb::new(&db_url).await?);

    let mut provider = None;
    for p in &providers {
        if p.canonicalize(
            MB,
            "https://musicbrainz.org/recording/00000000-0000-0000-0000-000000000000",
        )
        .await
        .is_some()
        {
            provider = Some(Arc::clone(p));
            break;
        }
    }
    let provider = provider.context("no MusicBrainz provider configured")?;

    let mut conn = Connection::open(&db_path)?;
    conn.busy_timeout(std::time::Duration::from_secs(60))?;
    let parents: Vec<String> = conn
        .prepare(
            "SELECT DISTINCT parent_identifier FROM entry_child \
             WHERE parent_source = ?1 AND parent_identifier LIKE ?2 \
               AND child_source = ?1 AND child_identifier LIKE ?2",
        )?
        .query_map(params![MB, RECORDING_LIKE], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    info!(
        "Re-fetching {} recording(s) with original children",
        parents.len()
    );

    let mut report = Report::default();
    let mut fetched: HashMap<String, Refetched> = HashMap::new();
    let mut failed: HashSet<String> = HashSet::new();
    let results = musiclib_rs::http::Activity::global()
        .track(
            futures::stream::iter(parents.into_iter().map(|url| {
                let provider = Arc::clone(&provider);
                async move {
                    let result = refetch(provider, url.clone()).await;
                    (url, result)
                }
            }))
            .buffer_unordered(args.concurrency.max(1))
            .collect::<Vec<_>>(),
        )
        .await;
    for (url, result) in results {
        match result {
            Ok(r) => {
                fetched.insert(url, r);
            }
            Err(e) => {
                warn!("Keeping {url} as is, re-fetch failed: {e}");
                failed.insert(url);
            }
        }
    }
    report.fetch_failures = failed.len();

    if !args.dry_run {
        let backup = db_path.with_extension(format!(
            "db.pre-multi-work-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs()
        ));
        info!("Backing up to {}", backup.display());
        conn.execute("VACUUM INTO ?1", params![backup.display().to_string()])?;
    }

    let tx = conn.transaction()?;
    rewrite(&tx, &fetched, &failed, &mut report)?;
    report.list_extras_fixed = fix_list_extras(&tx)?;
    if args.dry_run {
        tx.rollback()?;
    } else {
        tx.commit()?;
    }

    // The guard reads song counts, so it runs on the rewritten DB (in a dry
    // run, on the current one).
    let violations = edition_guard_violations(&MusicDb::new(&db_url).await?).await?;
    report.guard_violations = violations.len();
    for (relation, why) in violations.iter().take(20) {
        info!(
            "Guard: {} {}-{} ({}): {why}",
            relation.kind, relation.entry_a, relation.entry_b, relation.origin
        );
    }
    if !args.dry_run {
        let tx = conn.transaction()?;
        for (relation, _) in &violations {
            tx.execute(
                "DELETE FROM entry_relation WHERE entry_a = ?1 AND entry_b = ?2 AND kind = ?3",
                params![relation.entry_a, relation.entry_b, relation.kind],
            )?;
        }
        tx.commit()?;
    }

    info!(
        "{}{report:#?}",
        if args.dry_run { "[dry run] " } else { "" }
    );
    Ok(())
}
