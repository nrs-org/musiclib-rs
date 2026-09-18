# Rhai-Owned Softmatch Plugin Plan

## Goal

Move all softmatch-specific behavior into Rhai while keeping end-to-end runtime
within 50% of the current implementation.

The required boundary is strict:

- Rust provides only generic library, storage, indexing, vector, collection,
  transaction, and scripting APIs.
- Rhai owns entity interpretation, source preference, block-key derivation,
  candidate selection, feature definitions, thresholds, model decisions,
  barriers, merge winner selection, and cascade/rescoring behavior.
- No Rust symbol, constant, schema interpretation, or control flow should encode
  softmatch-specific concepts.

The target design is a Rhai package/plugin system over bulk native capabilities,
not a native matcher plugin.

```text
softmatch Rhai package
  owns candidate policy, thresholds, features, decisions, cascades
          |
          v
versioned generic host API
  EntryBatch | GenericIndex | VectorStore | Queue/Set | Transactions
          |
          v
Rust implementations
  SQLite | HNSW | regex/string kernels | persistence
```

## Prerequisite: the classifier must be validated first

This migration does not start on its own timeline. It starts once the
TypeSafe/Jev soft-dedup classifier's integration mode is settled and its
accuracy is validated, not before.

Rationale: which capabilities the generic host API must expose depends on
whether and how an external classifier judgment participates in final
policy. Migrating the existing heuristic pipeline first and bolting a
classifier capability on afterward risks a host API shaped around the wrong
policy, or a second migration once the classifier's real requirements are
known.

"Classifier is good" means, concretely:

- Precision/recall on a large, clean eval set (not the small silver-label
  sets used so far) meets an agreed threshold across all three entity types
  the softmatch pipeline handles (track, release, artist), not just the
  types where early results were strong.
- The integration mode is a settled production decision, not an open
  question: today's working hypothesis is "refine, not replace" — the
  existing heuristic proposes candidates and verdicts, the classifier
  corroborates or overrides near confidence boundaries — but this plan
  assumes whatever mode is chosen is fixed before Phase 0 begins.
- The non-determinism the classifier exhibits near decision boundaries is
  handled by a verdict cache keyed on `(pair, evidence-hash)`, and that
  caching has been exercised enough to trust it in production, since
  Phase 5 requires repeated runs to remain deterministic and idempotent.

Only once these hold does Phase 0 begin. Until then, changes to softmatch
policy continue to land in the existing Rust + Rhai hybrid, including
native predicates such as `derived_from` — this plan does not block that
work, and does not assume it will be reverted.

## Performance target

The migration must satisfy these release gates:

- No more than 1.5x the current optimized full-scan runtime.
- No more than 1.5x the current incremental-import p95 runtime.
- Candidate recall must not regress relative to the current implementation.
- Peak memory must remain bounded at the intended 100k-entry library size.

Current reference measurements from `docs/dedup-v2.md` are:

- 37,000 candidates through the legacy Rhai policy: 9.83 seconds.
- 48,039 candidates with semantic retrieval: 14.75 seconds, including 3.31
  seconds to build the HNSW indexes.
- 10,966 candidates through the native learned model: 0.72 seconds total, of
  which 0.60 seconds is scoring.

Expected performance with the proposed design:

| Path | Expected runtime relative to current |
|---|---:|
| Existing heuristic policy | 1.1-1.4x |
| Learned model with generic native kernels | 1.2-1.6x |
| Candidate generation in Rhai over bulk APIs | Locally 2-5x, but a small part of the heuristic runtime |
| Embedding and HNSW operations | Approximately unchanged |

The learned-model path is the highest-risk performance area. It must use generic
native set, similarity, and numerical kernels while keeping the selection and
meaning of features in Rhai.

## Plugin package

Package the implementation as ordinary Rhai source files:

```text
softmatch/
  plugin.toml
  main.rhai
  candidates.rhai
  features.rhai
  scorer.rhai
  cascade.rhai
```

An illustrative manifest:

```toml
api_version = 1
entrypoint = "main.rhai"
capabilities = ["library.read", "library.write", "index", "vectors", "judgment"]
```

The Rust runtime knows only a generic plugin lifecycle:

```rhai
fn init(host, options) {
    // Build and return script-owned state.
}

fn run(ctx) {
    // The script drives indexing, retrieval, scoring, and application.
}

fn destroy(ctx) {
    // Optional cleanup.
}
```

Rust must not retain a special `decide(a, b)` loop. The Rhai package decides
which entries and pairs exist, when they are evaluated, and how results affect
later work.

## Generic host API

### Library access

Expose raw music-library data in bounded batches:

```text
library.entries_batch(cursor, size, fields) -> EntryBatch
library.entries_by_ids(ids, fields) -> EntryBatch
library.changed_entry_ids(since) -> IdArray
library.relations_batch(cursor, size, fields) -> RelationBatch
```

The `fields` argument lets a script request only what it needs. The host must not
derive a softmatch-specific projection such as `EntryInfo`.

An entry may expose ordinary persisted music-library facts such as identifiers,
aliases, durations, release dates, contributions, and child edges. Rust must not
choose an authoritative title, classify sources as noisy, derive version markers,
or otherwise interpret those facts for matching.

### Generic index

Expose a namespaced postings index with batched operations:

```text
host.open_index(namespace) -> GenericIndex
index.replace_many(rows)
index.lookup_many(keys, max_results) -> LookupBatch
index.unindexed_ids(limit) -> IdArray
```

Rhai derives every key. Rust stores opaque byte or string keys and retrieves
members. Rust must not know channel names, duration buckets, title tokens,
trigrams, entry-type partitions, or block-size policy.

Example:

```rhai
let entries = library.entries_batch(cursor, 4096, REQUIRED_FIELDS);
let rows = [];

for entry in entries {
    rows.push([entry.id, block_keys(entry)]);
}

index.replace_many(rows);
```

### Vector storage and search

Expose generic vector operations:

```text
host.open_vectors(namespace, dimensions) -> VectorStore
vectors.upsert_many(rows)
vectors.missing_or_stale_many(keys) -> KeyArray
vectors.knn_many(queries) -> NeighborBatch
vectors.cosine_many(pairs) -> FloatArray
```

Rhai selects the embedding input, cache identity, namespaces, search sizes,
similarity thresholds, paging, and candidate policy. Rust implements vector
storage and nearest-neighbor mechanics.

### External judgment calls

By the time this migration starts, the validated classifier is part of
production policy (see the prerequisite above), so the host API must carry
it from Phase 1 onward rather than treating it as a later addition. Expose
it as a generic, provider-agnostic capability — Rust must not know it is
TypeSafe, Jev, or a classifier of any particular kind, only that scripts
can submit opaque requests and get opaque results back:

```text
host.open_judgment(namespace) -> JudgmentClient
judgment.submit_many(requests) -> JudgmentBatch
judgment.cached_results(keys) -> ResultBatch
```

A request is `{key, evidence_hash, payload}`. Rust treats `payload` and the
response body as opaque data it passes through to a configured provider; it
must not know what fields the payload contains, what a returned score means,
or at what value a decision should be made. Rhai builds the payload, reads
the result, and applies whatever threshold or decision logic it chooses —
same rule as every other feature in this document.

Non-determinism near decision boundaries is a property of the classifier,
not something Rhai should have to work around per call site. The host
therefore makes caching a mandatory part of the contract: a result for a
given `(namespace, key, evidence_hash)` is persisted the first time it is
produced and reused on every later call with the same key, unless the
script explicitly passes `force_refresh`. This is what keeps a second
`softmatch --apply` pass a no-op even though the underlying classifier is
not itself deterministic.

### Transactions and actions

Expose generic library mutations, preferably in batches:

```text
transactions.apply(actions) -> ApplyResults
```

Actions may cover generic library operations such as merging entries, inserting
or updating relations, and storing opaque review records. Rhai chooses action
types, endpoints, metadata, provenance, ordering, and winner/loser direction.

