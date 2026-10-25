# Plan: revalidate stale cached entity responses

Status: **backlog**. Not scheduled; written 2026-10-25 so the design is on
record. Builds on [plan-update.md](plan-update.md).

## Problem

Listing snapshots (`plan-update.md`) pick up *new* items: uploads, releases,
playlist entries. Everything else the importer fetches is an entity response
(a video, a release, a recording, an MB URL's relations), and those are served
from the DB HTTP cache forever: `DbHttpCache::get` never checks `expires_at`.
So edits to things already in the library never arrive:

- an editor adds an ISRC, a YouTube link or a cover relation to an MB
  recording or URL the library already has;
- a Spotify album gains a track credit, or a YouTube video's title changes;
- an MB URL lookup that 404'd once stays a 404. The cache holds 1,232 such
  `url?resource=` 404s from before the mirror tracked MBIDs (2026-10-25).

`plan-update.md` rejected honoring TTLs: refreshing everything once it's old
can't fit YouTube's 10k units/day. That still holds for a full sweep. What's
left is spending whatever budget a day leaves on the oldest responses.

## Model

After the walk and the uploads full-refresh pass, a **revalidation pass**
re-requests cached entity responses, oldest `created_at` first, bypassing the
cache (`http::bypass_cache`), and writes the fresh response back. The next
walk then reads the fresh body. Like listing refreshes, it only ever replaces
a response with a newer one.

Budgets are per source, because the constraints differ:

| Source | Constraint | Budget |
|---|---|---|
| YouTube Data API | 10k units/day, shared with refreshes | what `--quota-budget` leaves after the listing passes |
| Local MB server | no quota; capacity (Starlet, 10 workers) | fixed request cap per run, e.g. 5,000 |
| Spotify | hidden rolling rate limit, long `Retry-After` bans | small fixed cap, e.g. 500 |
| Discogs | 60/240 requests per minute | small fixed cap |

With about 130k cached MB URL responses, 5,000 per day cycles the MB part in
under a month. YouTube's leftover budget may not cover its part at all; that's
accepted, and it's revalidated oldest first so nothing starves forever.

### Which responses qualify

- Entity requests only. Listing pages are snapshot-managed, and fixed
  tracklists (album pages, `OLAK5uy_…` playlists) go with their entity.
- Coalesced requests (YouTube `videos`/`channels` by id, Spotify
  tracks/albums/artists) are cached per entity, so revalidation can batch them
  again: 50 YouTube videos cost 1 unit. That makes YouTube revalidation about
  50 times cheaper than it first looks.
- 404s from the local MB server revalidate first: they're cheap, and a stale
  one hides a link permanently.

### Picking the oldest

`cache_entry` has `created_at` and a `key` index only. The pass needs an index
on `(created_at)`, or a per-domain cursor, to find the oldest N without
scanning about a million rows each day. `key` starts with the method and URL,
so the domain is a prefix of `key`.

## Alternative for MusicBrainz: invalidate from replication packets

Replication packets say exactly which rows changed. The mirror applies only
the `url` table today. Reading `recording`, `release`, `l_*` (relationship)
and `isrc` changes too would let the pass revalidate exactly the MB entities
that changed, instead of cycling through all of them. But packets carry
internal row ids, not MBIDs, and relationship rows reference both ends by id,
so this needs an id-to-MBID map for every entity type the library touches.
Much more work; only worth it if the cycle above proves too slow.

## Open questions

- Should a revalidated response that changed trigger a re-walk of its pair
  right away, or wait for the next run's walk? Waiting is simpler, and the
  walk reaches every pair daily anyway.
- Does the pass belong in `import` (after the walk, like `full_refresh_pass`)
  or in its own binary run by `update.sh`? Its own binary keeps `import`'s
  run time predictable.
- Per-source caps: in `http.yaml` next to the scheduler, or `update.sh`
  environment variables like `QUOTA_BUDGET`?
