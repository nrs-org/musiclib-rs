# Plan: single-user player with remote control

Status: implemented (server `src/bin/server/playback.rs`, frontend `Remote`
module + reworked `Player` in `frontend/index.html`).

## Why

The Spotify Web Playback SDK is one-user: an account has one playback
session, and any Connect device can be told to play it. The web player
used to keep its queue entirely in each browser tab, so two tabs (or a
phone and a desktop) were two unrelated players. Make the musiclib player
behave the same way Spotify does: **one playback session per server**, played
by at most one **active device**, with every other open tab acting as a
remote control.

## Model

- The server owns the session (`PlaybackHub`): the queue (items, play
  order, Queue/Post-queue split, shuffle, repeat), the current track's
  `track_seq`, the chosen source, desired `playing`, position, `seek_seq`,
  volume, and the device list.
- A **device** is one browser tab. It subscribes to
  `GET /api/playback/events?device_id&name` (SSE). Connected streams *are* the
  device list; a stream closing removes the device. The id lives in
  `sessionStorage`, taken out on load and put back on `pagehide`, so a
  reload keeps its id but a duplicated tab gets a fresh one.
- Exactly one device (or none) is **active**. Only the active device mounts
  an embed; the rest show a "Playing on …" card and the same bar, driven
  by the server snapshot.
- Every control (bar buttons, library play/queue buttons, seek, volume,
  source switcher, device picker) is a **command**:
  `POST /api/playback/command`. Commands that start playback with no active
  device make the sender active (Spotify's "play here when nothing is
  playing").
- The active device **reports** reality (`POST /api/playback/report`):
  playing/paused, position, duration, mounted source, and `ended`.
  Reports carry `track_seq`, and a stale one is ignored. `ended` advances
  the queue on the server, so the same rule applies whoever is watching.
- State flows back as full snapshots over SSE (`tokio::sync::watch`, latest
  wins). The active device reconciles: it remounts when `track_seq` or
  `source` changes, seeks when `seek_seq` changes, and plays or pauses
  only when the server's `playing` flag *changes*. Edge-triggering avoids
  undoing a pause that the user made in the embed itself before the report
  arrived.
- Position is `(position_ms, playing)` as of the moment the snapshot was
  sent. Remotes interpolate from the time they receive it, so there is no
  clock-skew problem. Periodic progress reports are only rebroadcast when
  they drift from the extrapolation.
- "Shuffle all" and the auto-recommend top-up are shuffle-mix batches
  (`ShuffleMix`: familiar NRS-scored tracks plus new ones), generated in
  the browser because the mix's settings and play log are per-browser.
  "Shuffle all" sends its batch with `shuffle_library`, which plays it in
  the arranged order with shuffle mode off. For the top-up, the snapshot
  sets `wants_recommend` while fewer than three tracks are ahead (repeat
  off), and the active device answers with `recommend`; the server drops a
  batch that arrives when it is no longer needed. A track that ends with
  nothing after it waits for that batch, then advances.
- When the active device disconnects, playback stops (paused, position
  kept). If the same device id reconnects (a reload), it becomes active
  again in the paused state.
- The session (queue, position, volume, last active device) is persisted to
  `<data_dir>/playback.json`, debounced, and survives restarts paused.

## Video is a view, not a device

Spotify Connect semantics apply to *audio* only. Every non-active device
whose dock is showing (and tab visible) mounts a **muted, controls-free
mirror** of the active device's YouTube video and steers it to the session
position: play/pause to match, re-seek past 0.75s drift (2s cooldown so a
seek can land). The server rebroadcasts progress reports that drift more
than 0.5s from its extrapolation so mirrors stay close. YouTube only:
NicoNico's embed can't be muted, so a mirror of it would be a second audio
device. Sources with no video (Spotify, SoundCloud, preview-only embeds)
show the remote card instead.

## Per-device vs shared

- Shared (server): queue, shuffle/repeat, position, volume (global +
  per-provider balance, mute).
- Per-device (localStorage): source-provider preference, Spotify
  connection, and the shuffle mix's settings and play log. Every device
  notes a play when the session moves to a new track. The active device chooses the source unless one was picked
  explicitly with the switcher, because only it knows what it can play.

## Known limits

- Browser autoplay: transferring to a tab that has had no user gesture may
  leave the embed paused until "play" is pressed there.
- One session for everyone who can reach the server. There are no
  accounts; that is the point.
