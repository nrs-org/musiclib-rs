# Plan: a jobs view for every server job

Status: **agreed** 2026-10-26 (decisions at the end). Comes before merging `feat/update-roots`;
scheduling (plan-update-roots.md, last phase) follows it and plugs into it.

## Problem

Jobs (`src/bin/server/jobs.rs`) are a process-local map keyed by id: an
ingest, the library update, the source finder's `attach_source`, a
store-protocol ingest from NRS. The player sees a job only if the page that
started it keeps polling it:

- there's no way to list jobs, so another tab's import, an NRS ingest or a
  running update are invisible;
- a page reload forgets every job it was tracking;
- a finished job's record lives until the server restarts, then is gone, so
  there's no history ("what did last night's update do?");
- a job records only its `op`, not what it's about (which URL, which roots),
  so even a list couldn't label it;
- the update job reports one stage for its whole run.

## Model

### Job records

Every job gets, on top of today's status/progress/result/error:

| field | |
|---|---|
| `label` | what it's about, for people: "Import https://…", "Library update: 52 roots", "Attach source … to …" |
| `trigger` | `manual` (the player), `schedule` (the scheduler, later), `store_protocol` (NRS) |
| `params` | the request as JSON (URL and fetch options; root ids; …) |
| `started_at`, `finished_at` | for durations |

### History

Finished jobs are kept in `musiclib.db` (`job_run`): label, trigger, params,
status, timestamps, result, error, and the last progress detail (counters
and the failure list, not the rolling activity log). A row is written when a
job starts and updated when it ends, so a job cut off by a restart shows as
`interrupted` rather than vanishing. Retention: the newest 500, or 90 days.

The in-memory map keeps running jobs only; a finished one is read back from
the table.

### API

- `GET /api/jobs?state=running|finished&limit=&before=`: summaries (no
  activity log), newest first; `finished` pages through history.
- `GET /api/jobs/{id}`: as now, for running and finished jobs alike.
- `GET /api/jobs/events` (SSE, like playback's): a summary whenever a job
  starts, reports progress (at most about once a second per job) or ends,
  so the view doesn't poll.
- `DELETE /api/jobs/{id}`: cancel, as now.

### Update job progress

The update reports where it is: the mirror sync (packet *i* of *n*), then
each preset run ("preset 2/3: vtuber_fetch_discography, 52 roots", with the
ingest counters and per-domain activity it has now), then the full-refresh
pass, plus each root's outcome as its run ends.

### Scheduled jobs (with the scheduling phase)

The jobs list gains `scheduled`: upcoming runs (`op`, label, `next_run_at`)
from the schedule settings; a run the scheduler starts is an ordinary job
with `trigger: schedule`. This plan only reserves the slot; the data comes
with the scheduler.

## UI

- A **Jobs** mode next to Library / Graph / Player / Import / Settings, with
  a running-jobs indicator (count and spinner) that is visible in every mode
  and opens it.
- The view has three sections:
  - **Running**: live cards (stage, message, counters, current activity,
    per-domain HTTP, Cancel);
  - **Scheduled**: next runs (empty until scheduling);
  - **History**: a list of when, kind, label, status, duration and a
    one-line summary; opening one shows its result, failures, the per-root
    outcomes of an update, and Log download.
- The import page keeps its form, and its "Recent jobs" list becomes a
  short view of the same records (this page's kinds, newest first) linking
  to the Jobs view, instead of its own client-side tracking.

## Phases

1. Job records: label/trigger/params/timestamps at every `spawn` site;
   `job_run` table, `interrupted` on startup; `GET /api/jobs`.
2. SSE stream.
3. Update job stages.
4. Jobs view + indicator; the import page switched over.

## Decisions (2026-10-26)

1. History lives in the DB (`job_run`).
2. A Jobs mode, with a running-jobs indicator in every mode.
3. Live updates are pushed (SSE).
4. Every job kind is shown: ingests (player and store protocol), updates,
   `attach_source`.
