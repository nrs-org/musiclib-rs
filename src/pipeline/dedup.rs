use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use crate::musicdb::{BarrierGroupRow, LINKS_COMPLETE, MusicDb};
use crate::providers::FetchProvider;
use crate::providers::types::EntryFetchOptionsPool;
use serde::Deserialize;
use tracing::{info, warn};

use super::flush::{canonicalize_pair, flush};
use super::importer::import;
use super::state::{LinkVia, Pair, State};

/// Identifies one anchor within the dedup config: `(group_index, anchor_key)`.
/// Two pairs carrying different `AnchorId`s may never share an entry.
pub type AnchorId = (usize, String);

/// Declared barriers: sets of entities that must never be merged into one entry.
/// Stored in the DB (`DedupConfig::load`); this shape is also the old YAML
/// file format, read only by `import_yaml_barriers`. Empty by default → the
/// whole mechanism is a no-op.
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

impl DedupConfig {
    /// The barriers stored in `db` (`barrier_group`).
    pub async fn load(db: &MusicDb) -> anyhow::Result<Self> {
        Ok(Self {
            groups: db
                .barrier_groups()
                .await?
                .into_iter()
                .map(BarrierGroup::from)
                .collect(),
        })
    }
}

impl From<BarrierGroupRow> for BarrierGroup {
    fn from(row: BarrierGroupRow) -> Self {
        Self {
            name: row.name,
            anchors: row
                .anchors
                .into_iter()
                .map(|(key, pairs)| {
                    let members = pairs.into_iter().map(|(s, i)| format!("{s}:{i}")).collect();
                    (key, Anchor { members })
                })
                .collect(),
        }
    }
}

/// Store the barriers of YAML files in the old `dedup_barriers/*.yaml`
/// format, one `barrier_group` per group, `origin` `yaml:<file name>`. A
/// group already imported from the same file under the same name is
/// skipped, so importing twice adds nothing. Pairs are canonicalized as
/// stored. Returns how many groups were added.
pub async fn import_yaml_barriers(
    providers: &[Arc<dyn FetchProvider>],
    db: &MusicDb,
    paths: &[&Path],
) -> anyhow::Result<usize> {
    let existing: HashSet<(String, String)> = db
        .barrier_groups()
        .await?
        .into_iter()
        .map(|g| (g.origin, g.name))
        .collect();
    let mut added = 0;
    for path in paths {
        let config: DedupConfig = serde_yaml_ng::from_str(&std::fs::read_to_string(path)?)?;
        let file = path.file_name().map_or_else(
            || path.display().to_string(),
            |f| f.to_string_lossy().into_owned(),
        );
        let origin = format!("yaml:{file}");
        for group in config.groups {
            if existing.contains(&(origin.clone(), group.name.clone())) {
                continue;
            }
            let mut anchors: BTreeMap<String, Vec<Pair>> = BTreeMap::new();
            for (key, anchor) in group.anchors {
                for s in &anchor.members {
                    let Some(pair) = parse_pair(s) else {
                        warn!(
                            "dedup: invalid pair string {s:?} in {file} (need 'source:identifier')"
                        );
                        continue;
                    };
                    anchors
                        .entry(key.clone())
                        .or_default()
                        .push(canonicalize_pair(providers, &pair).await);
                }
            }
            db.insert_barrier_group(&group.name, &origin, &anchors)
                .await?;
            added += 1;
        }
    }
    Ok(added)
}

/// Entries currently holding any of a barrier's anchor pairs.
pub async fn entries_of_barrier(
    db: &MusicDb,
    barrier: &HashMap<Pair, AnchorId>,
) -> anyhow::Result<Vec<i64>> {
    let mut ids: HashSet<i64> = HashSet::new();
    for pair in barrier.keys() {
        if let Some(id) = db.find_entry_id_by_pair(&pair.0, &pair.1).await? {
            ids.insert(id);
        }
    }
    let mut ids: Vec<i64> = ids.into_iter().collect();
    ids.sort_unstable();
    Ok(ids)
}

