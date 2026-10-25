# Plan: add-only library updates from cron

## Problem

`musiclib.db` never picks up new uploads or releases. The cause is in the HTTP
cache, not the DB:

- The importer doesn't skip pairs already in the DB. Every run fetches every
  pair it reaches, `upsert_pair` overwrites metadata, and
  `commit_import_run` (`src/musicdb/mod.rs:1688`) drops stale edges of
  re-fetched parents.
- But `DbHttpCache::get` (`src/httpcache/db.rs:348`) never checks
  `expires_at`/`stale_at`. Every response is served forever, so a re-run sees
  exactly the listings of the first run. The TTLs in `http.yaml` are inert.

Honoring TTLs is the wrong fix. It refreshes *everything* once it's old, and
YouTube's 10k units/day can't pay for that. What changes between runs is
almost only **new items in growing listings**: a channel's uploads, an
artist's discography, a playlist. A video's own record or an album's
tracklist practically never changes.

Refreshing growing listings page by page is still too expensive. An utaite
with 1,500 uploads costs 30 `playlistItems` calls per run (O(N/50)) even when
nothing new was posted. Page-level caching also makes early stop impossible:
after new uploads push items down, the old cached pages 2..n no longer line up
with a fresh page 1 (offsets and YouTube page tokens are both positional), so
stitching fresh and cached pages skips or duplicates items. A playlist parent
counts as fetched completely, so a skipped item's edge would be deleted at
commit.

## Scope

- **Add-only.** New items are picked up. Removed items, renamed titles and
  changed metadata aren't goals. No `gone_at`, no alias replacement.
- **Run from cron**, unattended, within a quota budget.
- **Roots stay external.** The cron job runs `import --url-file roots.txt
  --fetch-options <name>`, one line per (roots file, options) pair. No roots
  table; fetch options stay unstored, as in `plan-bounded-import.md`.
- **The importer doesn't change.** It keeps reading every listing to the end
  (`src/pipeline/importer.rs:222`) and keeps its edge-replacement semantics.

## Model

**A growing listing is cached as one snapshot**, not as pages: the ordered
list of its `ChildRef`s, under a listing key such as
`youtube:uploads:UUxxxxxxxx`. Fixed listings (album tracklists, Discogs master
versions) stay on the per-page HTTP cache.

Each growing listing declares an **order**:

| Order | Listings | Refresh |
|---|---|---|
| `NewestFirst` | YouTube channel uploads | **incremental**: fetch pages until a whole page is already in the snapshot, merge, stop |
| `Unordered` | YouTube channel playlists, YouTube playlist items, Spotify artist albums, Spotify playlist items, MB artist browses, Discogs artist releases | **full**: fetch every page, replace the snapshot |

A run has a **listing mode**, set by `import --refresh[=full]`:

| Mode | Snapshot present | Snapshot missing |
|---|---|---|
| `cached` (default, no flag) | serve it, no requests | fetch pages through the normal HTTP cache, write snapshot with unknown age |
| `auto` (`--refresh`: daily cron, manual) | `NewestFirst`: incremental; `Unordered`: full | `NewestFirst`: build as in `cached`, then incremental; `Unordered`: fetch all pages fresh |
| `full` (`--refresh=full`, manual only) | fetch all pages fresh, replace | fetch all pages fresh, write snapshot |

The `cached`-mode miss path reads pages through the existing HTTP cache. The
first run after this ships builds every snapshot from the pages already cached
by earlier imports, costing no quota. All pages of a listing were cached by the
same walk, so they're consistent with each other. Their real age is unknown,
so they're written with `created_at = 0`, which puts them first in line for a
full refresh (below). An `auto` run builds a missing `NewestFirst` snapshot the
same way before refreshing it, so even a first `--refresh` run costs a page or
two per channel rather than a full fetch.

### Incremental refresh

```
fetched = []
for page in pages(fresh):
    fetched += page.items
    if every item in page is in snapshot (by item key): break
merged = fetched ++ [s for s in snapshot if s.key not in fetched]
```

- Stopping on a **whole known page** rather than the first known item
  tolerates small reorderings near the top (premieres, videos made public
  later). One extra call when there are new items, none otherwise.
