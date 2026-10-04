# Plan: per-entry prepare cache for softmatch scripts

Status (2026-10-04): sections 1–3 implemented, the blocking half of 4 too;
the per-text `text_embedding` table for scoring is still open.

Result on the live library (465,366 candidate pairs, `--csv`): scoring
597 s → 263 s (44.5 s of it `prepare` for 26,274 entries), whole run
10:03 → 4:29, every verdict and confidence identical to the unprepared path.
Peak memory 1.75 → 2.28 GB (cached feature sides). Remaining per-pair cost
≈ 0.47 ms. Pair facts now come from joined queries (golden test unchanged).

Follow-ups the same day: `prepare` encodes on the GPU when present (44.5 →
11.6 s; 302 of 465k verdicts differ from the exact CPU path, accepted);
Rhai is built with `sync` and the script verdicts run on all cores
(`script_verdicts`, ~230 s → 21 s; the matcher handle is an RwLock and
`decide` uses a per-call output buffer); semantic blocking computes exact
per-type top-k by tiled brute force instead of building HNSW graphs
(76 s → 1.1 s; 731/610 marginal candidate pairs differ, merges unchanged).
Full live run: 10:03 → 0:49 (scoring 42.7 s: prepare 11.8 s, scripts
20.6 s). Then script verdicts 20.6 → 4.5 s: `decide` asks for the relation
heads (~80% of tree walks) only on the RELATE path, tree nodes are packed
into one struct, `ffi` `Func` clones are an `Arc` bump, and — the big one —
Rhai string interning is off (`set_max_strings_interned(0)`): under `sync`
the interner is one global lock whose contended path sleeps 10 ms, which
kept the 16 scoring threads ~80% asleep. Full run 0:31 with `--csv`.

## Problem

`match.learned.rhai` scores about 1.4 ms per pair (628 s for the full run).
The hand-written `match.example.rhai` takes about 0.5 ms. We thought the fix
was to cache the string features per entry. Profiling shows that is not the
main cost.

## Measurements (2026-10-04)

**Live run.** `perf record` on `softmatch --script config/match.learned.rhai
--no-embed` against the live DB (26,573 entries, 368,161 candidate pairs).
The first 240 s were sampled, almost all of it in the scoring phase. Shares
are of main-thread CPU samples:

| Where | Share |
|---|---|
| `inference_matcher_score` (all cdylib work) | ~83% |
| ↳ `Matcher::embed` (title encoder, called lazily per pair) | **~58%** |
| ↳ `pair_features` (string and set features) | ~14% |
| ↳ `Matcher::outputs` (4 GBDT heads, 6 predicts) | ~10% |
| Rhai, FFI, and JSON glue outside the cdylib | ~3% (lower bound: some stacks were cut off) |

**All threads.** About 60% of CPU samples are in candle GEMM. Another ~13% is
rayon and crossbeam work-stealing overhead. Inside the encoder, the scalar
`gelu_erf` takes about half the forward pass.

**Parity fixture.** Encoder work excluded by preloading vectors (3,473 pairs):
`build_view` ×2 takes 83 µs, `pair_features` 259 µs, GBDT 178 µs. The matcher
already caches views per entry, which saves only about 75 µs per pair.

### Why the encoder dominates

`Matcher::features` embeds a pair's view texts on demand. Those texts are
`name`, `name [A] artist`, and artist names. Each call encodes only one or two
entries' texts, so most forward passes run on a tiny batch. With small
batches, rayon spin-up and per-call overhead cost more than the actual math.
Nothing embeds these texts ahead of time:

- The host's embedding phase embeds different strings (entry titles).
- The phase is skipped when `embeddings.db` is already warm.
- `--no-embed` turns it off entirely.

Each entry appears in about 14 candidate pairs (2 × 368k / 26.5k). So
per-entry work is cheap once it is computed. The problem is that it is
computed one or two entries at a time.

## Proposal

### 1. Host side: a batched `prepare` hook (musiclib-rs)

A script can optionally define:

```rhai
fn prepare(ctx, entries) { ... }   // array of Entry → array, same length
```

The host does the following:

