# Developer Guide

## Prerequisites

### With Nix (recommended)

The repo ships a `flake.nix` that provides a fully-pinned dev shell with
rustc, cargo, clippy, sqlite, and all pre-commit hooks wired up.

```bash
nix develop          # enter the dev shell
# or, if you use direnv:
echo "use flake" > .envrc && direnv allow
```

The shell installs these tools:

| Tool | Purpose |
|---|---|
| `rustc` / `cargo` | Build and test |
| `clippy` | Lint |
| `sqlite` | Inspect the DB directly |
| `nixfmt` / `statix` | Nix file formatting and linting |
| `uv` | Python package manager (used by the ytdlp server) |

Pre-commit hooks run automatically on `git commit`:
`rustfmt`, `nixfmt`, `statix`, end-of-file fixer, trailing-whitespace trimmer.

### Without Nix

Install a recent stable Rust toolchain via [rustup](https://rustup.rs/), then:

```bash
# Linux — openssl and pkg-config are required at link time
sudo apt install pkg-config libssl-dev   # Debian/Ubuntu
sudo pacman -S pkgconf openssl           # Arch
```

On macOS, Xcode command-line tools are sufficient.

---

## Build and test

```bash
cargo build              # debug build
cargo build --release    # optimised build (use for mb_extract_urls)
cargo test               # full test suite
cargo test <test_name>   # single test
cargo clippy             # lint
```

---

## Credentials and config

Credentials are read from environment variables. Copy the example configs into
the platform config directory and fill them in:

```bash
# Linux
mkdir -p ~/.config/musiclib-rs
cp config/providers.example.yaml ~/.config/musiclib-rs/providers.yaml
cp config/http.example.yaml      ~/.config/musiclib-rs/http.yaml

# macOS
mkdir -p "$HOME/Library/Application Support/nrs-org/musiclib-rs"
cp config/providers.example.yaml "$HOME/Library/Application Support/nrs-org/musiclib-rs/providers.yaml"
cp config/http.example.yaml      "$HOME/Library/Application Support/nrs-org/musiclib-rs/http.yaml"
```

Then create a `.env` file (gitignored) in the repo root with the relevant keys:

```bash
# YouTube Data API — https://console.cloud.google.com/
YOUTUBE_API_KEY=

# Spotify — https://developer.spotify.com/dashboard
SPOTIFY_CLIENT_ID=
SPOTIFY_CLIENT_SECRET=

# MusicBrainz — https://musicbrainz.org/account/applications
MUSICBRAINZ_TOKEN=

# Discogs — https://www.discogs.com/settings/developers
DISCOGS_USER_TOKEN=

# yt-dlp HTTP server (see below)
YTDLP_SERVER_URL=http://localhost:3000

# ytmusicapi HTTP server (see below)
YTMUSICAPI_SERVER_URL=http://localhost:9001

# MusicBrainz mirror (created by mb_extract_urls, optional but recommended)
MUSICBRAINZ_MIRROR_DB=/path/to/mb_mirror.db

# MusicBrainz replication (required only for mb_sync_replication)
METABRAINZ_ACCESS_TOKEN=
```

`dotenv` is loaded automatically at startup; you do not need to source the file
manually.

### Platform config directories

Config, cache, and data directories follow the
[`directories`](https://crates.io/crates/directories) crate conventions
(`ProjectDirs` with qualifier `""`, organisation `"nrs-org"`, application
`"musiclib-rs"`):

| Directory | Linux | macOS |
|---|---|---|
| Config | `~/.config/musiclib-rs/` | `~/Library/Application Support/nrs-org/musiclib-rs/` |
| Cache | `~/.cache/musiclib-rs/` | `~/Library/Caches/nrs-org/musiclib-rs/` |
| Data | `~/.local/share/musiclib-rs/` | `~/Library/Application Support/nrs-org/musiclib-rs/` |

When no explicit `--http-config` / `--registry-config` / `--db` flag is passed,
binaries look in these directories and fall back to built-in defaults if the
file is absent.

---

## Binaries

All binaries accept `--help` for the full flag list.

### `import` — ingest a URL into the music library

```bash
RUST_LOG=musiclib_rs=info,import=info \
cargo run --bin import -- <url> \
  --fetch-options config/fetch_options/fetch_discography.yaml
```

| Flag | Default | Notes |
|---|---|---|
| `<url>` | — | Starting URL (any supported platform) |
| `--fetch-options` | none | Fetch-options YAML; bare filename resolved relative to `<config_dir>/fetch_options/` |
| `--db` | `<data_dir>/musiclib.db` | SQLite music library |
| `--http-config` | `<config_dir>/http.yaml` | HTTP client config |
| `--registry-config` | `<config_dir>/providers.yaml` | Provider credentials |
| `--dedup-config` | auto from `<config_dir>/dedup_barriers/` | Barrier file(s); repeatable |
| `--skip-dedup` | false | Skip the pre-import dedup pass |

`scripts/import.sh` is a convenience wrapper that sets `RUST_LOG` and pins a
fixed example URL + vtuber fetch-options.

### `dedup` — re-apply barrier configs without ingesting new URLs

```bash
RUST_LOG=musiclib_rs=info,dedup=info cargo run --bin dedup
```

Barrier files are auto-loaded from `<config_dir>/dedup_barriers/*.yaml`. Pass
`--dedup-config <path>` (repeatable) to override. See
[CONFIG_REFERENCE.md](CONFIG_REFERENCE.md) for the barrier file format.

`scripts/dedup.sh` is a convenience wrapper that sets `RUST_LOG`.

### `mb_extract_urls` — bootstrap the MusicBrainz URL mirror

```bash
# Auto-download the latest full export (recommended):
cargo run --release --bin mb_extract_urls

# From a local file:
cargo run --release --bin mb_extract_urls -- /path/to/mbdump.tar.bz2

# From stdin:
curl -L 'https://data.metabrainz.org/.../mbdump.tar.bz2' \
  | cargo run --release --bin mb_extract_urls -- -
```

Writes to `<data_dir>/mb_mirror.db` by default; override with `-d <path>`.
Run with `--release` — the bzip2 decompression is CPU-bound.

### `mb_sync_replication` — keep the URL mirror up to date

```bash
cargo run --bin mb_sync_replication
# or with an explicit DB path:
cargo run --bin mb_sync_replication -- -d /path/to/mb_mirror.db
```

Fetches and applies all pending replication packets from MetaBrainz in
sequence. Requires `METABRAINZ_ACCESS_TOKEN` in the environment. Run
periodically (e.g. daily via cron) after the initial bootstrap.

### `update_fixtures` — refresh checked-in API test fixtures

```bash
cargo run --bin update_fixtures
cargo run --bin update_fixtures -- fixtures.yaml <filter>   # filter by path/url/backend
```

Hits the real APIs and overwrites the JSON fixture files used by tests.
Requires live credentials. Run this when an upstream API schema changes and
tests start failing on deserialization.

### `softmatch` — heuristic soft-dedup

Score candidate entry pairs with a Rhai script and optionally persist the
results (the learned script also asks TypeSafe's Jev about the pairs it
DEFERs when `TYPESAFE_API_KEY` is set; see CONFIG_REFERENCE.md). Dry-run by
default. Every verdict, from any backend, is soft/reversible — MERGE writes
a `same_identity` assertion via the same path the player's manual "link"
button uses, never a destructive `merge_entries`; that stays exclusively an
import-time/dedup-barrier operation.

```bash
cargo run --features ffi --release --bin softmatch
cargo run --features ffi --release --bin softmatch -- --apply   # write RELATE + soft MERGE decisions to DB
MUSICLIB_MATCH_CSV=out.csv cargo run --features ffi --release --bin softmatch  # one CSV row per scored pair (written by match.learned.rhai)
```

| Flag | Default | Notes |
|---|---|---|
| `--db` | `<data_dir>/musiclib.db` | SQLite music library |
| `--script` | `<config_dir>/match.rhai` | Rhai match script |
| `--apply` | false | Persist RELATE and soft MERGE (`same_identity`) decisions to the DB |
| `--embed-db` | `<data_dir>/embeddings.db` | SQLite embedding cache (sqlite-vec) |
| `--no-embed` | false | Disable semantic blocking even if `embed_batch` is defined |
| `--embed-dim` | `256` | Embedding dimension; must match the model (`384` for MiniLM) |
| `--embed-k` | `20` | KNN neighbours per entry |
| `--embed-threshold` | `0.45` | Min cosine similarity to surface a KNN pair as a candidate |
| `--embed-max-pages` | `1` | Max KNN pages per entry type; larger values are recall experiments |
| `--candidate-max-block` | `50` | Largest lexical/structural posting list expanded into pairs |
| `--candidate-ngram-k` | `30` | Character-ngram neighbors retained per entry |
| `--verbose-decisions` | false | Print every suggested merge/relation and full sources |
| `--dedup-config` | auto from `<config_dir>/dedup_barriers/` | Barrier file(s); repeatable |
| `--registry-config` | `<config_dir>/providers.yaml` | Provider credentials |
| `--http-config` | `<config_dir>/http.yaml` | HTTP client config |

The `--features ffi` flag is optional but required to load the native inference
cdylib (`config/inference/`). Without it the script falls back to the
dependency-free naive token-hash embedder. See [CONFIG_REFERENCE.md](CONFIG_REFERENCE.md)
for the `match.rhai` format.

Candidate retrieval is a union of bounded exact-name, token, character-ngram,
duration/credit, release-tracklist, and optional semantic-ANN channels. Import-time
soft-match is suggestion-only: it may warm embeddings and score focused pairs,
but does not write relationships or merge entries.

### `ytdlp_server` — local yt-dlp HTTP server

Required by the SoundCloud and NicoVideo backends.

```bash
YTDLP_PATH=yt-dlp PROVIDER_SERVER_ADDR=127.0.0.1:3000 \
cargo run --bin ytdlp_server
```

| Env var | Default | Notes |
|---|---|---|
| `YTDLP_PATH` | `yt-dlp` | Path to the yt-dlp executable |
| `PROVIDER_SERVER_ADDR` | `127.0.0.1:3000` | Listen address |

Point `YTDLP_SERVER_URL` at this server in your `.env`.

---

## Useful scripts

| Script | Purpose |
|---|---|
| `scripts/import.sh` | Run `import` with `RUST_LOG` set and a fixed example URL |
| `scripts/dedup.sh` | Run `dedup` with `RUST_LOG` set |
| `scripts/clear_yt_cache.sh [db]` | Delete all YouTube Data API entries from the HTTP cache DB (frees quota-counted cache) |

---

## Tests and fixtures

Each backend ships JSON fixtures next to its source (e.g.
`src/providers/backends/youtube_api/channel_watame.json`). Tests use
`test_utils::MockHttpClient` to replay these without hitting the network.

```bash
cargo test                        # run all tests
cargo test youtube                # filter by name
cargo test -- --nocapture         # show println! output
```

When an upstream API changes and fixtures go stale, run `update_fixtures` to
re-record them (needs live credentials), then re-run the test suite.

## Deduplication calibration data

The v2 deduplication design and labeling ontology live in
[`dedup-v2.md`](dedup-v2.md). Generate the first blind calibration batch from a
read-only music library snapshot with:

```bash
python3 scripts/export_dedup_calibration.py
python3 scripts/validate_dedup_ledger.py
python3 scripts/prepare_dedup_label_batches.py
```

The exporter writes ignored local artifacts under `data/`:

- `dedup-calibration.private.jsonl` contains sampling-frame, current-cluster,
  and proposer provenance for analysis;
- `dedup-calibration.blind.jsonl` is the only file that should be presented to
  annotators.

The blind projection physically omits the current matcher verdict, existing
human labels, source cluster IDs, sampling frame, and candidate-generator
metadata. The validator checks that it is exactly the private task with
`hidden` removed, verifies stable content hashes and unique evidence IDs, and
uses [`dedup-label-schema-v2.json`](dedup-label-schema-v2.json) when the optional
Python `jsonschema` package is installed.

For an entry-dedup dataset, keep `--masked-bridge 0`: masked source fragments
exercise source linking, not deduplication between library entries. The v3 pilot
was generated from the isolated playlist-expanded corpus with:

```bash
python3 scripts/export_dedup_calibration.py \
  --db data/dedup-corpus-v3.db \
  --playlist-db data/dedup-playlist-seed-v3.db \
  --private-out data/dedup-entry-pilot-v3.private.jsonl \
  --blind-out data/dedup-entry-pilot-v3.blind.jsonl \
  --manifest-out data/dedup-entry-pilot-v3.manifest.json \
  --exclude-task-ledger data/dedup-calibration.private.jsonl \
  --production 0 --masked-bridge 0 \
  --hard-confuser 160 --independent-miss 200 --uniform 40
```

The exporter aborts if a selected task does not contain two distinct current
entry IDs. Corpus origin is stratified as baseline, playlist anchor, or
expansion so a prolific discography seed cannot dominate the batch. The
resulting 400-item pilot is for candidate and annotation development, not a
representative production performance estimate. Only later pre-registered,
held-out evaluation exports should be used for headline metrics.

The labeling instructions are
[`dedup-label-prompt-v2.md`](dedup-label-prompt-v2.md). The batch preparer emits
JSON bundles of up to five tasks under ignored
`data/dedup-agent-batches/lane-a/`, with a deterministic randomized left/right
presentation and a default 50,000-character context budget. Prepare a
complementary second vote with the opposite presentation using:

```bash
python3 scripts/prepare_dedup_label_batches.py \
  --lane b --invert-sides --shuffle \
  --output-dir data/dedup-agent-batches/lane-b
```

Concatenate each annotator's JSONL responses and validate them before analysis:

```bash
python3 scripts/validate_dedup_annotations.py data/dedup-annotations.jsonl

# Or validate a sharded full pass with exact task coverage:
python3 scripts/validate_dedup_annotations.py --require-complete \
  --batches data/dedup-agent-batches/lane-a \
  data/dedup-agent-labels/luna-low-a
```

The accepted two-pass Luna calibration annotations are stored locally in
`data/dedup-agent-labels/luna-low-a/` and `luna-low-b/`. Each directory contains
exactly one schema-validated vote for all 200 tasks. Like the source task
ledgers, these generated artifacts are ignored by Git.

The 40 disagreements have a blind third vote and an append-only adjudication
ledger. MusicBrainz and provider-page enrichment were followed by authoritative
project-owner review. The effective result is 39 resolved items, no outstanding
evidence requests, and one explicitly excluded contaminated comparison. The
effective ledger is `data/dedup-adjudications.final.jsonl`; validate it with:

```bash
python3 scripts/validate_dedup_adjudications.py \
  data/dedup-adjudications.final.jsonl \
  --tasks data/dedup-calibration.blind.jsonl \
  --annotations data/dedup-agent-labels/luna-low-a \
    data/dedup-agent-labels/luna-low-b \
    data/dedup-adjudication/third-pass-astra
```

Generate a self-contained, searchable review page. The renderer also supports
unlabeled task ledgers, so the v3 pilot can be inspected before annotation:

```bash
python3 scripts/render_dedup_review.py
xdg-open data/dedup-review.html

python3 scripts/render_dedup_review.py \
  --tasks data/dedup-entry-pilot-v3.private.jsonl \
  --lane-a data/no-labels-v3-a --lane-b data/no-labels-v3-b \
  --adjudications data/no-adjudications-v3.jsonl \
  --output data/dedup-entry-pilot-v3.review.html
```
