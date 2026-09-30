use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use crate::musicdb::MusicDb;
use crate::providers::FetchProvider;
use crate::providers::types::EntryType;
use tracing::{debug, info, warn};

use super::dedup::{AnchorId, DedupConfig};
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
///
/// `run_id`, if given, tags every row this call inserts (see the `run_id`
/// column on `entry_source`/`entry_alias`/`entry_child`/`contribution`) so a
/// run interrupted by a crash can be found and purged later by
/// `MusicDb::recover_pending_import_runs`.
///
/// `final_flush` distinguishes the run's last flush (called once the whole
/// traversal has finished, or a one-shot caller like the `dedup` binary that
/// only ever flushes once) from a periodic mid-run flush triggered to relieve
/// memory pressure (see `pipeline::ingest`'s watchdog). A periodic flush only
/// ever *creates* rows — any equivalence class that would touch a pair
/// already present in the DB (whether from earlier history or from an
/// earlier periodic flush of this same still-pending run) is left buffered in
/// `state` instead of being written, and gets picked up again by the next
/// call. This is what makes a periodic flush's writes cleanly undoable by a
/// plain "delete everything tagged with this run_id": it never merges,
/// splits, or otherwise mutates a pre-existing row, so there's never
/// anything to unwind besides freshly inserted rows. `final_flush = true`
/// processes every class regardless (today's original, single-flush
/// behaviour) — this is safe to interrupt only in the same sense today's
/// single end-of-import flush already is (not perfectly atomic against a
/// crash mid-call), which is unchanged by this feature.
///
/// Returns the set of entry_ids written or surviving after merges in this flush.
/// The caller can pass this set to `softmatch::match_new_entries` for online
/// soft-dedup without re-scanning previously compared pairs.
pub async fn flush(
    state: Arc<State>,
    providers: &[Arc<dyn FetchProvider>],
    db: &MusicDb,
    dedup: &DedupConfig,
    run_id: Option<i64>,
    final_flush: bool,
) -> anyhow::Result<std::collections::HashSet<i64>> {
    let metadata = std::mem::take(&mut *state.metadata.lock().unwrap());
    let is_rel = std::mem::take(&mut *state.is_rel.lock().unwrap());
    let has_rel = std::mem::take(&mut *state.has_rel.lock().unwrap());

    // Account for what was just drained; anything re-buffered below (step 4b,
    // deferred classes) goes back in through `State::insert_metadata`/
    // `push_is_rel`/`push_has_rel`, which re-adds it to the counter.
    let drained_bytes: usize = metadata
        .iter()
        .map(|(p, m)| super::state::pair_bytes(p) + super::state::metadata_bytes(m))
        .sum::<usize>()
        + is_rel
            .iter()
            .map(|(a, b)| super::state::pair_bytes(a) + super::state::pair_bytes(b) + 16)
            .sum::<usize>()
        + has_rel.iter().map(super::state::edge_bytes).sum::<usize>();
    state.sub_bytes(drained_bytes);

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

    // 2b. Compile declared dedup barriers (entities that must never merge) into
    //     a map of canonical pair -> anchor id. Empty config => no-op.
    let (barrier, group_names) = dedup.compile(providers).await;

    // 3. Union-find: union by fresh is_rel, then by shared existing entry_id
    //    (so the local model agrees with the DB's prior grouping). Seeded with
    //    anchor labels from the barrier map; any union that would join two
    //    different anchors is refused, keeping the declared entities apart.
    let mut uf = UnionFind::new();
    for pair in &all_pairs {
        uf.make_set(pair);
    }
    for (pair, id) in &barrier {
        uf.make_set(pair);
        uf.set_label(pair, id.clone());
    }
    // Force-union every pair that shares an anchor label and is present in
    // all_pairs. This makes an anchor a positive merge directive: listing two
    // pairs under one anchor merges them even when no URL cross-linking
    // connects them. Pairs in the barrier but not yet imported are skipped so
    // no phantom entries are created.
    let mut by_anchor: HashMap<AnchorId, Vec<Pair>> = HashMap::new();
    for (pair, id) in &barrier {
        if all_pairs.contains(pair) {
            by_anchor.entry(id.clone()).or_default().push(pair.clone());
        }
    }
    for mates in by_anchor.values() {
        for i in 1..mates.len() {
            // Same label → union always succeeds (never Err).
            let _ = uf.union(&mates[0], &mates[i]);
        }
    }
    // Process is_rel in a stable order so the (arbitrary) assignment of any
    // unclaimed shared node is deterministic across runs.
    let mut is_rel_sorted = is_rel.clone();
    is_rel_sorted.sort();
    for (a, b) in &is_rel_sorted {
        if let Err((la, lb)) = uf.union(a, b) {
            debug!(
                "dedup: kept anchors {:?}/{} and {:?}/{} apart (group '{}') — cut edge {}:{} <-> {}:{}",
                la.1,
                group_names.get(la.0).map_or("?", |s| s.as_str()),
                lb.1,
                group_names.get(lb.0).map_or("?", |s| s.as_str()),
                group_names.get(la.0).map_or("?", |s| s.as_str()),
                a.0,
                a.1,
                b.0,
                b.1,
            );
        }
    }
    let mut existing_groups: HashMap<i64, Vec<Pair>> = HashMap::new();
    for (pair, maybe_id) in &existing {
        if let Some(id) = maybe_id {
            existing_groups.entry(*id).or_default().push(pair.clone());
        }
    }
    for group in existing_groups.values() {
        for i in 1..group.len() {
            // A refusal here is expected: it means the barrier separates entities
            // that a prior import wrongly merged under one entry_id. The split
            // logic in step 5 re-homes the separated classes onto fresh entries.
            let _ = uf.union(&group[0], &group[i]);
        }
    }

    // 4. Resolve each pair to its class representative.
    let pair_to_repr: HashMap<Pair, Pair> =
        all_pairs.iter().map(|p| (p.clone(), uf.find(p))).collect();
    let mut classes: HashMap<Pair, Vec<Pair>> = HashMap::new();
    for (pair, repr) in &pair_to_repr {
        classes.entry(repr.clone()).or_default().push(pair.clone());
    }

    // 4b. In a periodic (non-final) flush, only write classes made up
    //     entirely of brand-new pairs. A class that touches any pair already
    //     present in the DB (whether from earlier history or an earlier
    //     periodic flush within this same still-pending run) is deferred:
    //     its metadata/edges are handed back to `state` instead of being
    //     written, so a later flush (another periodic one, or the run's
    //     final flush) picks it up. See `flush`'s doc comment for why this
    //     is what keeps a periodic flush's writes cleanly undoable.
    //
    //     A `ChildEdge` carrying `original_relation_kind` is deferred as a
    //     whole regardless of its endpoints' freshness too: writing it needs
    //     `entry_id_of` for both ends (step 10b), and deferring only the
    //     entry_relation row while still writing the edge/contributions now
    //     would split one logical edge across two flush calls.
    let deferred_pairs: HashSet<Pair> = if final_flush {
        HashSet::new()
    } else {
        classes
            .values()
            .filter(|members| members.iter().any(|p| existing[p].is_some()))
            .flatten()
            .cloned()
            .collect()
    };

    let (metadata, deferred_metadata): (HashMap<Pair, PairMetadata>, HashMap<Pair, PairMetadata>) =
        metadata
            .into_iter()
            .partition(|(p, _)| !deferred_pairs.contains(p));
    // `is_rel` itself is never read again below (its only job was driving the
    // union-find above); only the deferred half needs to survive, so it can
    // be re-buffered for a later flush to redo that union-find with.
    let deferred_is_rel: Vec<(Pair, Pair)> = is_rel
        .into_iter()
        .filter(|(a, b)| deferred_pairs.contains(a) || deferred_pairs.contains(b))
        .collect();
    let defer_edge = |edge: &ChildEdge| -> bool {
        !final_flush
            && (deferred_pairs.contains(&edge.parent)
                || deferred_pairs.contains(&edge.child)
                || edge.original_relation_kind.is_some())
    };
    let (has_rel, deferred_has_rel): (Vec<ChildEdge>, Vec<ChildEdge>) =
        has_rel.into_iter().partition(|e| !defer_edge(e));

    if !deferred_pairs.is_empty() || !deferred_has_rel.is_empty() {
        debug!(
            "flush: deferring {} pair(s) and {} edge(s) touching existing/related entries to a later flush",
            deferred_metadata.len().max(deferred_pairs.len()),
            deferred_has_rel.len(),
        );
    }
    for (pair, meta) in deferred_metadata {
        state.insert_metadata(pair, meta);
    }
    for (a, b) in deferred_is_rel {
        state.push_is_rel(a, b);
    }
    for edge in deferred_has_rel {
        state.push_has_rel(edge);
    }

    let classes: HashMap<Pair, Vec<Pair>> = classes
        .into_iter()
        .filter(|(_, members)| !members.iter().any(|p| deferred_pairs.contains(p)))
        .collect();
    let all_pairs: HashSet<Pair> = all_pairs
        .into_iter()
        .filter(|p| !deferred_pairs.contains(p))
        .collect();

    // 5. Assign an entry_id to each class; collect merges where a class spans
    //    multiple existing entry_ids. Reconcile the class's entry_type from
    //    members' fresh metadata: new entries get the type at creation; existing
    //    entries are updated after merges.
    // A class may keep an existing entry_id only if it's the *sole* class
    // referencing it. When the barrier splits a contaminated entry, several
    // classes share one existing id; none may keep it, so each gets a fresh
    // entry and its pairs are re-pointed below (and the emptied entry is GC'd).
    let owned_ids = owned_existing_ids(&classes, &existing);

    let mut class_to_entry: HashMap<Pair, i64> = HashMap::new();
    let mut merges: Vec<(i64, i64)> = Vec::new();
    let mut existing_entry_types: Vec<(i64, EntryType)> = Vec::new();
    let mut split_count = 0usize;
    for (repr, members) in &classes {
        let class_type = reconcile_type(members, &metadata);
        // Existing ids uniquely owned by this class (sorted, min first).
        let owned = owned_ids[repr].clone();
        let entry_id = if owned.is_empty() {
            let new_id = db.insert_entry(class_type).await?;
            // Referencing existing entries but owning none => the barrier split
            // this class off a shared (contaminated) entry onto a fresh one.
            let referenced: BTreeSet<i64> = members.iter().filter_map(|p| existing[p]).collect();
            if !referenced.is_empty() {
                split_count += 1;
                debug!("dedup: split off existing {referenced:?} into new entry {new_id}");
            }
            new_id
        } else {
            let mut it = owned.into_iter();
            let winner = it.next().unwrap();
            for id in it {
                merges.push((id, winner));
            }
            if let Some(t) = class_type {
                existing_entry_types.push((winner, t));
            }
            winner
        };
        class_to_entry.insert(repr.clone(), entry_id);
    }
    if split_count > 0 {
        info!("dedup: split {split_count} class(es) off shared entries (barrier-enforced)");
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
                &meta.specific_data,
                run_id,
            )
            .await?;
        } else if existing[pair].is_none() {
            db.insert_stub_pair(&pair.0, &pair.1, entry_id, run_id)
                .await?;
        } else if existing[pair] != Some(entry_id) {
            // Existing stub pair that a split moved to a different entry.
            db.set_pair_entry(&pair.0, &pair.1, entry_id).await?;
        }
    }

    // 8. Aliases: one batch per fetched pair. `insert_aliases_for_pair` has no
    //    uniqueness constraint to lean on (a pair can legitimately carry two
    //    same-named aliases from different sources), so dedupe the exact
    //    (name, source, locale, primary) repeats here — an entity referenced
    //    by many children (e.g. an artist credited on hundreds of tracks) can
    //    otherwise accumulate thousands of identical observations.
    for (pair, meta) in &metadata {
        if meta.aliases.is_empty() {
            continue;
        }
        let mut seen = HashSet::new();
        let deduped: Vec<_> = meta
            .aliases
            .iter()
            .filter(|a| seen.insert((&a.name, &a.source, &a.locale, a.primary)))
            .cloned()
            .collect();
        if meta.aliases.len() >= 50 && deduped.len() * 4 < meta.aliases.len() {
            warn!(
                source = %pair.0,
                identifier = %pair.1,
                raw = meta.aliases.len(),
                deduped = deduped.len(),
                sample = ?deduped.iter().take(5).map(|a| (&a.name, &a.source, &a.locale)).collect::<Vec<_>>(),
                "pair accumulated a heavily duplicated alias list",
            );
        }
        db.insert_aliases_for_pair(&pair.0, &pair.1, &deduped, run_id)
            .await?;
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
            run_id,
        )
        .await?;
    }

    // 10. Contributions: one per (parent_pair, child_pair, role) observation.
    for edge in &has_rel {
        write_edge_contributions(db, edge, run_id).await?;
    }

    // 10b. Provider-asserted original->derived relations (cover/remix/
    // arrangement). Written directly from import data rather than the
    // heuristic/Jev pass, since a cover and its original can be just as
    // acoustically dissimilar as two unrelated covers — the whole point is
    // to not depend on content-similarity rediscovering this link.
    for edge in &has_rel {
        let Some(kind) = &edge.original_relation_kind else {
            continue;
        };
        let derived_id = entry_id_of(&edge.parent); // the cover/remix/arrangement
        let original_id = entry_id_of(&edge.child); // the original
        if derived_id == original_id {
            continue; // already merged (e.g. by dedup) — nothing to relate
        }
        let extra = serde_json::json!({
            "derived_entry": derived_id,
            "source_entry": original_id,
        })
        .to_string();
        db.upsert_relation(
            original_id,
            derived_id,
            kind,
            confidence_for_original_relation(kind),
            "provider",
            Some(&extra),
        )
        .await?;
    }

    // 11. GC entries left empty by a split (their pairs were re-pointed above).
    db.delete_orphan_entries().await?;

    let touched: std::collections::HashSet<i64> = class_to_entry.values().copied().collect();
    Ok(touched)
}

