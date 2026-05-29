use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use musiclib_rs::musicdb::MusicDb;
use tracing::info;

use super::state::{ChildEdge, Pair, State};

/// Reduce all collected events to DB writes.
///
/// 1. Look up which pairs already have entry_ids in the DB.
/// 2. Union-find over pairs using both fresh `is_rel` events and existing DB
///    groupings (pairs already sharing an entry_id).
/// 3. Resolve each equivalence class to a single entry_id, applying DB-level
///    merges when a class spans more than one existing entry.
/// 4. Write pair metadata (real rows for fetched pairs, stub rows for the
///    rest), aliases, child edges, contributions.
pub async fn flush(state: Arc<State>, db: &MusicDb) -> anyhow::Result<()> {
    let metadata = std::mem::take(&mut *state.metadata.lock().unwrap());
    let is_rel = std::mem::take(&mut *state.is_rel.lock().unwrap());
    let has_rel = std::mem::take(&mut *state.has_rel.lock().unwrap());

    info!(
        "flushing: {} pair(s), {} is_rel, {} has_rel",
        metadata.len(),
        is_rel.len(),
        has_rel.len(),
    );

    // 1. Enumerate every pair referenced by any event.
    let mut all_pairs: HashSet<Pair> = HashSet::new();
    for p in metadata.keys() {
        all_pairs.insert(p.clone());
    }
    for (a, b) in &is_rel {
        all_pairs.insert(a.clone());
        all_pairs.insert(b.clone());
    }
    for edge in &has_rel {
        all_pairs.insert(edge.parent.clone());
        all_pairs.insert(edge.child.clone());
    }

    // 2. Look up existing entry_ids.
    let mut existing: HashMap<Pair, Option<i64>> = HashMap::new();
    for pair in &all_pairs {
        let id = db.find_entry_id_by_pair(&pair.0, &pair.1).await?;
        existing.insert(pair.clone(), id);
    }

    // 3. Union-find: union by fresh is_rel, then by shared existing entry_id
    //    (so the local model agrees with the DB's prior grouping).
    let mut uf = UnionFind::new();
    for pair in &all_pairs {
        uf.make_set(pair);
    }
    for (a, b) in &is_rel {
        uf.union(a, b);
    }
    let mut existing_groups: HashMap<i64, Vec<Pair>> = HashMap::new();
    for (pair, maybe_id) in &existing {
        if let Some(id) = maybe_id {
            existing_groups.entry(*id).or_default().push(pair.clone());
        }
    }
    for group in existing_groups.values() {
        for i in 1..group.len() {
            uf.union(&group[0], &group[i]);
        }
    }

    // 4. Resolve each pair to its class representative.
    let pair_to_repr: HashMap<Pair, Pair> =
        all_pairs.iter().map(|p| (p.clone(), uf.find(p))).collect();
    let mut classes: HashMap<Pair, Vec<Pair>> = HashMap::new();
    for (pair, repr) in &pair_to_repr {
        classes.entry(repr.clone()).or_default().push(pair.clone());
    }

    // 5. Assign an entry_id to each class; collect merges where a class spans
    //    multiple existing entry_ids.
    let mut class_to_entry: HashMap<Pair, i64> = HashMap::new();
    let mut merges: Vec<(i64, i64)> = Vec::new();
    for (repr, members) in &classes {
        let mut existing_ids: HashSet<i64> = HashSet::new();
        for p in members {
            if let Some(id) = existing[p] {
                existing_ids.insert(id);
            }
        }
        let entry_id = if existing_ids.is_empty() {
            db.insert_entry().await?
        } else {
            let winner = *existing_ids.iter().min().unwrap();
            for &id in &existing_ids {
                if id != winner {
                    merges.push((id, winner));
                }
            }
            winner
        };
        class_to_entry.insert(repr.clone(), entry_id);
    }

    // 6. Apply DB-level entry merges before any further writes — this way all
    //    later inserts reference the surviving (winner) entry_ids.
    if !merges.is_empty() {
        info!("merging {} existing entry pair(s)", merges.len());
    }
    for (loser, winner) in &merges {
        db.merge_entries(*loser, *winner).await?;
    }

    let entry_id_of = |pair: &Pair| -> i64 { class_to_entry[&pair_to_repr[pair]] };

    // 7. Upsert pair rows (real metadata) or insert stub rows (no metadata,
    //    no prior DB row). Pairs that already exist in the DB without fresh
    //    metadata are left untouched — `merge_entries` already re-pointed
    //    their entry_id if needed.
    for pair in &all_pairs {
        let entry_id = entry_id_of(pair);
        if let Some(meta) = metadata.get(pair) {
            db.upsert_pair(
                &pair.0,
                &pair.1,
                entry_id,
                meta.entry_type,
                meta.release_date.as_deref(),
                Some(meta.extra.to_string()),
                &meta.specific_data,
            )
            .await?;
        } else if existing[pair].is_none() {
            db.insert_stub_pair(&pair.0, &pair.1, entry_id).await?;
        }
    }

    // 8. Aliases: one batch per fetched pair.
    for (pair, meta) in &metadata {
        if !meta.aliases.is_empty() {
            db.insert_aliases_for_pair(&pair.0, &pair.1, &meta.aliases)
                .await?;
        }
    }

    // 9. Child edges: deduped on the composite PK by `insert_child_edge`.
    for edge in &has_rel {
        db.insert_child_edge(
            &edge.parent.0,
            &edge.parent.1,
            &edge.child.0,
            &edge.child.1,
            edge.disc_no,
            edge.track_no,
        )
        .await?;
    }

    // 10. Contributions: one per (parent_pair, child_pair, role) observation.
    for edge in &has_rel {
        write_edge_contributions(db, edge).await?;
    }

    Ok(())
}

async fn write_edge_contributions(db: &MusicDb, edge: &ChildEdge) -> anyhow::Result<()> {
    for contrib in &edge.contributions {
        db.insert_contribution(
            &edge.parent.0,
            &edge.parent.1,
            &edge.child.0,
            &edge.child.1,
            contrib,
        )
        .await?;
    }
    Ok(())
}

struct UnionFind {
    parent: HashMap<Pair, Pair>,
    rank: HashMap<Pair, usize>,
}

impl UnionFind {
    fn new() -> Self {
        Self {
            parent: HashMap::new(),
            rank: HashMap::new(),
        }
    }

    fn make_set(&mut self, pair: &Pair) {
        self.parent
            .entry(pair.clone())
            .or_insert_with(|| pair.clone());
        self.rank.entry(pair.clone()).or_insert(0);
    }

    fn find(&mut self, pair: &Pair) -> Pair {
        let parent = self
            .parent
            .get(pair)
            .cloned()
            .unwrap_or_else(|| pair.clone());
        if parent == *pair {
            return pair.clone();
        }
        let root = self.find(&parent);
        self.parent.insert(pair.clone(), root.clone());
        root
    }

    fn union(&mut self, a: &Pair, b: &Pair) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return;
        }
        let rank_a = self.rank.get(&ra).copied().unwrap_or(0);
        let rank_b = self.rank.get(&rb).copied().unwrap_or(0);
        let (winner, loser) = if rank_a >= rank_b { (ra, rb) } else { (rb, ra) };
        self.parent.insert(loser, winner.clone());
        if rank_a == rank_b {
            *self.rank.entry(winner).or_insert(0) += 1;
        }
    }
}
