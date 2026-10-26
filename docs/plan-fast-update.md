# Plan: faster library updates

Status: Phases 1 and 2 done, 2026-10-26 (results under each). Follows
`plan-update-roots.md` and the `entry.changed_at` soft-dedup change
(f18e5e6).

## Where the time goes

An update that finds nothing new took 2 min 40 s in the player (debug build),
down from 20–38 min before soft-dedup checked only changed entries.

The vtuber preset (52 of the 56 roots) timed with the `import` CLI on a copy
of the library, debug build, listings served from snapshots, soft-dedup off:

| Step | Time | Work |
|---|---|---|
| Walk | 9 s | 26,280 fetches, all from the HTTP cache |
| Canonicalize before saving | 3 s | |
| Save + commit | ~59 s | 25,860 pairs, 56,221 is_rel, 78,381 has_rel |

The player takes about twice as long, because it runs the whole ingest on its
single-threaded runtime (`server/main.rs`: `current_thread` + `LocalSet`).
The walk gets one core instead of all of them, and the UI waits behind it.

The save rewrites everything it was given, changed or not, one statement at
a time, each in its own implicit transaction:

- `find_entry_id_by_pair` once per pair (step 2, and for barrier pairs).
- `upsert_pair` rewrites every column of every fetched pair, `fetched_at`
  included.
- `insert_aliases_for_pair`: one SELECT per pair, then the missing ones.
- `insert_child_edge`: one `INSERT … DO NOTHING` per edge (78k).
- `insert_contribution`: one `INSERT … WHERE NOT EXISTS` per credit.
- `commit_import_run`: per re-fetched parent, one SELECT of its children and
  one of its credits; per fetched pair, one SELECT of its aliases.

That is a few hundred thousand statements to change nothing.

## Phase 1: ingest off the player's main thread

Run update and ingest jobs on a multi-thread runtime, the way
`match_changed_entries_offloaded` already gives soft-dedup its own thread.
The CLI runs `ingest_entries` on a multi-thread runtime, so the future is
`Send`; the job's progress sink is `Arc` + `Mutex`.

Expected: the walk drops to roughly the CLI's 9 s, and the player stays
responsive during an update. Small change, low risk.

Done (`server/offload.rs`). An update with nothing new: 2:40 → 2:01. The
vtuber walk took 35 s, not 9 s: a real update re-reads listings over the
network, which the CLI timing (snapshots served) didn't.

## Phase 2: batch the save, skip unchanged writes

Same results, far fewer statements:

1. Look up entry ids for all pairs with chunked `IN` queries instead of one
   query per pair.
2. Load the stored rows of the fetched pairs in bulk. Upsert only pairs whose
   entry or metadata differ. Bump `fetched_at` for the rest with one chunked
   `UPDATE` (it means "last fetched": `import --backfill-links` resumes from
   it, so it has to keep moving).
3. Aliases, child edges, credits: load what's stored for the pairs and
   parents in bulk, insert only what's missing, as multi-row inserts.
4. `commit_import_run`: the same bulk loads for its stale-row checks.
5. Group writes into transactions of a bounded size, so the write lock is
   never held long (the jobs writer and soft-dedup also write).

Expected: save + commit from ~60 s to a few seconds. Risk: `flush.rs` is the
most delicate part of the importer (merges, splits, barriers, periodic
flushes). Guard: the existing flush tests, plus a test that the old and new
paths leave identical DBs for the same input.

Done (`musicdb/batch.rs`): bulk loads, then only the writes that change a
row, through the same single-row writers. Steps 1–3 and 5 as planned; 4
(transactions) wasn't needed, since an unchanged save writes almost nothing.
The vtuber preset on a copy of the library: save + commit 50 s → 18 s, and
every table identical to the old save's result (timestamps aside). What's
left is spread over the steps, much of it debug-build CPU (canonicalizing
26k pairs is 2.8 s). A row-value `IN (VALUES …)` for the bulk lookups saved
only ~1 s, so `pair_condition` stays as it was.

## Phase 3, maybe: don't re-walk what can't have changed

With Phases 1–2, an update with nothing new should take roughly 15–20 s, most
of it the walk. Skipping unchanged parts of the walk could cut that further,
but the walk is also how an update notices changes that don't come from a
listing:

- The MusicBrainz mirror: replication changes URL lookups with no HTTP
  request involved.
- Fetches that failed last time (dead links, 5xx) are retried.
- Edited fetch options, new code that reads cached responses differently, new
  roots.

A listing-driven update (walk only the new items of refreshed listings, plus
pairs whose mirror URLs changed, plus a periodic full walk) handles these, but
it's a different importer mode with its own failure modes. Decide after
measuring Phases 1–2.

## Order

Phase 1, measure, Phase 2, measure, then decide on Phase 3.
