# Semantic Blocking v2 — Cross-lingual pre-filter for soft-dedup

Status: **design settled + benchmarked, not yet implemented.** Supersedes the model
choice in `docs/plan-semantic-blocking.md` (that doc's architecture — token ∪ semantic
blocking, sqlite-vec KNN, script-owned embed backend — still stands; only the *model*
and the *transliteration handling* change).

Decision (2026-07-18): target **LaBSE** for max recall, validate via a **Python embed
sidecar first**, port to candle later.

Decision (2026-07-19): don't ship raw 471M LaBSE — **vocab-prune it to EN+JA**
(471M → ~130M params, ~940MB → ~260MB f16, no quality loss on the kept languages) before
the candle port, then **fine-tune the pruned model** on MB aliases + **romaji-augmented**
pairs. Rationale and compute budget below (Phases 2–3).

---

## TL;DR

The embedding used for semantic blocking is currently **Model2Vec (m2v)**, and it is
**at chance for the one job it exists to do** — putting cross-language duplicates near
each other. Measured cross-language recall@20: **m2v 1.4%**, MiniLM 26%, **LaBSE 56%**.
Switch the model (LaBSE) and add a deterministic romanization path for transliteration.
LaBSE is 471M params but **82% of that is its 100-language embedding table** — since we
only use EN+JA, **vocab-prune it to ~130M / ~260MB with no quality loss** (Phase 2), then
fine-tune on MusicBrainz aliases + romaji-augmented pairs (Phase 3).

Two things to fix on the way (both now fixed in-code — see "Bugs found" below):
- The deployed `embeddings.db` was built with the **naive fallback** embedding (the
  `ffi` feature is off by default), so semantic blocking has effectively been **disabled**
  in every softmatch run to date. The embed pass now emits a loud `warn!` when it detects
  the naive-fallback signature, so this can't recur silently — but the existing
  `embeddings.db` still has to be rebuilt.
- `inference_detect_language` returned a **non-NUL-terminated pointer** on its success
  path (fixed: interned `CString` labels; see below).

---

## Benchmark evidence

Method: 500 real MusicBrainz Japanese-artist title pairs (canonical JP title ↔ alias),
each query ranked against a pool of **54,279** titles = all 30k Latin-side aliases (so
the correct English translation must beat 30k *other* English translations) + ~24k
distinct JP recording titles. Script alone cannot game the ranking. Pool/queries
embedded per model; cosine ranking; recall@k = fraction of queries whose true partner is
in the top-k neighbours.

Cross-language blocking recall:

| Model | params | recall@1 | recall@10 | recall@20 | recall@100 | median rank |
|---|---|---|---|---|---|---|
| **m2v** (current default) | static | 0.000 | 0.014 | 0.014 | 0.026 | 26,490 |
| MiniLM-L12-multilingual (opt-in `minilm`) | 118M | 0.116 | 0.206 | 0.260 | 0.330 | 1,079 |
| distiluse-base-multilingual-v2 | 135M | 0.166 | 0.350 | 0.384 | 0.472 | 168 |
| **LaBSE** | 471M | **0.364** | **0.524** | **0.560** | 0.652 | **7** |

Per-category median-cosine margin (positive − random-pair baseline), splitting the
cross-script pairs by whether the Latin side is romaji (transliteration) or English
(translation); split ≈ 40% / 60% in the MB alias data:

| Model | transliteration margin | translation margin |
|---|---|---|
| m2v | +0.05 | +0.02 |
| MiniLM | +0.19 | +0.35 |
| LaBSE | +0.30 | +0.53 |

### What the numbers mean (four corrections to earlier reasoning)

1. **m2v is not "too similar" — it is at chance.** Median rank of the true partner is
   the *middle of the pool*. m2v is a static distillation of the multilingual MiniLM,
   and distillation collapsed cross-language recall 26% → 1.4%. **Corollary: static /
   Model2Vec distillation destroys cross-lingual alignment.** Distilling LaBSE back to a
   fast static table to "keep the 7µs speed" will almost certainly *not* preserve the
   win — cross-lingual matching needs a real contextual transformer at index time. (This
   is an empirical result, not a guess: the current m2v *is* that experiment, already
   run, already failed.)

2. **"Outputs are really close in similarity" is a red herring.** LaBSE's positive-pair
   cosine (0.577) is essentially identical to MiniLM's (0.588); the anisotropy floor
   (random cross-lang pairs) sits around 0.31. Absolute cosine is uninformative — LaBSE
   wins because it **ranks** true partners above the crowd (median rank 7 vs MiniLM's
   1,079), not because its numbers are more spread out. Optimising for "spread" (e.g.
   naive whitening as the main lever) targets the wrong quantity.

