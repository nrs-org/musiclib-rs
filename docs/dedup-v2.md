# Deduplication v2: evidence retrieval and calibration

Status: **active foundation work**. This supersedes the architecture and rollout
assumptions in `plan-soft-dedup.md` and `plan-semantic-blocking*.md`. Those files
remain useful as implementation history and benchmark notes.

## Objective

Deduplication is split into three independently measurable problems:

1. **Candidate retrieval:** find a bounded set of plausible cluster edges.
2. **Identity and graph judgment:** decide identity, then assert only concrete
   primitive relationships between distinct entities.
3. **Library policy:** map identity plus graph assertions to merge, relate, keep separate, or
   defer.

The system does not need to enumerate every positive pair. For a true component
of `s` source fragments, it only needs enough correct edges to connect the
component (as few as `s - 1`) while avoiding an erroneous bridge between distinct
components.

```text
immutable provider fragments
        |
        v
provenance-rich names and structural evidence
        |
        v
bounded exact | lexical | phonetic | MB | graph | semantic retrieval
        |
        v
candidate-edge union and cheap ranking
        |
        v
relationship judgment and cluster consistency checks
        |
        v
reversible MERGE | RELATE | KEEP_SEPARATE | DEFER
```

## Identity and relationship ontology v2

Labels describe facts separately from product policy.

### Factual entity relation

- `same_identity`: both views represent the same identity at the granularity
  defined below.
- `different_identity`: affirmative evidence supports distinct identities.
- `insufficient_evidence`: the packet does not justify either conclusion.
- Input-integrity failures are recorded separately as `left_mixed`,
  `right_mixed`, `wrong_type`, or `packet_insufficient`.

Absence of corroboration is not evidence for `different_identity`.

Relatedness is not an identity label. A pair is `different_identity` with zero
or more concrete primitive graph assertions.

### Type-specific identity granularity

| Type | `same_identity` | Examples of `different_identity` |
|---|---|---|
| Track | Same captured performance/recording across providers; encoding and harmless leading/trailing silence may differ | Edit, separately retained remaster, live take, remix, instrumental, cover, or another realization of one work |
| Release | Same issued edition/listing across providers | Format/territory edition, reissue, deluxe edition, or another release in one release group |
| Release group | Same conceptual release project | A separate related project |
| Artist | Same credited artistic identity | Persona, successor project, member/unit, collaboration identity, or underlying person |

These are calibration defaults, not hidden model assumptions. Ambiguous product
boundaries (notably remasters, video edits, release territories, and personas)
must be tested in the calibration batch and may produce a new ontology version.

### Primitive graph relationships

Only four directed predicates are stored:

- `member_of`: membership in a group, family, or containing entity;
- `derived_from`: transformation or successor lineage;
- `participates_in`: a role-bearing contribution or participation;
- `facet_of`: a separately modeled public identity representing an underlying
  identity.

Domain detail stays in relation metadata. Remix/edit/remaster are
`derived_from` transformations; performances can be `member_of` a work or
recording family; releases are `member_of` a release group; members and units
are `member_of` an artist group; collaborations use `participates_in`; personas
use `facet_of`. Unknown relatedness creates no edge.

Two releases in a release group produce two `member_of` edges to the group, not
a direct variant edge between releases. Pair annotations may compact this as
two subjects sharing one object; persistence expands it into binary edges.

### Name relation

Name evidence is labeled independently:

- `same_string`
- `orthographic_variant`
- `transliteration`
- `translation`
- `abbreviation`
- `generic_collision`
- `unrelated`
- `mixed`
- `unclear`

A transliteration or translation is candidate evidence, not proof of entity
identity. Version markers are entity-relation evidence, not language
normalization.

### Policy action

- `merge`
- `relate`
- `keep_separate`
- `defer`

`same_identity` maps to `merge`; `different_identity` with primitive assertions
maps to `relate`; `different_identity` without assertions maps to
`keep_separate`; and insufficient evidence maps to `defer`. Exceptions require
an explicit reason.

## Data model

The immutable unit is a provider fragment identified by `(source, identifier)`,
not a mutable `entry_id`. A task presents two entity views, each containing one
or more source fragments and their extracted evidence.

Content-addressed `item_id` values bind the schema version, evidence snapshot,
ontology version, policy version, entity type, and both order-independent view
hashes. Changing evidence or label semantics therefore creates a new task
identity instead of silently attaching a new meaning to old annotations.

