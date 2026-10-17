# Plan: listen and score

Status: implemented on `feat/listen-and-score` (2026-10-17). The plan
below is kept as written; **As built** at the end lists where the code
differs from it and what's still open.

## Goal

Score a track in NRS, approve its relation suggestions, and fix its library
data **while it plays**, from the musiclib player, without switching to the
nrs-app UI for the common cases.

Non-goals: a full impact editor in the player (the drawer links out to
nrs-app for that), a standalone review inbox (the drawer is scoped to the
current track only, for now).

## Principle: NRS is an optional extension

musiclib-rs does not depend on NRS. The player must work fully with no NRS
app running, configured, or ever installed:

- Everything musiclib owns (playback, the play log, library fixes) lives and
  is stored in musiclib, with no NRS call on its path.
- Everything NRS-facing goes through one optional server module,
  `nrs_bridge` (next to the existing `nrs_pool`). Its endpoints answer
  `503 {"error": ...}` when NRS is unreachable, never hang the player, and
  never fail a musiclib-owned request.
- The frontend asks `GET /api/nrs/status` once (and on the settings
  page's refresh). When NRS is unavailable the drawer hides its NRS tabs
  (Score, NRS relations) and shows only Library fixes. Nothing else in the
  player changes.
- NRS writes that can't happen now (listening time) are **recorded in
  musiclib's own data** and pushed later, so an NRS outage loses nothing
  and blocks nothing.
- No nrs-app code is needed for phase 1 and 2: the bridge uses nrs-app's
  existing `/api/v1` endpoints. The bridge is a client of NRS, the same as
  `nrs_pool`, and the store protocol stays the only thing NRS knows about
  musiclib.

## Rank target: which NRS entry a played track scores

The player plays one **edition** of an edition group (`EditionProjection`).
Not every edition is its own NRS entry:

- **Folded into the original.** Editions whose `derived_from`
  transformation is one of `musicdb::DEGENERATE_MARKERS` (`instrumental`,
  `short_ver`, `tv_size`, `game_ver`) are not ranked on NRS. Playing one
  scores, and accrues listening time on, the edition it derives from.
- **Their own entries.** Covers are separate NRS entries, and so are
  remixes and arrangements (provider-asserted original→derived editions).
  Playing a cover scores the cover, never the original it covers.
  `live`, `alt_version`, and full-performance transformations (`acoustic`,
  `medley`, named variants) are their own entries too (confirmed).

`rank_target(entry_id) -> entry_id` walks folded edges toward the original
until it reaches an edition that is its own entry. It lives in
`musicdb` next to `EditionProjection`, because it's a library fact (the
play log uses it too), not an NRS one. Identity (`SoftIdentityProjection`)
is already folded first: any member of the target's identity component
binds the same NRS entry.

## Play log (musiclib-owned)

Move the play log from localStorage (`shuffle_mix_plays`) to the server,
because it's needed for automatic listening time and it's useful on its own
(history, the shuffle cooldown across devices).

New table `play_event`:

| column | meaning |
|---|---|
| `id` | |
| `entry_id` | the edition actually played |
| `started_at` | unix seconds |
| `listened_seconds` | audible time, not position delta (seeks and pauses don't count) |
| `nrs_pushed_seconds` | how much of `listened_seconds` has been added to NRS |

- A play under 10 s of audible time is a skip: it's kept in the log
  (`listened_seconds` stays accurate) but never pushed to NRS and doesn't
  count as a play for the cooldown.
- The player reports a play as it goes: `POST /api/plays` when the track
  starts, `PATCH /api/plays/{id}` every ~30 s and on end/skip, and
  `navigator.sendBeacon` on `pagehide`. Losing up to the last heartbeat on a
  crash is acceptable.
- The shuffle cooldown reads `GET /api/plays?since=` instead of
  localStorage. The existing localStorage plays have no durations, so they
  are dropped, not migrated (the cooldown just starts fresh).

## Automatic listening time → NRS

A background task in `nrs_bridge` pushes listening time automatically, with
no prompt:

1. Skip plays under 10 s. Group unpushed time (`listened_seconds - nrs_pushed_seconds`) by
   `rank_target`.
2. Push only targets **already bound in NRS**. Plays of a track you've
   never scored stay in the log; they are pushed the moment the track gets
   bound (first score from the drawer, or adoption in nrs-app), so the
   backlog isn't lost. Listening alone never adopts an entry, so NRS
   doesn't fill up with everything shuffle played.
3. Upsert the entry's single `music_consumed_progress` impact (the track
   profile has exactly one consumed slot): `PUT .../impacts/{id}` with
   `duration_minutes = current + delta`, or `POST .../impacts` if none.
4. Mark the pushed seconds in `play_event`.

The task runs every few minutes and after each finished play. If NRS is
down it does nothing and retries next tick.

Known gap: if the NRS write succeeds and the musiclib mark fails, that delta
gets pushed twice. The two writes are in different databases, and the window
is small enough to accept.

## Bridge endpoints (player server, `/api/nrs/*`)

