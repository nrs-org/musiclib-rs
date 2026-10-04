# Plan: learned pair classifier for softmatch

Status: agreed 2026-10-03, in progress. Supersedes the decision half of
`plan-softmatch-validation.md` (its retrieval-recall and split-probe ideas are
reused here) and the "Decider" sketch in `plan-title-encoder.md`.

## Why

`config/match.example.rhai` decides with hand-set thresholds (title sim ≥ 0.90
and duration ±2 s → merge, etc.). Nobody knows how accurate it is, every rule
is a closed vocabulary (`version_tokens` misses "arrange"), and it runs
single-threaded at ~1.9k pairs/s. The September logistic model
(`softmatch --model`, `poc/dedup-poc`) was fit on 516 LLM-labeled pairs: too
few and too noisy to learn much from.

We can generate labels at scale, and we now have a title encoder that
understands music titles. So: supervised classification on generated data,
judged against a small human-labeled test set.

## Task

Input: a candidate pair from blocking (same entry type). Output, per type,
calibrated probabilities over:

| Class | Meaning (dedup-v2 ontology) | Policy |
|---|---|---|
| `same_identity` | same recording / edition / project / artist identity | MERGE (soft) |
| `derived_from(kind)` | one is a transformation of the other | RELATE |
| `unrelated` | anything else, including "related but no concrete edge" | DISTINCT |

Policy = thresholds on the probabilities; pairs between thresholds DEFER (to Jev
or the review queue).

**Ontology decisions (user, 2026-10-03):**
- Track: a **full MV** of a song and its audio recording are `same_identity`
  (merge), because a full MV can stand in for the audio track. Any other MV
  version (short ver., dance shot ver., close-up ver., …) is `derived_from`
  (relate): it can't be guaranteed to replace the audio. An **audio cut / short MV / TV size** is `derived_from` (relate,
  kind `edit`). Note: today's Rhai merges "MV short/cut version", which is now
  a labeled error.
- Live, remix, instrumental/off vocal/karaoke, cover, arrangement, lineup
  version (`… ver.`): `derived_from` with that kind.

Track kinds: `edit`, `live`, `remix`, `instrumental`, `cover`, `arrangement`,
`alt_version`. Releases/release groups/artists start with identity only
(same vs not); their relation edges (`member_of`, `facet_of`) come later.

## Data we generate (training + dev)

All keyed by `(source, identifier)` pairs so they survive re-imports.

1. **Library split probes (`same_identity`, library distribution).** Every
   entry that hard-ID linking unified from ≥ 2 sources is cut into two
   disjoint source halves (e.g. YouTube MV vs Spotify + MB). The halves are a
   positive pair built from exactly the kind of data we deduplicate (VTuber
   uploads, `- Topic` channels, MV vs audio). Hard-ID overlap is removed by
   construction, so the model must use soft evidence.
2. **MusicBrainz (local full DB in docker): relations and hard negatives.**
   - Recordings linked by MB recording↔recording relations (remix, edit,
     karaoke/instrumental, DJ-mix excluded) and recordings of the same work
     with performance attributes (live, cover, instrumental, karaoke) →
     `derived_from(kind)`.
   - Recordings of **different works by the same artist** → `unrelated` hard
     negatives (the costliest false merge: same artist, different song).
   - Recordings with a YouTube URL relationship → video title vs recording
     title positives (with durations).
   Restricted to the JP + EN tiers the encoder was trained on.
3. **Negatives come from the retriever.** Train on the pairs blocking actually
   produces, not random pairs. An unlabeled candidate is *not* a negative
   (unlinked duplicates exist — positive-unlabeled), so library negatives are
   only taken with affirmative evidence of difference (different MB works,
   different MB release groups, different ISRC *and* duration Δ > 10 s).
4. **LLM-adjudicated set (516 pairs, pilot-v3 + active-v4)** resolved against
   the v3 corpus: dev only, never train, reported as its own tier.

Every generated source is a separate tier in reports; the generator's biases
must not leak into the headline.

## Model

Features, all from data the scorer already has:

- **Encoder** (`gbnam8/jp-music-title-encoder`, 256-dim): max cosine over the
  alias cross product with `title [A] artist`; title-only cosine; artist-name
  cosine; and the pair vector `[|u−v|, u⊙v]` of the best-aligned aliases
  (where "(Electro Pop arrange)" shows up, so no closed marker list).
- **Structured:** min duration Δ over the duration sets (and signed ratio),
  artist overlap, same release / same release position, shared authoritative
  IDs, version-token symmetric difference (kept as a feature, not a gate), MV
  flags, script relation, numbers conflict, tracklist overlap (releases).

Head: gradient-boosted trees (LightGBM) per type, exported to JSON and
evaluated in Rust (a tree walker is ~100 lines, microseconds/pair, parallel).
Features are symmetrized; a separate small head predicts direction for
`derived_from`. Calibration: isotonic on the dev split.

Optional later (only if the errors are fine-grained title semantics): a
cross-encoder fine-tuned from the L6 student, run only on the defer band.

## Metric

**Headline, per entry type: merge recall at 99% merge precision** on the
human-labeled test set. False merges are the costly error; this number says
how much we can merge automatically while staying safe.

Test set: pairs sampled from the live snapshot's candidate pool, stratified
(type × current verdict × similarity bin), each with its inclusion
probability, so precision/recall are Horvitz–Thompson estimates of the whole
candidate population, not of the sample. Labeled by the user, pre-labels shown
as a hint. Split by entity cluster. Reported with confidence intervals — with
100 labels the intervals are wide and say so.

Secondary:
- PR-AUC for `same_identity` (threshold-free), relation-kind macro-F1,
  direction accuracy, calibration error (ECE).
- Cluster level: B³ precision/recall/F1 on split probes after transitive
  closure; count of soft-merge clusters holding two MB recording MBIDs
  (label-free false-merge proxy); largest cluster.
- Cost: pairs/s, wall time.

Baseline: the current Rhai verdicts scored on the same test set. Every change
lands with its delta.

## Labeling budget

100 pairs on 2026-10-03, more later (target ~600). Labeling UI: a private
artifact page with a shared store; labels are read back with `ArtifactData`
and frozen to `data/eval/gold/human-v1.jsonl`.

## Phases

1. **Snapshot + candidate pool + human batch 1 (100).** Live snapshot
   `data/eval/live-2026-10-03.db` (sha256 `2c34d36f…`), Rhai dry-run CSV,
   stratified sample with inclusion probabilities, labeling page.
2. **Eval scorer** (Python first, reads gold + any predictions CSV): headline
   + secondary metrics, Rhai baseline report.
3. **Generator:** library split probes + MB relations/hard negatives + retriever
   negatives; dataset card with counts per class/type/tier.
4. **Encoder vectors** for the snapshot (Python onnxruntime for training; Rust
   `ort` integration with a parity test for shipping).