The host validates referential integrity and applies actions atomically, but does
not reinterpret or reorder policy decisions.

### Generic collections

Provide native opaque implementations of common data structures:

- integer set;
- pair set;
- FIFO/deque;
- string interner;
- generic postings builder;
- compact integer arrays;
- immutable entry and relation batches.

These types are reusable scripting primitives. They avoid representing every
large collection as `Dynamic` maps and nested arrays while containing no
softmatch vocabulary or behavior.

### Generic computational kernels

Keep expensive, domain-neutral operations native:

- Unicode normalization;
- tokenization and character n-grams;
- regex operations;
- set intersection, union, and Jaccard similarity;
- minimum absolute numeric delta;
- maximum pairwise similarity;
- edit distance and Jaro-Winkler;
- dot products and cosine similarity;
- logistic evaluation;
- sorting, grouping, and deduplication.

Rhai selects which operation to use and assigns it semantic meaning.

For example:

```rhai
fn features(a, b, vectors) {
    #{
        name_similarity:
            max_pairwise_similarity(a.aliases, b.aliases, "sequence"),

        duration_similarity:
            linear_similarity(min_abs_delta(a.durations, b.durations), 30_000),

        artist_jaccard:
            set_jaccard(a.peer_ids, b.peer_ids),

        semantic_similarity:
            vectors.cosine(a.id, b.id),
    }
}

let probability = logistic(model.intercept, model.coefficients, feature_row);
```

The feature names, inputs, scales, coefficients, and thresholds remain visible
and editable in Rhai.

## Avoiding boundary overhead

### Use opaque batch-backed handles

Do not rebuild a large Rhai map for every entry and every comparison. Return an
immutable native `EntryBatch` backed by an `Arc` snapshot and expose lightweight
entry handles:

```rhai
let a = entries.ref(pair[0]);
let b = entries.ref(pair[1]);
let overlap = set_jaccard(a.peer_ids, b.peer_ids);
```

These handles expose raw fields through generic getters. They must not contain
derived matching state.

### Batch all I/O

Never cross the Rust-Rhai boundary once per alias, block key, vector component,
or database row. Use batch sizes in the thousands where memory allows:

- fetch entries in batches;
- replace block keys in batches;
- look up many keys at once;
- score vectors in batches;
- persist actions in transactions.

### Compile once

Compile the Rhai package to an AST once and reuse it for the whole run. Benchmark
`OptimizationLevel::Full` against the default `Simple` level. Full optimization
must be enabled only after native functions have correct purity and volatility
metadata, because it may evaluate pure calls with constant arguments during
optimization.

Keep fast operators enabled. Avoid closures in hot loops when a direct loop is
equivalent. Intern repeated source names, entry types, field names, and channel
names.

### Parallel dry-run scoring

Dry-run scoring can be partitioned into deterministic batches evaluated by
independent Rhai engine/context instances. Applying merges remains deterministic
and sequential, or operates in explicit epochs where Rhai owns the epoch and
rescore policy.

Parallelism is an optimization, not a substitute for batching. The single-thread
implementation must remain correct and deterministic.

## Why not a native matcher plugin?

### Rhai native plugin modules

Rhai's plugin macros generate native Rust modules registered with an engine.
They are useful for implementing the generic capability layer but do not provide
a stable dynamic shared-library ABI. A native softmatch plugin would also put
softmatch internals back into Rust and violate the required boundary.

### C FFI

Do not put candidate generation, scoring, or cascades behind the C ABI.

The existing generic C FFI layer is appropriate for narrow numerical libraries,
such as batch embedding, but not for transferring the matcher object graph. A
matcher ABI would introduce:

- manual allocation and ownership;
- undefined behavior from signature or lifetime mistakes;
- costly array, string, and map marshalling;
- restrictions on asynchronous and cross-thread callbacks;
- process-wide crashes from native plugin bugs;
- another non-Rhai location for softmatch policy.

C FFI remains acceptable for operations with large homogeneous buffers and a
narrow contract, for example:

