use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use musiclib_rs::musicdb::MusicDb;
use musiclib_rs::providers::FetchProvider;
use musiclib_rs::providers::types::EntryType;
use tracing::{info, warn};

use super::state::{ChildEdge, Pair, PairMetadata, State};

/// Reduce all collected events to DB writes.
///
/// 0. Canonicalize every pair at the storage boundary so only canonical pairs
///    are written; pairs no provider recognises fall back to their raw form.
/// 1. Look up which pairs already have entry_ids in the DB.
/// 2. Union-find over pairs using both fresh `is_rel` events and existing DB
///    groupings (pairs already sharing an entry_id).
/// 3. Resolve each equivalence class to a single entry_id, applying DB-level
///    merges when a class spans more than one existing entry.
/// 4. Write pair metadata (real rows for fetched pairs, stub rows for the
///    rest), aliases, child edges, contributions.
pub async fn flush(
    state: Arc<State>,
    providers: &[Arc<dyn FetchProvider>],
    db: &MusicDb,
) -> anyhow::Result<()> {
    let metadata = std::mem::take(&mut *state.metadata.lock().unwrap());
    let is_rel = std::mem::take(&mut *state.is_rel.lock().unwrap());
    let has_rel = std::mem::take(&mut *state.has_rel.lock().unwrap());

    // 0. Canonicalize every pair referenced by any event. `canonicalize` is
    //    offline shape-matching, so this works even for children we never
    //    fetched. Pairs that canonicalize are kept only in canonical form; the
    //    non-canonical form never reaches the DB. Pairs no provider recognises
    //    map to themselves and are stored verbatim (the `unknown_url` stub
    //    case). Cross-source equivalences survive because canonicalize never
    //    maps one real source into another's namespace — only the per-pair
    //    normalization relation collapses (its `is_rel` becomes a self-loop).
    let mut raw_pairs: HashSet<Pair> = HashSet::new();
    for p in metadata.keys() {
        raw_pairs.insert(p.clone());
    }
    for (a, b) in &is_rel {
        raw_pairs.insert(a.clone());
        raw_pairs.insert(b.clone());
    }
    for edge in &has_rel {
        raw_pairs.insert(edge.parent.clone());
        raw_pairs.insert(edge.child.clone());
    }
    let mut canon: HashMap<Pair, Pair> = HashMap::new();
    for pair in &raw_pairs {
        let c = canonicalize_pair(providers, pair).await;
        canon.insert(pair.clone(), c);
    }

    let metadata: HashMap<Pair, PairMetadata> = metadata
        .into_iter()
        .map(|(p, m)| (canon[&p].clone(), m))
        .collect();
    let is_rel: Vec<(Pair, Pair)> = is_rel
        .into_iter()
        .map(|(a, b)| (canon[&a].clone(), canon[&b].clone()))
        .filter(|(a, b)| a != b)
        .collect();
    let has_rel: Vec<ChildEdge> = has_rel
        .into_iter()
        .map(|mut edge| {
            edge.parent = canon[&edge.parent].clone();
            edge.child = canon[&edge.child].clone();
            edge
        })
        .collect();

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
    //    multiple existing entry_ids. Reconcile the class's entry_type from
    //    members' fresh metadata: new entries get the type at creation; existing
    //    entries are updated after merges.
    let mut class_to_entry: HashMap<Pair, i64> = HashMap::new();
    let mut merges: Vec<(i64, i64)> = Vec::new();
    let mut existing_entry_types: Vec<(i64, EntryType)> = Vec::new();
    for (repr, members) in &classes {
        let class_type = reconcile_type(members, &metadata);
        let mut existing_ids: HashSet<i64> = HashSet::new();
        for p in members {
            if let Some(id) = existing[p] {
                existing_ids.insert(id);
            }
        }
        let entry_id = if existing_ids.is_empty() {
            db.insert_entry(class_type).await?
        } else {
            let winner = *existing_ids.iter().min().unwrap();
            for &id in &existing_ids {
                if id != winner {
                    merges.push((id, winner));
                }
            }
            if let Some(t) = class_type {
                existing_entry_types.push((winner, t));
            }
            winner
        };
        class_to_entry.insert(repr.clone(), entry_id);
    }

    // 6. Apply DB-level entry merges before any further writes — this way all
    //    later inserts reference the surviving (winner) entry_ids. Then update
    //    entry_type for classes that already had a DB entry.
    if !merges.is_empty() {
        info!("merging {} existing entry pair(s)", merges.len());
    }
    for (loser, winner) in &merges {
        db.merge_entries(*loser, *winner).await?;
    }
    for (winner, t) in existing_entry_types {
        db.set_entry_type(winner, t).await?;
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

/// Compute the entry_type for an equivalence class from its members' metadata.
/// A `len() > 1` result means Part B missed a cross-type link — logged as a
/// warning and the lexically-smallest type is chosen deterministically.
fn reconcile_type(members: &[Pair], metadata: &HashMap<Pair, PairMetadata>) -> Option<EntryType> {
    let types: HashSet<EntryType> = members
        .iter()
        .filter_map(|p| metadata.get(p).map(|m| m.entry_type))
        .collect();
    match types.len() {
        0 => None,
        1 => types.into_iter().next(),
        _ => {
            warn!(
                "class spans multiple entry types {:?} — filtering gap",
                types
            );
            // Deterministic pick: Artist < Release < ReleaseGroup < Track by discriminant.
            types.into_iter().min_by_key(|t| *t as u8)
        }
    }
}

/// Canonicalize a pair via the first provider that recognises it; identity if
/// none do (the pair is then stored verbatim as a stub).
async fn canonicalize_pair(providers: &[Arc<dyn FetchProvider>], pair: &Pair) -> Pair {
    for provider in providers {
        if let Some(c) = provider.canonicalize(&pair.0, &pair.1).await {
            return (c.canonical_source_key.into_owned(), c.canonical_identifier);
        }
    }
    pair.clone()
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