Retain, where available:

- every alias with source, locale, primary flag, alias type, and sort name or
  supplied reading;
- duration observations, dates, credited artists, release membership, track
  position, and neighboring tracks;
- external identifiers and URLs;
- source fetch time and an evidence-content hash;
- snapshot, ontology, policy, candidate-generator, and preprocessing versions.

Generated representations never overwrite observed data. Each alias may have
separate normalized, parsed-title, phonetic, romanized, and semantic views with
their own method and version.

The annotation ledger is append-only and uses three record types:

- private `task` records, including selection/proposer metadata;
- blind `annotation` records from humans or models;
- `adjudication` records that reference, but never overwrite, individual votes.

The machine-readable contract is `docs/dedup-label-schema-v2.json`.

### Runtime migration boundary

The current Rust database still has a generic pairwise `entry_relation.kind`
string, and the legacy softmatch script emits `variant`. That storage is not yet
the primitive graph. Renaming `variant` to a primitive would be incorrect
because `member_of` needs a real object node such as a release group or work.

Runtime migration should add typed subject-predicate-object assertions with an
open metadata payload, then expand compact annotation assertions (for example,
two release subjects and one release-group object) into binary edges. Existing
`entry_child` and `contribution` rows can supply many initial `member_of` and
`participates_in` edges. Legacy `variant` edges remain provenance-bearing
review candidates until their object nodes and predicates can be established;
they must not be converted blindly.

## Candidate retrieval

Candidate generation is a union of independent bounded retrieval channels.

| Channel | Primary evidence | Bound |
|---|---|---|
| Exact | Canonical provider identifiers, normalized URLs, MBIDs, ISRC, barcode, catalog number, handles | Enumerate safe small buckets; refine and rank overloaded buckets |
| Lexical | All aliases, parsed base titles, word and character n-grams | Top-k full-text results with document-frequency suppression |
| Phonetic | Supplied readings and generated romanization variants | Bounded readings and top-k character-ngram results |
| MusicBrainz bridge | Local names/links to MB entity hypotheses and their aliases | Top-k anchor hypotheses, with ambiguity counts |
| Structural | Candidate artist correspondence, duration, release position, tracklist and neighborhood fingerprints | Hub cutoffs, MinHash/LSH, top-k common-neighbor scores |
| Semantic | Translation and nonliteral alias evidence | True ANN over individual alias documents, not one cluster title |

Each proposed edge records generator, query view, score, rank, index version, and
both endpoint content hashes. Per-channel quotas preserve diversity, and a
per-type total budget bounds scoring at approximately `O(N * B)` for a full pass
or `O(D * B)` for `D` dirty clusters.

A pair classifier cannot solve the quadratic problem because it cannot score a
pair that was never retrieved. Before representative labels exist, classifiers
may prioritize review but must not become the sole pruning gate.

## Romanized and multilingual names

Transliteration and translation use different channels.

For Japanese, prefer observed readings, then MusicBrainz aliases/sort names as
provenance-bearing hints, then a dictionary reading engine. Produce strict and
loose keys covering spacing, punctuation, long vowels, sokuon, syllabic `n`, and
common Hepburn/Kunrei variants such as `shi/si`, `chi/ti`, and `tsu/tu`.

Do not require Latin-script language identification before phonetic retrieval.
Generate readings from the native-script side and query them against all Latin
aliases. Short-title language detection is too ambiguous to be a safe gate;
artist, duration, release, and graph context reject accidental collisions.

Semantic embeddings are one replaceable translation-retrieval channel. They are
stored per selected alias, and the cache key includes encoder, tokenizer,
preprocessing version, and alias content hash. Cosine similarity never directly
authorizes a merge.

## Calibration sampling

The existing softmatch labels are candidate-selected and mainly cover predicted
positives. They can estimate conditional precision but cannot measure candidate
recall.

Entry-dedup evaluation tasks always compare two distinct complete `entry`
records. Source records already grouped inside one entry are evidence for that
entry, not separate dedup candidates. Splitting such a group (`masked_bridge`)
tests source linking and may be retained as a separate diagnostic, but it must
not be mixed into entry-dedup accuracy or recall.

Start with a calibration or pilot batch drawn from several entry-level frames:

- `production_candidate`: stratified current candidates, without exposing the
  matcher verdict;
