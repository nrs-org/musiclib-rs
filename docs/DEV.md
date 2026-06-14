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