```c
int embed_batch(
    const char **texts,
    size_t count,
    float **output,
    size_t *dimension
);
```

If independently distributed native accelerators are ever required, define a
versioned generic capability ABI rather than a matcher ABI:

```c
uint32_t capability_abi_version(void);

int capability_call(
    uint32_t operation,
    const uint8_t *input,
    size_t input_len,
    uint8_t **output,
    size_t *output_len
);

void capability_free(uint8_t *output, size_t output_len);
```

Allowed operations would be generic kernels such as batch regex, vector
distance, sorting, or hashing. They must never be operations such as
`generate_track_candidates` or `decide_merge`.

Unless third-party binary distribution is a real requirement, statically
registered Rust capability modules are faster, safer, and simpler.

## Migration phases

### Phase 0: establish measurements and enforcement

1. Add separate timing spans for loading, projection, block-key derivation,
   index access, semantic retrieval, feature extraction, script evaluation,
   persistence, and cascade work.
2. Preserve the existing 3k-entry benchmark corpus.
3. Add a 100k-entry synthetic retrieval benchmark.
4. Record incremental import p50, p95, and p99 timings.
5. Add candidate-set and verdict snapshots for parity testing.
6. Add a CI boundary check that rejects softmatch-specific vocabulary and
   modules in Rust, with an explicit allowlist for the generic plugin runtime.

Phase 0 exit criteria:

- reproducible baseline timings;
- stable candidate and decision snapshots;
- agreed vocabulary/architecture boundary enforced in CI.

### Phase 1: generic batch and collection APIs

1. Add `EntryBatch` and raw entry handles.
2. Add generic native `IdSet`, `PairSet`, and queue types.
3. Add batched raw library reads.
4. Add generic transactional action application.
5. Retain the current softmatch implementation while benchmarking the new data
   path in shadow mode.

Phase 1 exit criteria:

- raw entry traversal through Rhai is within 1.5x of equivalent Rust traversal;
- no unbounded materialization is required at 100k entries.

### Phase 2: move block-key derivation and retrieval policy

1. Replace `dedup_block_key` semantics with a generic namespaced postings API.
2. Move all exact-name, token, trigram, duration, credit, tracklist, and related
   key construction into Rhai.
3. Move block caps, neighbor counts, overlap thresholds, and channel union policy
   into Rhai.
4. Run old and new retrieval in shadow mode and compare candidate sets.

Phase 2 exit criteria:

- candidate recall is no worse than current;
- full-scan runtime is no more than 1.5x current;
- Rust contains no candidate-channel or key-format knowledge.

### Phase 3: move semantic retrieval policy

1. Generalize the embedding cache into a namespaced vector store.
2. Move embedding-title selection and stale-key derivation into Rhai.
3. Move K, thresholds, page counts, type partitions, and candidate inclusion
   policy into Rhai.
4. Keep vector storage and HNSW/SQLite-vector mechanics native.

Phase 3 exit criteria:

- semantic candidate parity or deliberate, measured improvement;
- HNSW build/search time remains approximately unchanged;
- no matching-specific embedding policy remains in Rust.

### Phase 4: move learned features and decisions

1. Add generic native set, cross-product similarity, numeric-delta, and logistic
   kernels.
2. Define every feature in Rhai.
3. Load model coefficients and thresholds in Rhai.
4. Route classifier-informed decisions through the `judgment` capability,
   using the caching and integration mode fixed by the prerequisite above —
   this phase does not reopen "should the classifier be used," only ports
   the already-settled behavior.
5. Remove `pipeline::dedup_model` and the Rust learned-feature extractor.
6. Compare probabilities and decisions, including classifier-informed ones,
   against the existing implementation.

Phase 4 exit criteria:

- probability differences are within an agreed floating-point tolerance;
- model-path runtime is no more than 1.5x current, or the phase is blocked for
  further batching/vectorization work;
- classifier-informed decisions match the validated pre-migration behavior,
  including verdict-cache hits on repeated evidence;
