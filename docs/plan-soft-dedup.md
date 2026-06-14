# Soft Deduplication — Heuristic Matcher Plan

Status: **design settled, not yet implemented.** Depends conceptually on, but is
orderable independently from, `docs/plan-barrier-merge.md`.

## Goal

Close the gap that URL cross-linking leaves: entities that are the same but share no
identifier, so the hard-dedup layer can't unify them. From the live-DB analysis
(2026-06-14): URL linking only reaches entities through the **MusicBrainz hub** —
entries with an MB pair average 3.6 sources; entries without one average 1.02 and are
98% singletons. ~187 redundant track entries, plus release/artist duplicates, sit as
unlinked islands (discogs tracklist rows, standalone Spotify/YouTube uploads).

Soft dedup adds a **content-based** layer that merges or relates these islands, and is
**reversible** — versions of a thing get a *relationship*, not a collapse.

## Pipeline placement & precedence

```
import  → URL cross-linking (hard merges via shared IDs)      ← high precision
dedup   → barriers (manual positive merges + negative splits) ← human authority
match   → heuristic soft dedup (this plan)                    ← fuzzy, lowest precedence
```

Precedence: **manual > URL > heuristic**. URL merges are ~99% accurate; heuristics are
probabilistic. Corrected principles (do **not** re-derive these wrongly):

- URL linking emits **only positive** ("merge") claims; it never asserts distinctness.
  Two clusters being separate just means *no shared ID was found* — never "judged
  distinct".
- Distinct authoritative IDs (two ISRCs / two MBIDs) do **not** imply distinct
  entities; upstream DBs contain duplicates. So a hard ID is **corroboration**, never
  a **veto**.
- The only hard veto against a heuristic merge is a **manual barrier** (a real
  negative edge).
- Merge-vs-relate is driven by **content** (duration/title/artist/discography), not by
  ID presence. Distinct same-namespace authoritative IDs are at most a *soft
  confidence penalty* (lean toward relate, raise the merge bar).

## Output: three-way, non-destructive

- **MERGE** — same entity → goes through the existing `merge_entries` path, vetoable
  only by a manual barrier (reuses the union-find in `flush`).
- **RELATE(kind)** — same work, different version → a new edge, no merge.
- **DISTINCT** — leave alone.

New table (entity-level, parallels `entry_child`):

```
entry_relation(entry_a, entry_b, kind, confidence, origin, enabled, extra)
  kind:    same_recording | alt_version | live | remix | instrumental |
           cover | release_variant | in_release_group | same_artist
  origin:  'heuristic' (later: 'manual')
  enabled: bool   -- tombstone: disabling reverses one decision, keeps history
```

`same_*` rows above a merge threshold are promoted to merges by the apply pass;
everything else stays as a reversible edge. This **is** the persistent positive-edge
store the reversibility discussion concluded soft dedup needs — barriers are the
negative half, this is the positive half.

## Stage shape

A new `match` binary + `src/pipeline/match.rs` module, sibling to `dedup`:

- **dry-run by default** → emits a ranked report (cluster, entities, decision,
  confidence, reason); `--apply` commits.
- Idempotent; never ingests new URLs.
- Config in `<config_dir>/match/{track,release,release_group,artist}.rhai` plus a
  thresholds YAML.

## Candidate generation (blocking)

Avoid O(n²); per type, union candidate pairs from cheap blocking keys, score only
within blocks:

- **tracks**: normalized-title first token; duration bucket `round(dur/5s)`; shared
  credited-artist entry. (later: embedding LSH bucket for cross-script titles)
- **releases / release_groups**: title token-trigram; shared artist; release year.
- **artists**: normalized-name token; shared-collaborator from the contribution graph
  (this is what bridges 桐生ココ-spotify ↔ 桐生ココ-MB even when the scripts differ).

## Feature library (Rust primitives)

Deterministic host functions exposed to the scripts (the script never recomputes
these; it only decides):

| primitive | use |
|---|---|
| `title_exact / title_jaccard / title_edit` | after NFKC + case/width fold + punct strip |
| `alias_best(a,b)` | max similarity over the alias cross-product (locales!) |
| `dur_delta_ms(a,b)` | tracks — the primary merge-vs-relate gate |
| `date_delta_days(a,b)` | releases / RGs |
| `artist_overlap(a,b)` | Jaccard over credited-artist entries |
| `tracklist_overlap(a,b)` | releases — Jaccard over child (title,dur) signatures |
| `discography_overlap(a,b)` | artists — shared releases/tracks they're credited on |
| `version_tokens(title)` | extract `live / remix / inst / acoustic / (XXXX ver.) / メドレー` |
| `shared_authoritative_ids(a,b)` / `distinct_authoritative_ids(a,b)` | corroboration / soft penalty (NOT a veto) |
| `title_embed_sim(a,b)` | **phase 3 only** — pre-trained multilingual cosine |