- `fetched` items come first and win on duplicates, so their titles are fresh.
- If the listing runs out before a known page, `merged` is just `fetched`,
  which is complete.
- Cost per channel: 1 call with no new uploads, `ceil(new/50) + 1` with new
  ones. Items that land deeper than the stop page are missed until that
  listing's next full refresh (below).

### Full refreshes spend the leftover budget, oldest first

A `--refresh` run has two steps:

1. **The walk**, as today, with `auto` listings: incremental refreshes of
   `NewestFirst` listings and full refreshes of `Unordered` ones. This is what
   finds new songs, so it goes first and gets the budget first.
2. **The full-refresh pass**, after the walk. It takes `NewestFirst`
   snapshots in `created_at` order (oldest full fetch first) and fully
   refreshes them until the run's YouTube budget is used up. Today only
   YouTube uploads are `NewestFirst`, so the pass is YouTube-only too.

```
for snap in snapshots where order = NewestFirst
                        and last_used_at >= now - 14d
                        and not refreshed in this run
                    order by created_at:
    if used_today >= budget: stop
    full_refresh(snap)
```

- Every day spends the whole budget, and the listings that waited longest go
  first. Nothing starves, unlike a budget gate in traversal order, and the
  first days after shipping refresh the unknown-age snapshots as fast as the
  quota allows.
- The pass runs on the snapshot table, not the walk, so it needs to rebuild
  a fetcher from a key alone (`youtube:uploads:UU…` → `ChannelUploadsPageFetcher`).
  Each backend registers a factory per listing kind.
- `last_used_at` (set whenever a walk reads the snapshot) keeps it from
  spending quota on listings no root reaches any more.
- Items a full refresh finds below the stop page are in the snapshot from
  then on, and the next run's walk imports them. That's at most a day's
  delay, for a case that's rare to begin with.
- How often a listing is fully refreshed falls out of the budget: total
  full-refresh cost divided by the daily leftover. The summary warns when the
  oldest `created_at` passes 7 days, i.e. when the budget no longer covers a
  weekly cycle.

### Daily quota budget

Only YouTube is budgeted. Its quota is 10k units per day, reset at midnight
Pacific Time, and every read endpoint this code uses costs 1 unit per call.
Spotify's limits aren't published (only a hidden rolling window), so a
counted number wouldn't mean anything; Spotify refreshes are left to the
scheduler's 429 handling, as today.

The cache DB keeps a per-day ledger, so every run on the same day (cron and
manual) draws from the same total:

```
quota_usage(day           TEXT PRIMARY KEY,  -- Pacific-time date, "2026-10-16"
            youtube_units INTEGER)
```

- `QuotaCounter` (`src/http/quota.rs`) adds to the row as requests go out,
  not just to its in-process counter. It stays below the scheduler, so
  retries count.
- `import --quota-budget <units>` caps the YouTube units this run may use for
  **refreshes**. Before each YouTube refresh page, the store checks
  `used_today < budget`. Once that fails, the remaining YouTube listings are
  served from their snapshots and the summary logs how many were skipped.
  An incremental refresh cut off partway is discarded (its fetched prefix
  may leave a gap), and the old snapshot is kept. Listings on other services
  aren't gated.
- New children found by a refresh are still fetched past the budget. They're
  coalesced (`videos.list`, 50 per unit) and the importer can't skip them
  without leaving stubs, so the budget is a soft cap below the real limit.
- The cron script passes 8000, leaving the rest for manual refreshes. Manual
  runs pass none, so only YouTube's own limit applies.

### Manual refresh

`import --refresh <url>` with the usual fetch options, e.g. Watame's channel
right after a release:

- the walk refreshes only the growing listings it reaches from that root: her
  uploads incrementally (1–2 units), her channel playlists and her Spotify and
  MusicBrainz discographies in full;
- the new song is fetched and flushed in the same run, so it shows up in the
  server right after;
- with no budget passed, the full-refresh pass does nothing, so a manual run
  costs only its walk;
- it draws from the same daily ledger as cron.

`--refresh=full <url>` forces full refreshes for everything that root reaches,
e.g. to pick up an upload that appeared below the stop page.

### Guarantees

