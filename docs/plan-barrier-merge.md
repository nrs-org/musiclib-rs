# Barrier System Upgrade — Unify `members`/`claim`, give anchors merge power

Status: **design settled, not yet implemented.**

## Goal

Turn the barrier config from a *negative-only* constraint layer into the project's
**manual signed-edge layer**: an anchor asserts a positive merge of its pairs (same
entity) *and* a negative separation from sibling anchors. This is the
human-authored override that sits above URL cross-linking and the future heuristic
matcher (precedence: **manual > URL > heuristic**).

Two changes, independent and orderable:

1. **Collapse `claim` into `members`** — they are mechanically identical today.
2. **Give an anchor the power to merge its pairs even without a shared URL.**

## Finding (verified)

`members` and `claim` differ *only* as two struct fields (`src/pipeline/dedup.rs:38`
and `:41`). Their single consumer chains them and treats them identically:

```rust
// dedup.rs:69
for s in anchor.members.iter().chain(anchor.claim.iter()) {
    let canon = canonicalize_pair(providers, &p).await;
    barrier.insert(canon, id);   // pair -> (group_index, anchor_key)
}
```

No other code, test, or config path reads them apart. The distinction is purely
documentary (`members` = "pairs that define this entity"; `claim` = "ambiguous
pairs that belong exclusively here"). Both already mean *the same thing*: "this
pair belongs to this anchor" = positive membership within the anchor + negative
exclusion across sibling anchors.

## Change 1 — collapse `claim` into `members`

- Remove the `claim` field from `Anchor`; keep a single `members: Vec<String>`.
- **Back-compat shim:** accept `claim` via `#[serde(alias = "claim")]` is *not*
  enough (serde alias maps one key to one field). Instead either:
  - (a) keep a deprecated `claim` field that `compile` still chains, but document it
    as an alias and emit a one-time `warn!` when non-empty; or
  - (b) hard-remove and migrate the existing config files
    (`<config_dir>/dedup_barriers/*.yaml`, e.g. the honeyworks/chico file) so all
    pairs live under `members`.
- Recommendation: **(b) hard-remove** — there is exactly one real config file and
  the semantics are trivial, as the user notes. Do (a) only if external configs
  exist that we can't edit.
- Update `docs/CONFIG_REFERENCE.md` (the only doc mentioning `claim`).

This change is behavior-preserving on its own (the consumer already unions the two).

## Change 2 — anchors merge without a shared URL

### Problem

Today an anchor label only *refuses* cross-anchor unions; it never *forces* a
same-anchor union. `flush` seeds labels (`set_label`) but the only unions come from
`is_rel` edges and existing-entry cliques. So listing two pairs under one anchor
**does not guarantee they merge** — they merge only if URL linking already connects
them. Concrete gap: the `chico` anchor lists two MusicBrainz artist IDs
(`dbd0f795…`, `e841cf2f…`) that are the same artist but have no shared URL between
them; under current behavior they stay as two separate entries.

(Note the corrected principle: distinct authoritative IDs do **not** imply distinct
entities — upstream DBs contain duplicates, exactly as MB does here. So forcing a
manual merge of two MBIDs is legitimate.)

### Fix

In `flush` (`src/pipeline/flush.rs`), after the barrier labels are seeded
(around line 117–120) and **before** resolving classes, force-union every pair that
shares an anchor label *and* is present in the union-find's working set:

```rust
// Group barrier pairs by anchor, union anchor-mates that were actually seen.
let mut by_anchor: HashMap<AnchorId, Vec<Pair>> = HashMap::new();
for (pair, id) in &barrier {
    if all_pairs.contains(pair) {            // only materialized pairs
        by_anchor.entry(id.clone()).or_default().push(pair.clone());
    }
}
for mates in by_anchor.values() {
    for i in 1..mates.len() {
        // same label => union always permitted (never an Err)
        let _ = uf.union(&mates[0], &mates[i]);
    }
}
```

Properties:

- **Positive within anchor**: anchor-mates merge regardless of URL evidence.
- **Negative across anchors**: unchanged — `union` still returns `Err` across
  different labels, so siblings stay apart and the split logic re-homes them.
- **No phantom entries**: only pairs in `all_pairs` are unioned, so a never-imported
  claimed pair still creates no row (step 4 iterates `all_pairs`).
- **Interacts cleanly with split/merge**: `owned_existing_ids` + the existing-clique
  union still apply; a forced merge of two existing entries yields a normal
  multi-id class → one wins, the rest are merged (`merge_entries`).

### Reversibility

Barrier config is **persisted and replayed every `dedup` run** (`dedup_db` re-imports
the affected entities and re-applies the merged barrier). So a manual forced merge is
reversible by editing one line (remove the pair from `members`) and re-running
`dedup` — no "baked into entry_id, can't un-bake" problem. This is exactly the
property that makes the manual layer the authoritative, reversible override.

## Steps

1. Change 1: collapse the field; migrate the config file; update `CONFIG_REFERENCE.md`.
2. Change 2: add the force-union loop in `flush`; keep cross-anchor refusal.
3. Tests:
   - same-anchor pairs with **no** `is_rel` edge end up in one entry (new behavior);
   - cross-anchor pairs still never share an entry (regression guard);
   - a forced merge of two pre-existing entries triggers `merge_entries` and GCs the
     loser;
   - removing a member from `members` + re-running `dedup` splits them back apart
     (reversibility).
4. Re-run `dedup` against the live DB; confirm the `chico` two-MBID case collapses to
   one entry and honeyworks/chico/chico-with-honeyworks stay separate.

## Out of scope

Machine-generated soft links — those are noisy, high-volume, and individually
tombstoneable, so they belong in the heuristic matcher's own positive-edge store
(`docs/plan-soft-dedup.md`), **not** in hand-authored barrier files.