3. **Translation is the majority case and the one semantic models handle best.** MB
   `en`-locale aliases are meaning-translations (`不協和音`→`Discord`, `無言の歯車`→
   `Silent Gears`), ≈60% of cross-script aliases. Every model scores translation higher
   than transliteration.

4. **Transliteration is fundamentally not an ML problem.** Even LaBSE is markedly weaker
   on romaji (`蛍`→`Hotaru`) because phonetic transliteration carries no *meaning* to
   embed. It is ≈40% of cross-script cases and is best solved **deterministically** by
   romanizing both sides.

---

## Why not a translation model / how to "combine" models

- **Translation model at runtime — no.** For blocking/retrieval a cross-lingual
  bi-encoder (LaBSE) *is* translation knowledge compressed into one vector. It beats
  translate-then-match on speed (one embedding vs a seq2seq decode per title) and on
  robustness: `不協和音` → "Discord"/"Dissonance"/"Disharmony" all land in one region,
  whereas string-matching a single decoded translation is brittle. Use a translation
  model only **offline**, as data augmentation for fine-tuning (Phase 3).
- **"Combine MiniLM + translation knowledge" — yes, as distillation, not score-fusion.**
  The principled combination is **multilingual knowledge distillation** (Reimers &
  Gurevych 2020, "make-multilingual"): train the student so `embed(JP) ≈ embed(EN)` on
  parallel pairs. That is exactly how LaBSE / distiluse / paraphrase-multilingual were
  built. Parallel data = MB aliases (real) + machine-translated titles (augmented). This
  is Phase 3. Naive max-of-two-cosines score fusion is not worth the second forward pass.

---

## Architecture (unchanged from v1, model swapped)

```
blocking:
  token blocking      (normalized first-token)          → same-script variants
  + romaji blocking   (romanize kanji/kana → Hepburn)    → transliteration   [NEW, Phase 1]
  + semantic KNN      (LaBSE, sqlite-vec, k=20, thresh)  → translation       [model swap, Phase 0]
  union, same-type filter → candidate pairs
scoring (Rhai decide_*):
  semantic_sim(a,b)   cached LaBSE cosine
  + romaji_sim(a,b)   normalized romaji string sim       [NEW, Phase 1]
```

The script-owned embed backend and sqlite-vec cache from
`docs/plan-semantic-blocking.md` are reused as-is. The only host contract that changes:
**`SoftMatchConfig.embed_dim` must become 768** (LaBSE) instead of 256.

---

## Phase 0 — Swap the blocking model to LaBSE (highest ROI)

Goal: 1.4% → 56% recall@20 with **zero training**, validated fast.

Path chosen: **Python embed sidecar first** (the Rhai layer already supports it — see
`register_http_fns` in `src/pipeline/embedding.rs`, which exposes `http_post_json(url,
map) -> map`). `docs/plan-semantic-blocking.md` already envisioned this server.

Steps:

1. **LaBSE embed server** (mirror the existing `ytdlp`/`ytmusicapi` sidecar pattern):
   `POST /embed { "texts": [...] } → { "vectors": [[...768...], ...] }`, backed by
   `sentence-transformers/LaBSE` (`normalize_embeddings=True`).
2. **Point the match script at it.** In `<config_dir>/match.rhai`, define
   `embed_batch(ctx, texts)` to call `http_post_json("http://localhost:PORT/embed",
   #{texts: texts})` and return `res.vectors`. (The example script's ffi/naive backends
   become the fallback.)
3. **Set `embed_dim = 768`** (CLI `--embed-dim 768` or config) so `EmbeddingCache::open`
   builds a `FLOAT[768]` sqlite-vec table. Use a fresh `--embed-db` so the stale naive
   256-d vectors are not reused.
4. **Re-run softmatch dry-run** on the live DB; measure how many known cross-language
   duplicates now surface as candidates vs before. Tune `--embed-threshold` /
   `--embed-k` against the LaBSE cosine distribution (positive median ≈ 0.58; expect a
   threshold around 0.45–0.5, but re-measure on real entries).

Later (post-validation): **port LaBSE to candle** for the pure-Rust deploy. LaBSE is a
plain 12-layer BERT (CLS pooling + a Dense(768→768, tanh) head + L2 norm); candle-
transformers already has `BertModel`. ~30–60ms/embed on CPU, but it is a **one-time cost
per entry** (cached in `embeddings.db`, invalidated on title change), so it is fine for a
library that grows incrementally. Note this is heavier than MiniLM (471M vs 118M) — the
weight asset is ~940MB f16, but **~82% of that is the multilingual embedding table**;
Phase 2 prunes it to EN+JA (~260MB) before this port.

Exit criteria: cross-language duplicates that token blocking misses now appear in the
candidate set; no regression in same-script recall.

---

## Phase 1 — Romanization prong (deterministic; covers the 40% LaBSE is weak on)