- **Atomic.** The snapshot is written in one upsert after its pages are
  fetched. A refresh that fails partway leaves the old snapshot, so a later
  run can never read a half-refreshed listing.
- **Once per run.** The store refreshes a key at most once per process, and
  concurrent readers of the same key (one pair claimed under several option
  sets) wait on the same future instead of refetching.
- **The importer still sees a complete listing**, so `commit_import_run`'s
  edge replacement stays correct. A removed upload stays in the
  `NewestFirst` snapshot until its next full refresh; an `Unordered` refresh
  drops it, and a playlist's edge to it is then removed at commit, as now.

### Where snapshots live

In the HTTP cache DB, as a new table: they're re-fetchable derived data like
the cache, not facts, so `musiclib.db` doesn't grow a dependency on them.

```
listing_snapshot(
  key          TEXT PRIMARY KEY,   -- "youtube:uploads:UU…"
  items        BLOB,               -- zlib JSON array of ChildRefs
  kind         TEXT,               -- "youtube:uploads", for the factory
  created_at   INTEGER,            -- last full fetch; 0 = unknown age
  refreshed_at INTEGER,            -- last incremental or full refresh
  last_used_at INTEGER             -- last time a walk read it
)
```

Without a configured `db_cache`, the store is in-memory: every run behaves as
a snapshot miss.

## Phase 1: listing store and snapshot source

New module `src/providers/listing.rs`:

```rust
pub enum ListingOrder { NewestFirst, Unordered }
pub enum ListingMode { Cached, Auto, Full }
pub struct ListingKey { pub kind: &'static str, pub id: String }

pub struct ListingStore { /* DB handle, mode, per-kind modes, budget, factories, per-run state */ }
```

- Snapshots hold the backend's existing `ChildRef`s (now
  `Serialize`/`Deserialize`), so backends keep their `PageFetcher`s as they
  are: no per-backend item type. An item's identity is its smallest
  `(source, identifier)` pair.
- `ListingStore::listing(key, order, fetcher) -> PaginatedChildSource`
  returns the same type backends build today, so `with_static_eval` and the
  declaration check keep working, via a new
  `PaginatedChildSource::from_stream` (`src/providers/types.rs`). The source
  is lazy, resolves the whole listing on first poll, then yields it.
- Refresh pages bypass every cache layer through `http::bypass_cache(fut)`,
  a task-local scope checked by `CacheHttpClient`, so no request builder
  needs a `no_cache` parameter. Not writing pages either: the snapshot is the
  only copy.
- Once per run: a per-key async lock plus a set of refreshed keys. Later
  readers of a refreshed key read its snapshot.
- On a failed or skipped refresh the store falls back to `cached` behaviour
  (old snapshot, else the HTTP cache), and only a walk with no copy at all
  yields its partial items plus the error, as a plain listing does.

Tests, with a mock fetcher that counts calls:

- incremental stop on the first fully known page; `fetched` wins on duplicate
  keys; exhausted listing replaces the snapshot;
- failure mid-refresh keeps the old snapshot;
- two concurrent readers in `auto` mode cause one refresh;
- `cached` mode with a snapshot makes zero calls;
- a snapshot built from cached pages has `created_at = 0`;
- budget: with `used_today >= budget`, `auto` serves the snapshot and makes
  no calls; a cut-off incremental refresh keeps the old snapshot; non-YouTube
  listings aren't gated.

## Phase 2: plumbing

- `build_providers(config, http)` (`src/providers/registry.rs:277`) takes an
  `Arc<ListingStore>` too. The `import` and `dedup` binaries build it from the
  `db_cache` path in `http.yaml` (`dedup` always uses `cached`).
- `import --refresh[=full]`; the bare flag means `auto`.
- `import --quota-budget <units>`, and the `quota_usage` ledger written by
  `QuotaCounter` (`src/http/quota.rs`).
- Per-kind opt-out, in `providers.yaml`:

  ```yaml
  listings:
    youtube:playlist_items: cached   # never refresh this kind
  ```

  Default: every growing kind follows the run's mode.
- The import summary logs, per kind: snapshots served, refreshed
  incrementally, refreshed fully, skipped for budget, new items found; plus
  YouTube units used by this run and today in total.

## Phase 3: YouTube