5. **Train + calibrate + report** vs baseline.
6. **Ship:** new `DedupModel` schema (trees), Rust feature extraction shared
   with the trainer (export features from Rust so train/serve can't drift).
7. Optional: cross-encoder for the defer band.

## Risks

- Generated data teaches the generator's quirks → the human test set is the
  only headline; generated tiers are dev.
- MB "different recordings" may still be duplicates → negatives only from
  different *works*.
- Train/serve feature drift → features computed by one Rust function, dumped
  for training.
- 100 labels can't pin 99% precision → report CIs; grow the set with
  disagreement-focused batches (where model and Rhai disagree).

## Results, night of 2026-10-03 (no human labels yet)

The human batch (`human-v1`, 100 pairs) is published for labeling but
unlabeled, so the headline metric doesn't exist yet. Everything below is on
generated dev data or the September LLM-adjudicated labels.

### What exists

`train/learned-matcher/` (uv project; outputs in gitignored `data/`):

| Script | Does |
|---|---|
| `sample_gold.py` | stratified human batch with inclusion weights → `data/eval/gold/<batch>.{items.jsonl,blind.json}` |
| `collect_labels.py` | labeling-page store → `<batch>.labels.jsonl` |
| `adjudicated_gold.py` | resolves pilot-v3 + active-v4 LLM labels onto the snapshot: 162 of 516 resolve (26 same / 31 related / 105 different) → batch `adjudicated-v34` |
| `generate.py` | training examples (below) |
| `features.py`, `build_features.py` | views + ~200 features, encoder vectors (v7-L6 ONNX, 256-d) |
| `train.py` | LightGBM 3-class, dev report vs Rhai, per-type merge thresholds, gold predictions |
| `evaluate.py` | weighted metrics + stratified-bootstrap 90% CIs, Rhai or model |

Snapshot `data/eval/live-2026-10-03.db`; Rhai dry run on it:
687k candidate pairs (track 15,093 MERGE / 5,766 RELATE), 397 s.

### Training data (v5: 250k examples)

| Tier | Track | Artist | Release | RG |
|---|---|---|---|---|
| probe (same, split halves) | 3,417 | 1,356 | 906 | 13 |
| probe_neg (half-view, MB-labeled) | ~3.5k | ~1.4k | ~1.0k | 19 |
| mb (real candidates, MB facts) | 117k (297 same, 6.6k related) | 80k | 18.6k | 10.4k |
| provider (entry_relation) | 1,435 related | | | |
| uploader_neg (same channel, Δdur > 3 s, no marker diff) | 4,312 | | | |

Lessons while building it:
- **Half-view confound.** With only split halves as positives, the model
  learned "both sides are halves → same" (artist half-view negatives scored
  p_same 0.7–0.9 on `shirobeats`/`BAEKObeats`). Fix: every probe entry also
  gets up to 3 MB-labeled negatives built from its half. Provider-overlap and
  source-count features are excluded for the same reason.
- **Rhai merges covers.** On real candidate pairs MB labels, Rhai's track
  MERGE precision is 0.20: same-titled covers by different artists ("KING",
  "Last Christmas") merged as one recording. The model sends them to related.
- **Release editions** ("Hololive Summer 2022" × 12 Discogs releases in one
  master) need a structural feature: `same_group` / `group_conflict`
  (shared parent release group). Release accuracy on adjudicated 0.60 → 0.90.
- **Template negatives** (`チノカテ / 角巻わため(Cover)` vs `BOY / …`,
  `Watame drum #1` vs `#2`) aren't in MB; same-uploader video pairs supply
  them.
- **MV vs audio** is the weakest same-identity case: only 297 MB
  `music video` positives exist in the library. MB's `music video` link also
  covers alternate cuts ("Dance Shot Ver.", "Close-up Ver."), which the
  model calls related. User (2026-10-03): those are related; only the full
  MV is same. v6 applies this to the MB labels.

### Numbers

Generated dev (entity-split, same generator as train, optimistic), v5:

| Type | same AP | related AP | same recall @ P≥0.99 | accuracy on MB real pairs (Rhai → model) |
|---|---|---|---|---|
| track | 0.988 | 0.957 | 0.87 | 0.950 → 0.992 |
| artist | 0.95 | 0.64 | 0.28 | 0.998 → 0.999 |
| release | 0.94 | 0.96 | 0.10 | 0.965 → 0.999 |

Encoder ablation (v1, track): same R@P99 0.909 with encoder vs 0.843
without, related AP 0.958 vs 0.940.

LLM-adjudicated set (162 pairs, unweighted). **Used for error analysis twice
(release groups, uploader negatives), so it is now a dev set, not a test
set.** v5 with dev-chosen merge thresholds (98% dev precision) and a defer band:

| | Rhai | v5 |
|---|---|---|
| merge precision (all) | 0.70 [0.56, 0.85] | 0.83 [0.67, 1.00] |
| merge recall (all) | 0.73 | 0.39 (artists all deferred) |
| link recall (same or related) | 0.51 | 0.93 |
| accuracy on decided pairs | 0.78 | 0.93 (13% deferred) |
| track merge P / R | 0.67 / 0.83 | 0.83 / 0.83 |
| release related recall | 0.00 | 0.94 |

Track is a clear win. Artist merge threshold (0.996) is too strict: 8 of 10
true artist merges deferred; Rhai merges 9/10 but with 3 false merges.

### Relation head (`train_relation.py`, rel-v1, track related pairs)

Kind (dev, 3k pairs): accuracy 0.95, macro-F1 0.72; cover F1 0.97,
instrumental 0.95, live 0.87, remix 0.78, arrangement 0.54 and edit 0.50
(51 / 32 examples: data-limited). Direction (swap-augmented, antisymmetric
by construction): accuracy 0.91, 0.97 on the 83% of pairs with
|p − 0.5| > 0.3; instrumental 0.98, live 0.96, cover 0.91, arrangement 0.65.

### Artists

The highest-scoring artist "negatives" on dev are mostly MB label noise or
genuine ambiguity: homonyms MB keeps apart (`HYDE`/`Hyde`, `MiU`/`MiU`),
aliases MB models as separate artists (Yuyoyuppe ↔ DJ'TEKINA//SOMETHING,
花譜 ↔ カフ). The lowest-scoring positives have no textual link (佐渡満 ↔
Mitsuru Sado). Text can't reach 98% precision on artists; deferring them to
Jev/review is the intended cascade, and real gains need collective evidence
(shared credits) and MB alias / relation data in the features.

### Next

1. Human labels → the real headline (merge recall @ P99, weighted).
2. Artist thresholds/data: artist positives are only split halves; MB
   alias/legal-name data could add real ones.
3. Kind + direction head for RELATE (needs asymmetric per-side features).
4. MV positives: MB-wide `music video` relations (38k) as extra views.
5. (On hold per user, 2026-10-03: not before human labels.) Ship: Rust feature extraction + tree evaluator + `ort` encoder, with a
   Python↔Rust feature parity test.

## Jev with the learned model (2026-10-03, overnight session 2)

Total Jev spend: **$1.31** (18k calls, 31M input tokens; cap was $5). Code:
`jev_client.py` (Python port of `src/pipeline/jev.rs` evidence view + prompts,
disk cache by request hash), `jev_audit.py`, `jev_teach.py`,
`jev_pool_estimate.py`, `silver_set.py`, `predict_gold.py`, `pool_predict.py`.

### Is Jev accurate? (`jev_audit.py`, 547 pairs with non-Jev labels)

The shipped track prompt (v1) contradicted the ontology: it calls TV size and
cover-credit tags "same", and independent covers "unrelated". An
ontology-aligned prompt (v2, then v2.1 adding stems, named versions, concert
shows, placeholder titles) fixed most of the gap:

| Category | Jev v1 | Jev v2.1 | model v7 |
|---|---|---|---|
| full MV = same | 0.67 | **0.97** | 0.76 (30% deferred) |
| cover vs original | 0.76 | 0.92 | 0.84 |
| sibling versions | 0.44 | 0.88 | 0.96 |
| live vs studio | 0.75 | 0.75 | 0.85 |
| hard unrelated (MB different works) | 1.00 | 1.00 | 0.97 |
| model DEFER band | 0.79 | 0.82 | — |
| artist split-half same | 0.96 | 0.96 | 0.67 (76% deferred) |
| artist MB-distinct, similar names | 1.00 | 1.00 | 1.00 |
| release split-half same | 0.93 | 0.93 | 1.00 (80% deferred) |

Jev never answers `unsure`; its confidence is the gate. Accuracy by
confidence: ≥0.95 → 0.98, 0.8–0.95 → ~0.92, <0.8 → 0.66–0.83. Artists ≥0.8
were 100% right. When Jev confidently (≥0.9) disagrees with a model decision,
Jev is right ~2/3 of the time (n=14): a "send to review" signal, not an
override. **Jev v2.1 should replace the shipped `jev.rs` track prompt.**

### Distillation (Jev as teacher)

`jev_teach.py` labels unlabeled candidate-pool pairs, stratified by where the
model is new or unsure (its merges/relates Rhai didn't make, Rhai merges it
turned into relates, DEFER, distinct boundary). Two rounds: 10k pairs on v7
strata, 4.3k on v8 strata. Confident answers (track ≥0.9, others ≥0.8) become
tier `jev` (≈5.9k examples: track 1.4k same / 1.8k related / 1.3k unrelated,
artist 0.7k same / 0.2k different, release 0.6k). v8 = v7 + round 1,
v9 = + round 2.

New features in v7 (from the novel-merge audit): `core_exact` /
`core_tri_max` (title with brackets, "/ artist" tail and markers stripped),
`bracket_jacc` / `bracket_xor`, `placeholder` ("Private video"), markers
`stem` and `performance` (昼公演, day 2, ...). They only pay off once the
Jev tier supplies examples of those cases.

### Silver test set (`silver-jev-v1`)

1,468 pairs from dev-split entities only (never trained on), stratified like
`human-v1` (type × Rhai verdict × title-sim band) with inclusion weights, Jev
v2.1 labels. Population-weighted, 90% bootstrap CIs in
`data/eval/reports/*-silver-jev-v1.json`. Judge error is a few points; v8/v9
learned from the same judge, so silver favours them — the human set remains
the arbiter.

| | Rhai | v6 | v7 | v8 | **v9** |
|---|---|---|---|---|---|
| track merge precision | 0.51 | 0.77 | 0.80 | 0.88 | **0.91** [0.87, 0.95] |
| track merge recall | 0.70 | 0.76 | 0.75 | 0.72 | **0.76** |
| track merge recall @ P≥0.99 | — | 0.25 | 0.23 | 0.35 | **0.46** |
| track relate precision / recall | 0.96 / 0.15 | 0.93 / 0.63 | 0.93 / 0.66 | 0.93 / 0.72 | 0.93 / **0.73** |
| artist merge precision / recall | 0.90 / **0.94** | 1.0 / 0.06 | 1.0 / 0.03 | 0.94 / 0.78 | 0.94 / 0.81 |
| release editions (related) recall | 0.00 | 0.93 | 0.93 | 1.00 | 1.00 |

Unbiased per-verdict check of v8 (fresh random Jev sample of its own
verdicts, training pairs excluded, all confidences): track MERGE precision
0.89 ± 0.03 vs Rhai 0.51 ± 0.05; artist MERGE 0.97 ± 0.02 vs Rhai 0.91 ±
0.05; track RELATE 0.94 ± 0.03. Where Rhai merges and v8 relates, Jev sides
with v8 14/14 (conf ≥ 0.9).

Non-Jev checks of v9: MB-labeled real pairs, track merge P/R 0.79/0.91
(v7 0.75/0.80); LLM-adjudicated set: v9 tracks merge P/R 0.69/0.92 — the
extra "false merges" there are mostly ontology disagreements (full MV vs
audio, which the old LLM labels call different/related) plus one real error
(two named-version instrumentals).

Relation head rel-v2 (trained with Jev kinds/directions): kind agreement with
Jev on silver related tracks 0.73 (weighted 0.77) vs rel-v1 0.65 (0.51);
direction 0.90 on dev (0.96 when confident). Edit/arrangement stay weak.

### Recommended runtime shape (not built; Rust is on hold)

1. v9 scores every candidate pair (MERGE / RELATE / DEFER / DISTINCT).
2. DEFER (~4.1k pairs pool-wide; ~$0.35 once, then only new pairs) goes to
   Jev with the v2.1 prompts. Apply Jev's answer when confidence ≥ 0.95
   (≥ 0.8 for artists); otherwise the review queue.
3. Optionally, Jev double-checks a sample of model MERGEs; a confident
   disagreement sends the pair to review.
4. Periodically re-distill: new Jev labels → retrain → the model absorbs
   them and the defer band shrinks.

### Open

- **Siblings — decided by the user (labeling-page note, 2026-10-03):** two
  covers of the same song "should not be directly related but rather related
  to a common original node". So `sibling` must not create a direct edge;
  each version gets `derived_from` the original (dev-2 `member_of` a work
  when no original entry exists). Next step: make `sibling` a 4th class
  (MB `mb_work_siblings` + Jev `sibling` are already kept apart in the raw
  labels; `generate.py` currently folds them into `related`), and score
  sibling pairs as DISTINCT at the pair level.
- First human labels (18 decided of 100, 2026-10-03): agreement with the
  human label Rhai 0.28, v9 0.61. v9's misses are same↔related confusions
  (ウェカピポ, Irreplaceable: same; Say So: related).
- Artist recall (0.81 vs Rhai 0.94): more Jev artist labels, or keep Rhai's
  exact-name + credit-overlap rule as an extra feature.
- Release merges: recall still low (0.06 on silver); most true release
  duplicates are already linked by hard IDs.

## Session 3 (2026-10-03 03:00–04:05): siblings, artist threshold, honest silver

**Why the ~0.7 numbers:** three causes, measured:
1. **Judge noise.** The track "same" pairs v9 missed have median Jev confidence
   0.51; most heavily weighted "related" misses are Jev errors on different
   songs with similar names (`アイドル十ヶ条` vs `アイドル`, `Beat Eater` vs
   `Beat It`). `evaluate.py --min-label-conf 0.8` now scores silver on
   confident judge labels only (898 of 1,468 pairs).
2. **Siblings.** Under the user's rule (no direct edge between two versions of
   one song), v9's relate precision is really 0.64–0.77: it related siblings.
