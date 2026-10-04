# Plan: run the full learned matcher from softmatch

Status: **implemented 2026-10-03** (see "Implementation" at the end). Model:
**v17 + rel-v8**, the v15 line without originality features (§3), retrained
twice during this work: v16 fixed a pad-token bug in the Python encoder, v17
made the cosine features reproducible across implementations. Model history
and evaluation: `plan-learned-matcher.md`. The runtime's verdicts match the
Python pipeline (`train/learned-matcher/`) pair for pair on the parity fixture.

## The three problems

1. **Model logic.** 220 features, an encoder, 676 trees, three relation heads.
2. **Missing data.** About two thirds of the features need data that the Rhai
   `decide(ctx, a, b)` call cannot see today.
3. **Originality.** Needs MusicBrainz first-release years and work years. Today
   no store holds these, and they are MB-specific.

## 1. Model logic goes into the inference cdylib; policy stays in Rhai

Rhai is an AST-walking interpreter. One pair of v15 is about 5k tree-node steps
plus ~500 lines of string features. In Rhai that is milliseconds per pair; in
Rust it is microseconds. The encoder must be native in any case. So:

| Where | What |
|---|---|
| `config/inference` (cdylib) | pair facts cache, feature extraction, title encoder, LightGBM evaluator, relation heads. Returns numbers only. |
| Rhai script | glue (send facts, ask for scores) and **policy**: per-type merge thresholds, DEFER band, generic-name guard → DEFER, DEFER → Jev, verdict and relation-metadata mapping. |

The thresholds stay in the script because they will change when the human
labels arrive, and they are a policy choice.

Note: v15 has **no calibration step**. The thresholds in
`models/v15/thresholds.json` apply to the raw softmax output. The evaluator
only needs softmax.

### C ABI (as built: `config/inference/src/matcher_abi.rs`)

```
h  = inference_matcher_open(bundle_dir)      // NULL → inference_matcher_last_error()
n  = inference_matcher_n_outputs(h)          // + inference_matcher_output_name(h, i)
rc = inference_matcher_put_facts(h, json)    // one facts object or an array
s  = inference_matcher_missing(h, pairs)     // JSON [[source, identifier], …] without facts
rc = inference_matcher_score(h, type, a_pairs, b_pairs, out_f64[n])   // 0 ok, 1 error, 2 missing facts
rc = inference_matcher_embed_batch(h, texts, n, &flat, &dim)        // semantic blocking
out = [p_same, p_related, p_sibling, p_unrelated, guard, p_sibling_structure,
       p_a_derived, p_kind_<kind>…]
```

The cdylib builds each entry's view as the **union of its pairs' facts**. This
is what Python's `Library.view(pairs)` does. It is also safe across in-memory
merges: an entry's pair set changes on a merge, but a pair's facts do not.
Encoder vectors are cached per text.

The cdylib takes JSON in and has no host or DB dependency. This allows a direct
parity test against Python (section 5).

## 2. Missing data: lazy, pair-keyed host facts, no EntryInfo growth

Agreed: adding fields to `EntryInfo` is wrong. Today `entry_to_rhai` builds a
full map for **both entries on every `decide` call**, so each new field costs
every script on every pair.

**2a. Lazy entries.** Register `EntryInfo` (as `Arc`) as a Rhai custom type
with property getters, in place of the eager map. A field is converted only
when a script reads it. All field reads in `match.example.rhai` are property
reads (`a.durations`, `a.pairs`, `a.peer_ids`, …), so the script stays
source-compatible. (Scripts that use map operations on an entry would break.
The example script has none.)

**2b. `pair_facts_json(source, identifier)` host function.** Lazy and
pair-keyed. It mirrors the pair-centric DB, so it can serve any script, not
only v15. Read from the DB on demand (with an LRU cache).

Contract `musiclib-pair-facts/1` (reference: `pair_facts()` in
`train/learned-matcher/export_parity.py`; golden output in the phase-1
fixture `facts.jsonl`). Order matters wherever a list is listed as ordered:

| field | value |
|---|---|
| `source`, `identifier` | the pair |
| `entry_id`, `entry_type` | its entry |
| `names` | `[[name, primary], …]` from `entry_alias`, ordered `primary` desc, then alias `id` |
| `durations` | sorted set: `duration_ms` (if non-zero) ∪ `duration_ms_all` |
| `release_date`, `release_type`, `primary_type` | raw `entry_source` columns |
| `contributions` | ordered by contribution `id`: `{artist: [s, i], artist_entry_id, artist_name, role, main}` |
| `parents` | `{entry_id, entry_type, disc, track}` per `entry_child` row with this pair as child (unordered) |
| `children` | `{entry_id, entry_type, name}` per child (unordered); `name` = the child pair's own first name, or null |
| `credited` | entry ids of the first 500 items credited to this pair (contribution `id` order), nulls kept |