- `ChannelUploadsPageFetcher` (`youtube_api/channel.rs:208`) → `NewestFirst`,
  key `youtube:uploads:<uploads playlist id>`.
- `ChannelPlaylistsPageFetcher` → `Unordered`, key
  `youtube:channel_playlists:<channel id>`.
- `PlaylistItemsPageFetcher` (`youtube_api/playlist.rs:128`) → `Unordered`,
  key `youtube:playlist_items:<playlist id>`.
- Fixture test: a recorded uploads listing, then a modified one with two new
  items at the front; the incremental run makes one `playlistItems` call more
  than the stop page and yields 2 new children.

## Phase 4: full-refresh pass

- A listing-kind registry: each backend registers
  `kind → fn(id) -> Box<dyn PageFetcher>` with
  `ListingStore::register_factory`. Phase 3's `youtube:uploads` is the
  first entry.
- `ListingStore::full_refresh_pass()`, called by `import` after the ingest,
  does nothing unless `--quota-budget` was given. It follows the loop in the
  Model section.
- `last_used_at` is set on every snapshot read, including in `cached` mode.
- Tests: oldest `created_at` goes first; the pass stops at the budget;
  snapshots refreshed by the walk this run are skipped; snapshots unused for
  14 days are skipped; a refresh that fails moves on to the next snapshot and
  leaves the failed one's row alone.

## Phase 5: other backends

`Unordered` snapshots for Spotify artist albums and playlist items, the three
MusicBrainz artist browses, and Discogs artist releases. These are usually
tens of items, so a full refresh is a few calls, and Spotify and MusicBrainz
only rate-limit.

`YtmusicDiscographySource` isn't paginated. It becomes a single-page
`Unordered` snapshot so it follows the same mode.

## Phase 6: cron

- `scripts/update.sh`: `import --refresh --quota-budget 8000` over each
  roots file `<config_dir>/update_roots/NAME.txt` (fetch options
  `NAME.yaml`), run daily. There's no separate weekly job: the full-refresh
  pass spends each day's leftover budget. With several roots files run one
  after another, all but the last pass `--no-full-refresh-pass`: an early
  run's pass would otherwise spend the budget later runs need for their
  incremental refreshes. The last one's pass works on the whole snapshot
  table, so nothing is lost.
- Manual refresh needs nothing extra: `import --refresh <url>`. Because of
  that form, `--refresh=full` requires the `=`.
- Document `--refresh`, `listings:` and the cron setup in
  `docs/CONFIG_REFERENCE.md` and `CLAUDE.md`.

## Verification

1. Import an utaite channel through its uploads (no `--refresh`). Then run
   with `--refresh`: YouTube count is 1 for the channel's uploads, and the DB
   is unchanged.
2. Drop the first 3 items from the channel's snapshot (a store-level test
   helper; the blob is zlib JSON), simulating 3 new uploads, and rerun with
   `--refresh`: 1–2 calls, 3 new tracks fetched
   (coalesced `videos.list`), existing entry ids unchanged.
3. Kill a `--refresh=full` run mid-listing: old snapshot intact; next run
   without the flag serves it with no calls.
4. A full Hololive `--refresh` run: the walk alone stays well under 8000
   YouTube units, and the pass spends the rest on the oldest snapshots. Over
   the first few daily runs, the unknown-age snapshots (`created_at = 0`)
   all get a full refresh, and the 7-day warning stops firing.
5. With `--quota-budget 1` after units were already used today, a
   `--refresh` run makes no refresh calls and logs every listing as skipped;
   a manual `--refresh <url>` with no budget still refreshes that root.

## Later (not in this plan)

- Honoring TTLs in `DbHttpCache::get`, for entity responses. Today's inert
  TTLs keep entity metadata frozen, which is fine for add-only.
- Removed items (`gone_at`), alias replacement on rename.
- Fair ordering for the walk's own refreshes. The walk reaches listings in
  traversal order, so if the walk alone ever exhausts a budget, the same late
  roots miss their incremental refresh every day. Incrementals cost about 1
  unit per channel, so this only matters at thousands of channels.
- A refresh button in the server that runs `import --refresh <url>` for an
  entry's root.
- Pruning the per-page cache entries that snapshots made dead.
