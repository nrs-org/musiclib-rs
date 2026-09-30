use std::collections::HashMap;
use std::sync::Arc;

use crate::musicdb::MusicDb;
use crate::providers::FetchProvider;
use crate::providers::types::EntryFetchOptionsPool;
use serde::Deserialize;
use tracing::{info, warn};

use super::flush::{canonicalize_pair, flush};
use super::importer::import;
use super::state::{Pair, State};

/// Identifies one anchor within the dedup config: `(group_index, anchor_key)`.
/// Two pairs carrying different `AnchorId`s may never share an entry.
pub type AnchorId = (usize, String);

/// Declared barriers: sets of entities that must never be merged into one entry.
/// Empty by default → the whole mechanism is a no-op.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct DedupConfig {
    #[serde(default)]
    pub groups: Vec<BarrierGroup>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BarrierGroup {
    /// Human label, used only in logs.
    #[serde(default)]
    pub name: String,
    pub anchors: HashMap<String, Anchor>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Anchor {
    /// Pairs that identify this entity (`source:identifier`, split on first `:`).
    /// All listed pairs are assigned to this anchor: they are positive members
    /// (force-merged together) and negative constraints (kept apart from sibling
    /// anchors in the same group). No `claim` distinction — every pair listed
    /// here is treated identically.
    #[serde(default)]
    pub members: Vec<String>,
}

/// Split a `source:identifier` config string on the first colon. The identifier
/// keeps every later colon, so URLs survive (`unknown_url:http://...`).
fn parse_pair(s: &str) -> Option<Pair> {
    s.split_once(':')
        .map(|(a, b)| (a.to_string(), b.to_string()))
}

impl DedupConfig {
    /// Compile the declarations into a barrier map `canonical pair -> AnchorId`,
    /// plus the per-group display names. Every config pair is canonicalized
    /// through the providers so its key matches the canonical pairs flush uses.
    pub async fn compile(
        &self,
        providers: &[Arc<dyn FetchProvider>],
    ) -> (HashMap<Pair, AnchorId>, Vec<String>) {
        let mut barrier: HashMap<Pair, AnchorId> = HashMap::new();
        let mut group_names: Vec<String> = Vec::new();
        for (gi, group) in self.groups.iter().enumerate() {
            group_names.push(if group.name.is_empty() {
                format!("group{gi}")
            } else {
                group.name.clone()
            });
            for (key, anchor) in &group.anchors {
                let id = (gi, key.clone());
                for s in &anchor.members {
                    let Some(p) = parse_pair(s) else {
                        warn!("dedup: invalid pair string {s:?} (need 'source:identifier')");
                        continue;
                    };
                    let canon = canonicalize_pair(providers, &p).await;
                    if let Some(prev) = barrier.insert(canon.clone(), id.clone())
                        && prev != id
                    {
                        warn!(
                            "dedup: {}:{} claimed by two anchors ({prev:?} and {id:?}); last wins",
                            canon.0, canon.1
                        );
                    }
                }
            }
        }
        (barrier, group_names)
    }
}

/// One barrier config file with its declarations compiled to a barrier map and
/// its on-disk identity resolved.
struct CompiledFile {
    /// Absolute path — the stable key used in `dedup_state` / `entry_dedup`.
    abs_path: String,
    /// Current file mtime (ns since epoch).
    mtime_ns: i64,
    /// Canonical pair -> anchor id for this file's declarations.
    barrier: HashMap<Pair, AnchorId>,
}

/// Resolve a config path to its absolute form (stable DB key) and current mtime.
fn file_identity(path: &str) -> anyhow::Result<(String, i64)> {
    let abs = std::fs::canonicalize(path)?.to_string_lossy().into_owned();
    let mtime_ns = std::fs::metadata(path)?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos() as i64;
    Ok((abs, mtime_ns))
}

/// Entries currently holding any of a barrier's anchor pairs.
async fn entries_of_barrier(
    db: &MusicDb,
    barrier: &HashMap<Pair, AnchorId>,
) -> anyhow::Result<Vec<i64>> {
    let mut ids: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for pair in barrier.keys() {
        if let Some(id) = db.find_entry_id_by_pair(&pair.0, &pair.1).await? {
            ids.insert(id);
        }
    }
    Ok(ids.into_iter().collect())
}

/// True iff the DB respects this barrier: no single entry holds pairs from two
/// different anchors. A DB-only check — no fetching. Under the no-corruption
/// assumption, "respected" for an unchanged config means we're at the fixed
/// point and may skip re-importing.
async fn barrier_respected(
    db: &MusicDb,
    barrier: &HashMap<Pair, AnchorId>,
) -> anyhow::Result<bool> {
    let mut by_entry: HashMap<i64, std::collections::HashSet<AnchorId>> = HashMap::new();
    for (pair, anchor) in barrier {
        if let Some(id) = db.find_entry_id_by_pair(&pair.0, &pair.1).await? {
            by_entry.entry(id).or_default().insert(anchor.clone());
        }
    }
    Ok(by_entry.values().all(|anchors| anchors.len() <= 1))
}

/// Merge several files' declarations into one config for flush. Flush always
/// applies *every* barrier (an already-satisfied one is a no-op), so the scope
/// can be narrow while the constraints stay global.
pub fn merge_configs(configs: &[(String, DedupConfig)]) -> DedupConfig {
    DedupConfig {
        groups: configs
            .iter()
            .flat_map(|(_, c)| c.groups.iter().cloned())
            .collect(),
    }
}

/// Refresh the `entry_dedup` tags so each config points at the entries that now
/// hold its anchors. Call after any flush that used these configs.
pub async fn reconcile_tags(
    providers: &[Arc<dyn FetchProvider>],
    db: &MusicDb,
    configs: &[(String, DedupConfig)],
) -> anyhow::Result<()> {
    for (path, config) in configs {
        let Ok((abs_path, _)) = file_identity(path) else {
            continue;
        };
        let (barrier, _) = config.compile(providers).await;
        let ids = entries_of_barrier(db, &barrier).await?;
        db.set_config_entries(&abs_path, &ids).await?;
    }
    Ok(())
}

/// Reimport-based dedup over one or more barrier config files. Re-drives the
/// importer over the affected existing entities (shallow — each entity's own
/// sources only, no children), then flushes, which re-applies the merged barrier
/// (split/merge) against the rebuilt `is_rel` graph. This rebuilds the
/// same-entity edges that aren't persisted, so a changed config takes effect
/// without ingesting new URLs. Re-fetches are served from the HTTP cache where
/// possible.
///
/// Per file, re-import is triggered when **either** gate fires:
/// - **mtime changed** (or first apply): the config may have *relaxed* a
///   barrier, which the validity check below cannot see — so we re-import the
///   file's old affected entries (from the `entry_dedup` tags) plus its current
///   anchors' entries.
/// - **validity fails**: an unchanged config whose anchors share an entry.
///
/// Fully-removed config files (tags exist, path no longer passed) are treated as
/// a relaxation of their whole barrier: their tagged entries are re-merged and
/// their bookkeeping rows dropped.
///
/// When nothing needs re-importing, this only refreshes tags/mtimes — no fetch.
pub async fn dedup_db(
    providers: Arc<Vec<Arc<dyn FetchProvider>>>,
    db: &MusicDb,
    configs: &[(String, DedupConfig)],
) -> anyhow::Result<()> {
    // Compile each file: absolute path, mtime, barrier map.
    let mut files: Vec<CompiledFile> = Vec::with_capacity(configs.len());
    for (path, config) in configs {
        let (abs_path, mtime_ns) = file_identity(path)?;
        let (barrier, _names) = config.compile(providers.as_slice()).await;
        files.push(CompiledFile {
            abs_path,
            mtime_ns,
            barrier,
        });
    }

    // Decide the re-import scope (set of entry ids).
    let mut scope: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for f in &files {
        let changed = db.get_dedup_mtime(&f.abs_path).await? != Some(f.mtime_ns);
        let needs = if changed {
            // Relaxation/tightening: cover what the OLD config touched (tags) and
            // what the NEW anchors point at.
            for id in db.entries_for_config(&f.abs_path).await? {
                scope.insert(id);
            }
            true
        } else {
            !barrier_respected(db, &f.barrier).await?
        };
        if needs {
            for id in entries_of_barrier(db, &f.barrier).await? {
                scope.insert(id);
            }
        }
    }

    // Config files removed since last run: re-merge their old tagged entries.
    let current: std::collections::HashSet<&str> =
        files.iter().map(|f| f.abs_path.as_str()).collect();
    let mut removed: Vec<String> = Vec::new();
    for path in db.tagged_config_paths().await? {
        if !current.contains(path.as_str()) {
            for id in db.entries_for_config(&path).await? {
                scope.insert(id);
            }
            removed.push(path);
        }
    }

    // Re-import the scoped entries (shallow) and flush the merged barrier.
    let ids: Vec<i64> = scope.into_iter().collect();
    let seeds = db.pairs_by_entry_ids(&ids).await?;
    if seeds.is_empty() {
        info!("dedup: all barrier configs satisfied — nothing to fetch");
    } else {
        info!(
            "dedup: re-importing {} pair(s) across {} entr{}",
            seeds.len(),
            ids.len(),
            if ids.len() == 1 { "y" } else { "ies" },
        );
        // Shallow: fetch each seed entity and follow its is_rel sources, but
        // recurse into no children (root = the empty no-rules sentinel).
        let pool = Arc::new(EntryFetchOptionsPool::default());
        let root = EntryFetchOptionsPool::DEFAULT_ID;
        let state = Arc::new(State::new());
        let subs = seeds.into_iter().map(|pair| {
            import(
                Arc::clone(&state),
                Arc::clone(&providers),
                Arc::clone(&pool),
                pair,
                root,
            )
        });
        crate::http::Activity::global()
            .track(futures::future::join_all(subs))
            .await;
        let merged = merge_configs(configs);
        flush(state, providers.as_slice(), db, &merged, None, true).await?;
    }

    // Persist mtimes and refresh tags for present files; drop removed files.
    for f in &files {
        db.set_dedup_mtime(&f.abs_path, f.mtime_ns).await?;
        let ids = entries_of_barrier(db, &f.barrier).await?;
        db.set_config_entries(&f.abs_path, &ids).await?;
    }
    for path in &removed {
        db.set_config_entries(path, &[]).await?;
        db.delete_dedup_mtime(path).await?;
    }

    Ok(())
}
