# Plan: J-Pop title encoder for softmatch retrieval

Status: proposal, no code yet (2026-10-02).

Supersedes the *model* decision in `docs/plan-semantic-blocking-v2.md` (ship a
vocab-pruned, fine-tuned LaBSE). LaBSE becomes a teacher only; what ships is a
small distilled student. That doc's benchmark evidence and the "never distill
to a static table" finding still stand. Depends on the retrieval half of
`docs/plan-softmatch-validation.md` — nothing here is judged without it.

## Problem

Softmatch is a two-stage pipeline: cheap **retrieval** (which pairs to look
at) then a **decider** (merge / relate / distinct). Two issues:

1. **Retrieval doesn't scale.** Top-k KNN over a general-purpose embedding
   looks fine on a small library, because k is a meaningful fraction of the
   pool. At production size the true partner must rank in the top k among
   millions, so the embedding has to be sharp *for song titles*. It also has
   to be cheap, since every entry is embedded. Model2Vec (removed in 6cc37c6)
   was cheap but at chance cross-lingually. LaBSE is better (56% recall@20)
   but slow on CPU, and it doesn't understand titles: `[Official MV] Song A`
   and `[Official MV] Song B` embed close because the shared template
   dominates the pooled vector.
2. **The decider uses expensive tools.** A general model (Jev, or LaBSE) is
   overkill for the per-pair decision.

This plan is mostly about (1). (2) gets a cascade design at the end and its
own plan later.

### Why LaBSE fails here

The cause is the training objective, not the architecture. LaBSE learned to
match whole translated sentences, where shared structure *is* evidence of a
match. Self-attention can learn to ignore a template, but only if training
shows it negatives that share the template and differ only in the core title.
LaBSE never saw those. We fix it two ways, and measure each one separately:

- **Before the model:** parse the title deterministically and embed the core
  title, not the raw alias.
- **In the model:** train with template-sharing hard negatives so template
  tokens carry no identity signal.

## Scope

- **In:** Japanese-domain music (J-Pop, anime, vtuber, Vocaloid, game, idol),
  including the English and romaji titles that domain uses heavily. Plus a
  smaller English-language (Western) tier for general title syntax.
- **Out:** other languages and scripts (Hangul, Cyrillic, non-English Latin
  languages). musiclib-rs doesn't need them, and they cost model capacity and
  vocabulary.

## Target retrieval architecture

```
alias ──► core-title parse (Rust) ──┬─► exact / token / trigram keys   (existing channels, now on core title)
                                    ├─► transliteration keys            (new, deterministic)
                                    └─► title encoder ─► HNSW / vec0    (new model, replaces semantic_ann input)
                                                    ∪ duration_credit / tracklist channels (unchanged)
```

1. **Core-title parse in Rust.** Move `parse_clean_title` /
   `parse_video_title` / `version_tokens` out of `match.rhai` into
   `pipeline/softmatch.rs` (or a new `pipeline/title.rs`). Output: core title,
   artist fragments, marker/version tokens. Every channel indexes the core
   title. The Rhai script keeps receiving the parsed fields, so its rules don't
   re-parse.