`artist_name` is the artist pair's best name: its own first name (same
order as `names`); if it has none, the first name of the first pair of its
entry that has names, pairs sorted by `(source, identifier)`. Python's
training code used DB row order there; switching to sorted order changed no
verdict in the fixture.

The script calls this only for pairs that `ml_has_pair` says are missing.
Cost is O(library pairs) once, not O(candidate pairs).

Known small skew: artist identity uses entry ids at fetch time. If two artists
merge later in the same run, cached facts keep the old id until the next run.
Python has the same behaviour within one snapshot.

To verify: the online path (`match_new_entries`) runs after the new pairs are
flushed, so the DB holds their facts.

**Considered and rejected:** the cdylib reads SQLite directly. This is simpler,
but it ties a plugin to the internal schema and pulls sqlite into the inference
crate. It also loses the JSON-in parity test.

## 3. Originality: dropped from the runtime

Ablation (2026-10-03): `v15-noorig` + `rel-noorig` are v15 / rel-v6 retrained
on the **same labels** (the MB originality label rule stays) without the
`orig_*` features. On confident silver (Jev conf ≥ 0.8):

| tracks | v15 | v15-noorig |
|---|---|---|
| merge P / R | 0.972 / 0.857 | **0.990 / 0.875** |
| relate precision | 0.932 | 0.940 |
| sibling P / R | 0.708 / 0.888 | 0.728 / 0.888 |
| direct-link precision | 0.981 | 0.981 |
| artists merge P / R | 0.990 / 0.986 | 1.000 / 0.961 |

v13 → v15's gain came from the **labels**, which need MB data only at
training time. At runtime the features add nothing measurable (silver
differences are inside the CIs). Human labels (n = 18): v15 9, noorig 7. The
2 differences: one cover-sibling pair noorig calls RELATE; one derived pair
noorig DEFERs. Too few to outweigh silver; recheck when more labels exist.

**Decision: ship `v15-noorig` + `rel-noorig`.** No `extra` column, no work
first-year DB, no MB data at runtime. A shared-work-id feature (`same_work`,
from `work-rels`) is a possible later experiment; it would need only the
trivial `extra` data.

## 4. Encoder in the inference crate

`gbnam8/jp-music-title-encoder` (v7-L6, e5-small-based BERT). Load it with the
candle `BertModel` path already in `embed.rs`, behind a feature. Do not use
`ort`, to keep the crate free of ONNX runtime.

Must match Python: tokenizer truncation 48, the pooling inside the ONNX graph
(check: mean pooling over the mask), first 256 dims, L2 norm. Texts are
`title [A] primary_artist_name`, or the bare name when there is no primary
artist.

To verify: the HF repo has safetensors (else convert once).

## 5. Parity harness (the main risk)

`features.py` is ~500 lines of string handling. Python `str.lower`, `re`
versus Rust `regex`, trigram sets, alias dedupe order, the 40-name cap and
tie-breaks in "best aligned alias pair" can all diverge quietly.

- `export_parity.py`: for ~2k pairs (gold + silver + a random pool sample),
  dump the facts JSON per pair, the 220 features, the 4 probabilities and the
  verdict.
- Rust tests in `config/inference`, in this order: trees from Python features
  (isolates the evaluator), then the encoder (cosine ≥ 0.9999), then features
  from facts. Gate: every feature within 1e-5, every verdict identical.
- End to end: run softmatch with the v15 script on `live-2026-10-03.db` and
  compare verdicts with `models/v15-noorig/pool_preds.parquet` (687k pairs:
  9,143 MERGE / 27,002 RELATE / 10,151 SIBLING / 5,039 DEFER; the 136k
  cross-type pairs are DISTINCT unscored).

## 6. Verdict mapping in the script

| v15 | softmatch verdict |
|---|---|
| MERGE | `merge(p_same, "v15")` |
| RELATE | `relate(kind, p, "v15", #{direction, …})`; the structure head can turn it into SIBLING |
| SIBLING | `distinct` (no direct edge, per the ontology; the link to a common original is future work) |
| DEFER | `defer`, then the existing Jev path |

