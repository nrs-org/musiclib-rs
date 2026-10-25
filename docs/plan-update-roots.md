# Plan: update roots managed in the player

Status: **agreed** 2026-10-26 (decisions at the end). Builds on [plan-update.md](plan-update.md),
which this partly reverses: there, roots stayed external (`update_roots/*.txt`
files read by `scripts/update.sh`, "no roots table"). Here the player owns
them.

## Goal

The library update is driven from the web UI:

- register **roots** (a URL plus the fetch options to walk it with), list
  them, edit, remove;
- **preview** a root's filtering before adding it: which of its playlists
  (and other children) its fetch options take or skip, and why;
- **run the update** (every enabled root, or one) as a server job with
  progress, the way ingest runs now;
- later, the server **schedules** the update itself (daily), replacing cron.

## What exists

- `POST /api/ingest` (`src/bin/server/player.rs`) already takes a URL and a
  fetch-options file + entry point (`/api/fetch-options/files`,
  `/entry-points`) and runs `pipeline::ingest::ingest_entries` as a job with
  live progress (`jobs.rs`, `ingest_progress.rs`).
- The server **doesn't use listing snapshots**: `ListingStore` is only wired
  into the `import` binary (`src/bin/import/main.rs`). Server ingests read
  growing listings through the plain HTTP cache, so they never see new items.
- `ingest_entries` has a `max_fetches` cap and crash-safe `import_run`s.
- The MB mirror check (`mb_freshness::check_before_import`) and the sync
  (`mb_sync_replication`) are CLI-side.

## Model

### Roots table (in `musiclib.db`)

| column | |
|---|---|
| `id` | |
| `url` | as entered; canonicalized on save, the canonical pair stored too and unique: one root per pair, with one set of fetch options |
| `fetch_options_file`, `entry_point` | the ids the ingest form already uses |
| `label` | optional; defaults to the root entry's display name after its first run |
| `enabled` | disabled roots are kept but skipped by updates |
| `added_at` | |
| `last_run_at`, `last_status`, `last_stats` | outcome of the last update that ran it: ok / failed, error, and a small JSON (entries added, listings refreshed, new items, YouTube units) |

Fetch options stay files, referenced by name, as everywhere else. If a file
or entry point disappears, the root shows as broken and updates skip it.

**Removing a root only stops updating it.** What it imported stays: updates
are add-only, and the same entries are usually reachable from other roots.
(Removing content belongs with identity retraction, not here.)

### Update job

`POST /api/update` (optionally `{root_ids}`) starts one job, and only one at
a time:

1. MB mirror: sync it up to the server's sequence (never past it), with the
   lag check, reusing `mb_sync_replication`'s code as a library function.
   The mirror is ours, so a scheduled update needs nothing outside the
   server. Refuse to go on if the server is too far behind or the mirror is
   ahead.
2. Group enabled roots by (file, entry point) and run `ingest_entries` per
   group with a `ListingStore` in `Auto` (refresh) mode and the day's quota
   budget, exactly like `update.sh` does per roots file.
3. Full-refresh pass with the leftover budget.
4. Write each root's `last_*` columns; the job result carries the listing
   summary (with the "no earlier copy" count) and the quota used.

Manual ingest gets a "refresh listings" checkbox (the `--refresh <url>`
equivalent) once the server has a `ListingStore`; without it, ingest serves
snapshots (`Cached` mode), so it sees what the last update saw.

### Preview: filtering

`POST /api/roots/preview {url, fetch_options}` fetches the root and reads its
**direct children** (for a channel: its playlists, and its uploads only if a
rule would read them), and for each child reports the rule that decided it:
fetched with which options set, stubbed, or skipped. For the vtuber options
that is the playlist whitelist/blacklist by title, the main thing worth
checking before adding a channel. Children whose decision depends on
unfetched data (a duration rule) show as undecided rather than being
fetched.

It writes nothing to the library, and it costs what reading the root's
listings costs (a few units for a channel); the responses go into the HTTP
cache, so adding the root afterwards reuses them.

A deeper dry run (walk the whole graph without flushing, with fetch and
quota caps, reporting fan-out and failures) is deferred.

### Scheduling (later)

A setting for the daily update time; the server starts the update job then,
if none is running. Out of scope until the rest works; cron + a CLI call does
the job meanwhile.

## Phases

1. **Roots table + API**: migration, `GET/POST/PATCH/DELETE /api/roots`,
   canonicalize on save. A one-off import of the existing
   `update_roots/*.txt` files.
2. **Server listing store**: build a `ListingStore` and quota ledger in
   `AppState` (shared with the HTTP cache DB, as in `import`); ingest uses
   `Cached` mode, plus the refresh checkbox.
3. **Update job**: the steps as a library function (MB sync, walk, pass,
   per-root results), run by `POST /api/update` and by `import --update`;
   `update.sh` and the roots files are removed.
4. **Preview**: the filtering report above.
5. **UI**: a Roots panel next to the ingest form: list with last-run status,
   add (with preview), edit options, enable/disable, remove, "update now",
   job progress reusing the ingest job view.
6. **Scheduling**, later.

## Decisions (2026-10-26)

1. Roots live in the DB. The CLI reads them too (`import --update`, which
   runs the update job's steps), and `scripts/update.sh` with
   `update_roots/*.txt` goes away after phase 1's one-off import.
2. The update syncs the MB mirror itself (step 1 above).
3. Preview means the filtering report; a capped full dry run is for later.
4. One root per URL: no second root with other fetch options.