2. **Transliteration keys.** Kana → romaji (deterministic); kanji → reading
   via a morphological analyzer (lindera or vibrato with IPADIC/UniDic —
   choose by accuracy on MB artist aliases). Fold romanization variants
   (ō/ou/oh/o, tsu/tu, shi/si, ji/zi, n'/n) into one key. Covers the ~40% of
   cross-script pairs that are transliterations (蛍 ↔ Hotaru), with no model.
3. **Title encoder** for what's left: translations, fuzzy variants, noisy
   video titles the parser misses. Input is the core title (optionally plus
   the primary artist, decided by eval). Output is an L2-normalized vector of
   ≤256 dims. Queried with k plus a similarity floor.

Step 3 is only worth its cost if it adds recall on top of steps 1+2. The
harness's per-channel marginal recall decides that.

## Training data

All from the local MB mirror (`musicbrainz-docker-db-1`), plus YouTube titles
fetched for MB-linked videos. Sizes measured 2026-10-02.

### Tiers

| Tier | Selection | Share of training mix |
|---|---|---|
| **JP** | recording's artist credit includes an artist whose `area` or `begin_area` is inside Japan (via `area_containment`), **or** a release containing it has language `jpn` | ~80% |
| **EN** | release language `eng` and no Japan-area artist on the credit | ~20%, mixed in throughout (not a separate later stage — sequential fine-tuning forgets) |
| dropped | anything else; any title with Hangul, Cyrillic, Arabic, Thai, etc.; Latin titles that the `inference` LangID puts outside en/ja | 0 |

The JP-tier raw pool: 123k artists, 2.35M recordings, 108k recording aliases,
63k artist aliases, 31k works with aliases, **27k recordings with a
YouTube/NicoNico link** (of 202k overall).

### Pair types and graded targets

The encoder is a *retrieval* model, so things the decider should RELATE
(versions) must still land near each other. Targets are graded, not binary:

| Grade | Pair | Source |
|---|---|---|
| 1.0 same | MB recording title + artist ↔ its linked video's title | 27k JP (+ EN tier) real pairs; YouTube fetch |
| 1.0 same | MB recording name ↔ recording alias; artist name ↔ artist alias | 108k + 63k JP |
| 1.0 same | core title ↔ synthetic template-wrapped core title | templates mined from real video titles × 2.35M JP titles |
| 0.9 translation | work name ↔ work alias in the other language (ja ↔ en) | 31k JP works |
| 0.6 version | recordings of the same work with different names (live / instrumental / remix / TV size / cover) | MB `l_recording_work` |
| 0.1 hard negative | **same template, different song** (other videos on the same channel; synthetic same-template wrap) | YouTube fetch + synthetic |
| 0.1 hard negative | same artist, other recording; same title, different artist | MB |
| 0.0 | in-batch random | — |

**Template mining:** cluster the real video titles by their residue after
removing the known core title and artist (e.g. `【MV】{t} / {a}`,
`{a}「{t}」Official Music Video`, `{t} feat. {a} [MV]`). Keep templates seen on
at least N channels. Wrap clean titles with sampled templates. Each wrapped
positive gets a hard negative: the same template around a different title by
the same artist.

**Optional, more real noise:** for JP artists with a YouTube channel link in
MB, list the channel's uploads and weak-label them by exact core-title match
against that artist's MB recordings. Noisier, but it's the real
distribution. Do this only if the eval shows the synthetic data plateauing.

### Leakage exclusion

The live library is the evaluation set (split probes), so training must not
see it:

- Drop every MB recording, release, work and artist whose MBID appears in the
  eval snapshot's `entry_source`.
- **Recommended, stricter:** also drop every recording credited to any artist
  present in the eval snapshot, and every YouTube channel present in it.
  Otherwise channel templates and same-artist negatives memorized in training
  inflate the eval. Report a secondary "seen-artist" eval separately so we
  still know how the model does on artists it trained on.

Every dataset build writes a card (`data/title-encoder/<build>/card.json`)
with the queries, row counts per tier and pair type, the exclusion counts and
the MB replication sequence. Builds are reproducible from the card.

## Model

### Vocabulary pruning first

All candidate base models use the XLM-R SentencePiece vocabulary (~250k
tokens). Tokenize the full JP+EN title corpus and keep the tokens that occur
(expected ~30–40k), then remap the embedding matrix. The embedding table drops
from ~96M to ~13M params at 384-d. That cuts file size, memory and load time.
It does **not** cut per-token compute (measured, see
`labse-inference-speed-investigation`), which has to come from depth and
width below. Pruning first also lets the teacher fine-tune fit in 6 GB of
VRAM.

### Teacher

Fine-tune a 12-layer model on the data above. Candidates:
- vocab-pruned LaBSE (best cross-lingual start, 768-d);
- multilingual-e5-small (12L, 384-d);
- paraphrase-multilingual-MiniLM-L12-v2 (12L, 384-d).

Pick the one with the best eval after fine-tuning. Losses:

- **InfoNCE** (multiple-negatives ranking) on grade-1.0 pairs, with in-batch
  negatives plus explicit hard negatives. Mask version pairs (0.6) out of the
  in-batch negatives so they aren't pushed apart.
- **CoSENT** on graded pairs, to get the ordering same > translation >
  version > hard negative > random.
- **Cross-lingual distillation** (Reimers & Gurevych 2020) on ja↔en
  translation pairs: student(ja) ≈ teacher_LaBSE(en). This keeps translation
  ability that our small translation set can't teach alone. Precompute the
  LaBSE vectors once (~10–15 min on the 3060).
- **Hard-negative mining rounds:** after each epoch, re-embed the corpus with
  the current model, pull the top-ranked wrong neighbours, and add them as
  hard negatives.

### Student (what ships)

Distill the fine-tuned teacher into small students, initialized from the
teacher's pruned vocabulary and a subset of its layers. Sweep:

| Student | Layers × width | Non-embedding params (approx.) |
|---|---|---|
| S | 4 × 256 | ~3M |
| M | 6 × 256 | ~5M |
| L | 6 × 384 | ~11M |

Loss: MSE to the teacher's embeddings over the whole unlabeled JP+EN title
corpus (millions of titles, no labels needed), plus a small InfoNCE term on
labeled pairs. Optionally add **Matryoshka** loss so one model can be
truncated to 64/128/256 dims at deploy time. That's a free size/recall knob.

**Choose the size from the curve:** ship the smallest student whose harness
retrieval recall stays within ~1–2 points of the teacher's, at the CPU budget
below.

### Compute (estimates — the first run measures real throughput)

- Teacher run: ~1 h on the RTX 3060 (6 GB), fp16, short sequences.
- Student run: noticeably less than a teacher run.
- Realistic total: 5–10 runs across both, about a GPU-day spread over
  sessions, mostly unattended.
- Training stack: Python + PyTorch + sentence-transformers, managed with `uv`
  (raw venvs break on this Nix machine), in `train/title-encoder/`. Outputs go
  to the gitignored `data/title-encoder/`.

## Evaluation

The first five evals, in order of importance, run against every baseline and
candidate model. Baselines:
- **lexical-only:** existing channels + core-title parse + transliteration
  keys;
- **off the shelf:** LaBSE, MiniLM-L12-multi, e5-small;
- **teacher** and **each student**.

1. **Harness split probes** on the live snapshot
   (`plan-softmatch-validation.md`): retrieval recall by entry type and by
   script stratum (same script / CJK↔Latin / mixed), plus the encoder
   channel's marginal recall over the lexical + transliteration channels.
   **This is the gate.**
2. **Recall@k vs pool size.** The same queries against 10k / 100k / 1M title
   pools, padded with held-out MB JP+EN titles as distractors. This is the
   direct test of "works small, fails at scale": a good model's curve stays
   flat as the pool grows.
3. **Template-confusion test.** Held-out `template(A)` queries, where the pool
   contains `template(B)` from the same channel. The metric is how often the
   true partner outranks every same-template distractor. This targets the
   `[Official MV]` failure directly.
4. **MB 500-pair cross-language benchmark** (method in
   `plan-semantic-blocking-v2.md`, 54k pool). Kept for continuity: LaBSE
   scores 56% recall@20 with median rank 7. Rebuild it with the leakage
   exclusion applied.
5. **CPU cost** through the shipped runtime: ms/title batched and
   length-sorted, at 1 thread and all 16 threads; model load time; file size;
   peak RSS.

To make the evals model-agnostic, the harness takes
`--embeddings <file>`: precomputed vectors keyed by a hash of the input text.
Any model, including Python-only ones, can then be evaluated before any Rust
integration exists.

**Ship gate (proposed, to confirm):**
- Eval 1 recall ≥ LaBSE's;
- Eval 2's curve no worse than LaBSE's;
- Eval 3 ≥ 95%;
- Eval 5 ≤ 1 ms/title batched on this machine's CPU.

## Runtime integration

- Export the student to ONNX and verify parity against PyTorch (cosine >
  0.9999 on a fixed text set, as a checked-in test like
  `config/inference/tests/parity.rs`).
- Run it in the `inference` cdylib behind a new `onnx` feature using the `ort`
  crate. This replaces the candle MiniLM path (measured 18× slower than torch
  on CPU). The C ABI (`inference_embed_batch`) stays unchanged.
- The host passes **parsed core titles** into the embed hook, instead of raw
  `best_title`.
- Set `embed_dim` to the student's output dim and `embed_model_id` to the
  student's version string. Rebuild `embeddings.db` (its current contents are
  naive-fallback vectors anyway). The online path then embeds only new
  entries.