Goal: catch transliteration (`蛍`↔`Hotaru`, `炉心融解`↔`Roshin Yuukai`) without ML.

1. **Rust romanizer.** kanji/kana → Hepburn romaji. Options: `lindera` or `vibrato`
   (MeCab-style, needs a unidic/ipadic dictionary asset, gives readings) → Hepburn; or
   the `kakasi` crate (self-contained kanji→romaji). Reuse readings already available:
   MB `sort_name` frequently holds the kana reading; some providers supply readings.
   Prefer a supplied reading, fall back to the romanizer.
2. **Fold into token blocking.** Add the romanized form as an extra blocking key, so
   `蛍` and `Hotaru` both normalize to `hotaru` and land in the same block — **no KNN
   needed** for transliteration. This subsumes the transliteration case into the cheap
   existing token pass.
3. **`romaji_sim(a, b)` scoring feature** — normalized-Hepburn string similarity
   (Levenshtein/Jaccard), registered alongside `semantic_sim`, so the Rhai `decide_*`
   can corroborate a transliteration match.

This prong is independent of Phase 0 and can land in parallel.

---

## Phase 2 — Shrink LaBSE to EN+JA via vocabulary pruning

Goal: keep the 56% recall but cut the deploy asset from ~940MB to ~260MB, with **no
retraining required**. This is the answer to "471M is too much for our system."

**Where LaBSE's parameters live.** LaBSE is *not* a big transformer — it is a plain
12-layer BERT-base body (~85M params) bolted onto a huge multilingual embedding table:

| Component | Params | Share |
|---|---|---|
| WordPiece embeddings (501,153 × 768) | ~385M | **82%** |
| Encoder body (12 × BERT-base layer) | ~85M | 18% |
| Dense(768→768, tanh) head | ~0.6M | <1% |
| **Total** | **~471M** | |

We only ever embed EN + JA text (+ Latin, which also covers romaji). Most of that 385M
embedding table indexes tokens our inputs never emit and can be deleted:

- Count WordPiece token frequency over an EN+JA corpus (MB titles/aliases + a monolingual
  dump), keep the top-N used tokens + BERT specials, slice those rows out of the embedding
  matrix, and remap the tokenizer. EN+JA (+Latin) lands around **50–80k tokens**.
- Budget: 60k × 768 ≈ 46M embeddings + 85M body ≈ **~130M params** (≈3.5× shrink); f16
  asset **~940MB → ~260MB**.
- Quality on the kept languages is essentially unchanged — you deleted rows the EN/JA
  inputs never index. A short continued fine-tune (Phase 3) recovers any edge loss.
- **Precedent, not speculation:** `cointegrated/LaBSE-en-ru` (David Dale) did exactly this
  for EN+RU (~1.8GB → ~0.5GB, quality preserved for the kept languages) with a published
  pruning script — swap ru→ja.

**Critical:** prune to a smaller *transformer*, **never back to a static table**. The m2v
result in this doc (26% → 1.4%) *is* that experiment already run and failed — a contextual
encoder must stay at index time.

This step reduces size/RAM, **not** per-embed latency. If latency is also a constraint, see
Phase 4. Re-run the 500-pair recall harness after pruning; expect ≈56%, unchanged.

---

## Phase 3 — Domain fine-tune the pruned model (+ romaji augmentation)

Goal: push translation recall past off-the-shelf, and make the model degrade gracefully on
romaji. Fine-tune the **Phase-2 pruned model** (not raw LaBSE).

Data source: the **live self-hosted MB Postgres** (`musicbrainz-docker-db-1`). Available
now:
- **Positives:** 83,399 `(recording.name, recording_alias.name)` pairs from
  Japan-area-artist recordings (30,963 JP→Latin cross-script; the rest same-script
  spelling/format variants). Extend to all locales for a broader multilingual set.
- **Hard negatives:** different recordings sharing an artist credit (same-artist,
  different-song). This is the signal that stops the model collapsing an artist's
  catalogue together.
- **MT augmentation:** machine-translate JP titles → EN (e.g. NLLB-200) to synthesize
  extra parallel pairs (offline use of a translation model — the *only* place one belongs).
- **Romaji augmentation [NEW]:** romanize JP titles (pykakasi / MeCab+unidic → Hepburn) and
  add `kanji/kana ↔ romaji` (free synthetic positives) and `romaji ↔ EN` (composed) pairs,
  so the model learns that romaji strings sit near their kanji forms — directly targeting
  the transliteration weakness LaBSE has off-the-shelf.
  **Honest ceiling:** `romaji ↔ kanji` is already solved deterministically by Phase 1, and
  `romaji ↔ EN` is phonetic → meaning (genuinely hard, low value). So romaji training is
  graceful-fallback *insurance under* the Phase 1 romanizer (for when a supplied reading is
  missing), not a replacement for it.