## Scripting layer (Rhai)

Policy lives in **Rhai** (pure-Rust, no C dep, matches the existing config-matcher
style). One script per entity type; each receives two entity structs + a feature
struct and returns `merge(conf, reason)` / `relate(kind, conf, reason)` / `distinct()`.
The script can do nothing but classify, so it's safe to iterate weekly against the
dry-run report. Expensive ops stay in Rust.

```rhai
// track.rhai (illustrative)
fn decide(a, b, f) {
    let t = max(f.title_exact ? 1.0 : 0.0, f.title_jaccard);   // + title_embed_sim in phase 3
    if t < 0.82 { return distinct(); }
    let marked = version_tokens(a.title) != version_tokens(b.title);
    if f.dur_known && f.dur_delta_ms <= 2000 && !marked {
        let conf = t - (f.distinct_authoritative_ids > 0 ? 0.1 : 0.0); // soft penalty, not veto
        return merge(conf, "title + duration ±2s");
    }
    if f.artist_overlap >= 0.5 { return relate(classify_version(a, b), t, "title match, duration differs"); }
    distinct()
}
```

## Per-type heuristic strategies

### Tracks  (primary signal: duration)
- title match **+** `dur ≤ ±2s` **+** no version marker → **MERGE** `same_recording`.
- title match **+** dur differs OR a version marker → **RELATE** as the classified
  kind (`live`/`remix`/`instrumental`/`alt_version`).
- no-duration discogs clusters (61 of them) → fall back to **release-group context**:
  if both are track *N* of releases already related as `release_variant`, treat as
  `same_recording`. (The 飛んでk case: 9 discogs `?track=1` rows under one variant set.)
- `distinct_authoritative_ids` (two ISRCs) → lower confidence / prefer relate; never a
  hard block.

### Releases  (different IDs = different products → prefer relate)
- title match **+** date within ~30d **+** `tracklist_overlap ≥ 0.8` → **RELATE**
  `release_variant`, and attach both to a shared release_group (create it if absent).
  This is the "hololive summer 2022 ×9" case; relating the variants then collapses the
  per-track duplication via the track release-group fallback above.
- **MERGE** only on title + an exact, ID-free identical tracklist (rare).

### Release groups  (coarsest → merge liberally)
- title match **+** artist overlap **+** same primary_type → **MERGE**.
- RGs synthesized by the release step are deduped here.

### Artists  (name alone is weak → require a second signal)
- `alias_best ≥ 0.9` **and** (`discography_overlap > 0` OR shared collaborator)
  → **MERGE** `same_artist`. Catches 桐生ココ 40↔485, airani iofifteen 298↔382:
  the island shares credited tracks with the MB-anchored entry.
- name match with **zero** discography overlap → **RELATE** `same_artist`
  (low confidence) for human review; never auto-merge.

## Reversibility & safety

- Heuristic edges are persisted in `entry_relation` and **replayed** each run, so a
  false positive is undone by flipping one `enabled` bit (far better than barrier-only
  wholesale reversal).
- Merges funnel through the same union-find the barriers constrain → a manual barrier
  always vetoes a heuristic merge.
- Optional **mutual-best-match** within a block before merging, to stop a chain of
  mediocre links snowballing one giant entry.
- Confidence bands: `> merge_threshold` → merge; `relate_threshold..merge_threshold` →
  write relation for review; below → nothing.

## Rollout

1. **Feature library + `entry_relation` migration + report-only**, one hardcoded rule;
   validate feature numbers against the known clusters (飛んでk tracks, hololive-summer
   ×9 releases, 桐生ココ artists).
2. **Add Rhai** + the four type scripts; tune thresholds against the dry-run report.
3. **Add pre-trained embeddings** (candle + LaBSE / multilingual-MiniLM, inference
   only, content-hash cached) as one extra `title_embed_sim` feature — widens recall on
   cross-script titles; never a classifier.
4. **Enable `--apply`** with merges behind union-find + barriers; relations on from the
   start (non-destructive).

## Open questions

- Exact merge/relate thresholds per type (tune against the report in phase 2).
- Whether `entry_relation` should also absorb the persisted URL `is_rel` edges so the
  *entire* grouping becomes recomputable offline (the full signed-edge upgrade) — a
  larger follow-up, not required for v1.
- Version-kind taxonomy (`alt_version` vs `live`/`remix`/…) — start coarse, refine.
