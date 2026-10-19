# Plan: store image URLs

**Status (2026-10-19):** implemented on `feat/player-credits-graph`, all four
phases; the backfill has only been run on a copy of the DB so far. Where the
code differs from the text below:

- `pair_image` is a SeaORM entity, created by the schema sync like the other
  tables; no `user_version` step.
- `origin` is only `fetch` or `listing`. The Cover Art Archive URL comes from
  the release's own response (`cover-art-archive.front`), so it's `fetch`.
- The OS media controls get the resolved image URL (`cover::resolve`), not
  the `/cover` redirect: MPRIS clients load `artUrl` themselves and may not
  follow redirects or reach the server.
- Inheritance also covers a release without an image taking its release
  group's.

## Problem

The library stores no images. No table or column holds an image URL, and no
backend copies one into `EntityResult`. Spotify's `ArtistResponse` even
deserializes `images` (`src/providers/backends/spotify/artist.rs`) and then
drops it.

The player's artist and release pages show pictures anyway: `src/bin/server/
art.rs` derives candidate URLs from source identifiers at display time, and
the browser tries them in order. That works only where a URL can be derived
from an id:

- **Cover Art Archive** from MusicBrainz ids. It's a guess: the URL 404s when
  the release has no art, and archive.org can take ~10 s per image.
- **`i.ytimg.com`** from YouTube video ids.
- **Spotify** needs an oEmbed lookup in the browser for each album or artist.
  Spotify throttles bursts, so the frontend queues them 3 at a time with
  retries.
- **Discogs, SoundCloud and Niconico** get nothing. Their image URLs can't be
  derived from an id, and Discogs needs a token.

Now-playing art is worked out in the browser that's playing: it asks Spotify
for album art with its own login, or builds a YouTube thumbnail URL, and
reports the result as `mounted.art`. `media_controls.rs` passes that to the
OS media controls as `cover_url`. Without Spotify connected in that browser,
a Spotify track has no art anywhere.

The providers already return image URLs in responses we fetch. Keeping them
fixes every case above.

## Audit: what the HTTP cache already holds

Checked on 2026-10-19 against `~/.cache/musiclib-rs/http_cache.db` (890k
entries, 2.8 GB). `DbHttpCache` never deletes expired entries; a refetch
overwrites them. So the response of every pair ever fetched is still there,
and a backfill doesn't need to refetch.

| Provider / kind | Fetched pairs | Response cached | Image in the response |
|---|---|---|---|
| Spotify album / artist | 1,421 / 1,295 | all | `images[]` (640/300/64) |
| Spotify track | 5,525 | all | **none**, see below |
| YouTube video / playlist / channel | 3,955 / 338 / 1,329 | all | `snippet.thumbnails` |
| YouTube `@handle` channel | 1,287 | 1,203 | `snippet.thumbnails` |
| Discogs release / master / artist | 450 / 74 / 1,444 | all | `images[]` (fetched with the user token) |
| Discogs track (`release/{n}?track=N`) | 2,242 | all (the release's response) | none of its own; inherits the release's |
| MusicBrainz release | 2,539 | all | `cover-art-archive.front` flag only |
| MusicBrainz release group / recording / artist | all | all | nothing |
| Niconico (yt-dlp server) | ~144 | all | `thumbnail` (signed) + `thumbnails[]` (unsigned) |
| SoundCloud (yt-dlp server) | few | all | `thumbnail`; the one sampled was a 404 error body |

The listings hold images for children too, including stubs that were never
fetched: Spotify `/artists/{id}/albums` items have `images`, YouTube
`playlists` and `playlistItems` items have `snippet.thumbnails`, and Discogs
`/artists/{id}/releases` items have `thumb`.

**Spotify tracks.** All 131k cached `/v1/tracks/{id}` bodies (June to October
2026) have `album` cut down to `{id, name, external_urls}`, with no `images`.
The checked-in fixture `track_beautiful_circle.json` has them. The coalescer
passes Spotify's JSON through unchanged (`spotify/coalesce.rs`, `split`), so
this is what Spotify returns for this app now; the cause is unverified. The
album id is still there: 4,686 tracks have their album's response cached and
839 don't. Fetching those 839 albums is ~42 batched `/v1/albums?ids=` calls.

**MusicBrainz `inc=`.** Missing `inc` values don't matter for images.
MusicBrainz has no images of its own, and every release lookup carries the
`cover-art-archive` block regardless of `inc`. Release-group lookups don't
carry it.

**Hazards.**
- `scripts/clear_yt_cache.sh` deletes every YouTube entry. Running it before
  the backfill loses ~7k cached YouTube responses.
- Niconico's `thumbnail` is signed (`?key=…`) and may expire. The unsigned
  `https://nicovideo.cdn.nimg.jp/thumbnails/{n}/{n}` in `thumbnails[]` is the
  one to keep.