- `hard_confuser`: distinct current/MB entities sharing strong name or context
  signals;
- `independent_miss`: proposed by an alternative retriever but absent from the
  production candidate set;
- `uniform`: a small same-type non-candidate control sample.

The v3 pilot's independent retrievers use bounded character-trigram lexical
blocks, nearby-duration plus shared-credit blocks for tracks, and tracklist
overlap blocks for releases. Sampling is stratified by entry type, script
relation, retrieval channel, and corpus origin. Expansion-only pairs are capped
through those origin strata so a prolific seed artist cannot dominate.

After the ontology stabilizes, build an approximately 4,000-pair pilot plus deep
counterpart searches for roughly 250 stratified anchor entities. Every item
records its sampling frame, stratum, eligible population, selected population,
and inclusion probability when the sampling design supports one.

MusicBrainz provides silver proposals and evidence, not unquestioned labels:

- same-MB-entity aliases and URL links are positive candidates, not automatic
  local merges;
- distinct MBIDs are not automatic negatives;
- recording/work and release/release-group relations are useful family cases;
- same-credit, same-title, and homonym neighborhoods provide hard confusers;
- snapshot sequence and entity-family keys are fixed before dataset splitting.

Train/dev/test partitions are grouped by latent entity, work, release family,
and artist family. Random pair splitting would leak aliases and near-identical
catalog context across partitions.

## Blind annotation protocol

Generate a physically separate public task projection that omits `hidden`; do
not rely on CSS or UI hiding. Annotators must not see the current verdict,
candidate generator, score, expected relation, or previous rationale.

The intended workflow is:

1. A cheap model extracts structured evidence.
2. Annotator A makes an independent judgment.
3. Annotator B tries to falsify it independently.
4. A human adjudicates disagreements, abstentions, all proposed merges, and
   high-impact cluster changes.
5. Cluster-level consistency checks reopen contradictory neighborhoods.

Repeated samples from one model checkpoint are correlated votes, not independent
experts. Model consensus is silver/training data; frozen evaluation labels are
human-adjudicated and evidence-backed.

## Metrics

Candidate retrieval and judgment are evaluated separately.

Retrieval:

- gold-component connectivity and recall versus candidate budget;
- candidates per entity at p50/p95/p99 and maximum;
- unique positive yield and overlap per channel;
- miss rate in independent-retriever and deep-anchor audits;
- slices by type, source pair, script/name relation, title frequency, and missing
  context;
- overloaded buckets, graph hubs, and ANN recall against an exact sample.

Judgment and clustering:

- merge precision at each automatic-merge coverage;
- false merges per 10,000 anchors;
- related/unrelated distinct-identity confusion, calibration, and deferred fraction;
- pairwise and B-cubed clustering metrics;
- over-merged cluster rate, largest contaminated component, and constraint or
  transitivity violations.

Absolute global recall is not identifiable from candidate-only labels. Report
masked-positive recall, audited-anchor recall, and a design-based sampled
non-candidate miss estimate separately.

## Rollout

1. Land ontology, task schema, and deterministic calibration exporter.
2. Generate and independently label the calibration batch; revise ontology only
   by versioning it.
3. Add candidate provenance and alias-level evidence preservation to the runtime.
4. Build exact, lexical, structural, MusicBrainz-bridge, and Japanese phonetic
   retrieval baselines.
5. Freeze grouped natural/challenge/temporal evaluation panels.
6. Benchmark semantic models as an additional alias-level channel.
7. Train a small calibrated ranker only after retrieval recall is measurable.
8. Enable cluster-safe, reversible automatic merges only at a separately chosen
   high-precision operating point.

## Current calibration checkpoint

### Entry-level v3 checkpoint (2026-09-14)

The current entry-level corpus contains 3,102 supported entries. Its 400-pair
pilot has 55 `same_identity`, 328 `different_identity`, and 17
`insufficient_evidence` effective labels. All tasks compare two distinct,
complete musiclib entries; source fragments within one entry are not treated as
the production deduplication target.

The Python PoC retrieves a bounded union of exact-name, romanized-name,
identifier, duration/credit, release-tracklist, character-ngram, token, and
in-process LaBSE HNSW neighbors. On the full corpus it emits about 65.7k of
1.81M possible same-type pairs (3.62%). Mean candidate degree is about 42,
p95 is 78, and the observed maximum is higher because a popular entry can be
selected by many other entries even though every entry contributes only its
top 50 outgoing candidates.

