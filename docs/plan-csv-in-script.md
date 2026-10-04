# Plan: CSV diagnostics move into the match script

Status: implemented (2026-10-04). Deviation: the CSV helpers live in
`match.learned.rhai` itself, not a `csv.rhai` module, so `decide` doesn't go
through the module resolver on every pair.

## Why

`softmatch --csv` is written by Rust (`write_csv_row`), with columns shaped by
the heuristic script: `main_title_sim` and `markers_conflict` call script hooks
that `match.learned.rhai` stubs to 0/false, and `LazyFeatures` computes
duration/release/artist columns in Rust (`same_release` reads
`EntryInfo.track_positions`, which also holds non-release parents). The
learned model's own numbers (`p_same`, the class probabilities, the guard,
kind/direction) never reach the file, nor do a verdict's `origin`,
`model_version` or relation metadata.

The CSV becomes the script's business. Rust only offers a generic, shared
file handle; the script writes rows where it already has the verdict and its
diagnostics in hand.

## musiclib (generic)

1. **A shared file handle.** `file_create(path)` (truncate) and
   `file_append(path)` return a `File` value; `f.write(s)` and
   `f.write_line(s)` write to it. Underneath it is
   `Arc<Mutex<BufWriter<std::fs::File>>>`: the script opens it once in `init`
   and keeps it in `ctx`, the host's per-call copies of `ctx` copy only the
   `Arc`, and any number of scoring threads can write through it. Each call
   writes its whole string under the lock, so lines never interleave. The
   buffer is flushed when the last copy drops (end of run) and by an explicit
   `f.flush()`. Paths are relative to the working directory.
2. **Verdict maps reach `refine` intact.** `Scored` keeps the script's
   original map, and `refine`'s `item.verdict` is that map rather than one
   rebuilt from the parsed `Verdict`: extra keys `decide` attaches (e.g.
   `v.diag = #{p_same: …}`) survive into `refine`.
3. **Remove:** `SoftMatchConfig::csv_path`, `softmatch --csv`,
   `write_csv_row`, `csv_field`/`fmt_pairs_csv`, `LazyFeatures`, and the
   `main_title_sim` / `markers_conflict` hook calls. No `observe` hook: Rust
   has no CSV path left at all.

## Scripts

- `config/csv.rhai`: a small module (`field(v)` quoting, `row(values)`).
- `match.learned.rhai`:
  - `init`: when `env_var("MUSICLIB_MATCH_CSV")` is set, `file_create` it,
    write the header, keep the handle as `ctx.csv`. Unset → no handle, no
    rows, so online import is unaffected.
  - `decide` attaches `v.diag` with the head outputs it already computed (no
    second scoring call) and, for a DISTINCT verdict, writes the row itself:
    those pairs never reach `refine`. That is ~89% of rows, written in
    parallel.
  - `refine` writes the rows for the pairs it gets (every non-DISTINCT one),
    with the final verdict: Jev's answer where it asked, the model's
    otherwise.
  - Both go through one `emit_row(ctx, a, b, verdict)` helper. Columns:
    verdict, kind, confidence, reason, origin, model_version, metadata (JSON),
    type, both entries' id/title/sources, then the `diag` columns.
- `match.example.rhai`: drops its `main_title_sim` / `markers_conflict` CSV
  hooks.

## What the CSV no longer has

- **Pre-filter skips** (`SOFT_SAME`, `SOFT_BARRIER`, `BARRIER`): those pairs
  are dropped before `decide`, so the script never sees them. Their counts go
  into the run's log summary instead.
- **`candidate_channels`** (which retrieval channels found the pair): dropped;
  it only mattered for judging retrieval recall.
- **Row order** follows the scoring threads, not pair order.

## Order of work

1. `File` type + `file_create` / `file_append` / `write` / `write_line` /
   `flush`; a host test writing from several threads at once.
2. `Scored` keeps the raw map and `refine` gets it back; skip counts logged.
3. `csv.rhai` + `match.learned.rhai` (`diag`, `emit_row`, env var).
4. Remove the Rust CSV path, the flag and `LazyFeatures`; update
   `match.example.rhai`, `CONFIG_REFERENCE.md`, `DEV.md`.
5. Check: a dry run on the eval snapshot with `MUSICLIB_MATCH_CSV` set writes
   one row per scored pair, and the verdict counts match the run summary.