## Model

**An image belongs to a pair**, like aliases and contributions. A pair has at
most one stored image: the provider's picture for that exact entity (album
cover, artist photo, video thumbnail).

New table, not a column on `entry_source`. Stubs need images too, and the
backfill then writes without touching `entry_source` rows:

```sql
CREATE TABLE pair_image (
  source     TEXT NOT NULL,
  identifier TEXT NOT NULL,
  url        TEXT NOT NULL,
  width      INTEGER,          -- NULL when the provider doesn't say
  height     INTEGER,
  origin     TEXT NOT NULL,    -- 'fetch' | 'listing' | 'derived'
  run_id     INTEGER,
  PRIMARY KEY (source, identifier)
);
```

Added as `PRAGMA user_version` 7 in `MusicDb`'s migration ladder.

**One URL per pair, about 300 px.** The UI shows 34 px row thumbnails and
~170 px tiles and heros (~340 px at 2×). Pick the provider's smallest variant
with a long side ≥ 300 px, else its largest. That gives Spotify 300, YouTube
`medium` (320×180), Discogs `uri` (≤600), and Niconico's unsigned original.
Storing every size isn't worth a second table yet.

**`origin` sets precedence.** `fetch` (from the pair's own response) beats
`listing` (from a parent's listing item). `listing` beats `derived` (a URL
built from data, like the CAA one below). A write never replaces a
higher-origin row. This is the same rule `StubInfo` follows: listing data is
used only for a pair that wasn't fetched.

## Write side (import)

- `EntityResult` gets `image: Option<Image>`, where `Image { url, width,
  height }`.
- `ChildRef` gets `image: Option<Image>`, from the listing item.
- `PairMetadata` and `StubInfo` each get `image`. `flush.rs` writes
  `pair_image` beside `upsert_pair`, under the same `run_id`, so run rollback
  covers it.

Each backend gets one extraction function per response type. It takes the
parsed response and returns `Option<Image>`. The table below is the spec;
the backfill script reimplements the same picks in Python.

| Backend | Pair image (`fetch`) | Child image (`listing`) |
|---|---|---|
| Spotify | album, artist: `images[]` | artist albums: `items[].images` |
| YouTube | video, playlist, channel: `snippet.thumbnails` | playlists, playlistItems: `snippet.thumbnails` |
| Discogs | release, master, artist: `images[]` (primary first) | artist releases: `thumb` |
| MusicBrainz | release: `https://coverartarchive.org/release/{mbid}/front-250` when `cover-art-archive.front`, as `derived` | none |
| yt-dlp (Niconico, SoundCloud) | the unsigned `thumbnails[]` entry, else `thumbnail` | none |

Spotify tracks, Discogs tracks, and MusicBrainz recordings and release groups
store nothing; the read side resolves them (below).

## Read side: `GET /api/entries/{id}/cover`

One URL per entry for its picture. The server picks the image and answers
with a redirect; the browser loads the image straight from the provider.
Image bytes are never proxied or stored.

- **302** to the image, with `Cache-Control: max-age=86400`, so the browser
  asks once per entry per day.
- **404** when the entry has no known-good image. The frontend shows its
  letter placeholder.

The server resolves the image over the entry's soft-identity component, in
this order:

1. **Stored:** the component's `pair_image` rows, `fetch` before `listing`
   before `derived`, then by provider (Spotify, Discogs, YouTube, Niconico,
   SoundCloud, Cover Art Archive).
2. **Inherited:** for an entry with no stored image of its own.
   - A track uses the image of a release that lists it (`entry_child`
     parent). This covers Spotify tracks, Discogs tracks and MusicBrainz
     recordings.
   - A release group uses one of its releases' images.
3. **Derived, reliable only:** an `i.ytimg.com` thumbnail from a YouTube
   video id.

It never redirects to a guess: no Cover Art Archive URL without the
`cover-art-archive.front` flag behind it, and no Spotify oEmbed. A redirect
to a broken image is worse than a 404.

**Cost:** a stored URL can still die (a deleted video, a removed Discogs
image). Today's candidate list falls through to the next option; the
redirect shows the placeholder instead. That's accepted.

**Consumers:**

- **Frontend:** `artBox` becomes `<img src="/api/entries/{id}/cover"
  loading="lazy">`, with the placeholder shown on error. That covers the
  artist page, release pages, search results, the queue and the player bar.
  The candidate list, the oEmbed queue and its `localStorage` cache go.
- **Now playing:** the server sets the OS media controls' `cover_url` to the
  cover of the entry playing, independent of the browser. MPRIS needs an
  absolute URL, so it points at the server's own address. The browser's
  `Spotify.getTrackArt` lookup and `mounted.art` go.
- **Discography rows** drop their `art` field; the client builds the URL from
  `entry_id`.

## Backfill

A one-off Python script, `scripts/backfill_images.py`, run with `uv run`.
It uses only the standard library (`sqlite3`, `zlib`, `json`) — no Rust
compile and no code shared with the importer. It reads `http_cache.db`,
writes `pair_image`, and makes no network requests.

1. For every pair in `entry_source`, map it to the cache key of its own
   lookup (the mapping from the audit), decode the body (zlib when
   `body_encoding` says so), apply the pick from the table above, and write
   it as `fetch`.
2. For every cached listing (Spotify artist albums, YouTube playlists and
   playlistItems, Discogs artist releases), write child images as `listing`
   for pairs in `entry_source` that have no `fetch` image.
3. Print a count per provider and kind.

`INSERT OR REPLACE` with the origin check in SQL makes a re-run harmless.
It runs after phase 1, once any binary has opened the DB and created
`pair_image`. Back up `musiclib.db` first.

Not handled: the 839 Spotify albums missing from the cache. They pick up
images the next time an import reaches them. Until then their tracks get
`/cover` from another release that lists them, else a YouTube thumbnail,
else a 404.

## Phases

1. **Schema and write side.** `pair_image`, the `Image` fields on
   `EntityResult`/`ChildRef`/`PairMetadata`/`StubInfo`, flush, and per-backend
   extraction with unit tests against the existing fixtures (most already
   contain images). New imports start storing images.
2. **Backfill.** Run `scripts/backfill_images.py` once on the real DB.
3. **`/cover` endpoint**, and the frontend and media controls switched to it.
4. **Cleanup.** Remove `art.rs`'s candidate lists, the frontend's oEmbed
   queue and `Spotify.getTrackArt`.

## Out of scope

- Downloading or caching image bytes. URLs only; the browser caches.
- Embedded cover art from local files (`local` backend).
- Choosing a picture per user (e.g. preferring the anime-style cover over the
  photo).
- Fetching the 839 uncached Spotify albums in the backfill.
- Refreshing images that change upstream. The add-only update model
  (`plan-update.md`) doesn't refresh metadata either.

## Open questions

1. **One size or several?** One ~300 px URL covers today's UI. A full-screen
   now-playing view would want 640 px. Spotify's and YouTube's URLs encode
   size predictably, so `/cover?size=large` could rewrite the stored URL
   later without storing a second one.

Settled:

- **Spotify oEmbed fallback:** dropped. `/cover` redirects only to
  known-good URLs.
- **Inherit at read time or copy at write time:** read time. `/cover`
  resolves one entry per request and the redirect is cached, so the join is
  cheap, and it stays correct when edges change.