- Length-sorted batching inside `inference_embed_batch` (the one free speed
  win in the LaBSE investigation).

## Decider (follow-up plan, sketched here for direction)

Cascade, cheapest first:

1. **Hard rules:** shared hard ID → same; barrier or marker conflict →
   distinct.
2. **A learned pairwise scorer** (GBDT or logistic) over:
   - encoder cosine on core titles;
   - transliteration match;
   - duration delta;
   - artist overlap;
   - release position;
   - version-token difference.

   Microseconds per pair, parallel across cores (today's Rhai is single
   threaded at ~1.9k pairs/s), calibrated into merge / separate / defer bands.
3. **The defer band only** goes to Jev or the human review queue. No general
   model in the hot path.

## Phases

| # | Phase | Exit criterion |
|---|---|---|
| 0 | Retrieval half of the validation harness, with `--embeddings`; baselines recorded (lexical-only, LaBSE, MiniLM, e5-small) | Baseline report checked in under `data/eval/reports/` |
| 1 | Core-title parse in Rust + transliteration keys | Measured marginal recall; tells us how much is left for the encoder |
| 2 | Data build: MB tier queries, YouTube fetch, template mining, synthetic wraps, leakage exclusion, dataset card | Card with counts; spot-check 100 random pairs per type |
| 3 | Teacher fine-tune + evals 1–4 | Teacher beats LaBSE on evals 1–3 |
| 4 | Student sweep + Matryoshka + eval 5 | Size/recall/speed curve; one student picked |
| 5 | ONNX + `ort` in `inference`, parity test, host passes core titles, re-embed | Online import uses it; harness numbers reproduce from Rust |
| 6 | Decider cascade | Separate plan |

## Decisions (2026-10-02)

1. **Data order:** clean MB data first (name<->alias *and* alias<->alias,
   song groups linking video recordings back to audio via work + credit).
   Template mining and the synthetic set are built while that trains, and are
   used for a second phase only if the first is promising.
2. **Training stack:** Python + PyTorch (CUDA) via `uv`, in
   `train/title-encoder/`. Runtime integration comes later.
3. **CPU budget:** aim for <= 1 ms/title; up to ~10 ms is acceptable.
4. **Encoder input:** core title only for now; conditioning on the primary
   artist is a later experiment.
5. **Leakage:** strict — library entities, their credited artists, and any
   artist sharing a library artist's name are excluded, with all their
   recordings and works.
6. **YouTube fetch:** done for the MB-linked videos of non-excluded credits
   (19,273 videos, 386 quota units).

## Results so far (2026-10-02)

