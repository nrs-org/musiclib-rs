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

## Phase 3: don't re-walk what can't have changed

Status: proposed.

### Where an update's time goes now

After Phases 1–2, an update with nothing new takes 1:12. The vtuber preset is
most of it: walk 34 s, save 21 s, commit 5 s. Timing every listing refresh in
that walk (an `import --update` on a copy of the library):

| Listing kind | Refreshes | Last one done |
|---|---|---|
| ytmusic:discography | 65 | 29.5 s |
| discogs:artist_releases | 33 | 9.8 s |
| youtube:playlist_items | 124 | 7.4 s |
| everything else | 324 | ≤ 6.1 s |

So the walk is two things running side by side: listing refreshes over the
network, and re-walking ~26,000 entries from the HTTP cache (9 s on its own,
measured with snapshots served). The save and commit then handle all 25,860
pairs again.

The ytmusicapi server ran a build from before 280307a/94e320b, and 36 of the
65 YouTube Music refreshes failed with a 500. It was restarted on
2026-10-26; all 65 now succeed. They still end last: the server is
single-threaded, so `http.yaml` caps it at 2 requests, at about 0.9 s each.

### 3a. A threaded ytmusicapi server

`ytmusicapi_server.py` uses `HTTPServer`. With `ThreadingHTTPServer` and one
`YTMusic` client per thread, the cap for `localhost:9001` can go to 6–8. The
YouTube Music refreshes would then finish in ~5–10 s instead of 30 s,
alongside the others. Watch for YouTube Music throttling at the higher cap.

Done: ytmusicapi has no async client, so it's threads. 8 discography
requests at once take 1.7 s, against 10.6 s one at a time; the example cap
is now 8.

### 3b. Prune known entries from the walk

An update's walk only needs to reach (1) the roots, (2) every entity that owns
a growing listing, so the listing gets refreshed, and (3) whatever is new. An
entity fetched in an earlier run that owns no growing listing comes back from
the HTTP cache exactly as before, and so does everything under it. Re-walking
it changes nothing, so a pruned walk skips it.

**The rule.** In `importer::import`, after canonicalizing and claiming, a
pair is skipped (not fetched, nothing below it walked) when all of these
hold:

- The run is a pruned walk (see "When to walk in full").
- The pair is stored as fetched and not gone (`entry_source.fetched_at` set,
  `gone_at` null). That's the library's own record, so a run that was rolled
  back or cancelled leaves nothing marked as done.
- The pair owns no growing listing. Owners: artists of every source, and
  playlists (YouTube and Spotify playlists are `Release`s, so this goes by
  the provider's `external_type`, not entry type). A YouTube album playlist
  counts as an owner too; it's only re-read from the cache, which is cheap.
- It isn't a root.

The parent still records its edge and stub for the skipped child, as now, so
a re-fetched playlist keeps its full child list at commit. The skip check
runs against an in-memory set of fetched pairs loaded once per run, not one
query per pair.

**What a pruned walk misses**, and how each is covered:

| Change | Covered by |
|---|---|
| Fetches that failed last time (dead links, 5xx) | Never fetched, so never pruned: retried as now |
| New MusicBrainz links for a known pair (mirror replication) | The next full walk |
| A growing listing below a pruned entity (a collab's channel reached through a known video) | The next full walk |
| Edited fetch options | Full walk when the preset file changes |
| Code that reads cached responses differently | Full walk on demand |

**When to walk in full.** Per root, store the time and preset-file hash of
its last full walk (`update_root`). A root is walked in full when it has
never been, when its preset file changed, or when its last full walk is
older than 7 days (configurable). `POST /api/update {"full": true}` and
`import --update --full` force it. Presets are one run each, so a preset
with any root due for a full walk runs in full.

**Expected effect** with nothing new: the walk drops from 26k fetches to the
roots, the listing owners and the albums under them, roughly 1–3k from the
cache; the save and commit shrink with it. The listing refreshes then set
the pace: ~10 s with 3a.

### Alternatives considered

- Skip items already in a listing's previous snapshot. Snapshots are written
  when a refresh succeeds, not when the run commits, so items found by a run
  that was then cancelled or rolled back would never be imported.
- Refresh some listing kinds less often (e.g. YouTube Music discographies
  weekly). Simpler than 3a, but new releases show up later. Could still be
  added per kind later if refreshes stay slow.

### Order

3a first: small, independent, and it removes the floor 3b would hit. Then
3b, verified like Phase 2: run a pruned and a full update on copies of the
library and compare. With nothing new upstream they must leave the same rows
(timestamps aside).
