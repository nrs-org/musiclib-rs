# Plan: move Jev out of musiclib, into the script + inference

Status: implemented (2026-10-04). Decisions on the open questions: Jev is on
whenever `TYPESAFE_API_KEY` is set (logged at init and per refine chunk);
`match.example.rhai` gets no `refine`; `jev_verdict_cache` is dropped (the
live DB had 0 rows, so there was nothing to migrate). Differences from the
plan below: the client is `inference_typesafe_*` (not `_jev_`), and the view
dedupes track positions by what Jev sees and treats YouTube video ids as
opaque handles. Follow-up: the copied v1 prompts were replaced by prompt v2.1
(the learned matcher's ontology: derived/sibling for tracks, identity-only for
other types), kept in `config/jev/<type>.json` and sent in their written key
order; Jev's `derived` becomes a `derived_from` RELATE.

## Why

Soft-dedup puts decisions in the Rhai script, and Rust provides the building
blocks it calls. Jev (`src/pipeline/jev.rs`) doesn't fit that. Its prompts, the
evidence view, the routing rule (`jev_entry_types`: every non-DISTINCT script
verdict goes to Jev, and Jev's answer replaces it) and the choice→verdict
mapping are all fixed in Rust. Two problems follow:

- Jev can't be pointed at the cases that actually need it (DEFER). Today it
  re-decides confident learned MERGEs too, and it's only reachable through
  `softmatch --jev-types`. Online soft-dedup (import, server) never uses it.
- Changing a prompt means rebuilding musiclib.

## Who owns what

| Layer | Owns |
|---|---|
| **script** (`match.learned.rhai`, via `config/jev.rhai`) | Which pairs go to Jev, the evidence view, the questions/prompts, mapping Jev's answer to a verdict |
| **inference** (new feature `typesafe`) | The TypeSafe HTTP call, auth, retry/backoff, running requests concurrently, the response cache |
| **musiclib** | A generic `refine` hook, plus pair facts. Knows nothing about Jev |

The inference side stays generic: it knows the TypeSafe `systemone` request and
response format, but nothing about music.

## 1. musiclib: a generic `refine` hook

`decide` runs on every core and blocks each thread while it runs, so a network
call inside it would stall the scoring pool. Instead, add an optional second
pass:

```rhai
// items: [#{a: Entry, b: Entry, verdict: <decide's map>}, …]
// returns: one verdict map per item (return item.verdict to keep it)
fn refine(ctx, items) { … }
```

- `score_candidates` calls it after `script_verdicts`. Pairs come in chunks
  (e.g. 256) and each chunk is one `run_blocking` call on the scoring task.
  DISTINCT verdicts aren't passed. A missing hook means no change. A chunk
  that errors keeps its original verdicts and logs a warning, like
  `prepare` does.
- The script returns verdict maps built with the usual constructors. They can
  carry two optional fields, `origin` and `model_version`, which go into the
  `dedup_feedback` / `entry_relation` rows that are already written today.
  Default is `"heuristic"` / none. This replaces the hard-coded
  `"jev"` / `jev::MODEL_VERSION` in `apply_candidate`.
- **Remove:** `src/pipeline/jev.rs`, `SoftMatchConfig::{jev_entry_types,
  jev_concurrency}`, `softmatch --jev-types`, the Jev branch in
  `apply_candidate`, and the `jev_verdict_cache` table and its
  `get`/`upsert` functions (drop it in a migration; its rows key on the old
  evidence view and couldn't be reused anyway).

### Pair-facts additions (additive, still `musiclib-pair-facts/1`)

The evidence view needs titles of entries other than `a`/`b`. Today it gets
them from `entry_infos_by_ids` plus a custom DB walk for release groups.
Most of this is already in pair facts:

| View field | Source |
|---|---|
| titles, durations, dates, types, sources | `Entry` fields |
| handles | `Entry.pairs` (moves to script regex) |
| credited_names (track) | `contributions[].artist_name` ✓ |
| release_tracks | `children[].name` ✓ |
| release_group_ids | `parents[]` where `entry_type == "release_group"` ✓ |
| track_positions.release_title | `parents[]`: **add `name`** |
| credited_names (artist) | `credited` is ids only: **add `credited_names`** (capped) |

`PairFacts` in inference doesn't deny unknown fields, so the matcher isn't
affected.

## 2. inference: feature `typesafe`

```text
h  = inference_jev_open(config_json)   // {api_key, cache_path, concurrency, base_url?, timeout_s?}
                                       // NULL on error → inference_jev_last_error()
s  = inference_jev_ask_batch(h, requests_json)
                                       // in:  [ {state, model, questions}, … ]
                                       // out: [ {"ok": <response>} | {"error": "…"}, … ]
     inference_free_string(s); inference_jev_close(h)
```

- **Blocking, concurrent inside.** A batch fans out on up to `concurrency`
  threads (`ureq`, already a host dependency). One call per refine chunk, so
  Rhai never waits on a single request. A batch is fine to call from one
  thread at a time; the handle is `Sync`.
- **Retry:** on 429/5xx, honour `Retry-After`, then back off exponentially
  with a capped number of attempts. Same idea as `DomainScheduler`, but
  standalone, because inference doesn't link musiclib's HTTP stack.
- **Cache:** a SQLite file at `cache_path`, keyed by `sha256` of the
  canonical request body (sorted-key JSON) → response JSON. Because the key
  is the content, any prompt or view change invalidates it automatically.
  That replaces the `MODEL_VERSION` bump rule. Errors are not cached.
- **Dependencies** behind the feature: `ureq`, `rusqlite` (bundled), `sha2`,
  `serde_json`.
- **Tests:** `base_url` points at a local mock HTTP server. Cover cache hit
  and miss, errors mixed with successes in one batch, and a 429 retry.

## 3. Script: `config/jev.rhai` + routing in `match.learned.rhai`

`config/jev.rhai` is a module (the engine already resolves `import`
relative to the script):

- `open()`: `ffi`-binds the functions above, reads `TYPESAFE_API_KEY`, and
  uses `<cache_dir>/typesafe.db`. Returns `()` when no key or library is
  available, so refine becomes a no-op.
- `view(entry)`: a port of `entry_view` that reads `Entry` fields plus
  `pair_facts_json(entry.pairs)`. Includes `extract_handle` as a regex port,
  and the same caps (8 titles, 8 credited, 12 tracks, 5 positions, 6 handles).
- `questions(entry_type)`: the four `identity` prompts plus `title_match` /
  `duration_consistent`, copied word for word from `jev.rs`.
- `to_verdict(answer)`: `same_identity` → merge, `related_variant` →
  relate, `different_identity`/`unrelated` → distinct, otherwise defer.
  Each verdict gets `origin = "jev"` and `model_version = "typesafe-jev/2"`.

`match.learned.rhai`:

```rhai
fn refine(ctx, items) {
    if type_of(ctx.jev) != "map" { return items.map(|it| it.verdict); }
    // policy: only the learned model's undecided pairs
    let ask = items.filter(|it| it.verdict.verdict == "defer");
    … build requests, jev::ask_batch, map answers back by index …
}
```

`init()` opens the matcher and Jev independently, so a missing Jev doesn't
turn off the matcher. With no `TYPESAFE_API_KEY`, nothing changes from today.

## Behaviour changes

- Jev goes from re-checking every MERGE/RELATE/DEFER for the chosen types to
  **DEFER only**, decided by the script.
- Jev runs wherever `match.rhai` runs, including online import and the server,
  as long as `TYPESAFE_API_KEY` is set (see open question 1).
- Requests now bypass musiclib's `DomainScheduler` and HTTP cache. Jev had
  `no_cache: true` anyway, so only the shared rate limiting is lost, and
  `concurrency` covers that.

## Order of work

1. Pair-facts additions, with a test.
2. inference `typesafe` feature + ABI + tests.
3. `refine` hook + `origin`/`model_version` on verdict maps, with a
   host test using a tiny inline script.
4. `config/jev.rhai` + routing in `match.learned.rhai`. Check parity: on a
   handful of pairs, the old `jev.rs` and new script views should produce the
   same request JSON. Do this before step 5.
5. Remove `jev.rs` and its config/CLI/table. Update
   `docs/CONFIG_REFERENCE.md` and the `match.example.rhai` header.