Code: `train/title-encoder/` (see its README). Data, models and eval reports:
gitignored `data/title-encoder/`.

### Data

- MB export (replication seq 189407), leakage-excluded: 17,651 artists,
  1.87M recordings, 216k works removed.
- The first JP-tier rule (any artist credited on a `jpn`-language release)
  pulled in Western/classical artists via Japanese reissues; tier is now
  area-first, with release language deciding only for artists with no area.
  Classical movements/catalogue numbers and medleys are filtered from pairs.
- `pairs.py`: 345k training rows (JP 276k / EN 69k), incl. ~143k
  cross-script JP pairs; 67k with same-artist hard negatives.
- YouTube: 19,273 MB-linked videos fetched (386 quota units) → 6.2k JP + ~8k
  EN real (recording name, video title) pairs, 5,465 mined templates.
- `synth.py`: 299k same-template triplets (negative = same template, same
  artist, different song) + 13k real video rows (negative = same channel).

### Evaluation sets

- **Library** (`eval_library.py`): 10,944 queries from 26.5k library entries,
  pool = all aliases of the same entry type (track pool 11k). Only 0.4% of
  library (query, target) pairs appear verbatim in training. Lower bound:
  library duplicates count as misses.
- **MB held-out**: ~3k pairs from hash-held-out groups/credits.
- **Video held-out**: 230 real (recording → YouTube title) pairs vs ~18k titles.
- **Template confusion**: 1,144 held-out artists; corpus = one template around
  up to 10 of that artist's songs; only the core title discriminates.

### Models (library recall@20 unless noted)

| Model | all | cross-script | lexically hard | video→clean | template @1 | CPU ms/title (16 thr) |
|---|---|---|---|---|---|---|
| lexical char-ngram (SVD) | 0.606 | 0.289 | 0.397 | 0.545 | — | — |
| LaBSE | 0.614 | 0.447 | 0.409 | 0.411 | 0.601 | ~18 (torch) |
| MiniLM-L12-multi | 0.557 | 0.290 | 0.317 | 0.500 | — | — |
| multilingual-e5-small | 0.785 | 0.360 | 0.625 | 0.607 | 0.840 | 3.3 (onnx, 12L) |
| **v1**: e5-small + clean MB (1 epoch) | 0.936 | 0.794 | 0.885 | 0.948 | 0.949 | 3.3 |
| **v2**: v1 + synthetic/real video + MB replay | 0.935 | 0.807 | 0.887 | 0.941 | 0.978 | 3.3 |
| **v3**: v2 + cross-script artist hard negatives | 0.941 | 0.840 | 0.899 | 0.942 | 0.977 | 3.3 |
| **v3-L6**: 6-layer student of v3, vocab-pruned (**recommended**) | **0.944** | **0.848** | 0.902 | 0.942 | 0.974 | **1.23** (4.4 @1 thread) |
| **v3-L4**: 4-layer student of v3, vocab-pruned (sub-1 ms option) | 0.929 | 0.793 | 0.876 | 0.935 | 0.962 | **0.82** (2.9 @1 thread) |
| **v2-L6**: 6-layer student, vocab-pruned | 0.932 | 0.794 | 0.881 | 0.941 | 0.975 | **1.08** (4.0 @1 thread) |
| **v2-L4**: 4-layer student, vocab-pruned | 0.912 | 0.718 | 0.846 | 0.934 | 0.962 | **0.84** (2.7 @1 thread) |

Students are distilled from v2 (MSE to teacher embeddings on 400k titles +
contrastive replay), keeping evenly spaced teacher layers. **L6 is the sweet
spot**: within ~1 point of the teacher at ~1 ms/title; L4 trades ~9 points
of cross-script recall for 20% more speed. ONNX sizes (fp32, pruned): 12L
470 MB unpruned, L6 158 MB, L4 144 MB.

Vocab pruning (250k → 75k tokens, 118M → 50M params for 12L, 36M for L4)
changes no metric (pruned v2: 0.935 / 0.808). ONNX export parity: cosine
1.000000 vs PyTorch.

### Scale: recall@20 as the pool grows

Library pools padded with MB titles/names (`scale_distractors.jsonl`, 1.4M
track titles, 521k artist names; distractors only, never queries). Pool per
type ≈ 11k → 111k → 511k.

| Model | tracks | artists | cross-script |
|---|---|---|---|
| LaBSE | 0.613 → 0.521 → 0.456 | 0.586 → 0.515 → 0.454 | 0.447 → 0.368 → 0.310 |
| e5-small base | 0.806 → 0.744 → 0.678 | 0.726 → 0.688 → 0.646 | 0.360 → 0.281 → 0.234 |
| v2 teacher | 0.979 → 0.970 → 0.958 | 0.852 → 0.784 → 0.720 | 0.807 → 0.702 → 0.610 |
| v3 (11k → 511k) | 0.980 → 0.959 | 0.871 → 0.740 | 0.840 → 0.645 |
| **v3-L6** (11k → 511k) | 0.980 → 0.950 | 0.881 → 0.762 | 0.848 → 0.676 |
| v3-L4 (11k → 511k) | 0.972 → 0.929 | 0.852 → 0.706 | 0.793 → 0.573 |
| v2-L6 | 0.978 → 0.963 → 0.948 | 0.849 → 0.783 → 0.716 | 0.794 → 0.690 → 0.592 |
| v2-L4 | 0.968 → 0.950 → 0.929 | 0.809 → 0.725 → 0.653 | 0.718 → 0.582 → 0.475 |