/// True iff the DB respects this barrier: no single entry holds pairs from two
/// different anchors. A DB-only check — no fetching.
async fn barrier_respected(
    db: &MusicDb,
    barrier: &HashMap<Pair, AnchorId>,
) -> anyhow::Result<bool> {
    let mut by_entry: HashMap<i64, HashSet<AnchorId>> = HashMap::new();
    for (pair, anchor) in barrier {
        if let Some(id) = db.find_entry_id_by_pair(&pair.0, &pair.1).await? {
            by_entry.entry(id).or_default().insert(anchor.clone());
        }
    }
    Ok(by_entry.values().all(|anchors| anchors.len() <= 1))
}

/// Re-apply every stored barrier the DB violates (two of its anchors' pairs
/// share an entry) by regrouping the entries it touches. A DB-only check
/// when none is violated. Barriers made in the player are applied as they
/// are made; this catches the rest (an imported YAML barrier, a restored
/// DB).
pub async fn enforce_barriers(
    providers: &Arc<Vec<Arc<dyn FetchProvider>>>,
    db: &MusicDb,
) -> anyhow::Result<()> {
    let config = DedupConfig::load(db).await?;
    let mut scope: HashSet<i64> = HashSet::new();
    for group in &config.groups {
        let one = DedupConfig {
            groups: vec![group.clone()],
        };
        let (barrier, _) = one.compile(providers).await;
        if !barrier_respected(db, &barrier).await? {
            info!("dedup: barrier '{}' is violated; regrouping", group.name);
            scope.extend(entries_of_barrier(db, &barrier).await?);
        }
    }
    if scope.is_empty() {
        info!("dedup: all barriers satisfied — nothing to regroup");
        return Ok(());
    }
    let mut ids: Vec<i64> = scope.into_iter().collect();
    ids.sort_unstable();
    regroup_entries(providers, db, &ids, &[]).await?;
    Ok(())
}