/// Confidence for a provider-asserted original->derived relation. `"cover"`
/// is Work-mediated (the "original" is inferred from an untagged Work
/// performance, which can occasionally return more than one near-duplicate
/// candidate — see the KING example in the design doc), so it gets a touch
/// less confidence than the unambiguous direct-relation kinds.
fn confidence_for_original_relation(kind: &str) -> f64 {
    match kind {
        "cover" => 0.85,
        _ => 0.9,
    }
}

async fn write_edge_contributions(
    db: &MusicDb,
    edge: &ChildEdge,
    run_id: Option<i64>,
) -> anyhow::Result<()> {
    for contrib in &edge.contributions {
        db.insert_contribution(
            &edge.parent.0,
            &edge.parent.1,
            &edge.child.0,
            &edge.child.1,
            contrib,
            run_id,
        )
        .await?;
    }
    Ok(())
}

/// For each class, the existing entry_ids it may keep: an id is owned only if a
/// single class references it. Contested ids (shared by a barrier-driven split)
/// are owned by no class, so those classes get a fresh entry. Returned sorted
/// (min first) so the kept id is deterministic.
fn owned_existing_ids(
    classes: &HashMap<Pair, Vec<Pair>>,
    existing: &HashMap<Pair, Option<i64>>,
) -> HashMap<Pair, BTreeSet<i64>> {
    let mut id_class_count: HashMap<i64, usize> = HashMap::new();
    for members in classes.values() {
        let ids: HashSet<i64> = members.iter().filter_map(|p| existing[p]).collect();
        for id in ids {
            *id_class_count.entry(id).or_insert(0) += 1;
        }
    }
    classes
        .iter()
        .map(|(repr, members)| {
            let owned = members
                .iter()
                .filter_map(|p| existing[p])
                .filter(|id| id_class_count[id] == 1)
                .collect();
            (repr.clone(), owned)
        })
        .collect()
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
pub(crate) async fn canonicalize_pair(providers: &[Arc<dyn FetchProvider>], pair: &Pair) -> Pair {
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
    /// Anchor label per set, keyed by root. Reading is always via `find`, so
    /// stale entries left on former roots after a union are never observed.
    labels: HashMap<Pair, AnchorId>,
}

impl UnionFind {
    fn new() -> Self {
        Self {
            parent: HashMap::new(),
            rank: HashMap::new(),
            labels: HashMap::new(),
        }
    }

    fn make_set(&mut self, pair: &Pair) {
        self.parent
            .entry(pair.clone())
            .or_insert_with(|| pair.clone());
        self.rank.entry(pair.clone()).or_insert(0);
    }

    /// Pin an anchor label onto `pair`'s set. Call after `make_set`.
    fn set_label(&mut self, pair: &Pair, id: AnchorId) {
        let root = self.find(pair);
        self.labels.insert(root, id);
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

    /// Union `a` and `b`. Refused (returns `Err` with the two conflicting
    /// anchors) if their sets carry different anchor labels — that is what keeps
    /// declared-distinct entities in separate equivalence classes.
    fn union(&mut self, a: &Pair, b: &Pair) -> Result<(), (AnchorId, AnchorId)> {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return Ok(());
        }
        let la = self.labels.get(&ra).cloned();
        let lb = self.labels.get(&rb).cloned();
        if let (Some(la), Some(lb)) = (&la, &lb)
            && la != lb
        {
            return Err((la.clone(), lb.clone()));
        }
        let rank_a = self.rank.get(&ra).copied().unwrap_or(0);
        let rank_b = self.rank.get(&rb).copied().unwrap_or(0);
        let (winner, loser) = if rank_a >= rank_b { (ra, rb) } else { (rb, ra) };
        self.parent.insert(loser, winner.clone());
        if rank_a == rank_b {
            *self.rank.entry(winner.clone()).or_insert(0) += 1;
        }
        // Carry the surviving label onto the new root.
        if let Some(lbl) = la.or(lb) {
            self.labels.insert(winner, lbl);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Pair {
        ("x".to_string(), s.to_string())
    }

    /// Two anchors chained through an unclaimed shared node stay in separate
    /// classes; a claimed node lands with its declared anchor.
    #[test]
    fn barrier_keeps_anchors_apart() {
        let hw: AnchorId = (0, "hw".into());
        let chico: AnchorId = (0, "chico".into());

        let a = p("anchor_hw");
        let b = p("anchor_chico");
        let shared = p("shared_unclaimed");
        let claimed = p("claimed_by_hw");

        let mut uf = UnionFind::new();
        for n in [&a, &b, &shared, &claimed] {
            uf.make_set(n);
        }
        // Barrier seeding: anchors + the claimed node.
        uf.set_label(&a, hw.clone());
        uf.set_label(&b, chico.clone());
        uf.set_label(&claimed, hw.clone());

        // Edges as flush would see them. The HW fetch links its anchor to its
        // shared node and to its claimed URL; chico's fetch also reaches both.
        assert!(uf.union(&a, &shared).is_ok());
        assert!(uf.union(&a, &claimed).is_ok());
        // shared is now HW; chico trying to join it (and the claimed URL) is refused.
        assert!(uf.union(&shared, &b).is_err());
        assert!(uf.union(&b, &claimed).is_err());

        // Anchors land in different classes.
        assert_ne!(uf.find(&a), uf.find(&b));
        // The unclaimed shared node attached to whichever anchor reached it first (HW).
        assert_eq!(uf.find(&shared), uf.find(&a));
        // The claimed node is with HW, never with chico.
        assert_eq!(uf.find(&claimed), uf.find(&a));
        assert_ne!(uf.find(&claimed), uf.find(&b));
    }

    /// A contested existing id (shared by two split classes) is kept by neither;
    /// an uncontested id is kept; a class spanning two of its own ids keeps the
    /// min and merges the rest.
    #[test]
    fn split_assigns_owned_ids() {
        let mut classes: HashMap<Pair, Vec<Pair>> = HashMap::new();
        // Class A and B both reference entry 534 (contested) — a split.
        classes.insert(p("a"), vec![p("a"), p("a2")]);
        classes.insert(p("b"), vec![p("b")]);
        // Class C uniquely owns 700 and 701 (a legit merge).
        classes.insert(p("c"), vec![p("c"), p("c2")]);

        let existing: HashMap<Pair, Option<i64>> = [
            (p("a"), Some(534)),
            (p("a2"), None),
            (p("b"), Some(534)),
            (p("c"), Some(700)),
            (p("c2"), Some(701)),
        ]
        .into_iter()
        .collect();

        let owned = owned_existing_ids(&classes, &existing);
        // 534 is contested → neither A nor B may keep it (both get new entries).
        assert!(owned[&p("a")].is_empty());
        assert!(owned[&p("b")].is_empty());
        // C owns both, sorted min first.
        assert_eq!(
            owned[&p("c")].iter().copied().collect::<Vec<_>>(),
            [700, 701]
        );
    }

    /// Same-anchor members merge freely.
    #[test]
    fn same_anchor_merges() {
        let chico: AnchorId = (0, "chico".into());
        let m1 = p("chico_mbid_1");
        let m2 = p("chico_mbid_2");
        let mut uf = UnionFind::new();
        uf.make_set(&m1);
        uf.make_set(&m2);
        uf.set_label(&m1, chico.clone());
        uf.set_label(&m2, chico.clone());
        assert!(uf.union(&m1, &m2).is_ok());
        assert_eq!(uf.find(&m1), uf.find(&m2));
    }

    /// The force-union loop merges same-anchor pairs that are in all_pairs even
    /// when no is_rel edge connects them — this is the chico two-MBID case.
    /// A pair listed in the barrier but absent from all_pairs is not unioned
    /// (no phantom entry created).
    #[test]
    fn force_union_merges_same_anchor_without_is_rel() {
        let chico: AnchorId = (0, "chico".into());

        let m1 = p("chico_mbid_1");
        let m2 = p("chico_mbid_2");
        let absent = p("chico_mbid_not_imported");

        let all_pairs: HashSet<Pair> = [m1.clone(), m2.clone()].into();
        let barrier: HashMap<Pair, AnchorId> = [
            (m1.clone(), chico.clone()),
            (m2.clone(), chico.clone()),
            (absent.clone(), chico.clone()),
        ]
        .into();

        let mut uf = UnionFind::new();
        for pair in &all_pairs {
            uf.make_set(pair);
        }
        for (pair, id) in &barrier {
            uf.make_set(pair);
            uf.set_label(pair, id.clone());
        }

        // Simulate the force-union loop.
        let mut by_anchor: HashMap<AnchorId, Vec<Pair>> = HashMap::new();
        for (pair, id) in &barrier {
            if all_pairs.contains(pair) {
                by_anchor.entry(id.clone()).or_default().push(pair.clone());
            }
        }
        for mates in by_anchor.values() {
            for i in 1..mates.len() {
                let _ = uf.union(&mates[0], &mates[i]);
            }
        }

        // m1 and m2 are in the same class despite no is_rel edge.
        assert_eq!(uf.find(&m1), uf.find(&m2));
        // The absent pair is NOT in the same class as m1/m2 (skipped by loop).
        assert_ne!(uf.find(&absent), uf.find(&m1));
    }

    fn track_metadata() -> PairMetadata {
        PairMetadata {
            entry_type: EntryType::Track,
            release_date: None,
            specific_data: crate::providers::types::EntrySpecificData::Track {
                duration_ms: vec![],
                positions: HashMap::new(),
            },
            aliases: vec![],
        }
    }

    /// Two distinct covers of the same original, each carrying
    /// `original_relation_kind: Some("cover")` on their `ChildEdge` to the
    /// shared original — the exact "no sibling mesh" shape this feature
    /// promises: two `entry_relation` rows (`original<->cover_1`,
    /// `original<->cover_2`), and no row directly between the two covers.
    #[tokio::test]
    async fn flush_writes_original_relation_without_sibling_mesh() -> anyhow::Result<()> {
        let db = MusicDb::new("sqlite::memory:").await?;
        let original = p("original");
        let cover1 = p("cover_1");
        let cover2 = p("cover_2");

        let state = State::new();
        for pair in [&original, &cover1, &cover2] {
            state
                .metadata
                .lock()
                .unwrap()
                .insert(pair.clone(), track_metadata());
        }
        for cover in [&cover1, &cover2] {
            state.has_rel.lock().unwrap().push(ChildEdge {
                parent: cover.clone(),
                child: original.clone(),
                disc_no: None,
                track_no: None,
                contributions: vec![],
                original_relation_kind: Some("cover".to_string()),
            });
        }

        flush(
            Arc::new(state),
            &[],
            &db,
            &DedupConfig::default(),
            None,
            true,
        )
        .await?;

        let original_id = db
            .find_entry_id_by_pair(&original.0, &original.1)
            .await?
            .expect("original entry");
        let cover1_id = db
            .find_entry_id_by_pair(&cover1.0, &cover1.1)
            .await?
            .expect("cover_1 entry");
        let cover2_id = db
            .find_entry_id_by_pair(&cover2.0, &cover2.1)
            .await?
            .expect("cover_2 entry");

        let relations = db.all_relations().await?;
        assert_eq!(relations.len(), 2, "exactly one edge per cover, no more");
        for rel in &relations {
            assert_eq!(rel.kind, "cover");
            assert_eq!(rel.origin, "provider");
            let pair = (rel.entry_a.min(rel.entry_b), rel.entry_a.max(rel.entry_b));
            assert_eq!(pair.0, original_id.min(pair.0));
        }
        // Both covers are linked to the original...
        let linked_to_original: HashSet<i64> = relations
            .iter()
            .flat_map(|r| [r.entry_a, r.entry_b])
            .filter(|id| *id != original_id)
            .collect();
        assert_eq!(linked_to_original, HashSet::from([cover1_id, cover2_id]));
        // ...and never directly to each other.
        assert!(
            !relations.iter().any(|r| {
                [r.entry_a, r.entry_b] == [cover1_id, cover2_id]
                    || [r.entry_a, r.entry_b] == [cover2_id, cover1_id]
            }),
            "covers must not be related directly to one another"
        );

        Ok(())
    }

    /// A periodic (non-final) flush writes a brand-new pair immediately and
    /// tags its row with the given run_id -- verified indirectly by rolling
    /// that run back afterwards and confirming the row disappears, since only
    /// something the periodic flush itself wrote could vanish that way.
    #[tokio::test]
    async fn periodic_flush_writes_fresh_pairs_tagged_with_run_id() -> anyhow::Result<()> {
        let db = MusicDb::new("sqlite::memory:").await?;
        let track = p("brand_new_track");

        let state = Arc::new(State::new());
        state
            .metadata
            .lock()
            .unwrap()
            .insert(track.clone(), track_metadata());

        flush(
            Arc::clone(&state),
            &[],
            &db,
            &DedupConfig::default(),
            Some(42),
            false,
        )
        .await?;

        let entry_id = db.find_entry_id_by_pair(&track.0, &track.1).await?;
        assert!(
            entry_id.is_some(),
            "periodic flush should write brand-new pairs immediately"
        );

        db.rollback_import_run(42).await?;
        let entry_id_after_rollback = db.find_entry_id_by_pair(&track.0, &track.1).await?;
        assert!(
            entry_id_after_rollback.is_none(),
            "rolling back the run should undo what the periodic flush wrote"
        );

        Ok(())
    }

    /// A periodic flush defers an entire class once any member already has a
    /// DB row: a sibling pair linked to it via is_rel is held back too, not
    /// written under a fresh entry of its own. The run's final flush picks
    /// the deferred class back up and resolves it onto the existing entry.
    #[tokio::test]
    async fn periodic_flush_defers_classes_touching_existing_entries() -> anyhow::Result<()> {
        let db = MusicDb::new("sqlite::memory:").await?;
        let existing_pair = p("already_in_library");
        let new_sibling = p("newly_discovered_sibling");

        // Simulate a pre-existing (already committed, no run_id) entry.
        let pre_existing_entry = db.insert_entry(Some(EntryType::Track)).await?;
        db.upsert_pair(
            &existing_pair.0,
            &existing_pair.1,
            pre_existing_entry,
            None,
            &track_metadata().specific_data,
            None,
        )
        .await?;

        let state = Arc::new(State::new());
        state
            .metadata
            .lock()
            .unwrap()
            .insert(existing_pair.clone(), track_metadata());
        state
            .metadata
            .lock()
            .unwrap()
            .insert(new_sibling.clone(), track_metadata());
        state
            .is_rel
            .lock()
            .unwrap()
            .push((existing_pair.clone(), new_sibling.clone()));

        // Periodic flush: the class {existing_pair, new_sibling} touches the
        // pre-existing entry, so it must be deferred whole.
        flush(
            Arc::clone(&state),
            &[],
            &db,
            &DedupConfig::default(),
            Some(7),
            false,
        )
        .await?;
        let sibling_id = db
            .find_entry_id_by_pair(&new_sibling.0, &new_sibling.1)
            .await?;
        assert!(
            sibling_id.is_none(),
            "a class touching a pre-existing entry must not be written by a periodic flush"
        );

        // The run's final flush resolves the deferred class onto the
        // pre-existing entry.
        flush(state, &[], &db, &DedupConfig::default(), Some(7), true).await?;
        let sibling_id = db
            .find_entry_id_by_pair(&new_sibling.0, &new_sibling.1)
            .await?
            .expect("final flush should write the deferred pair");
        assert_eq!(
            sibling_id, pre_existing_entry,
            "sibling should land on the pre-existing entry, not a new one"
        );

        Ok(())
    }
}