3. **Artist threshold** was tuned on noisy MB negatives (0.971), deferring
   clear cases (`KIKUO - Topic`/`Kikuo`, `X (CV. …)`/`X`).

**Changes (v10, rel-v3):**
- 4th class `sibling` (MB `mb_work_siblings` + Jev `sibling`); policy verdict
  `SIBLING` = no direct edge. Shared policy in `policy.py`.
- Structure head (`structure.txt` in rel-v3): left derived / right derived /
  sibling, swap-augmented on signed features; turns a RELATE into SIBLING
  when P(sibling) > 0.5. Dev sibling P/R 0.92/0.78; on silver it helps less
  (siblings there rarely carry cover markers: telling the original from two
  covers of a public-domain song needs world knowledge).
- Artist name normalisation (`artist_core`: `- Topic`, `(CV. …)`, Discogs
  `(n)`, `(All)`, `… Ch. group`), `generic_name` + `placeholder` guard:
  never auto-merge "Release - Topic" / "Private video" pairs (DEFER).
- Thresholds: tracks on all dev tiers at 98%; artists on the Jev dev tier
  only at 99% (→ 0.835); releases on probe/jev tiers; silver pairs excluded.

**Silver (Jev labels ≥ 0.8 confidence, 898 pairs, population-weighted):**