Track retrieval is nearly scale-flat for the trained models (−2 to −4 points
over a 46× larger pool, vs −16 for LaBSE). **Artist names and cross-script
pairs are what don't scale** (−13 to −20 points): name transliteration is
ambiguous, so the dense model alone can't carry it — dictionary romaji keys
and artist-specific handling are needed there.

### Findings

- LaBSE is barely better than lexical on this task and worst on template
  confusion, confirming the "structure dominates" problem.
- Most of the gain comes from phase 1 (clean MB). Phase 2 mainly fixes
  template confusion (0.949 → 0.978) and real video titles (held-out @1
  0.70 → 0.835).
- v2's library r@1 dropped (0.565 → 0.538): it ranks *other vtubers' covers*
  of the same song first, because it learned the performer part of a video
  title is not identity. Fine for recall@k, but at production scale a hit
  song with hundreds of covers could flood top-k. Artist conditioning, or
  re-ranking retrieved candidates by artist, should move up.
- v3 adds 41k cross-script artist triplets whose negative shares one name
  part (`堀内 正人` → `Masato Horiuchi`, not `Marina Horiuchi`): cross-script
  0.807 → 0.840, artists 0.852 → 0.871, and +2–3 points at 511k, with no
  regression elsewhere.
- **Recommended model: v3-L6** (`data/title-encoder/models/e5s-v3-L6/pruned`,
  `model.onnx` 158 MB fp32): it matches or beats its 12-layer teacher (the
  distillation also saw the artist triplets) at ~1.2 ms/title on 16 threads.
- Remaining cross-script misses are mostly artist names, kanji ↔ romanized
  (`蛇石 徹` ↔ `Toru Hebiishi`), where the model matches only one name part:
  the case for the dictionary-based romaji keys in phase 1. Many "missed"
  tracks are actually library duplicates (`Uchiagehanabi` → `Uchiage Hanabi`).
- The library has junk aliases such as `Private video`.

### Artist conditioning experiment

**Cover flooding, measured.** With v3-L6 at the 511k pool, 304 of 6,085
track queries (5%) miss the top 20. In 67 of those, 10+ of the top 20 are the
same song (other covers/versions); in 79 more, 3–9 are. About half of the
remaining misses at scale come from covers crowding out the right version.

**Setup.** `eval_library --artist` attaches each alias's own source's primary
artist (`contribution`: main `listed_artist` index 0, else `uploader`) and
keeps same-title/different-artist aliases as separate pool items, so covers
no longer collapse into one item. 12,052 of 14,087 track items have an
artist. A model given as `path@cond` embeds `title [A] artist`.

**Zero-shot** (v3-L6, no conditioned training), tracks, 11k pool:

| Input | r@1 | r@10 | r@20 |
|---|---|---|---|
| title only | 0.389 | 0.914 | 0.976 |
| `title [A] artist` | 0.552 | 0.949 | 0.978 |

Appending the artist already lets the encoder separate covers (+16 points
top-1). `cond_data.py` builds 205k conditioned triplets (92k with true cover
negatives: same MB work, credits with no artist in common; artist slot
varied between credit name and artist aliases) and `cover_test.jsonl`
(1,844 held-out works) for a trained variant (v4c).

**Trained (v4c)**: v3 + `cond_triplets` + plain replay. Held-out cover test
@1 0.951 → 0.997; MB/video/template tests unchanged. Library, artist mode:

| Tracks (artist mode) | 11k: r@1 / r@10 / r@20 | 511k: r@1 / r@10 / r@20 |
|---|---|---|
| v3, title only | 0.365 / 0.907 / 0.975 | 0.348 / 0.876 / 0.958 |
| v3 + artist (zero-shot) | 0.436 / 0.922 / 0.970 | 0.426 / 0.913 / 0.963 |
| v4c, title only | 0.397 / 0.916 / 0.978 | 0.378 / 0.890 / 0.960 |
| **v4c + artist** | **0.626 / 0.974 / 0.989** | **0.621 / 0.969 / 0.985** |

Conditioning fixes the cover flood: at 511k, track misses @20 drop from
4.2% to 1.5% and the model is nearly scale-flat (0.989 → 0.985). v4c still
works with title-only input (no worse than v3), so one model serves both.
Cross-script (all types, artist mode) at 511k: 0.634 → 0.730.

Implication for integration: embed tracks as `core title [A] primary artist`
when the entry has a primary artist (the per-source credit or uploader), and
title-only otherwise. Artist entries themselves stay unconditioned.

**Distilled (v4c-L6, vocab-pruned) — new recommended model**
(`data/title-encoder/models/e5s-v4c-L6/pruned`, `model.onnx` 158 MB fp32,
1.23 ms/title on 16 threads, 4.4 ms on 1): cover test 0.999, template 0.973.