1. Collects the distinct entries that appear in `to_score`, after the
   barrier and soft-identity filters.
2. Groups them by `entry_type`.
3. Calls `prepare` in chunks of `PREPARE_BATCH` (default 512). Large chunks
   give the cdylib large encoder batches.
4. Stores each returned value in a `HashMap<i64, Dynamic>` keyed by entry id.
   The map lives for the whole scoring phase.
5. Exposes the value read-only as `a.prepared` on the `Entry` type. It is
   `()` when the script has no `prepare` hook or returned `()` for that entry.

**This is how the script chooses what to cache.** The host stores only what
`prepare` returns, and it does not know or care what the value is. A script
that wants several things returns a map, e.g. `#{ view: 17, title_norm: "…" }`.
A script with no `prepare` hook pays nothing.

The cache is safe without invalidation. During scoring, entries are immutable
`Arc<EntryInfo>`s, and soft merges are written to the DB but never mutate the
in-memory entries. So no entry's prepared value can go stale within a run.

I suggest no named per-field registration API (e.g. `register_entry_cache
("x", f)`). It would make each value lazy, and lazy values lose the batching
that delivers the speedup. One eager batched hook is simpler and faster. If
we later need lazy, unbatched values, we can add them on top.

### 2. cdylib side: opaque view handles

The expensive per-entry data is Rust data inside the cdylib: encoder vectors,
trigram sets, and normalized strings. Converting it to Rhai values would cost
more than it saves. So for the learned matcher, `prepared` holds an integer
handle, and the data stays in the cdylib. The host still owns the batching
and the entry → handle mapping. New C ABI:

```c
// pairs_json: [[type, [[source, id], ...]], ...] for one batch of entries.
// Writes one view handle per entry into out_handles (i64, -1 = no facts).
// Feeds missing facts via the existing put_facts path first (see below).
int32_t inference_matcher_prepare(void* h, const char* entries_json, int64_t* out_handles);
// Score two prepared views; same output row as inference_matcher_score.
int32_t inference_matcher_score_views(void* h, int64_t va, int64_t vb, double* out);
```

`prepare` does four things per batch:

1. Builds the views.
2. Collects every view text not yet in `vectors` and encodes them in one call,
   which `TitleEncoder::embed` already length-sorts and chunks.
3. Precomputes the per-side parts of `pair_features` into a `PreparedView`
   stored next to the `View`:
   - `norm(name)`, plus trigram and token sets
   - `core_title` and its trigrams
   - `bracket_contents`
   - `markers`
   - `artist_core`
   - CJK ratio, placeholder and generic flags
   - `digits` of each normalized name
   - `unit64` vectors (`sims` currently re-normalizes every vector on every pair)
4. Returns the view ids.

`pair_features` then reads these fields instead of recomputing them. This is
the "cache string features per entry" idea. It is worth doing, but it is the
smaller win (≤14%).

**Missing facts.** In the script, `prepare` calls `inference_matcher_missing`
and `pair_facts_json` once per batch, not once per pair. The retry on `rc == 2`
in `score()` goes away.

`inference_matcher_score` stays, so the parity tests and other callers keep
working.

### 3. `match.learned.rhai`

- Add `prepare(ctx, entries)`: feed the missing facts for the batch, call
  `inference_matcher_prepare`, and return the handles.
- In `decide`, call `score_views(a.prepared, b.prepared)`. Fall back to the
  current path when `prepared` is `()`.

### 4. Persistent per-text vectors and title-encoder blocking

> **Status (2026-10-04):** the blocking half is done. `embeddings.db` records a
> model id (`embedding_model_id` hook), the script picks each entry's text
> (`embed_text` hook), and `match.learned.rhai` embeds tracks as
> `title [A] artist` (`inference_matcher_entry_text`). The encoder is now a
> hand-written BERT forward (fused, rayon-parallel element-wise ops; candle's
> gemm kernels; no padding): 0.96 ms/text versus 4.1 ms with candle's
> `BertModel`, matching the Python vectors to cosine ≥ 0.999999. A full re-embed
> of the live library takes 35 s (5 s texts, 30 s encoding), down from 101 s.
> With `--features vulkan` (llama.cpp, f16 GGUF, 512-token packed calls,
> flash attention, 4 contexts) blocking vectors come from the GPU, and the
> cache now stores f32 blobs in one transaction per 512-entry chunk instead of
> one auto-committed JSON-text INSERT per entry: a full re-embed takes 9.1 s
> (0.8 s setup, 4.2 s building texts, 4.1 s encode + store). Building texts
> (one Rhai `embed_text` call per entry) is now the largest part.
> Still open: the `text_embedding` per-text table for scoring.