| | Rhai | v9 | **v10** |
|---|---|---|---|
| track merge P / R | 0.66 / 0.84 | 0.99 / 0.92 | **0.98 / 0.90** |
| track relate precision (siblings count as wrong) | 0.98 (R 0.21) | 0.77 | **0.88** |
| track sibling recall | 0 | 0 | **0.59** |
| track direct-link precision | 0.85 | 0.85 | **0.93** |
| artist merge P / R | 0.97 / 0.97 | 1.00 / 0.89 | **0.996 / 1.00** |

All silver labels: v10 artist P/R 0.97/0.99 (Rhai 0.90/0.94); track merge
0.90/0.73. Weighted track relate recall (0.62–0.65, CI ±0.25) is dominated by
a few low-similarity pairs whose weights are ~6k each.

**MB-labeled audit (no Jev labels), v10 vs Jev v2.1:**

| | Jev v2.1 | v10 |
|---|---|---|
| full MV = same | 0.97 | 0.88 |
| cover vs original (derived) | **0.60** | 0.84 |
| sibling versions | **0.52** | 0.67 |
| model RELATE (any label) | 0.75 | 0.88 |
| model DEFER band | 0.85 | — |
| release split-half same | 0.93 | defers 93% |

Jev is strong on **identity** and weak on **structure** (derived vs
sibling): it often calls original-vs-cover "sibling". So: delegate the defer
band's identity question to Jev; never let Jev overrule the model's
derived/sibling call.