| v4c-L6 | 11k: r@1 / r@20 | 511k: r@1 / r@20 |
|---|---|---|
| tracks, `title [A] artist` | 0.602 / 0.987 | 0.597 / 0.981 |
| tracks, title only | 0.401 / 0.978 | 0.380 / 0.950 |
| cross-script (all types, with artist) | 0.478 / 0.856 | 0.344 / 0.739 |
| artists (unconditioned) | 0.710 / 0.873 | 0.545 / 0.757 |

It loses almost nothing to its 12-layer teacher (tracks @511k r@20 0.981 vs
0.985) and is ~1 point behind v3-L6 on unconditioned artist names.

### Model-mined artist negatives (v5)

`mine_negatives.py` embeds 183k training-split artist names (110k artists)
with v4c and takes each anchor's nearest names that belong to a *different*
artist (identical strings skipped) as negatives: 79k triplets of the
model's own confusions (`Akeo Watanabe` vs `Akio Watanabe`, `石川三恵子` vs
`石川綾子`). v5 = v4c + those + replay. Artist mode, `title [A] artist`:

| r@20 | 11k: v4c → v5 | 511k: v4c → v5 |
|---|---|---|
| artists | 0.869 → 0.878 | 0.750 → 0.778 |
| cross-script | 0.870 → 0.881 | 0.730 → 0.768 |
| tracks | 0.989 → 0.989 | 0.985 → 0.987 |
| all | 0.949 → 0.953 | 0.909 → 0.919 |

No regressions (MB test @1 0.618, best so far; cover 0.996; template 0.976).
Another mining round with v5 is the obvious continuation.

**Distilled (v5-L6, vocab-pruned) — new recommended model**
(`data/title-encoder/models/e5s-v5-L6/pruned`, 158 MB ONNX, 1.18 ms/title on
16 threads, 4.4 ms on 1; cover 0.999, template 0.970). With artist
conditioning, r@20 at 11k → 511k: artists 0.890 → 0.792, cross-script
0.883 → 0.785, tracks 0.988 → 0.983, all 0.954 → 0.920. Again the student
edges out its 12-layer teacher on artists (0.792 vs 0.778 at 511k).

**Round 2 (v6)**: re-mined with v5 (79k triplets, subtler confusions such as
`矢島公紀` vs `矢島寵児`), trained from v5. Teacher r@20 with artist
conditioning, 11k → 511k: artists 0.883 → 0.787 (v5: 0.878 → 0.778),
cross-script 0.890 → 0.782 (v5: 0.881 → 0.768), tracks unchanged at
0.989 → 0.987. Template test 0.980 (best so far). Gains are shrinking:
a third round is likely not worth it without new negative sources.

**Distilled (v6-L6, vocab-pruned)**
(`data/title-encoder/models/e5s-v6-L6/pruned`, 158 MB ONNX, ~1.2–1.3 ms/title
on 16 threads, 4.4 ms on 1; cover 0.999, template 0.973). With artist
conditioning, r@20 at 11k → 511k: artists 0.892 → 0.802, cross-script
0.883 → 0.799, tracks 0.986 → 0.983, all 0.954 → 0.923. Marginal over
v5-L6 (+1 point artists/cross at 511k).

### Artist wrappers (v7)

**Why.** Sorting the v6-L6 artist misses showed that most are not
transliteration failures. Roughly: legal name ↔ stage name with no text
link (`Shunsuke Doi` ↔ `じん`), very short ambiguous names (`しの`), and
**platform wrappers**: `Takane Lui - Topic` ranks other `- Topic` channels
first (0.87), and `Niko Ch. 虎金妃笑虎 - FLOW GLOW` ranks other FLOW GLOW members
first. Wrappers come from YouTube (`- Topic`, `VEVO`, `Official`,
`channel`/`チャンネル`, `A Ch. B - Group`, `【agency】`), Discogs (`(n)`, `様`)
and SoundCloud (`(All)`). All artist training data so far was clean MB names,
so nothing taught the model to discount them. v2 did teach this for track
titles. The 12-layer teacher is no better than the L6 student on artists,
so model capacity was not the limit.

**Rule baseline first** (`artist_name.py`, multi-vector max-sim over name
fragments, affiliations detected across the corpus): v6-L6 artists r@20
@511k 0.805 → 0.830, video→clean 0.819 → 0.887. It works, but it's brittle:
each fix broke something else (`(CV:…)`, unspaced dashes, `koyori` taken for
an affiliation), and it can't tell `《IzumoKasumi》Project channel` (name in
brackets) from `樋口楓【にじさんじ】` (agency in brackets). Casefolding the
fragments hurts badly (cross-script −8): the model relies on case.

**Data** (`channels.py`, `artist_wrap.py`; leakage-filtered artists only):
- 33.3k titles of MB-linked YouTube channels (675 quota units). 15.7k equal
  an artist name, 12.5k wrap one, 5.2k contain none (romanized, compressed
  or nicknames).
- 146 templates mined by masking the artist's own names (`{a} - Topic`,
  `{a} Official`, `{b} / {a}`, `{a}【公式】`, …; seen on >= 3 artists).