All 55 labeled positives are retrieved, but this is not an unbiased recall
estimate: the labeled pilot was sampled from related retrieval frames. At a
forced 0.5 scorer cutoff, grouped out-of-fold accuracy is 93.7%, merge precision
is 78.2%, and merge recall is 78.2%. A threshold chosen for 97% observed
precision makes no automatic merges. Therefore the candidate architecture is
ready for iteration, while the scorer and automatic-merge policy are not ready
for deployment.

The next active-learning batch contains 116 previously unseen pairs across six
positive-enriched strata: exact confusers, non-exact romanization,
duration/credit, release-tracklist overlap, semantic-only neighbors, and
non-exact model-boundary cases. It spans 167 entries and is a training/failure
discovery set, not a representative performance panel.

Two independent Luna-max passes agree on 113/116 active-learning items (97.4%).
Blind third votes from workers that had not seen the corresponding item resolve
all three disagreements. The effective result is 21 `same_identity` and 95
`different_identity`, with no abstentions or outstanding evidence requests.
All 20 exact-confuser pairs are `same_identity`; among the 96 non-exact pairs,
only one duration/credit candidate is `same_identity`. The sampled romanized,
semantic-only, release-tracklist, and model-boundary strata contain no positive
non-exact examples. This is evidence that candidate-pool labeling alone has a
very low positive yield and cannot supply a non-exact recall benchmark cheaply.

Before claiming 95–97%, freeze a separate evaluation panel with three reported
components:

1. counterfactual retrieval positives made by partitioning robustly linked
   multi-source entries into two complete pseudo-entries, stratified by entity
   type, script relation, and source pair;
2. a deployment-weighted blind sample from each candidate channel and rank
   bucket, labeled independently and adjudicated, for scorer precision;
3. a fresh temporal/corpus-root holdout that was absent from threshold and
   feature development.

Do not mix the 116 active-learning labels into that final panel, and do not
report one aggregate metric that hides retrieval misses, scorer deferrals, or
merge versus separate precision.

The combined 516-label development set (76 same, 423 different, 17
insufficient) now yields 97.27% decided accuracy under grouped out-of-fold
scoring. The semantic version has 98.04% observed merge precision, 65.79% merge
recall, and 95.39% automatic coverage. These are development diagnostics: both
active-learning selection and threshold choice used this dataset, so they are
not a 98% production claim. Compared with the lexical scorer, semantic features
add only 1.32 percentage points of merge recall while nearly doubling candidate
volume; the semantic channel is more valuable for retrieval than scoring here.

A separate source-linker-grounded retrieval harness replaces each of 1,058
multi-source entries with two disjoint pseudo-entries and leaves the rest of the
corpus as distractors. It contains 191 naturally non-exact positive probes. The
channel-bounded lexical/structural union retrieves 98.30% overall and 90.58% of
non-exact probes. Adding LaBSE HNSW at k=20/0.45 reaches 98.87% overall and
93.72% non-exact recall. A k=50/0.35 setting costs 175,640 candidates and only
reaches 98.96%/94.24%, so it is not the default.

The challenge-probe audit also surfaced apparent contamination inside several
current source clusters (visibly unrelated artist names grouped together).
Consequently, this harness is linker-grounded rather than unquestionable gold;
the non-exact probes, especially misses, need a one-time cluster-integrity audit
before using their recall as a release gate.

### Runtime scaling checkpoint (2026-09-15)

The first Rust retrieval port generates 37,000 bounded lexical/structural pairs
for 3,104 entries, versus roughly 4.8 million unrestricted pairs. A quiet
optimized dry run scores those pairs through the legacy Rhai policy in 9.83
seconds. Adding one k=20 semantic page produces 48,039 total pairs and completes
in 14.75 seconds on the development machine, including 3.31 seconds to build
four type-local HNSW graphs. These timings use the fallback embeddings and say
nothing about model quality, but they isolate the runtime shape.

SQLite-vec remains the durable embedding cache; it is no longer used for one
brute-force KNN query per entry during a full scan. A run-local HNSW graph now
provides approximate retrieval, avoiding a hidden O(N²) semantic stage at a
possible 100k-entry library size. The HNSW search width matches the PoC
(`ef >= max(50, 3k)`). A 100k synthetic benchmark and counterfactual recall
comparison remain release gates; do not infer them from the 3k timing.