Pool-wide v10: 9,983 MERGE (track 8.8k, artist 1.1k), 31.6k RELATE, 4.0k
SIBLING (no edge), 4.8k DEFER (track 3.9k, release 0.7k, artist 0.2k) → ~$0.33
of Jev to judge once.

### Session 3, continued (04:05–04:22): v11–v13, convergence

- **v11:** title-level tracklist features for releases (`tracklist_title_jacc`,
  `_cover`, `_len_ratio`, `_extra`: compares child track titles, so it works
  when the two releases' tracks were never merged). Release dev AP 0.93 →
  0.95; first release merges on silver (recall 0.10–0.16 at precision 1.0).
- **MV round:** 1.3k Jev labels on pool pairs with exactly one video side
  ($0.10). Jev agreed with v10's MV merges 87%, relates 83% (+9% sibling),
  siblings 86%; half of v10's MV defers were same. **v12** = + these; rel-v4.
- **Hyper-parameters** (picked on dev log-loss only): smaller trees generalise
  better (31 leaves 0.0931 vs 63 leaves 0.0961; 255 leaves 0.1056). **v13** =
  v12 data, 31 leaves.
- v11 / v12 / v13 are within noise of each other on silver, the MB audit and
  the adjudicated set: more Jev rounds and tuning have stopped paying. Next
  gains need human labels or new signal (world knowledge for
  original-vs-cover, MB-wide MV data, collective artist evidence).
- **Calibration** (v13, confident silver): ECE 0.019 tracks, 0.034 artists;
  scores are bimodal (500/566 track pairs < 0.1 or > 0.97, those bins 100%
  right); mild over-confidence only in 0.3–0.7, the defer band.

**Current recommendation: v13 + rel-v4** (structure head), thresholds in
`models/v13/thresholds.json`. Confident silver: track merge P/R 0.99/0.905,
relate P 0.88, sibling R 0.60, direct-link P 0.93; artist P/R 1.00/0.96;
adjudicated (LLM labels): accuracy 0.93, direct-link precision 0.98.
Total Jev spend for the whole project: ≈ $1.45.

### Session 3, end (04:22–04:45): originality from MB years → v15

v13's remaining confident-silver track errors were mostly **siblings called
related** (22: `Sleigh Ride` × `Sleigh Ride` by two performers, Christmas
standards, `Country Roads`): deciding derived vs sibling needs to know which
recording is the original.

- `mb_years.sql` / `mb_rec_artists.sql` (≈20 s on the MB docker): first
  release year of each library recording, first year any recording of its
  works came out, and recording → artist MBIDs.
- Features: `orig_gap_min` / `orig_gap_max` / `orig_known` (years between a
  side's first release and its song's first recording; 0 = the original) and
  signed `s_orig_gap` for the structure head. 6.9k track entries have it.
- **Labels by the user's definition of the original:** for MB pairs sharing
  one work, by *different performers (MB artist ids)* or with a performance
  attribute: both gaps ≥ 2 years → `sibling`; one ≤ 1 and the other ≥ 2 →
  `derived` (the later side). Same performer without attributes stays
  unlabeled (MB often holds the same recording twice). First attempt without
  the performer check, and then with credit-name strings, turned real
  duplicates into "siblings" (merge recall 0.905 → 0.75 / 0.86).

**v15 + rel-v6** (31 leaves), confident silver (Jev ≥ 0.8), vs v13 + rel-v4:

| tracks | v13 | **v15** |
|---|---|---|
| merge P / R | 0.990 / 0.905 | 0.972 / 0.857 |
| relate precision | 0.882 | **0.932** |
| sibling P / R | 0.872 / 0.603 | 0.708 / **0.888** |
| direct-link precision | 0.933 | **0.981** |
| artists merge P / R | 1.000 / 0.961 | 0.990 / **0.986** |

v15 loses 5 of v13's 102 confident merges (2 to DEFER, which Jev would
judge; some others, e.g. `Annie's Song` × 2, may be two performers' versions
where Jev's "same" is the error). **Current recommendation: v15 + rel-v6.**
Shipping caveat: the originality features need MB work + first-release-year
data at runtime (not in the local URL-only mirror today).

Pool-wide v15: 9,258 MERGE, 27,297 RELATE, 9,968 SIBLING (no direct edge),
5,324 DEFER. Of Rhai's 15,093 track merges: 5,589 merge, 3,566 relate, 4,286
sibling, 1,642 defer.

### Originality features ablation (2026-10-03) → v15-noorig

Same labels as v15, `orig_*` features dropped (`train.py --drop orig`,
`train_relation.py --drop orig`). Confident silver tracks: merge P/R
0.990 / 0.875 (v15 0.972 / 0.857), relate P 0.940 (0.932), sibling P/R
0.728 / 0.888 (0.708 / 0.888), direct-link P 0.981 (0.981); artists
1.000 / 0.961 (0.990 / 0.986). Human (n = 18): 7 vs 9. The originality gain
was the **label rule**, not the features. **Runtime recommendation:
v15-noorig + rel-noorig** (no MB year data needed at runtime).

Bug found while exporting parity fixtures: `pool_predict.py` cached views by
entry id with the *pair's* type, but 20% of the Rhai candidate pool
(136,486 pairs) is cross-type (e.g. release × track; the Rhai script returns
DISTINCT for them). Entries seen first in a cross-type pair got a wrong-type
view for all their pairs. Fixed: views use the entry's own type, cross-type
pairs are DISTINCT unscored. Pool counts for v15-noorig after the fix:
9,143 MERGE / 27,002 RELATE / 10,151 SIBLING / 5,039 DEFER. Earlier pool-wide
counts in this doc carry the bug (small effect). Gold features
(`build_features.py`) key views by (type, pairs) and were not affected, but
43 gold items (41 silver, 2 human) are cross-type pairs scored as same-type.

### Encoder pad-token bug → v16; reproducible cosines → v17 (2026-10-03)

Found while porting to Rust (docs/plan-v15-runtime.md):

- **Pad tokens were attended.** `tokenizer.json` pads each batch to its
  longest text, and `features.Encoder.embed` set the attention mask to 1 over
  the padded length. Pad tokens were attended and mean-pooled, so a text's
  vector depended on its batch neighbours: median cosine 0.68 against the same
  text embedded alone (proper mask: 1.0). Every encoder feature in v1–v15 was
  computed from these vectors, and no runtime could reproduce them. Fixed (mask
  from the encoding), re-embedded (36k texts, 2 min), rebuilt features,
  retrained → **v16 + rel-v7**. Confident silver tracks vs v15-noorig: merge R
  0.875 → 0.935, sibling P 0.728 → 0.799, relate P 0.940 → 0.948; artists
  merge R 0.961 → 0.926 (dev-chosen threshold rose to 0.982). Human 8/18, no
  false merge.
- **float32 cosines are not reproducible.** The trees split at ulp level near
  1.0 (identical texts), and numpy's float32 BLAS sums in a kernel-specific
  order: the Rust port flipped 20 of 3.5k parity verdicts with every
  non-vector feature equal. `features.cos` now normalises and multiplies in
  float64 and rounds to float32 (identical texts give exactly 1.0 anywhere);
  means are float64 → **v17 + rel-v8**. Silver within noise of v16 (track merge
  P/R 0.986 / 0.919, release-group recall 0.694 → 0.463 on 12 pairs). Human
  8/18 with **one false merge**: `Say So` × `Say So` (MB original vs remix,
  identical titles, no durations), p_same 0.844 against the dev-chosen track
  threshold 0.828. A threshold-band case; the threshold is script policy.
- v17 pool: 9,680 MERGE / 27,347 RELATE / 10,489 SIBLING / 4,800 DEFER
  (cross-type pairs DISTINCT unscored).

**Current runtime model: v17 + rel-v8** (`bundle.py` → data/learned-matcher/bundle/v17).