- 22 hand-written VTuber shapes (`{n} Ch. {a} - {g}`, `{a} / {r}【{g}】`, …).
  `{g}` comes from 37k MB JP label / group names. Library affiliations and
  library artist names are removed from that pool, so the library eval sees
  only unseen groups.
- 261k triplets in total: real (title, name) rows with a same-template
  negative, and synthetic wraps whose negative is **the same template and
  group around another artist**.
- Held out: 3% of artists and 10% of group names. `artist_wrap_test`
  (2,742 queries, each ranked against its own corpus: the right alias vs
  ~10 other artists in the same wrapper and group, plus their clean names)
  and `channel_test` (522 real held-out titles vs all 33k titles + names).
  The first artist_wrap scores went *down* because the IR evaluator merges
  every query's corpus into one pool, which holds the query's own artist
  unmarked; `PerQueryEvaluator` fixes this.

**v7** = v6 + 180k wrapper triplets + v6's replay mix (lr 1.5e-5, 1 epoch);
**v7-L6** distilled as before, plus 120k wrapper triplets, vocab-pruned
(ONNX 159 MB, parity 1.000000, 1.14 ms/title on 16 threads, 3.8 ms on 1).

Held-out (@1 unless noted):

| | v6 | v7 | v6-L6 | v7-L6 |
|---|---|---|---|---|
| artist_wrap (unseen artists + groups) | 0.704 | 0.938 | 0.667 | **0.942** |
| channel titles @1 / @20 | 0.456 / 0.538 | 0.843 / 0.885 | 0.454 / 0.521 | **0.835 / 0.881** |
| MB test @1 | 0.615 | 0.619 | 0.598 | 0.603 |
| video @1 | 0.804 | 0.791 | 0.778 | 0.770 |
| template | 0.979 | 0.976 | 0.973 | 0.968 |
| cover | 0.997 | 0.997 | 0.997 | 0.997 |

Library, artist mode, `title [A] artist`, r@20 at 11k → 511k:

| | tracks | artists | cross-script | all |
|---|---|---|---|---|
| v6-L6 | 0.986 → 0.983 | 0.892 → 0.802 | 0.883 → 0.799 | 0.954 → 0.923 |
| v7 (teacher) | 0.992 → 0.989 | 0.901 → 0.817 | 0.902 → 0.804 | 0.962 → 0.932 |
| **v7-L6** | **0.991 → 0.985** | **0.909 → 0.828** | **0.908 → 0.824** | **0.963 → 0.934** |

Artists r@1 at 511k: 0.613 → 0.645. Once again the student beats its
teacher on artists.

**Model vs rules** (artists @511k, r@20 all / video→clean):

| | raw | platform strip | full rule parse |
|---|---|---|---|
| v6-L6 | 0.805 / 0.819 | 0.816 / 0.862 | 0.830 / 0.887 |
| v7-L6 | 0.829 / **0.896** | 0.833 / 0.896 | 0.835 / 0.896 |

v7-L6 alone matches v6-L6 plus the full parser overall, and beats it on
video→clean titles. On top of v7, the full parser adds +0.6 and the lossless
platform strip (`platform_strip`: `- Topic`, `VEVO`, `(n)`, `(All)`, `様`)
adds +0.3. Decision: **no fragment parser at runtime**; `platform_strip`
is optional.

Remaining artist misses at 511k are mostly the no-text-link and short-name
buckets. Text retrieval can't fix those; the planned fix is artist
candidates from track overlap (collective matching).

## Integration contract (for the Rust / `ort` side)