Before scoring: `a.entry_type != b.entry_type` → `distinct`, as in
`match.example.rhai`. 20% of the candidate pool (136k pairs) is cross-type;
the model was never trained on such pairs.

SIBLING → `distinct` is settled: `Verdict::Separate` is only made by the old
logistic `--model` path (`dedup_model.rs`), where its one effect is to skip
persisting a dedup suggestion. Scripts cannot return it.

Host gap: `call_script` parses only `merge` and `relate`; anything else,
including a `defer` map, becomes `Distinct`. Phase 5 adds `defer` parsing
(→ `Verdict::Defer`, which persists a suggestion and can go to Jev).

## Phases

1. **Done 2026-10-03.** Python: `export_parity.py` → fixtures in
   `data/learned-matcher/parity/<model>/` (gitignored; v17: 3,473 pairs,
   gold + pool stratified type × verdict, with pair facts, encoder texts and
   vectors). Features rebuilt from
   facts alone equal `Library.view` features on all 240 non-orig columns.
   Rust tests that need it skip when the directory is missing.
2. Inference crate: LightGBM `model.txt` evaluator (numeric splits,
   `decision_type` missing/default-left bits, multiclass softmax) and the relation
   heads. Parity on Python features.
3. Inference crate: title encoder. Parity on vectors.
4. Inference crate: facts → features. Parity on features and verdicts.
5. Host: lazy `EntryInfo` type; `pair_facts_json`; `defer` in script
   verdicts.
6. Script `config/match.v15.rhai`, end-to-end comparison on the live snapshot.

Phases 2–4 touch only the plugin (inference crate) and fixtures. Phase 5 is
the only change to musiclib-rs core, and it adds no schema.

## Implementation (2026-10-03)

All phases done; nothing committed yet.

| Piece | Where |
|---|---|
| LightGBM evaluator, facts → features, title encoder (candle), C ABI | `config/inference/src/matcher/`, `matcher_abi.rs` (feature `matcher`) |
| Parity tests (4 stages) | `config/inference/tests/matcher_parity.rs` |
| Bundle (models + encoder) | `train/learned-matcher/bundle.py` → `data/learned-matcher/bundle/v17/` |
| Lazy `Entry` type, `pair_facts_json`, `defer(conf, reason)` | `src/pipeline/softmatch.rs`, `src/pipeline/pair_facts.rs`, `MusicDb::sqlite_path` |
| Policy script | `config/match.learned.rhai` |
| Runtime vs Python comparison | `train/learned-matcher/compare_runtime.py` |

Model changes forced by the port (details in `plan-learned-matcher.md`):
- **v16**: the Python encoder attended pad tokens (batch-dependent vectors).
- **v17**: cosine features defined as float64-then-float32, because float32
  BLAS order flips ulp-level tree splits near 1.0.

Parity on the v17 fixture (3,473 pairs):
1. Trees on Python's features: max |Δp| 1.1e-16, all verdicts equal.
2. Features from facts with Python's vectors: every feature within 1e-5, 0
   verdict flips.
3. Rust encoder vs onnxruntime: min cosine 0.999999 (623 texts).
4. End to end with the Rust encoder: 0 verdict flips, max |Δp| 6.6e-3.

Host facts: `pair_facts_json` equals the reference `facts.jsonl` on all
13,493 pairs (`pair_facts::tests::matches_reference_facts`).

End-to-end softmatch run (dry run, copy of `live-2026-10-03.db`, fresh
`--embed-db`, `--csv`):
- 26,573 entries; embed phase 136 s (title encoder, 26,571 titles); scoring
  628 s for 463,332 candidate pairs (≈ 1.4 ms/pair); 0 script errors.
- Verdicts: track 8,705 merge / 33,008 relate / 4,020 defer; release 49 /
  843 / 732; release group 36 / 0 / 10; artist 1,006 / 235 / 2,482.
- Against the Python pool predictions on the 368,571 shared candidate pairs:
  **agreement 0.99995**. The 19 differences are threshold-band cases
  (MERGE ↔ DEFER at p_same ≈ threshold, 2 RELATE ↔ DISTINCT), from encoder
  float noise.

Known gaps / follow-ups:
- Speed: string features (NFKC, regexes, trigrams) are recomputed per pair;
  caching them per view would cut the 1.4 ms/pair.
- Thresholds are dev-chosen; re-tune with the human labels (v17 false-merges
  the `Say So` remix pair at p_same 0.844 vs the 0.828 track threshold).
- `ffi::tests::calls_inference_cdylib` needs `target/release/libinference.so`
  built with `minilm`; the library is now built with `matcher` only.