The PoC now exports two `musiclib-dedup-logistic/1` artifacts with raw-space
per-type coefficients/intercepts and learned merge/defer/separate thresholds.
`model.json` retains all research features. `runtime-model.json` is retrained on
the 19-feature `musiclib-entry-info/1` profile and can be selected explicitly by
`softmatch --model`; unsupported features and mismatched embedding model IDs
are rejected before scoring. The native path emits merge/separate/defer review
rows and remains dry-run by default. Automatic merges still require both
`--apply` and `--apply-merges`.

The runtime scorer deliberately excludes semantic similarity: on this panel it
slightly improves both coverage and merge recall, and it decouples calibrated
probabilities from the embedding backend. Embeddings remain a candidate-
retrieval channel, where the run-local HNSW index bounds cost.

On the current 1,029-entry database, an optimized read-only native-model run
generates and scores 10,966 lexical/structural candidates in 0.72 seconds total
(0.60 seconds scoring) with embeddings disabled. Writing the full diagnostic
CSV is intentionally slower because it also calls the legacy Rhai diagnostic
helpers for every row; that review path is not representative of batch scoring
throughput.

### Reversible soft identity

Enabled `entry_relation` rows now carry `same_identity` and
`different_identity` assertions. The database projects same-identity edges into
deterministic virtual components while treating different-identity edges as
hard cannot-link barriers; conflicting same edges are reported and skipped.
Either relation can be tombstoned without physically moving `entry_source`
rows. Human corrections append to `dedup_feedback`, including model, feature,
and evidence snapshots, and supersede earlier judgments rather than deleting
them. Learned MERGE/DEFER candidates can be stored in `dedup_suggestion` with
review state preserved across rescoring by the same model version.

This layer is independent of irreversible `merge_entries`. The future player
should render virtual components and use the feedback API; hard merge remains a
separate explicit maintenance action.

The initial local calibration export contains 200 tasks: 50 production
candidates, 60 masked current-cluster bridges, 50 exact-name hard confusers,
and 40 uniform same-type controls. It covers 68 artist, 54 release, 16 release
group, and 62 track pairs, including 32 CJK/Latin and 42 mixed-script pairs.
This composition is deliberately diagnostic and is not a representative test
set.

A closed-book eight-item agent pilot against the v2 contract produced five
`same_identity` and three `insufficient_evidence` judgments and was used to
settle the packet format.

The pilot also replaced verdict-relative `supports`/`contradicts` citation roles
with claim-specific `supports_same_identity`, `supports_different_identity`,
`supports_primitive_relation`, and `context` roles. This lets abstentions record
evidence for competing hypotheses without treating one side as the verdict.

The full 200-item calibration set now has two independent low-reasoning Luna
votes per item. The first pass used the original frame-ordered lane; the second
used complementary left/right presentation and deterministic frame shuffling
to avoid confounding worker identity with sampling frame. Both 200-item ledgers
pass complete-coverage, JSON Schema, item-ID, citation, primitive-reference,
side-order, and policy-consistency validation.

First-pass labels were 77 `same_identity`, 110 `different_identity`, and 13
`insufficient_evidence`. Second-pass labels were 61, 124, and 15 respectively.
They agree on 160/200 items (80%): 57 same, 97 different, and 6 insufficient.
Of the 40 disagreements, 24 are direct same-vs-different conflicts and 16
involve one abstention.

A blind third pass, targeted evidence enrichment, and project-owner review have
now adjudicated those 40 disagreements. The append-only final ledger contains
62 records because later evidence and human decisions supersede earlier
deferrals. Its effective state is 39 resolved, zero `needs_more_evidence`, and
one `excluded` contaminated comparison. The final ledger passes full input,
supersession-chain, invariant, and JSON Schema validation.

This is a useful calibration set, but it is still not a gold performance set:
the 160 Luna agreements have not received independent adjudication, and the
sampling frames are deliberately diagnostic rather than representative.

The pilot exposed and fixed three evidence-packet defects before scale-up:

- non-artist structural children were mislabeled as artist credits;
- `parent_release` overloaded release-group and artist-credit relationships;
- provider statuses such as Discogs `Accepted` were presented as release types.

The release-edition boundary is structural: releases with materially different
tracklists are `different_identity`, while both may assert `member_of` the same
release group. The tracklist difference is evidence, not a relationship type.