Today `embeddings.db` holds one vector per entry, made from `best_title` by
whatever `embed_batch` the script defines. Those vectors are used only for
the top-k candidate search (`semantic_ann`). The live file holds naive
token-hash vectors with 1–6 non-zero values out of 256, so the search
duplicates the lexical blocking. The file has no model id:
`--embedding-model-id` is only compared against `--model`, never stored. So
after a script switch, the old vectors are served without any warning.

Changes:

- **New table `text_embedding(model_id, text, vector)`.** The key is
  `(model_id, text)`. The host loads vectors from it into the matcher before
  `prepare` (a new `inference_matcher_put_vectors` call). `prepare` returns
  only the texts it had to encode, and the host writes those back. After the
  first run, a rerun encodes only new or changed texts.
- **Store a model id with the per-entry blocking vectors**, in
  `entry_embedding_meta` or a meta row for the file. A mismatch means
  re-embed, never reuse. The script reports its model id, e.g. from
  `init()`'s ctx or a `model_id(ctx)` hook. That makes the separate
  `--embed-db` for each script unnecessary.
- **Use the title encoder for blocking, conditioned the way it was trained.**
  For tracks, the blocking text becomes `best_title [A] primary artist`. Other
  types keep the plain title. The script builds this text from the entry, so
  `embed_batch` receives `Entry` objects (or the host passes the artist too).
  The vectors come from the same per-text cache, so blocking and scoring share
  one encoding pass.
- **`match.learned.rhai` becomes the script to run.** `match.example.rhai`
  stays as the dependency-free example, with naive blocking.

Open question: should blocking use one vector per entry (`best_title`) or
search with every alias (multi-vector, max-sim)? Multi-vector catches more
pairs but makes the KNN index about 2× larger. Start with one vector per
entry, then measure recall on the retrieval probes
(`data/dedup-entry-retrieval-probes-v4.jsonl`).

## Expected effect (to confirm by measuring)

- **Encoder.** Measured with candle on 16 CPU threads, using the parity texts:
  3.7 ms/text in large batches versus 7.2 ms/text in batches of 3. The fixture
  has about 2 texts per entry, which suggests about 52k texts for the live
  library. That estimate puts the encoder at about 375 s lazy versus 195 s
  batched. **Batching only halves it.** Encoding is still the biggest cost on
  every run until vectors persist across runs (section 4). Then only new or
  changed texts need encoding.
- **Per-side string precompute.** Should cut `pair_features` by about half.
- **Remaining per-pair cost.** Should be GBDT plus the cross terms (sims,
  Jaccards, set intersections): roughly 0.3–0.4 ms. That is in line with the
  old Rhai script.

## Not in this plan (possible follow-ups)

- **GPU encoding** without CUDA (less pressing now that the CPU encoder does
  ~1 ms/text): llama.cpp/ggml's Vulkan backend, with the
  encoder converted to GGUF and token ids fed from the HF tokenizer. This only
  pays off on cold runs (a new model or a fresh DB). It needs its own parity
  check (f16 weights, ggml GELU). See the `labse-inference-speed-investigation`
  memory for why a from-scratch Vulkan engine was dropped.
- **GBDT speed** (~10%): tree layout and fewer redundant predicts.

## Validation

- `matcher_parity` must still pass. Add a case that scores every fixture row
  through `prepare` + `score_views` and checks that the result is bit-identical
  to `score`.
- Run the full `softmatch` with the learned script before and after the
  change. Compare wall time and confirm the CSV verdicts are identical.