- Rust does not know feature names, decision thresholds, or what the
  classifier's payload/response fields mean.

### Phase 5: move orchestration and cascade behavior

1. Move work-queue creation and ordering into Rhai.
2. Move barrier and soft-identity precedence into Rhai.
3. Move winner/loser selection into Rhai.
4. Move reindex, re-embed, and rescore triggers into Rhai.
5. Make Rust apply only explicit generic transactional actions.

Phase 5 exit criteria:

- repeated runs remain deterministic and idempotent;
- merge-cascade parity tests pass;
- Rust has no softmatch-specific work queue, verdict enum, or apply loop.

### Phase 6: remove the old implementation

Remove or replace:

- `pipeline::softmatch`;
- `pipeline::dedup_model`;
- the softmatch-specific parts of `pipeline::embedding`;
- `EntryInfo`, `Verdict`, and `SoftMatchConfig`;
- softmatch-specific block-key and suggestion APIs;
- import and maintenance wiring that hardcodes matching thresholds;
- Rust tests that encode softmatch policy.

Retain tests for generic capabilities and move policy tests into Rhai-driven
integration fixtures.

Phase 6 exit criteria:

- CI boundary enforcement passes;
- all correctness, recall, determinism, and performance gates pass;
- the Rhai package is the only implementation of softmatch behavior.

## Benchmark matrix

Every migration phase should run this matrix:

| Workload | Metrics |
|---|---|
| Current development DB, lexical only | total time, candidate time, scoring time, candidates/sec |
| Current development DB, semantic enabled | total time, HNSW time, KNN time, candidates/sec |
| Learned model | feature rows/sec, decisions/sec, total time |
| Incremental import, one changed entry | p50/p95/p99 latency and host-call count |
| Incremental import, large flush | total time and candidate recall |
| 100k synthetic library | total time, peak RSS, index size, candidate count |
| Merge cascade fixture | deterministic result, rescored-pair count, total time |

Track Rust-Rhai boundary calls explicitly. An increase in call count proportional
to aliases, keys, or vector dimensions is a design regression even if the small
benchmark still passes.

## Risks and mitigations

### Learned-model regression exceeds 50%

Mitigation: add generic batch kernels rather than reintroducing named features in
Rust. Prefer operations over arrays of pairs to one native call per feature per
pair.

### Excessive `Dynamic` allocation

Mitigation: use immutable native batch handles, compact arrays, interned strings,
and native generic collections. Do not serialize through JSON inside the process.

### N+1 database access

Mitigation: make batch APIs the primary interface and instrument host-call counts.
Do not expose convenient single-row calls on performance-critical paths, or mark
them diagnostic-only.

### Generic APIs quietly become softmatch APIs

Mitigation: require every native operation to be explainable without matching
terminology and usable by an unrelated script. Enforce forbidden vocabulary and
review new capability methods as architecture changes.

### Parallel evaluation changes results

Mitigation: parallelize only pure scoring over immutable snapshots. Apply actions
in deterministic order and let Rhai explicitly request subsequent epochs.

### Classifier non-determinism breaks idempotency

Mitigation: the `judgment` capability's per-`(namespace, key, evidence_hash)`
cache is mandatory, not an optional optimization — a second `softmatch
--apply` pass must hit cached results rather than re-querying, so it stays a
no-op even though the underlying classifier can return different verdicts
on identical live calls. Do not ship Phase 4's classifier routing without
this cache in place and exercised.

### Full Rhai optimization changes side effects

Mitigation: benchmark it behind a feature flag and correctly register native
function purity and volatility. Keep the default optimization mode until parity
tests prove the full mode safe.

## Final architecture rule

Policy lives in Rhai; bulk mechanics live in Rust.

If a Rust function name or implementation needs to know what constitutes a
matching candidate, feature, conflict, verdict, winner, or rescore condition, it
is on the wrong side of the boundary. If it loads raw facts, stores opaque keys,
performs a generic data-structure operation, calculates a domain-neutral
primitive, or applies an explicit transaction, it belongs in the native
capability layer.