| Route | Does |
|---|---|
| `GET /api/nrs/status` | reachable?, `nrs_url` (for links out) |
| `GET /api/nrs/now/{entry_id}` | rank target, its NRS entry id (or unbound), its score/rank, its `music` and consumed impacts, the albums/artists reaching it (from `nrs_pool`), an nrs-app link |
| `POST /api/nrs/now/{entry_id}/score` | quick score of the target or one of its albums: binds it if needed (`/entries/adopt`), then upserts the `music` / `visual` impact and optionally adds one response impact |
| `GET /api/nrs/now/{entry_id}/visual` | proxies `/entries/{nrs}/media` (MV, album covers) and `/visual-grades?kind=` for the target or an album, to grade against |
| `GET /api/nrs/now/{entry_id}/related` | proxies `/entries/{nrs}/related` |
| `POST /api/nrs/now/{entry_id}/related` | proxies the picked suggestions to `/entries/{nrs}/related` |

Lookup by URL reuses `nrs_pool`'s fetch. `nrs_pool` keeps only scored
entries today, so the build also keeps a `library entry id → NRS entry id`
map of **all** bound entries. A bridge write that binds or scores
invalidates the cached pool.

## The drawer (frontend)

A "Now playing" drawer in the player, following the current track, with
keyboard shortcuts so you can use it without leaving what you're doing.

**Score** (NRS available):
- Header: the rank target ("Scoring the original: <title>" when the
  playing edition is folded), the score/rank or "not in NRS yet", and
  minutes listened (pushed + pending).
- Music quality slider (`music_base`, 0–100%).
- One quick response: `nei` / `aei` / `cry` chip, factor, sign, up to two
  anchors picked from `/api/v1/palette`.
- Visual: the MV grade for the track (`visual`, `visual_kind` 0.8 as in
  the track profile), with `base` and `unique` sliders, the MV from
  `/media`, and nearby existing grades from `/visual-grades` to compare
  against.
- Album: each album the track is played from (or the release being played),
  with its score, an album-art grade (`visual`, `visual_kind` 0.25 as in
  the album profile; the cover from `/media`), and a link out. Scoring an
  album binds it if it isn't in NRS yet.
- A link out to the entry in nrs-app for everything else (editing or
  deleting impacts, review flags).

**NRS relations** (NRS available): the target's pending suggestions
(`contains` with its releases, remix/cover links) with approve/skip.

**Library fixes** (always available):
- The edition group, with relate/unrelate (existing
  `/api/relations/*` endpoints).
- Mark this edition's transformation (to fix a wrong fold, e.g. a
  "cover" that is really a TV-size cut).
- Later: soft-identity candidates for this entry, using the softmatch
  "score a chosen set of entries" path.

## Phases

1. **Play log on the server.** `play_event`, `/api/plays`, the cooldown
   switched over. No NRS involvement; the player gets better on its own.
2. **Bridge + drawer.** `rank_target`, `nrs_bridge` (status, now, quick
   score, related), the drawer's Score and NRS relations tabs, and the
   automatic consumed-time push.
3. **Visual and album grading.** The MV grade, the album rows with
   album-art grades, and the `/visual` proxy.
4. **Library fixes tab.** The edition and relation controls, then
   soft-identity candidates.

## Decided

- `live` and `alt_version` editions are their own NRS entries.
- Plays under 10 s are skips.
- The drawer scores albums too, including album art, and grades the
  track's MV.

## As built

Differences from the plan:

- **`nrs` Cargo feature** (default on, needs `player`) holds `nrs_pool` and
  `nrs_bridge`. `--no-default-features --features player` builds a player
  with no NRS code at all. The drawer and the shuffle mix treat the missing
  routes (404) like an unreachable NRS app.
- **Export ledger, not a column.** `play_event` has no NRS column. Exported
  seconds live in `play_export (play_id, target, exported_seconds)`, where
  `target` names the consumer (`"nrs"`). The play log stays NRS-free.
- **Play routes:** `POST /api/plays/{id}` (not PATCH, so `sendBeacon` can
  send the last report), and `GET /api/plays/last?since=` for the cooldown.
  The cooldown's "Clear" now resets a per-browser cutoff; the log is kept.
- **Bridge routes:** `POST /api/nrs/score` (by `nrs_id` or library
  `entry_id`, with `set` upserts and `add` one-offs), `POST /api/nrs/export`
  (run an export now), and a generic `/api/nrs/v1/*` passthrough in place of
  the per-feature `/visual` and `/related` proxies. A write through the
  passthrough marks the pool stale, since approving a suggestion can bind
  entries.
- **Export cadence:** every 2 minutes (first run 20 s after start), not
  after each finished play.
- **MV fallback:** when NRS has no video for the target (or the target isn't
  in NRS yet), the MV shown is the target's own YouTube source.
- **Fixing a fold:** `POST /api/relations/derive` (`MusicDb::set_edition_derivation`)
  writes a manual `derived_from` row with the chosen direction and
  transformation, and tombstones the other non-provider edition-kind rows
  between the two editions' identity components. Otherwise a heuristic row
  from a duplicate of the original keeps the old fold.
- **Library tab:** shows the edition group with what each edition scores
  as, and the fix form for the playing edition (not for the original). It
  has no relate/unrelate buttons; "Open in library" goes to the entry page,
  which has them.

- **Single-session player** (merged with
  `docs/plan-single-user-player.md`): only the active device logs plays,
  one per `track_seq` it mounts; a source switch or fallback keeps the play,
  and losing the active role finishes it. The drawer follows the session's
  current track on every device. Queue items carry an optional
  `release_id`, so a tracklist's album reaches the drawer from the server's
  queue.

Not built yet:

- Existing visual grades to compare against (`/visual-grades`).
- Soft-identity candidates in the Library tab.
