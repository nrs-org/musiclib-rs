use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use crate::musicdb::{
    ImageOrigin, LINKS_COMPLETE, LinkKey, LinkRow, MusicDb, PROVIDER_ORIGIN, link_key,
};
use crate::providers::FetchProvider;
use crate::providers::std_values::{StandardProviderKeys, normalize_isrc};
use crate::providers::types::EntryType;
use tracing::{debug, info, warn};

use super::dedup::{AnchorId, DedupConfig};
use super::state::{ChildEdge, IsRel, Pair, PairMetadata, State, StubInfo};

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
    let stubs = std::mem::take(&mut *state.stubs.lock().unwrap());

    // Account for what was just drained; anything re-buffered below (step 4b,
    // deferred classes) goes back in through `State::insert_metadata`/
    // `push_is_rel`/`push_has_rel`, which re-adds it to the counter.
    let drained_bytes: usize = metadata
        .iter()
        .map(|(p, m)| super::state::pair_bytes(p) + super::state::metadata_bytes(m))
        .sum::<usize>()
        + is_rel
            .iter()
            .map(|r| super::state::is_rel_bytes(&r.a, &r.b, &r.asserted_by))
            .sum::<usize>()
        + has_rel.iter().map(super::state::edge_bytes).sum::<usize>()
        + stubs
            .iter()
            .map(|(p, s)| super::state::pair_bytes(p) + super::state::stub_bytes(s))
            .sum::<usize>();
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
    for r in &is_rel {
        raw_pairs.insert(r.a.clone());
        raw_pairs.insert(r.b.clone());
        raw_pairs.insert(r.asserted_by.clone());
    }
    for edge in &has_rel {
        raw_pairs.insert(edge.parent.clone());
        raw_pairs.insert(edge.child.clone());
    }
    for p in stubs.keys() {
        raw_pairs.insert(p.clone());
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
    let is_rel: Vec<IsRel> = is_rel
        .into_iter()
        .map(|r| IsRel {
            a: canon[&r.a].clone(),
            b: canon[&r.b].clone(),
            asserted_by: canon[&r.asserted_by].clone(),
            via: r.via,
        })
        .filter(|r| r.a != r.b)
        .collect();
    let has_rel: Vec<ChildEdge> = has_rel
        .into_iter()
        .map(|mut edge| {
            edge.parent = canon[&edge.parent].clone();
            edge.child = canon[&edge.child].clone();
            edge
        })
        .collect();

    // Remember, for the whole run, which non-artist parents were fetched and
    // every edge seen, by canonical pair: at commit these replace older edges
    // under re-fetched parents. Recorded before deferral so it doesn't matter
    // which flush ends up writing them. (A parent's metadata and its edges can
    // land in different flushes, so the two are tracked separately.) Every
    // fetched pair's aliases replace its older ones the same way; an artist
    // fetch lists all of them, just not all of its children. So do the
    // identity links each pair stated.
    for (pair, meta) in &metadata {
        if meta.entry_type != EntryType::Artist {
            state.note_fetched_parent(pair.clone());
        }
        state.note_aliases(pair, &meta.aliases);
    }
    for edge in &has_rel {
        state.note_edge(edge);
    }
    for rel in &is_rel {
        state.note_link(rel);
    }

    // Two raw forms of one child can collapse to the same canonical pair;
    // keep whichever listing gave a name/duration.
    let mut canon_stubs: HashMap<Pair, StubInfo> = HashMap::new();
    for (p, info) in stubs {
        let entry = canon_stubs.entry(canon[&p].clone()).or_insert(StubInfo {
            entry_type: info.entry_type,
            name: None,
            duration_ms: None,
            image: None,
        });
        entry.name = entry.name.take().or(info.name);
        entry.duration_ms = entry.duration_ms.or(info.duration_ms);
        entry.image = entry.image.take().or(info.image);
    }
    let stubs = canon_stubs;

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
    for r in &is_rel {
        all_pairs.insert(r.a.clone());
        all_pairs.insert(r.b.clone());
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

    // 2c. An entry this flush touches that holds a barrier pair joins the
    //     flush whole. Otherwise a merge into or out of it (`merge_entries`
    //     re-points every pair of the losing entry) can carry an anchor pair
    //     the union-find never saw into another anchor's entry.
    let mut barrier_entries: HashSet<i64> = HashSet::new();
    for pair in barrier.keys() {
        if let Some(id) = db.find_entry_id_by_pair(&pair.0, &pair.1).await? {
            barrier_entries.insert(id);
        }
    }
    let touched_barrier_entries: BTreeSet<i64> = existing
        .values()
        .flatten()
        .copied()
        .filter(|id| barrier_entries.contains(id))
        .collect();
    for &id in &touched_barrier_entries {
        for pair in db.pairs_by_entry_ids(&[id]).await? {
            existing.entry(pair.clone()).or_insert(Some(id));
            all_pairs.insert(pair);
        }
    }

    // 2d. With `LINKS_COMPLETE` on, step 3 groups some entries by
    //     their recorded links instead of keeping all their pairs together:
    //     one in which a link this run retracted is no longer stated by
    //     anything (it splits), and every barrier entry from 2c (a pair two
    //     anchors share then pulls only what links to it, not the whole
    //     entry, to the anchor that reaches it first). All of their pairs
    //     join this flush.
    let regrouped = if final_flush {
        regrouped_entries(&state, db, &is_rel, &touched_barrier_entries).await?
    } else {
        HashMap::new()
    };
    for (&id, regroup) in &regrouped {
        for pair in &regroup.members {
            existing.entry(pair.clone()).or_insert(Some(id));
            all_pairs.insert(pair.clone());
        }
    }

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
    let mut is_rel_sorted: Vec<(&Pair, &Pair)> = is_rel.iter().map(|r| (&r.a, &r.b)).collect();
    is_rel_sorted.sort();
    for &(a, b) in &is_rel_sorted {
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
    for (id, group) in &existing_groups {
        if let Some(regroup) = regrouped.get(id) {
            for (a, b) in &regroup.links {
                let _ = uf.union(a, b);
            }
            continue;
        }
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
    // The deferred half of `is_rel` is re-buffered for a later flush to redo
    // the union-find with; the rest is only stored as links (step 9b).
    let (deferred_is_rel, is_rel): (Vec<IsRel>, Vec<IsRel>) = is_rel
        .into_iter()
        .partition(|r| deferred_pairs.contains(&r.a) || deferred_pairs.contains(&r.b));
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
    for r in deferred_is_rel {
        state.push_is_rel(r.a, r.b, r.asserted_by, r.via);
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

    // Stub info is recorded when a child is listed, but its edge only once
    // the parent's whole listing has finished (importer step 7), so it can
    // arrive a flush early. Keep it for pairs written now; hand the rest back
    // to `state` until the final flush, which drops what nothing references.
    let (stubs, pending_stubs): (HashMap<Pair, StubInfo>, HashMap<Pair, StubInfo>) =
        stubs.into_iter().partition(|(p, _)| all_pairs.contains(p));
    if !final_flush {
        for (pair, info) in pending_stubs {
            state.insert_stub(pair, info);
        }
    }

    // 5. Assign an entry_id to each class; collect merges where a class spans
    //    multiple existing entry_ids. Reconcile the class's entry_type from
    //    members' fresh metadata: new entries get the type at creation; existing
    //    entries are updated after merges.
    // A class keeps an existing entry_id if it's the *sole* class referencing
    // it. When the barrier splits a contaminated entry, several classes share
    // one existing id; the class holding the entry's oldest pairs keeps it (so
    // relations and feedback on it stay attached), and every other class gets
    // a fresh entry with its pairs re-pointed below.
    let contested = contested_existing_ids(&classes, &existing);
    let run_ids = db
        .pair_run_ids_by_entry_ids(&contested.iter().copied().collect::<Vec<_>>())
        .await?;
    let keepers = split_keepers(&classes, &existing, &contested, &run_ids);
    let owned_ids = owned_existing_ids(&classes, &existing, &keepers);

    let mut class_to_entry: HashMap<Pair, i64> = HashMap::new();
    let mut merges: Vec<(i64, i64)> = Vec::new();
    let mut existing_entry_types: Vec<(i64, EntryType)> = Vec::new();
    let mut split_count = 0usize;
    for (repr, members) in &classes {
        let class_type = reconcile_type(members, &metadata);
        // Existing ids uniquely owned by this class (sorted, min first).
        let owned = owned_ids[repr].clone();
        let entry_id = if owned.is_empty() {
            // A class nobody fetched takes its type from what listed it. Only
            // for new entries: an existing entry's type came from a fetch.
            let new_type = class_type.or_else(|| stub_type(members, &stubs));
            let new_id = db.insert_entry(new_type).await?;
            // Referencing existing entries but owning none => the barrier
            // or a retracted link split this class off a shared entry onto a
            // fresh one, which keeps the old entry's type.
            let referenced: BTreeSet<i64> = members.iter().filter_map(|p| existing[p]).collect();
            if let Some(&old) = referenced.first() {
                if new_type.is_none() {
                    db.copy_entry_type(old, new_id).await?;
                }
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
        info!(
            "dedup: split {split_count} class(es) off shared entries (barrier or retracted link)"
        );
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
            if let Some(image) = &meta.image {
                db.set_pair_image(&pair.0, &pair.1, image, ImageOrigin::Fetch, run_id)
                    .await?;
            }
            if let Some(privacy) = meta.privacy {
                db.set_provider_privacy(&pair.0, &pair.1, privacy).await?;
            }
        } else {
            if let Some(image) = stubs.get(pair).and_then(|s| s.image.as_ref()) {
                db.set_pair_image(&pair.0, &pair.1, image, ImageOrigin::Listing, run_id)
                    .await?;
            }
            let duration_ms = stubs.get(pair).and_then(|s| s.duration_ms);
            if existing[pair].is_none() || duration_ms.is_some() {
                // New stub row, or a duration for an existing stub that had
                // none; never touches a fetched row or its entry_id.
                db.upsert_stub_pair(&pair.0, &pair.1, entry_id, duration_ms, run_id)
                    .await?;
            }
            if existing[pair].is_some_and(|id| id != entry_id) {
                // Existing stub pair that a split moved to a different entry.
                db.set_pair_entry(&pair.0, &pair.1, entry_id).await?;
            }
        }
    }

    // 7b. Listing names for pairs that are still stubs (no-op once fetched).
    for (pair, info) in &stubs {
        if metadata.contains_key(pair) {
            continue;
        }
        if let Some(name) = &info.name {
            db.insert_stub_alias(&pair.0, &pair.1, name, run_id).await?;
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

    // 9b. Identity links, with what stated them.
    let links: Vec<LinkRow> = is_rel
        .iter()
        .map(|r| LinkRow {
            link: link_key(&r.a, &r.b),
            asserted_by: r.asserted_by.clone(),
            via: r.via.as_str(),
        })
        .collect();
    db.insert_links(&links, run_id).await?;
    let split_stale: Vec<LinkRow> = regrouped
        .into_values()
        .flat_map(|regroup| regroup.stale)
        .collect();
    db.delete_links(&split_stale).await?;

    // 10. Contributions: one per (parent_pair, child_pair, role) observation.
    for edge in &has_rel {
        write_edge_contributions(db, edge, run_id).await?;
    }

    // 10b. Provider-asserted original->derived relations (cover/remix/
    // arrangement). Written directly from import data rather than the
    // heuristic/Jev pass, since a cover and its original can be just as
    // acoustically dissimilar as two unrelated covers — the whole point is
    // to not depend on content-similarity rediscovering this link.
    //
    // An edition means "the same song": an entry whose pairs perform several
    // songs (a karaoke stream video merged with the per-song MB recordings
    // that link to it) is a container, so it gets no edition relation to any
    // one song's original (`docs/plan-multi-work-recordings.md`). The pairs'
    // own `has_rel` edges above still record which song each one covers.
    let derived_ids: Vec<i64> = has_rel
        .iter()
        .filter(|edge| edge.original_relation_kind.is_some())
        .map(|edge| entry_id_of(&edge.parent))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let containers = db.multi_song_entries(&derived_ids).await?;
    for edge in &has_rel {
        let Some(kind) = &edge.original_relation_kind else {
            continue;
        };
        let derived_id = entry_id_of(&edge.parent); // the cover/remix/arrangement
        let original_id = entry_id_of(&edge.child); // the original
        if derived_id == original_id || containers.contains(&derived_id) {
            continue; // already merged (e.g. by dedup), or a container of songs
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
            PROVIDER_ORIGIN,
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

/// An entry step 3 groups by its remaining links: every pair it has, and
/// the stored links among them still stated by something.
struct Regroup {
    members: Vec<Pair>,
    links: Vec<LinkKey>,
    /// Its stored links this run retracted, deleted with the split so the
    /// two agree even if the run is rolled back afterwards.
    stale: Vec<LinkRow>,
}

/// The entries step 3 groups by their links, when `LINKS_COMPLETE`
/// is on: those in which a link this run retracted is no longer stated by
/// anything (no other stored row, no fresh `is_rel`), and `barrier_ids`.
/// In a barrier entry with no such link, a pair linked to nothing inside it
/// (left there by an earlier barrier split) stays with the largest group
/// rather than splitting off on its own. Called by the final flush, once
/// every pair's links for the run are noted in `state`.
async fn regrouped_entries(
    state: &State,
    db: &MusicDb,
    is_rel: &[IsRel],
    barrier_ids: &BTreeSet<i64>,
) -> anyhow::Result<HashMap<i64, Regroup>> {
    if db.setting(LINKS_COMPLETE).await?.as_deref() != Some("1") {
        return Ok(HashMap::new());
    }
    let stale: HashSet<LinkRow> = db
        .stale_links(&state.peek_replacements())
        .await?
        .into_iter()
        .collect();
    let fresh: HashSet<LinkKey> = is_rel.iter().map(|r| link_key(&r.a, &r.b)).collect();
    let mut ids = BTreeSet::new();
    for row in &stale {
        let (a, b) = &row.link;
        if fresh.contains(&row.link) {
            continue;
        }
        for pair in [a, b] {
            if let Some(id) = db.find_entry_id_by_pair(&pair.0, &pair.1).await? {
                ids.insert(id);
            }
        }
    }
    let disconnected = ids.clone();
    ids.extend(barrier_ids);
    let mut out = HashMap::new();
    for id in ids {
        let members = db.pairs_by_entry_ids(&[id]).await?;
        let rows = db.links_among(&members).await?;
        let alive: HashSet<&LinkKey> = rows
            .iter()
            .filter(|r| !stale.contains(*r))
            .map(|r| &r.link)
            .collect();
        let dead = rows
            .iter()
            .any(|r| stale.contains(r) && !alive.contains(&r.link) && !fresh.contains(&r.link));
        if !dead && !barrier_ids.contains(&id) {
            continue;
        }
        let mut links: Vec<LinkKey> = alive.into_iter().cloned().collect();
        links.sort();
        keep_unlinked_pairs(db, id, &members, &mut links, !dead).await?;
        let stale_here: Vec<LinkRow> = if dead {
            rows.into_iter().filter(|r| stale.contains(r)).collect()
        } else {
            Vec::new()
        };
        if disconnected.contains(&id) && dead {
            info!(
                "entry {id}: a re-fetch retracted a link inside it; regrouping its {} pair(s) by {} remaining link(s)",
                members.len(),
                links.len()
            );
        }
        out.insert(
            id,
            Regroup {
                members,
                links,
                stale: stale_here,
            },
        );
    }
    Ok(out)
}

/// Link each of the entry's pairs found deleted upstream (`gone_at`) to its
/// largest group: a pair that can't be fetched states no links, so missing
/// ones say nothing about it (a merged-away MusicBrainz ID returns 404).
/// With `unlinked`, so is every pair no link inside the entry touches.
async fn keep_unlinked_pairs(
    db: &MusicDb,
    entry_id: i64,
    members: &[Pair],
    links: &mut Vec<LinkKey>,
    unlinked: bool,
) -> anyhow::Result<()> {
    let linked: HashSet<&Pair> = links.iter().flat_map(|(a, b)| [a, b]).collect();
    let rows = db.source_rows_by_entry_ids(&[entry_id]).await?;
    let fetched: HashSet<Pair> = rows
        .iter()
        .filter(|r| r.fetched_at.is_some())
        .map(|r| (r.source.clone(), r.identifier.clone()))
        .collect();
    let gone: Vec<Pair> = rows
        .into_iter()
        .map(|r| (r.source, r.identifier, r.gone_at))
        .filter(|(s, i, gone_at)| {
            gone_at.is_some() || (unlinked && !linked.contains(&(s.clone(), i.clone())))
        })
        .map(|(s, i, _)| (s, i))
        .collect();
    if gone.is_empty() {
        return Ok(());
    }
    let mut uf = UnionFind::new();
    for pair in members {
        uf.make_set(pair);
    }
    for (a, b) in links.iter() {
        let _ = uf.union(a, b);
    }
    let mut groups: HashMap<Pair, Vec<&Pair>> = HashMap::new();
    for pair in members.iter().filter(|p| !gone.contains(p)) {
        groups.entry(uf.find(pair)).or_default().push(pair);
    }
    // The largest group, then attach to its smallest fetched pair: a stub in
    // it (an ISRC two barrier anchors share) can end up with the other anchor.
    let Some(target) = groups
        .into_values()
        .max_by(|a, b| {
            a.len()
                .cmp(&b.len())
                .then_with(|| b.iter().min().cmp(&a.iter().min()))
        })
        .and_then(|group| {
            group
                .into_iter()
                .min_by_key(|p| (!fetched.contains(*p), *p))
        })
        .cloned()
    else {
        return Ok(());
    };
    for pair in &gone {
        links.push(link_key(pair, &target));
    }
    Ok(())
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

/// Existing entry_ids referenced by more than one class: entries a
/// barrier-driven split is breaking apart.
fn contested_existing_ids(
    classes: &HashMap<Pair, Vec<Pair>>,
    existing: &HashMap<Pair, Option<i64>>,
) -> BTreeSet<i64> {
    let mut id_class_count: HashMap<i64, usize> = HashMap::new();
    for members in classes.values() {
        let ids: HashSet<i64> = members.iter().filter_map(|p| existing[p]).collect();
        for id in ids {
            *id_class_count.entry(id).or_insert(0) += 1;
        }
    }
    id_class_count
        .into_iter()
        .filter(|&(_, count)| count > 1)
        .map(|(id, _)| id)
        .collect()
}

/// For each contested id, the class that keeps it: the one holding the most
/// of the entry's pairs from its earliest import run (a `None` run, written
/// before runs were tracked, counts as earliest), then the most of its pairs
/// overall, then the smallest representative. Contamination arrives with a
/// later import, so the earliest pairs are the entry's original identity.
/// `run_ids` maps every pair of the contested entries, including pairs outside
/// this flush, to its `(entry_id, run_id)`.
fn split_keepers(
    classes: &HashMap<Pair, Vec<Pair>>,
    existing: &HashMap<Pair, Option<i64>>,
    contested: &BTreeSet<i64>,
    run_ids: &HashMap<Pair, (i64, Option<i64>)>,
) -> HashMap<i64, Pair> {
    let mut earliest: HashMap<i64, Option<i64>> = HashMap::new();
    for &(id, run) in run_ids.values() {
        earliest
            .entry(id)
            .and_modify(|e| *e = (*e).min(run))
            .or_insert(run);
    }
    // Per contested id: (pairs from its earliest run, pairs, repr) of the best class.
    let mut best: HashMap<i64, (usize, usize, &Pair)> = HashMap::new();
    for (repr, members) in classes {
        let mut held: HashMap<i64, (usize, usize)> = HashMap::new();
        for pair in members {
            let Some(id) = existing[pair].filter(|id| contested.contains(id)) else {
                continue;
            };
            let run = run_ids.get(pair).and_then(|&(_, run)| run);
            let counts = held.entry(id).or_default();
            counts.0 += usize::from(earliest.get(&id).is_none_or(|&e| e == run));
            counts.1 += 1;
        }
        for (id, (oldest, total)) in held {
            let beats = |&(b_oldest, b_total, b_repr): &(usize, usize, &Pair)| {
                (oldest, total, std::cmp::Reverse(repr))
                    > (b_oldest, b_total, std::cmp::Reverse(b_repr))
            };
            if best.get(&id).is_none_or(beats) {
                best.insert(id, (oldest, total, repr));
            }
        }
    }
    best.into_iter()
        .map(|(id, (_, _, repr))| (id, repr.clone()))
        .collect()
}

/// For each class, the existing entry_ids it may keep: an id is owned if a
/// single class references it, or if the class is that contested id's split
/// keeper. Every other class referencing a contested id gets a fresh entry.
/// Returned sorted (min first) so the kept id is deterministic.
fn owned_existing_ids(
    classes: &HashMap<Pair, Vec<Pair>>,
    existing: &HashMap<Pair, Option<i64>>,
    keepers: &HashMap<i64, Pair>,
) -> HashMap<Pair, BTreeSet<i64>> {
    classes
        .iter()
        .map(|(repr, members)| {
            let owned = members
                .iter()
                .filter_map(|p| existing[p])
                .filter(|id| keepers.get(id).is_none_or(|keeper| keeper == repr))
                .collect();
            (repr.clone(), owned)
        })
        .collect()
}

/// Compute the entry_type for an equivalence class from its members' metadata.
/// A `len() > 1` result means Part B missed a cross-type link — logged as a
/// warning and the lexically-smallest type is chosen deterministically.
/// The entry type the listings gave a class's stub members, if they agree.
fn stub_type(members: &[Pair], stubs: &HashMap<Pair, StubInfo>) -> Option<EntryType> {
    let types: HashSet<EntryType> = members
        .iter()
        .filter_map(|p| stubs.get(p).map(|s| s.entry_type))
        .collect();
    (types.len() == 1)
        .then(|| types.into_iter().next())
        .flatten()
}

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
    if pair.0 == StandardProviderKeys::ISRC {
        return (pair.0.clone(), normalize_isrc(&pair.1));
    }
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
    use crate::pipeline::state::LinkVia;

    fn p(s: &str) -> Pair {
        ("x".to_string(), s.to_string())
    }

    /// ISRCs are stored in one case whatever the provider returned, so a
    /// lower-case Spotify ISRC and its MB recording share a dedup key.
    #[tokio::test]
    async fn canonicalize_pair_normalizes_isrc() {
        let pair = ("isrc".to_string(), "ushm21927082".to_string());
        assert_eq!(
            canonicalize_pair(&[], &pair).await,
            ("isrc".to_string(), "USHM21927082".to_string())
        );
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

    /// A contested existing id (shared by two split classes) is kept only by
    /// the class holding the entry's oldest pairs; an uncontested id is kept; a
    /// class spanning two of its own ids keeps the min and merges the rest.
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

        // B's pair predates A's; "z" is a pair of 534 outside this flush.
        let run_ids: HashMap<Pair, (i64, Option<i64>)> = [
            (p("a"), (534, Some(5))),
            (p("b"), (534, Some(3))),
            (p("z"), (534, Some(9))),
        ]
        .into_iter()
        .collect();

        let contested = contested_existing_ids(&classes, &existing);
        assert_eq!(contested.iter().copied().collect::<Vec<_>>(), [534]);
        let keepers = split_keepers(&classes, &existing, &contested, &run_ids);
        let owned = owned_existing_ids(&classes, &existing, &keepers);
        // 534 is contested → B, holding its oldest pair, keeps it; A gets a new entry.
        assert!(owned[&p("a")].is_empty());
        assert_eq!(owned[&p("b")].iter().copied().collect::<Vec<_>>(), [534]);
        // C owns both, sorted min first.
        assert_eq!(
            owned[&p("c")].iter().copied().collect::<Vec<_>>(),
            [700, 701]
        );
    }

    /// The earliest run decides before size; an untracked (`None`) run is the
    /// earliest; equal counts fall back to the larger class, then the smaller
    /// representative.
    #[test]
    fn split_keeper_prefers_oldest_then_largest() {
        let classes: HashMap<Pair, Vec<Pair>> = [
            (p("a"), vec![p("a"), p("a2"), p("a3")]),
            (p("b"), vec![p("b")]),
        ]
        .into_iter()
        .collect();
        let existing: HashMap<Pair, Option<i64>> = [
            (p("a"), Some(1)),
            (p("a2"), Some(1)),
            (p("a3"), Some(1)),
            (p("b"), Some(1)),
        ]
        .into_iter()
        .collect();
        let contested = contested_existing_ids(&classes, &existing);
        let keeper = |runs: [Option<i64>; 4]| {
            let run_ids = ["a", "a2", "a3", "b"]
                .into_iter()
                .zip(runs)
                .map(|(s, run)| (p(s), (1, run)))
                .collect();
            split_keepers(&classes, &existing, &contested, &run_ids)[&1].clone()
        };
        // The lone untracked pair outranks three later ones.
        assert_eq!(keeper([Some(2), Some(2), Some(2), None]), p("b"));
        // Same run everywhere: the larger class keeps the id.
        assert_eq!(keeper([Some(2), Some(2), Some(2), Some(2)]), p("a"));
        // One oldest pair each, same size otherwise: the larger class still wins.
        assert_eq!(keeper([Some(1), Some(2), Some(2), Some(1)]), p("a"));

        let tied: HashMap<Pair, Vec<Pair>> = [(p("x"), vec![p("x")]), (p("w"), vec![p("w")])]
            .into_iter()
            .collect();
        let existing: HashMap<Pair, Option<i64>> =
            [(p("x"), Some(7)), (p("w"), Some(7))].into_iter().collect();
        let run_ids = [(p("x"), (7, Some(1))), (p("w"), (7, Some(1)))]
            .into_iter()
            .collect();
        let contested = contested_existing_ids(&tied, &existing);
        assert_eq!(
            split_keepers(&tied, &existing, &contested, &run_ids)[&7],
            p("w")
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
            image: None,
            privacy: None,
            entry_type: EntryType::Track,
            release_date: None,
            specific_data: crate::providers::types::EntrySpecificData::Track {
                duration_ms: vec![],
                positions: HashMap::new(),
                songs: vec![],
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

    /// A karaoke-stream video that MusicBrainz models as one recording per
    /// song, each linking to the video (so all of them share one entry), is
    /// a container of songs: each recording's cover edge is kept as a
    /// `has_rel`, but the entry gets no edition relation to either original.
    #[tokio::test]
    async fn flush_writes_no_original_relation_for_a_multi_song_entry() -> anyhow::Result<()> {
        let db = MusicDb::new("sqlite::memory:").await?;
        let stream = p("stream_video");
        let song = |work: &str| PairMetadata {
            specific_data: crate::providers::types::EntrySpecificData::Track {
                duration_ms: vec![],
                positions: HashMap::new(),
                songs: vec![vec![work.to_string()]],
            },
            ..track_metadata()
        };

        let state = State::new();
        {
            let mut metadata = state.metadata.lock().unwrap();
            metadata.insert(stream.clone(), track_metadata());
            for (segment, original, work) in
                [("seg_a", "orig_a", "work_a"), ("seg_b", "orig_b", "work_b")]
            {
                metadata.insert(p(segment), song(work));
                metadata.insert(p(original), song(work));
                state.push_is_rel(p(segment), stream.clone(), p(segment), LinkVia::Fetch);
                state.has_rel.lock().unwrap().push(ChildEdge {
                    parent: p(segment),
                    child: p(original),
                    disc_no: None,
                    track_no: None,
                    contributions: vec![],
                    original_relation_kind: Some("cover".to_string()),
                });
            }
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

        let stream_id = db.find_entry_id_by_pair(&stream.0, &stream.1).await?;
        assert_eq!(
            db.find_entry_id_by_pair("x", "seg_a").await?,
            stream_id,
            "the per-song recordings share the video's entry"
        );
        assert!(db.all_relations().await?.is_empty());
        assert_eq!(
            db.all_child_rows().await?.len(),
            2,
            "per-recording edges kept"
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
        state.push_is_rel(
            existing_pair.clone(),
            new_sibling.clone(),
            existing_pair.clone(),
            LinkVia::Fetch,
        );

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

    fn release_metadata() -> PairMetadata {
        PairMetadata {
            image: None,
            privacy: None,
            entry_type: EntryType::Release,
            release_date: None,
            specific_data: crate::providers::types::EntrySpecificData::Release {
                release_type: None,
                num_discs: None,
                num_tracks: None,
            },
            aliases: vec![],
        }
    }

    fn listed_track(state: &State, parent: &Pair, child: &Pair, name: &str) {
        state.push_has_rel(ChildEdge {
            parent: parent.clone(),
            child: child.clone(),
            disc_no: None,
            track_no: Some(1),
            contributions: vec![],
            original_relation_kind: None,
        });
        state.insert_stub(
            child.clone(),
            StubInfo {
                image: None,
                entry_type: EntryType::Track,
                name: Some(name.to_string()),
                duration_ms: Some(180_000),
            },
        );
    }

    async fn flush_final(db: &MusicDb, state: State) -> anyhow::Result<()> {
        flush(
            Arc::new(state),
            &[],
            db,
            &DedupConfig::default(),
            None,
            true,
        )
        .await?;
        Ok(())
    }

    async fn aliases_of(db: &MusicDb, pair: &Pair) -> anyhow::Result<Vec<(String, bool)>> {
        Ok(db
            .all_alias_rows()
            .await?
            .into_iter()
            .filter(|a| a.source == pair.0 && a.identifier == pair.1)
            .map(|a| (a.name, a.primary_alias))
            .collect())
    }

    /// A listed-but-never-fetched child is stored as a stub carrying the
    /// listing's type, name and duration. A later fetch promotes it in place,
    /// and after that listings no longer add names to it.
    #[tokio::test]
    async fn unfetched_child_is_stored_as_stub_and_promoted_by_fetch() -> anyhow::Result<()> {
        let db = MusicDb::new("sqlite::memory:").await?;
        let album = p("album");
        let track = p("track");

        let state = State::new();
        state.insert_metadata(album.clone(), release_metadata());
        listed_track(&state, &album, &track, "Song");
        flush_final(&db, state).await?;

        let track_id = db
            .find_entry_id_by_pair(&track.0, &track.1)
            .await?
            .expect("stub row");
        let row = db.source_rows_by_entry_ids(&[track_id]).await?.remove(0);
        assert_eq!(row.fetched_at, None);
        assert_eq!(row.duration_ms, vec![180_000]);
        assert_eq!(
            db.entry_rows_by_ids(&[track_id]).await?[0].entry_type,
            "track"
        );
        assert_eq!(
            aliases_of(&db, &track).await?,
            vec![("Song".to_string(), false)]
        );

        let state = State::new();
        state.insert_metadata(track.clone(), track_metadata());
        flush_final(&db, state).await?;
        assert_eq!(
            db.find_entry_id_by_pair(&track.0, &track.1).await?,
            Some(track_id)
        );
        let row = db.source_rows_by_entry_ids(&[track_id]).await?.remove(0);
        assert!(row.fetched_at.is_some(), "a fetch promotes the stub");

        let state = State::new();
        state.insert_metadata(album.clone(), release_metadata());
        listed_track(&state, &album, &track, "Song (Some Other Listing)");
        flush_final(&db, state).await?;
        assert_eq!(
            aliases_of(&db, &track).await?,
            vec![("Song".to_string(), false)]
        );
        Ok(())
    }

    /// A fetched pair's own image is stored as `fetch`; a stub's image from
    /// its parent's listing as `listing`, and a later fetch replaces it.
    #[tokio::test]
    async fn images_are_stored_with_their_origin() -> anyhow::Result<()> {
        use crate::providers::types::Image;
        let db = MusicDb::new("sqlite::memory:").await?;
        let album = p("album");
        let track = p("track");
        let images = |db: &MusicDb| {
            let db = db.clone();
            let pairs = vec![album.clone(), track.clone()];
            async move {
                let mut rows: Vec<(String, String, bool)> = db
                    .pair_image_rows(&pairs)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|r| (r.identifier, r.url, r.fetched))
                    .collect();
                rows.sort();
                rows
            }
        };

        let state = State::new();
        state.insert_metadata(
            album.clone(),
            PairMetadata {
                image: Some(Image::new("cover.jpg", Some(300), Some(300))),
                ..release_metadata()
            },
        );
        listed_track(&state, &album, &track, "Song");
        state.insert_stub(
            track.clone(),
            StubInfo {
                image: Some(Image::new("thumb.jpg", None, None)),
                entry_type: EntryType::Track,
                name: None,
                duration_ms: None,
            },
        );
        flush_final(&db, state).await?;
        assert_eq!(
            images(&db).await,
            vec![
                (album.1.clone(), "cover.jpg".into(), true),
                (track.1.clone(), "thumb.jpg".into(), false),
            ]
        );

        let state = State::new();
        state.insert_metadata(
            track.clone(),
            PairMetadata {
                image: Some(Image::new("own.jpg", None, None)),
                ..track_metadata()
            },
        );
        flush_final(&db, state).await?;
        assert_eq!(
            images(&db).await[1],
            (track.1.clone(), "own.jpg".into(), true)
        );
        Ok(())
    }

    /// Stub info is recorded when a child is listed, but the edge only once
    /// the whole listing finished, so it can reach a periodic flush first. It
    /// must survive until the flush that writes the pair.
    #[tokio::test]
    async fn stub_info_waits_for_its_edge() -> anyhow::Result<()> {
        let db = MusicDb::new("sqlite::memory:").await?;
        let album = p("album");
        let track = p("track");

        let state = Arc::new(State::new());
        state.insert_stub(
            track.clone(),
            StubInfo {
                image: None,
                entry_type: EntryType::Track,
                name: Some("Song".to_string()),
                duration_ms: Some(180_000),
            },
        );
        flush(
            Arc::clone(&state),
            &[],
            &db,
            &DedupConfig::default(),
            None,
            false,
        )
        .await?;
        assert!(
            db.find_entry_id_by_pair(&track.0, &track.1)
                .await?
                .is_none()
        );
        assert_eq!(state.stubs.lock().unwrap().len(), 1, "stub info is kept");

        state.insert_metadata(album.clone(), release_metadata());
        state.push_has_rel(ChildEdge {
            parent: album.clone(),
            child: track.clone(),
            disc_no: None,
            track_no: Some(1),
            contributions: vec![],
            original_relation_kind: None,
        });
        flush(
            Arc::clone(&state),
            &[],
            &db,
            &DedupConfig::default(),
            None,
            true,
        )
        .await?;
        let track_id = db
            .find_entry_id_by_pair(&track.0, &track.1)
            .await?
            .expect("stub row");
        let row = db.source_rows_by_entry_ids(&[track_id]).await?.remove(0);
        assert_eq!(row.duration_ms, vec![180_000]);
        assert_eq!(
            aliases_of(&db, &track).await?,
            vec![("Song".to_string(), false)]
        );
        Ok(())
    }

    fn artist_metadata() -> PairMetadata {
        PairMetadata {
            image: None,
            privacy: None,
            entry_type: EntryType::Artist,
            release_date: None,
            specific_data: crate::providers::types::EntrySpecificData::Artist,
            aliases: vec![],
        }
    }

    fn edge(parent: &Pair, child: &Pair, role: Option<&str>) -> ChildEdge {
        ChildEdge {
            parent: parent.clone(),
            child: child.clone(),
            disc_no: None,
            track_no: None,
            contributions: role
                .map(|role| crate::providers::types::Contribution {
                    role: role.to_string(),
                    main_artist: true,
                    extra: serde_json::Value::Null,
                    source: "x".into(),
                })
                .into_iter()
                .collect(),
            original_relation_kind: None,
        }
    }

    /// One import run over `fetched` (pair, metadata) and `edges`: flush, then
    /// commit (`commit = true`) or roll back.
    async fn run(
        db: &MusicDb,
        fetched: Vec<(Pair, PairMetadata)>,
        edges: Vec<ChildEdge>,
        commit: bool,
    ) -> anyhow::Result<()> {
        run_with_links(db, fetched, edges, vec![], commit).await
    }

    /// `run`, with `links` as (a, b, stated by, via) `is_rel` events.
    async fn run_with_links(
        db: &MusicDb,
        fetched: Vec<(Pair, PairMetadata)>,
        edges: Vec<ChildEdge>,
        links: Vec<(&Pair, &Pair, &Pair, LinkVia)>,
        commit: bool,
    ) -> anyhow::Result<()> {
        let state = Arc::new(State::new());
        for (pair, meta) in fetched {
            state.insert_metadata(pair, meta);
        }
        for e in edges {
            state.push_has_rel(e);
        }
        for (a, b, by, via) in links {
            state.push_is_rel(a.clone(), b.clone(), by.clone(), via);
        }
        let run_id = db.start_import_run().await?;
        flush(
            Arc::clone(&state),
            &[],
            db,
            &DedupConfig::default(),
            Some(run_id),
            true,
        )
        .await?;
        if commit {
            db.commit_import_run(run_id, &state.take_replacements())
                .await?;
        } else {
            db.rollback_import_run(run_id).await?;
        }
        Ok(())
    }

    async fn children_of(db: &MusicDb, parent: &Pair) -> anyhow::Result<Vec<String>> {
        let mut out: Vec<String> = db
            .all_child_rows()
            .await?
            .into_iter()
            .filter(|r| r.parent_source == parent.0 && r.parent_identifier == parent.1)
            .map(|r| r.child_identifier)
            .collect();
        out.sort();
        Ok(out)
    }

    async fn credited_on(db: &MusicDb, track: &Pair) -> anyhow::Result<Vec<String>> {
        let mut out: Vec<String> = db
            .all_contrib_rows()
            .await?
            .into_iter()
            .filter(|r| r.source == track.0 && r.identifier == track.1)
            .map(|r| r.artist_identifier)
            .collect();
        out.sort();
        Ok(out)
    }

    /// Re-fetching an album and a track replaces their edges at commit: a
    /// track dropped from the album and a removed credit disappear. An
    /// artist's discography edges are only ever added to, and a rolled-back
    /// run deletes nothing.
    #[tokio::test]
    async fn refetch_replaces_stale_edges_at_commit() -> anyhow::Result<()> {
        let db = MusicDb::new("sqlite::memory:").await?;
        let (artist, album, t1, t2) = (p("artist"), p("album"), p("t1"), p("t2"));
        let (performer, composer) = (p("performer"), p("composer"));

        run(
            &db,
            vec![
                (artist.clone(), artist_metadata()),
                (album.clone(), release_metadata()),
                (t1.clone(), track_metadata()),
            ],
            vec![
                edge(&artist, &album, None),
                edge(&artist, &t2, None),
                edge(&album, &t1, None),
                edge(&album, &t2, None),
                edge(&t1, &performer, Some("performer")),
                edge(&t1, &composer, Some("composer")),
            ],
            true,
        )
        .await?;

        // Upstream changed: t2 left the album, the composer credit was removed,
        // and the artist listing (policy-filtered) now shows only the album.
        let refetch = || {
            (
                vec![
                    (artist.clone(), artist_metadata()),
                    (album.clone(), release_metadata()),
                    (t1.clone(), track_metadata()),
                ],
                vec![
                    edge(&artist, &album, None),
                    edge(&album, &t1, None),
                    edge(&t1, &performer, Some("performer")),
                ],
            )
        };

        let (fetched, edges) = refetch();
        run(&db, fetched, edges, false).await?;
        assert_eq!(children_of(&db, &album).await?, vec!["t1", "t2"]);
        assert_eq!(credited_on(&db, &t1).await?, vec!["composer", "performer"]);

        let (fetched, edges) = refetch();
        run(&db, fetched, edges, true).await?;
        assert_eq!(children_of(&db, &album).await?, vec!["t1"]);
        assert_eq!(credited_on(&db, &t1).await?, vec!["performer"]);
        assert_eq!(children_of(&db, &artist).await?, vec!["album", "t2"]);
        Ok(())
    }

    fn named(mut meta: PairMetadata, names: &[(&str, bool)]) -> PairMetadata {
        meta.aliases = names
            .iter()
            .map(|&(name, primary)| crate::providers::types::Alias {
                name: name.to_string(),
                source: "x".into(),
                locale: None,
                extra: serde_json::Value::Null,
                primary,
            })
            .collect();
        meta
    }

    async fn names_of(db: &MusicDb, pair: &Pair) -> anyhow::Result<Vec<(String, bool)>> {
        let mut out = aliases_of(db, pair).await?;
        out.sort();
        Ok(out)
    }

    /// A re-fetch replaces a pair's aliases at commit, artists included: a
    /// renamed primary name and a dropped alias disappear, from the search
    /// index too, and a listing name goes once the pair itself is fetched.
    /// A rolled-back run, or a fetch reporting no aliases, deletes nothing.
    #[tokio::test]
    async fn refetch_replaces_stale_aliases_at_commit() -> anyhow::Result<()> {
        let db = MusicDb::new("sqlite::memory:").await?;
        let (artist, album, t1) = (p("artist"), p("album"), p("t1"));
        let state = State::new();
        state.insert_metadata(
            artist.clone(),
            named(artist_metadata(), &[("Old Name", true), ("Kept", false)]),
        );
        state.push_has_rel(edge(&artist, &album, None));
        state.insert_stub(
            album.clone(),
            StubInfo {
                entry_type: EntryType::Release,
                name: Some("Listing Title".into()),
                duration_ms: None,
                image: None,
            },
        );
        flush_final(&db, state).await?;
        assert_eq!(
            names_of(&db, &album).await?,
            vec![("Listing Title".to_string(), false)]
        );

        let refetch = || {
            vec![
                (
                    artist.clone(),
                    named(artist_metadata(), &[("New Name", true), ("Kept", false)]),
                ),
                (
                    album.clone(),
                    named(release_metadata(), &[("Album Title", true)]),
                ),
                (t1.clone(), track_metadata()),
            ]
        };
        run(&db, refetch(), vec![], false).await?;
        assert_eq!(names_of(&db, &artist).await?.len(), 2);

        run(&db, refetch(), vec![], true).await?;
        assert_eq!(
            names_of(&db, &artist).await?,
            vec![("Kept".to_string(), false), ("New Name".to_string(), true)]
        );
        assert_eq!(
            names_of(&db, &album).await?,
            vec![("Album Title".to_string(), true)]
        );
        assert!(
            db.search_entry_ids_by_alias("Old Name", 10)
                .await?
                .is_empty()
        );
        assert!(
            db.search_entry_ids_by_alias("Listing", 10)
                .await?
                .is_empty()
        );
        assert_eq!(db.search_entry_ids_by_alias("New Name", 10).await?.len(), 1);

        run(&db, vec![(artist.clone(), artist_metadata())], vec![], true).await?;
        assert_eq!(names_of(&db, &artist).await?.len(), 2);
        Ok(())
    }

    /// A pair the provider reports deleted is marked at commit, keeping its
    /// data, and unmarked by its next successful fetch. A rolled-back run
    /// marks nothing.
    #[tokio::test]
    async fn not_found_marks_pair_gone_until_refetched() -> anyhow::Result<()> {
        let db = MusicDb::new("sqlite::memory:").await?;
        let t1 = p("t1");
        run(&db, vec![(t1.clone(), track_metadata())], vec![], true).await?;
        async fn gone_at(db: &MusicDb) -> anyhow::Result<Option<i64>> {
            let id = db.find_entry_id_by_pair("x", "t1").await?.unwrap();
            Ok(db.source_rows_by_entry_ids(&[id]).await?[0].gone_at)
        }

        for commit in [false, true] {
            let state = Arc::new(State::new());
            state.note_gone(t1.clone());
            let run_id = db.start_import_run().await?;
            flush(
                Arc::clone(&state),
                &[],
                &db,
                &DedupConfig::default(),
                Some(run_id),
                true,
            )
            .await?;
            if commit {
                db.commit_import_run(run_id, &state.take_replacements())
                    .await?;
            } else {
                db.rollback_import_run(run_id).await?;
            }
            assert_eq!(gone_at(&db).await?.is_some(), commit);
        }
        assert!(db.find_entry_id_by_pair(&t1.0, &t1.1).await?.is_some());

        run(&db, vec![(t1.clone(), track_metadata())], vec![], true).await?;
        assert_eq!(gone_at(&db).await?, None);
        Ok(())
    }

    async fn provider_relations(db: &MusicDb) -> anyhow::Result<Vec<String>> {
        let mut out: Vec<String> = db
            .all_relations()
            .await?
            .into_iter()
            .filter(|r| r.origin == PROVIDER_ORIGIN)
            .map(|r| r.kind)
            .collect();
        out.sort();
        Ok(out)
    }

    /// A cover relation goes at commit once a re-fetch of the cover no
    /// longer links it to the original; a rolled-back run keeps it, and the
    /// remix's relation to the same original is untouched.
    #[tokio::test]
    async fn refetch_drops_provider_relation_without_edge() -> anyhow::Result<()> {
        let db = MusicDb::new("sqlite::memory:").await?;
        let (original, cover, remix) = (p("original"), p("cover"), p("remix"));
        let derived = |from: &Pair, kind: &str| {
            let mut e = edge(from, &original, None);
            e.original_relation_kind = Some(kind.to_string());
            e
        };
        let tracks = |pairs: &[&Pair]| {
            pairs
                .iter()
                .map(|&p| (p.clone(), track_metadata()))
                .collect::<Vec<_>>()
        };
        run(
            &db,
            tracks(&[&original, &cover, &remix]),
            vec![derived(&cover, "cover"), derived(&remix, "remix")],
            true,
        )
        .await?;
        assert_eq!(provider_relations(&db).await?, vec!["cover", "remix"]);

        run(&db, tracks(&[&cover]), vec![], false).await?;
        assert_eq!(provider_relations(&db).await?, vec!["cover", "remix"]);

        run(&db, tracks(&[&cover]), vec![], true).await?;
        assert_eq!(provider_relations(&db).await?, vec!["remix"]);
        Ok(())
    }

    async fn entry_of(db: &MusicDb, pair: &Pair) -> anyhow::Result<i64> {
        Ok(db.find_entry_id_by_pair(&pair.0, &pair.1).await?.unwrap())
    }

    async fn stored_links(db: &MusicDb, pairs: &[&Pair]) -> anyhow::Result<usize> {
        let pairs: Vec<Pair> = pairs.iter().map(|&p| p.clone()).collect();
        Ok(db.links_among(&pairs).await?.len())
    }

    /// Recording R wrongly links Spotify track S, merging them. Once R's
    /// re-fetch stops stating the link it is deleted, but the entry only
    /// splits with `LINKS_COMPLETE` set, and only when nothing else
    /// (here, album A's listing) still states it. The split-off pair keeps
    /// the entry's type.
    #[tokio::test]
    async fn retracted_link_splits_entry_only_when_enabled() -> anyhow::Result<()> {
        use LinkVia::{Fetch, Listing};
        let (r, s, album) = (p("r"), p("s"), p("album"));
        async fn merged(db: &MusicDb) -> anyhow::Result<bool> {
            Ok(entry_of(db, &p("r")).await? == entry_of(db, &p("s")).await?)
        }
        for enabled in [false, true] {
            let db = MusicDb::new("sqlite::memory:").await?;
            // A new DB starts complete; one with unrecorded links doesn't.
            db.set_setting(
                crate::musicdb::LINKS_COMPLETE,
                if enabled { "1" } else { "0" },
            )
            .await?;
            run_with_links(
                &db,
                vec![(r.clone(), track_metadata())],
                vec![],
                vec![(&r, &s, &r, Fetch)],
                true,
            )
            .await?;
            assert!(merged(&db).await?);
            assert_eq!(stored_links(&db, &[&r, &s]).await?, 1);

            run(&db, vec![(r.clone(), track_metadata())], vec![], true).await?;
            assert_eq!(stored_links(&db, &[&r, &s]).await?, 0);
            assert_eq!(merged(&db).await?, !enabled);
            if enabled {
                let rows = db.entry_rows_by_ids(&[entry_of(&db, &s).await?]).await?;
                assert_eq!(rows[0].entry_type, "track");
            }
        }

        // Still stated by another asserter: no split.
        let db = MusicDb::new("sqlite::memory:").await?;
        db.set_setting(crate::musicdb::LINKS_COMPLETE, "1").await?;
        run_with_links(
            &db,
            vec![
                (r.clone(), track_metadata()),
                (album.clone(), release_metadata()),
            ],
            vec![edge(&album, &r, None)],
            vec![(&r, &s, &r, Fetch), (&r, &s, &album, Listing)],
            true,
        )
        .await?;
        run(&db, vec![(r.clone(), track_metadata())], vec![], true).await?;
        assert!(merged(&db).await?);
        assert_eq!(stored_links(&db, &[&r, &s]).await?, 1);
        Ok(())
    }

    /// A pair found deleted upstream states no links, so a split leaves it
    /// with the entry's largest group instead of on its own.
    #[tokio::test]
    async fn split_keeps_gone_pair_with_largest_group() -> anyhow::Result<()> {
        use LinkVia::Fetch;
        let (r, s, i, old) = (p("r"), p("s"), p("i"), p("old"));
        let db = MusicDb::new("sqlite::memory:").await?;
        db.set_setting(crate::musicdb::LINKS_COMPLETE, "1").await?;
        // r states i, s and `old`, all one entry.
        run_with_links(
            &db,
            vec![
                (r.clone(), track_metadata()),
                (old.clone(), track_metadata()),
            ],
            vec![],
            vec![
                (&r, &i, &r, Fetch),
                (&r, &s, &r, Fetch),
                (&r, &old, &r, Fetch),
            ],
            true,
        )
        .await?;
        let state = Arc::new(State::new());
        state.note_gone(old.clone());
        let run_id = db.start_import_run().await?;
        flush(
            Arc::clone(&state),
            &[],
            &db,
            &DedupConfig::default(),
            Some(run_id),
            true,
        )
        .await?;
        db.commit_import_run(run_id, &state.take_replacements())
            .await?;

        // r drops s and `old` (merged away upstream); `old` is gone, so it
        // stays while s splits off.
        run_with_links(
            &db,
            vec![(r.clone(), track_metadata())],
            vec![],
            vec![(&r, &i, &r, Fetch)],
            true,
        )
        .await?;
        let entry = entry_of(&db, &r).await?;
        assert_eq!(entry_of(&db, &i).await?, entry);
        assert_ne!(entry_of(&db, &s).await?, entry);
        assert_eq!(entry_of(&db, &old).await?, entry);
        Ok(())
    }

    /// Recordings `orig` and `club` are barrier anchors in entries of their
    /// own, each with a Spotify track. A later flush that only sees the two
    /// Spotify tracks linked (a shared ISRC) must not merge the entries:
    /// that would carry both anchors, never in the flush, into one.
    #[tokio::test]
    async fn barrier_holds_for_anchor_pairs_outside_the_flush() -> anyhow::Result<()> {
        use LinkVia::Fetch;
        let config: DedupConfig = serde_yaml_ng::from_str(
            "groups:
  - name: g
    anchors:
      orig: { members: ['x:orig'] }
      club: { members: ['x:club'] }",
        )?;
        let (orig, club, s1, s2) = (p("orig"), p("club"), p("s1"), p("s2"));
        let db = MusicDb::new("sqlite::memory:").await?;
        for (anchor, track) in [(&orig, &s1), (&club, &s2)] {
            let state = State::new();
            state.insert_metadata(anchor.clone(), track_metadata());
            state.push_is_rel(anchor.clone(), track.clone(), anchor.clone(), Fetch);
            flush(Arc::new(state), &[], &db, &config, None, true).await?;
        }
        assert_ne!(entry_of(&db, &orig).await?, entry_of(&db, &club).await?);

        let state = State::new();
        state.insert_metadata(s1.clone(), track_metadata());
        state.push_is_rel(s1.clone(), s2.clone(), s1.clone(), Fetch);
        flush(Arc::new(state), &[], &db, &config, None, true).await?;
        assert_ne!(entry_of(&db, &orig).await?, entry_of(&db, &club).await?);
        Ok(())
    }

    /// An ISRC `bridge` both anchors list: with links recorded, the anchor
    /// that reaches it takes only the bridge, and the club mix's own Spotify
    /// track (linked to the club mix, not the bridge) stays put. A stub that
    /// nothing inside the entry links to stays too.
    #[tokio::test]
    async fn barrier_entry_regroups_by_links() -> anyhow::Result<()> {
        use LinkVia::Fetch;
        let config: DedupConfig = serde_yaml_ng::from_str(
            "groups:
  - name: g
    anchors:
      orig: { members: ['x:orig'] }
      club: { members: ['x:club'] }",
        )?;
        let (orig, club, bridge, track, leftover) =
            (p("orig"), p("club"), p("bridge"), p("track"), p("leftover"));
        let db = MusicDb::new("sqlite::memory:").await?;
        db.set_setting(crate::musicdb::LINKS_COMPLETE, "1").await?;
        let state = State::new();
        state.insert_metadata(club.clone(), track_metadata());
        state.insert_metadata(track.clone(), track_metadata());
        for (a, b) in [(&club, &bridge), (&club, &track), (&bridge, &leftover)] {
            state.push_is_rel(a.clone(), b.clone(), a.clone(), Fetch);
        }
        flush(Arc::new(state), &[], &db, &config, None, true).await?;
        // `leftover` was linked only through the bridge, by a pair outside.
        db.delete_links(&[LinkRow {
            link: link_key(&bridge, &leftover),
            asserted_by: bridge.clone(),
            via: crate::musicdb::LINK_VIA_FETCH,
        }])
        .await?;

        let state = State::new();
        state.insert_metadata(orig.clone(), track_metadata());
        state.push_is_rel(orig.clone(), bridge.clone(), orig.clone(), Fetch);
        flush(Arc::new(state), &[], &db, &config, None, true).await?;
        let club_entry = entry_of(&db, &club).await?;
        assert_ne!(entry_of(&db, &orig).await?, club_entry);
        assert_eq!(entry_of(&db, &track).await?, club_entry);
        assert_eq!(entry_of(&db, &leftover).await?, club_entry);
        Ok(())
    }
}