Recipe: `sentence-transformers` fine-tune with `MultipleNegativesRankingLoss` (in-batch
negatives; seed same-artist songs into a batch to make them hard negatives). Consider
embedding `title + primary artist` rather than title alone, for disambiguation (open
question from v1). Re-run the recall benchmark (same harness) to quantify the lift before
adopting.

**Compute budget.** A single A100 covers all of Phases 2–3 comfortably — vocab pruning is
minutes (mostly CPU); the MNRL fine-tune on ~100k pairs is tens of minutes to ~1–2h/run at
batch 128–256, seq-len 64 (MNRL wants big in-batch-negative batches, which 40/80GB affords);
NLLB-200 (600M/1.3B) MT augmentation fits on the same GPU. The bottleneck is data curation
and the benchmark loop, not GPU time.

---

## Phase 4 (optional) — Layer distillation / dim reduction (only if latency, not size, bites)

Phase 2 fixes the asset size; this fixes per-embed *latency* if the candle CPU path is too
slow. Skip unless measured to be needed — the doc's own framing is that embedding is a
one-time, cached cost per entry, so size (Phase 2) is the likelier real constraint.

- **Layer distillation (12 → 6).** Teacher = the Phase-3 fine-tuned model; student = a
  6-layer BERT that mimics the teacher's sentence embeddings (MSE) on the parallel + a
  monolingual corpus (Reimers & Gurevych "make-multilingual"). Roughly halves body compute.
- **Dim reduction (768 → 256).** A trained Dense projection or Matryoshka-style truncation
  shrinks the sqlite-vec index and KNN cost (reverts `embed_dim` back to 256). Does not cut
  encoder compute.
- **Critical (again):** distill to a transformer, never a static table — see the m2v
  cautionary tale above.

---

## Phase 5 (optional) — Whitening

Cheap post-hoc linear transform (mean-center + PCA/ZCA on the corpus) to de-anisotropise
the space. May sharpen ranking further; only worth it if measured to help — it is a
polish, not the main lever.

---

## Bugs found during investigation (both fixed)

1. **`inference_detect_language` non-NUL-terminated return** (`config/inference/src/lib.rs`).
   The success path returned `detect_language(s).as_ptr() as *const c_char`, where
   `detect_language` returns `&'static str` (`&e.weights.labels[mi]`) — a `str` has **no
   trailing NUL**. Any C/FFI consumer doing `strlen`/`read_cstr` over-read into adjacent
   heap; only the `b"und\0"` fallback was safe.
   **Fixed:** `Weights` now carries `labels_c: Vec<CString>` (interned NUL-terminated
   copies built in `load_tflite`). The argmax is shared via `detect_language_idx`, and the
   C ABI returns `detect_language_cstr(s).as_ptr()` — a valid, always-terminated pointer
   into static storage. `langid_argmax_matches_reference` still passes.
2. **Silent naive-embedding fallback.** With `ffi` off (the default), the example script
   fell back to the token-hash naive embedding and the "loaded-but-failing" guard made
   it indistinguishable from success — so `embeddings.db` silently filled with 256-d
   near-orthogonal junk (1–8 non-zero dims). The script's fallback only `print`s to stdout,
   which never reaches `tracing`, so nothing showed in the logs.
   **Fixed:** `embed_stale_entries` now tracks the densest vector produced in the pass and
   calls `report_embed_health(dim, max_nonzero)`. Real embedders are essentially fully
   dense; if the whole batch stays under 25% density it emits a loud `warn!` naming the
   naive-fallback cause and the `ffi`-feature fix, otherwise an `info!` confirming a dense
   `{dim}`-d backend. This is script-agnostic — it flags any misconfiguration that yields
   degenerate vectors, not just the example script.

---

## Reproducing the benchmark

- Dump pairs (tuples-only, tab-separated) from the MB mirror:
  ```sql
  SELECT r.name, ra.name, COALESCE(ra.locale,''), COALESCE(ra.sort_name,'')
  FROM recording r
  JOIN artist_credit_name acn ON acn.artist_credit = r.artist_credit
  JOIN artist a ON a.id = acn.artist
  JOIN area ON area.id = a.area
  JOIN recording_alias ra ON ra.recording = r.id
  WHERE area.name = 'Japan' AND r.name <> ra.name
    AND length(r.name) > 1 AND length(ra.name) > 1;
  ```
  Distractor corpus = `SELECT DISTINCT r.name … LIMIT 30000`.
- Embed pool + JP-side queries per model; rank the true Latin-side partner; report
  recall@k and median rank. The current cdylib exposes `inference_embed_batch` over its
  C ABI (callable from Python via `ctypes`); sentence-transformers models via `.encode`.
- LaBSE median rank 7 / 54,279 is the target the fine-tuned model should beat.
```
