# Plan: softmatch validation harness

Status: proposal, no code yet.

## Why

Before refining the softmatch heuristic (`config/match.example.rhai` +
`pipeline/softmatch.rs`) we need a fixed yardstick so every change shows a
measured delta instead of a vibe. Today there is none that measures what ships:

- A full dry run on the live library (26,573 entries, 2026-10-02) produces
  368k candidate pairs and 15,092 track MERGEs (more than one per track entry),
  5,759 track RELATEs, 1,113 artist MERGEs, 266 release_group MERGEs, and
  **zero** release MERGE/RELATE. Nobody knows whether those track merges are
  90% or 50% right.
- Scoring is single-threaded Rhai at ~1.9k pairs/s (193 s of a 194 s run).
  Retrieval takes ~1 s. "Doesn't scale" needs to become a number we track.
- The September tooling (`poc/dedup-poc`, `scripts/export_dedup_*`) evaluates
  a Python *reimplementation* of candidate generation against a frozen 3.1k
  corpus. It never runs the Rust/Rhai path that actually ships.
- The 516 LLM-labeled + adjudicated pairs (pilot-v3: 400, active-v4: 116) are
  keyed to that corpus. Only ~144 resolve against the re-imported live DB
  (the rest have pairs that no longer exist there).

## Shape: two snapshots, three metric families

| Snapshot | Size | Used for |
|---|---|---|
| `data/dedup-corpus-v3.db` (existing, migrated on a copy) | 3.1k entries | **Decision quality** on the 516 labels, all of which resolve here |
| `data/eval/live-<date>.db` (frozen copy of `musiclib.db`) | 26.5k entries | **Retrieval recall** on label-free probes, **constraint violations**, **cost** |

Snapshots are immutable once a baseline is recorded against them; the report
records each snapshot's sha256. Re-snapshotting is a deliberate event that
resets the baseline.

### 1. Retrieval (does the pair reach `decide` at all?)

Ground truth, all label-free and derived from the snapshot:

- **Split probes** (port `export_dedup_retrieval_probes.py` to read musiclib
  schema directly): take an entry that cross-linking unified from ≥2 sources,
  partition its `(source, identifier)` pairs into two disjoint halves, and
  replace the entry with two pseudo-entries built from each half. Hard-ID
  overlap is gone by construction (the halves share no pair), so the matcher
  must reunite them on soft evidence. Stratified by `script_relation`
  (same-script / cjk↔latin / mixed) and exact-alias-overlap — this is the
  stratum that answers "do we need a semantic model, or does romaji blocking
  cover it?"
- **Provider relations**: the 6.2k MB-asserted `entry_relation` rows
  (cover 5,753 / remix 303 / arrangement 143) are positives for RELATE
  retrieval and *negatives* for MERGE.

Metrics: recall per entry type and stratum; candidates per entry (the budget);
per-channel **marginal** recall (recall lost when that channel alone is
removed), which is how any embedding model or new channel has to earn its keep.

### 2. Decision quality (is the verdict right?)

Gold, in trust tiers reported separately, never pooled silently:

| Tier | Source | Labels |
|---|---|---|
| `provider` | MB relations | different_identity + relation kind |
| `probe` | split probes | same_identity |
| `adjudicated` | pilot-v3 + active-v4 effective judgments (LLM + blind adjudication) | same / different / insufficient, + relations |
| `human` | future: player UI `dedup_feedback`, review of diffs | any |

Metrics per entry type and tier: MERGE precision (the headline — a false
merge is the costly error), MERGE recall, RELATE precision/recall by kind,
confusion matrix, and end-to-end recall = retrieval × decision. Every false
MERGE is listed with both sides' titles/sources in the report.

**Dev/test split**: tuning against the same pairs we report on overfits fast.
Assign each gold item to dev (70%) or test (30%) by hash of its *entity
cluster* (connected component of the item's pairs), so one entity never lands
on both sides. Refinement iterates on dev; test is read only when comparing a
finished change against baseline.

### 3. Constraint violations (label-free precision proxy, live snapshot)

Take the transitive closure of soft MERGEs and count clusters that contain
two different IDs from a namespace that should be 1:1 within one entity
(first candidates: `musicbrainz` recording/release/artist MBIDs). A cluster
spanning two MB recordings is almost always a false merge. Also report the
largest-cluster size and cluster-size histogram — runaway transitive chains
are a scaling failure mode the pairwise metrics can't see.

### 4. Cost

Candidates generated, pairs/s, wall time per phase, peak RSS — for both the
full scan (`match_db`) and the online path (`match_new_entries`, simulated by
treating a fixed 1k-entry slice of the live snapshot as "new").

## Harness

A Rust binary `softmatch_eval` (not Python), so it measures exactly what
ships:

```
cargo run --release --bin softmatch_eval -- \
  --snapshot data/eval/live-2026-10-02.db --gold data/eval/gold.jsonl \
  --script config/match.example.rhai --out data/eval/reports/<name>.json \
  [--baseline data/eval/reports/baseline.json]
```

- Read-only against the snapshot (opens a temp copy; nothing is applied).
- Builds the in-memory `EntryInfo` universe, swaps probed entries for their
  two halves (all probes at once — halves of different probes coexist fine),
  then runs the real retrieval + scoring. Needs one refactor in
  `softmatch.rs`: expose "build `EntryInfo` from an arbitrary pair subset" and
  "retrieve + score over an in-memory universe" as functions separate from
  the DB-writing `match_db` loop.
- Writes `report.json` and a self-contained `report.html`. With
  `--baseline`, adds a diff section: metric deltas, plus every gold item whose
  verdict changed (fixed / broke). Changed verdicts on *unlabeled* pairs are
  exported as the next labeling batch — the cheapest place to spend label
  effort.

Gold file: one JSONL record per item, keyed by stable pairs, never entry ids:

```json
{"item_id": "...", "tier": "adjudicated", "split": "dev", "entry_type": "track",
 "left": [["discogs", "https://..."]], "right": [["spotify", "..."]],
 "identity": "same_identity" | "different_identity" | "insufficient_evidence",
 "relations": [{"kind": "derived_from", "metadata": {...}}],
 "snapshot": "sha256:..."}
```

At eval time each side resolves to the entry/entries holding those pairs in
that snapshot; items whose pairs are missing are counted as "unresolvable"
(coverage is part of the report), not silently dropped.

## Phases

1. **Gold + snapshots.** Freeze the live snapshot; migrate a copy of
   the v3 corpus. Exporter (Rust, in the eval binary or a sibling) writes
   probes + provider-relation items; a one-off script converts pilot-v3 /
   active-v4 effective judgments into gold records with dev/test split.
2. **Eval binary, retrieval + cost.** Refactor softmatch for the in-memory
   universe; report recall/strata/channel marginals/cost. Record baseline.
3. **Decision + constraint metrics, baseline diff, HTML report.**
4. Only then: refine (romaji blocking, embeddings, track/release rules,
   parallel scoring), each change landing with its report delta.

## Open questions

- Should the `adjudicated` tier count toward the headline numbers, or only be
  reported alongside? (Memory notes it is noisy: LLM labels, and one known
  mislabel in the older 12-pair UI set.)
- Is the 3.1k v3 corpus representative enough of the current library for
  decision metrics, or do we also want a fresh labeled batch on the live
  snapshot early (e.g. 200 pairs, sampled including non-candidates)?
- Which namespaces are safe 1:1 for constraint violations beyond MB MBIDs?
  (Spotify track IDs and ISRCs are many-to-one per recording, so they're out.)