What runtime code must reproduce to get the same vectors as training/eval
(model: Hugging Face [`gbnam8/jp-music-title-encoder`](https://huggingface.co/gbnam8/jp-music-title-encoder),
formerly `data/title-encoder/models/e5s-v7-L6/pruned/`; local copies were deleted after upload):

- **Files:** `model.onnx` (graph includes mean pooling + L2 normalize;
  inputs `input_ids`, `attention_mask` as int64 `[batch, seq]`; output
  `embedding` float32 `[batch, 384]`), `tokenizer.json` (HF `tokenizers`
  format, XLM-R SentencePiece Unigram pruned to 75,458 tokens; loads with the
  Rust `tokenizers` crate directly).
- **Architecture:** BERT, 6 layers, hidden 384, max length used in training
  **48 tokens** (truncate; titles are short).
- **Tokenization:** `tokenizers` with special tokens added (`<s> … </s>`,
  via the file's TemplateProcessing). No prefix (no e5 `query: `).
- **Input text:**
  - tracks: `"{title} [A] {primary artist}"` when the source has a primary
    artist (main `listed_artist` at index 0, else `uploader`), else
    `"{title}"`. The separator is literally ` [A] `.
  - artists, releases, release groups: the name alone, raw (`Takane Lui -
    Topic`, `Niko Ch. 虎金妃笑虎 - FLOW GLOW` as stored; v7 learned the
    wrappers). Optionally strip the platform suffixes first
    (`artist_name.platform_strip`: `- Topic`, `VEVO`, Discogs `(n)`/`様`,
    SoundCloud `(All)`; +0.3 artists r@20); no other name parsing.
  - Titles are embedded as stored (raw alias); the model was trained on
    raw, template-wrapped video titles, so core-title parsing is optional.
- **Similarity:** dot product of the (already normalized) vectors = cosine.
- **Dimensions:** the vector can be truncated to its first N dims and
  re-normalized (Matryoshka, inherited from the teachers). v7-L6 with artist
  conditioning at the 511k pool, r@20 tracks / artists / cross-script / all:
  384: 0.985 / 0.828 / 0.824 / 0.934; **256: 0.983 / 0.824 / 0.811 / 0.931**.
  (v6-L6 for the lower dims: 128 costs 1–3 points, 64 costs 2–7.)
  256 is essentially free and matches the current `embed_dim` default;
  128 costs 1–3 points for a 3x smaller index.
- **Batching:** sort inputs by length before batching (the one free CPU
  win); ~1.1 ms/title on 16 threads, ~3.8 ms on 1, tokenization included.
- **Parity check:** `export_onnx.py` asserts min cosine 1.000000 between
  ONNX and PyTorch on 256 library titles; a Rust port should reproduce the
  same vectors to ~1e-5 on a fixed text set.

### Romaji-key channel, measured before writing Rust (`eval_romaji.py`)

Prototype of phase 1's transliteration keys: pykakasi Hepburn reading,
long-vowel/variant folding, sorted tokens (name-order invariant), exact-key
blocks capped at 50. Marginal over v6-L6 (`title [A] artist`) at the 511k
pool, r@20:

| | encoder | ∪ romaji keys | gain |
|---|---|---|---|
| artists, cross-script | 0.686 | 0.737 | +5.0 |
| artists, all | 0.802 | 0.817 | +1.5 |
| tracks, cross-script | 0.900 | 0.915 | +1.6 |
| tracks, all | 0.983 | 0.986 | +0.3 |

The encoder already captures most of what transliteration gives. Phase 1's
romaji keys are worth it as a cheap blocking channel for **artist names**
(the weakest spot); for tracks they're a minor add. Name readings are the
limit (pykakasi reads `正人` as `masahito`, not `masato`); a name-aware
reading dictionary would help more than better folding.

## Status and next steps (end of 2026-10-02 session)

**Model work wrapped up (2026-10-02).** Artifacts are on Hugging Face: the
v7-L6 student `gbnam8/jp-music-title-encoder` (public, with ONNX), the v7
teacher `gbnam8/jp-music-title-encoder-teacher` (public), and the training
data, eval reports and logs in `gbnam8/jp-music-title-encoder-data` (private:
it holds YouTube API data and library identifiers). Local models, the `mb-v1`
build and the training venv were deleted to free disk space; resuming
training needs `uv sync` plus a download of the dataset repo into
`data/title-encoder/`.

**Use:** `gbnam8/jp-music-title-encoder` on Hugging Face (see the integration
contract above): `title [A] primary artist` for tracks, name alone otherwise,
truncated to 256 dims. At a 511k-title pool it retrieves the right track in
the top 20 for 98.3% of library queries, artist names 82%, cross-script
pairs 81%, at ~1.1 ms/title on CPU. LaBSE at the same pool size (title-only
eval): 46% / 45% / 31%.

**Lineage:** e5-small → v1 (clean MB) → v2 (+ synthetic/real video titles)
→ v3 (+ cross-script artist negatives) → v4c (+ artist conditioning with
cover negatives) → v5, v6 (+ two rounds of model-mined artist negatives) → v7 (+ artist
platform/channel wrappers), each distilled into a 6-layer student and
vocab-pruned.

**Next, in order of expected value:**
1. Runtime integration (plan phase 5): `ort` in the `inference` crate,
   `tokenizers` for `tokenizer.json`, host passes `title [A] artist`,
   `embed_dim = 256`, rebuild `embeddings.db`.
2. Artist candidates from **track overlap** (collective matching): most
   remaining artist misses (legal ↔ stage names, very short names) have no
   textual link at all.
3. Romaji keys for **artist names** as a blocking channel (+5 points
   cross-script artists measured on v6); a name-reading dictionary matters
   more than folding rules.
4. The Rust validation harness (`docs/plan-softmatch-validation.md`) so the
   integrated pipeline is measured end to end, not just the encoder.
5. Further mining rounds show diminishing returns (+1 point per round);
   new signal would come from new data (e.g. channel uploads of JP artists),
   not more rounds.

**v6-L4 (4-layer, pruned)**, `title [A] artist`, r@20 at 11k → 511k: tracks
0.984 → 0.976, artists 0.874 → 0.757, cross-script 0.857 → 0.730, all
0.946 → 0.904; cover 0.998, template 0.961. ONNX 144 MB. CPU timing this run
was 3.2 ms (1 thread) / 1.9 ms (16 threads), noisier than earlier 4-layer
runs (0.82 ms at 16 threads); re-benchmark on an idle machine before choosing
L4 over L6 on speed. vs v6-L6: −0.7 tracks, −4.5 artists, −6.9 cross-script
at 511k.