/// Regroup `entry_ids` under the stored barriers, writing the splits and
/// merges that follow: an unmerge made in the player, a barrier removed, a
/// violated one. With the DB's links complete (`LINKS_COMPLETE`) this
/// replays the entries' stored identity links through a flush, fetching
/// nothing. Otherwise it re-imports their pairs shallowly (each pair's own
/// sources, no children; served from the HTTP cache where possible), which
/// restates the links. Where an entry splits, the part holding a pair of
/// `keep` keeps its id. Returns the entries written or surviving.
pub async fn regroup_entries(
    providers: &Arc<Vec<Arc<dyn FetchProvider>>>,
    db: &MusicDb,
    entry_ids: &[i64],
    keep: &[Pair],
) -> anyhow::Result<HashSet<i64>> {
    let config = DedupConfig::load(db).await?;
    let pairs = db.pairs_by_entry_ids(entry_ids).await?;
    if pairs.is_empty() {
        return Ok(HashSet::new());
    }
    let state = Arc::new(State::new());
    for pair in keep {
        state.prefer_keeper(pair.clone());
    }
    if db.setting(LINKS_COMPLETE).await?.as_deref() == Some("1") {
        for row in db.links_among(&pairs).await? {
            let (a, b) = row.link;
            state.push_is_rel(a, b, row.asserted_by, LinkVia::from_stored(row.via));
        }
    } else {
        info!(
            "dedup: re-importing {} pair(s) across {} entr{}",
            pairs.len(),
            entry_ids.len(),
            if entry_ids.len() == 1 { "y" } else { "ies" },
        );
        // Shallow: fetch each pair and follow its is_rel sources, but
        // recurse into no children (root = the empty no-rules sentinel).
        let pool = Arc::new(EntryFetchOptionsPool::default());
        let root = EntryFetchOptionsPool::DEFAULT_ID;
        let subs = pairs.into_iter().map(|pair| {
            import(
                Arc::clone(&state),
                Arc::clone(providers),
                Arc::clone(&pool),
                pair,
                root,
            )
        });
        crate::http::Activity::global()
            .track(futures::future::join_all(subs))
            .await;
    }
    let run_id = db.start_import_run().await?;
    match flush(
        Arc::clone(&state),
        providers,
        db,
        &config,
        Some(run_id),
        true,
    )
    .await
    {
        Ok(touched) => {
            db.commit_import_run(run_id, &state.take_replacements())
                .await?;
            Ok(touched)
        }
        Err(e) => {
            if let Err(rollback_err) = db.rollback_import_run(run_id).await {
                warn!("failed to roll back regroup run {run_id}: {rollback_err}");
            }
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Pair {
        ("x".to_string(), s.to_string())
    }

    async fn entry_of(db: &MusicDb, pair: &Pair) -> i64 {
        db.find_entry_id_by_pair(&pair.0, &pair.1)
            .await
            .unwrap()
            .unwrap()
    }

    /// Recording `r`, a wrongly linked track `s` and the ISRC `i` that `s`
    /// brought, merged by stored links. A barrier with `s` and `i` on one
    /// side splits them off with no fetching, and deleting it merges them
    /// back through the same links. Both runs commit.
    #[tokio::test]
    async fn barrier_split_and_undo_replay_stored_links() {
        let db = MusicDb::new("sqlite::memory:").await.unwrap();
        let providers: Arc<Vec<Arc<dyn FetchProvider>>> = Arc::new(Vec::new());
        let (r, s, i) = (p("r"), p("s"), p("i"));
        let state = Arc::new(State::new());
        state.push_is_rel(r.clone(), s.clone(), r.clone(), LinkVia::Fetch);
        state.push_is_rel(s.clone(), i.clone(), s.clone(), LinkVia::Fetch);
        flush(state, &[], &db, &DedupConfig::default(), None, true)
            .await
            .unwrap();
        let entry = entry_of(&db, &r).await;
        assert_eq!(entry_of(&db, &s).await, entry);

        let anchors = BTreeMap::from([
            ("kept".to_string(), vec![r.clone()]),
            ("split".to_string(), vec![i.clone(), s.clone()]),
        ]);
        let id = db
            .insert_barrier_group("test", "ui", &anchors)
            .await
            .unwrap();
        regroup_entries(&providers, &db, &[entry], std::slice::from_ref(&r))
            .await
            .unwrap();
        assert_eq!(entry_of(&db, &r).await, entry);
        assert_ne!(entry_of(&db, &s).await, entry);
        assert_eq!(entry_of(&db, &i).await, entry_of(&db, &s).await);

        let removed = db.delete_barrier_group(id).await.unwrap().unwrap();
        assert_eq!(removed.anchors, anchors);
        let split_off = entry_of(&db, &s).await;
        regroup_entries(&providers, &db, &[entry, split_off], &[])
            .await
            .unwrap();
        assert_eq!(entry_of(&db, &s).await, entry_of(&db, &r).await);
        assert!(db.barrier_groups().await.unwrap().is_empty());
    }

    /// Importing the old YAML files stores each group once, by file name.
    #[tokio::test]
    async fn yaml_import_is_idempotent() {
        let db = MusicDb::new("sqlite::memory:").await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairs.yaml");
        std::fs::write(
            &path,
            "groups:\n  - name: g\n    anchors:\n      a: { members: ['x:a1', 'x:a2'] }\n      b: { members: ['x:b'] }\n",
        )
        .unwrap();
        for _ in 0..2 {
            import_yaml_barriers(&[], &db, &[path.as_path()])
                .await
                .unwrap();
        }
        let groups = db.barrier_groups().await.unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].origin, "yaml:pairs.yaml");
        assert_eq!(groups[0].anchors["a"], vec![p("a1"), p("a2")]);
        let config = DedupConfig::load(&db).await.unwrap();
        assert_eq!(config.groups[0].anchors["b"].members, vec!["x:b"]);
    }
}
