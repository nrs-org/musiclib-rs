# Plan: make every track playable — ISRC bridge + playback links

Brief plan, to revisit. Builds on `plan-video-player.md` (the player) and
touches `plan-bounded-import.md` (track→album pull-in).

## Problem

A track is playable only if one of its sources passes `embed_for`
(`src/bin/server/player.rs:207`): `youtu.be/…`, `open.spotify.com/track/…`, a
SoundCloud track, or `nicovideo.jp/watch/…`. Tracks imported from MusicBrainz
are often not playable, because MB has no streaming URL rel on the recording.

Measured on the NRS library (`nrs-next/musiclib.db`, 2026-10-13):

- 339 of 676 NRS-referenced tracks were in the library but not playable.
- MB had ISRCs for 216 of them; Spotify `isrc:` search matched 185. Imported
  by hand, 183 merged into the MB entry via the existing ISRC → MB path
  (playable 49% → 76%).
- 156 remain: 123 with no ISRC in MB, 31 with an ISRC Spotify doesn't know, 2
  length mismatches (a remaster/edit under the same ISRC).

The library will grow, so this has to happen inside import and playback, not
as one-off scripts.

## 1. ISRC bridge (exact, no review)

Today only Spotify → ISRC → MB exists (`spotify/track.rs:79` stores the ISRC,
`musicbrainz/types.rs:195` resolves it via `lookup_isrc`). The reverse is
missing:

- Add `isrcs` to the recording `PARTS` (`musicbrainz/recording.rs:96`) and
  store each as an `isrc` source. Currently ISRCs are only fetched for work
  ranking (`recording.rs:494`) and dropped.
- Normalise ISRCs to upper case everywhere a source is stored. Spotify
  sometimes returns lower case (`ushm21927082`), which made one Spotify track
  fail to merge with its MB recording.
- Spotify resolver: for an `isrc` source, search `isrc:<code>` and keep
  results whose `external_ids.isrc` matches exactly. Among several releases of
  the same ISRC, prefer the closest duration.
- Guard: skip when the duration differs from MB by more than ~5s (a remaster
  or edit can reuse an ISRC).

Hard identity, so it can feed dedup like any other provider source.

## 2. Playback links (search, not identity)

For tracks still not playable after import, find something that *sounds* like
the recording without claiming it *is* the recording. A re-upload, a
different edition, or an official MV is fine for playback, but must never
merge two recordings.

- **Storage:** a source row flagged `playback_only` (or a separate
  `playback_link` table) carrying `origin` (`searched` / `user`), a score and
  a timestamp. Dedup, hard-identity matching and soft-match ignore these rows;
  `playable_sources` includes them, ranked below real sources.
- **Resolver pass** (after dedup/soft-match): for each non-playable track,
  search YouTube Music, then Spotify, by title + primary artist. Score on
  title similarity, artist match, duration within ~3s, and reject
  `cover` / `live` / `TV size` / `remaster` / `instrumental` markers unless the
  recording's own title has them.
  - High confidence → write a playback link.
  - Medium → a suggestion in the existing review UI.
  - Low → nothing.
- **Manual links:** let a user attach a playback URL directly. NRS already
  does this informally ("reprint for playback" YouTube re-uploads).

## 3. Search on play (last resort)

When the player is asked for a track with no playable source, run the same
scorer on demand. Persist the first acceptable hit as a `searched` playback
link, so it is searched once rather than on every play. Add a "wrong song"
action that deletes the link and records the rejected candidate so it isn't
picked again.

## Open questions

- Storage shape: flag on `entry_source` vs separate table. A flag is less
  code, but every identity query must remember to exclude it.
- YouTube Music search runs through ytmusicapi; `ytmusicapi_server.py` only
  exposes `/artists/` and `/albums/` today and needs a `/search` endpoint.
  (It also binds `0.0.0.0`; it should probably be `127.0.0.1`.)
- Quota: YouTube Data API is quota-counted; prefer ytmusicapi for search and
  the Data API only for metadata.
- Imports from a Spotify track pull in its album's whole tracklist (about
  1.8k extra entries for 184 roots on the NRS library). That is
  `plan-bounded-import.md`'s problem, but the ISRC bridge will trigger it
  more often, so land a "track only" fetch mode first or alongside.

## Order

1 first: small, exact, high yield. Then the storage decision, then 2, then 3
on top of 2's scorer.

## Status (2026-10-13)

Step 1 landed on master: `8b3ac9f` (MB recordings/releases store `isrcs`),
`70a638e` (no URL lookup for non-URL ids), `077db55` (Spotify
`resolve_external_source` via `isrc:` search, exact-ISRC re-check). Gaps left
from step 1:

1. **ISRC case.** `lookup_isrc` compares case-insensitively, but stored
   `isrc` sources are raw, and dedup matches identifiers exactly. A Spotify
   track stored as `ushm21927082` never merges with `USHM21927082`. Upper-case
   ISRCs in one place at canonicalization (the `registry::canonicalize` chain
   used by `pipeline/ingest.rs`), not per backend.
2. **Duration guard.** `lookup_isrc` returns every Spotify track carrying the
   ISRC; a label-reused ISRC on an edit/remaster merges the wrong version (NRS
   example: ハロ/ハワユ, Spotify 4:58 vs MB 4:50). Drop matches more than ~5s
   off the recording. `resolve_external_source(entry_type, sources)` has no
   duration today: needs a parameter or a small context struct, so update the
   MB impl too.
3. **All matches attached.** Keep several Spotify releases of one ISRC (useful
   playback sources), but only those that pass 2, since each is also a dedup
   key.
4. **Quota.** One Spotify search per ISRC'd MB track at import. No code
   change; document an `api.spotify.com` scheduler limit alongside the HTTP
   cache.
