# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
# Build
cargo build

# Run tests
cargo test

# Run a single test by name
cargo test <test_name>

# Update test fixtures (fetches real API data into JSON files)
cargo run --bin update_fixtures          # uses fixtures.yaml
cargo run --bin update_fixtures -- fixtures.yaml <filter>  # optional filter by path/url/backend

# Lint
cargo clippy
```

The `update_fixtures` binary requires a `YOUTUBE_API_KEY` env var (load via `.env`).

## Architecture

`musiclib-rs` is a music metadata aggregation library. It fetches and normalizes music data from multiple external sources (YouTube Data API, Spotify, MusicBrainz, Discogs, etc.) into a unified type system.

### Core abstractions (`src/providers/`)

- **`CanonicalizeProvider`** — given a raw URL, returns a `CanonicalizeResult` with a canonical identifier, entry type, and external type string.
- **`FetchProvider: CanonicalizeProvider`** — fetches a full `EntityResult` for a canonical identifier.
- **`RawFetchProvider: CanonicalizeProvider`** — lower-level; fetches raw `serde_json::Value` via a callback. Used by the `update_fixtures` binary to write test fixtures to disk.
- **`TryDefault`** — fallible constructor (reads credentials from env vars).

`EntityResult` holds: release date, `ExternalSources` (a `HashMap<source_name, HashSet<identifier>>`), entry-type-specific data (`EntrySpecificData`), child references, contributions (artist roles), and aliases.

### Backend implementations (`src/providers/backends/`)

Each backend is a module (or file) implementing the provider traits. The only fully wired-up backend is `youtube_api`; others (`spotify`, `musicbrainz`, `discogs`, `nicovideo`, `local`, `soundcloud`, `ytdlp`) are stubs.

`youtube_api::Provider` wraps a `YoutubeClient` and dispatches on URL pattern (`match_video_url` / `match_playlist_url` / `match_channel_url`). It needs `YOUTUBE_API_KEY` in the environment.

### HTTP layer (`src/http/`)

`HttpClient` trait with a single `make_request` method. The default impl uses `reqwest`. There is also a `MockHttpClient` (test-only) that maps `(Method, url)` → pre-canned `Response`.

`BodyExtractor` decouples response deserialization from transport: `bytes`, `text`, `json<T>`, and `auto` (content-type sniffing) variants are provided.

### HTTP cache (`src/httpcache/`)

`HttpCache` trait with `get`/`set` keyed by a cache-key string. Two impls: `MemoryHttpCache` (in-process) and `DbHttpCache` (SQLite via SeaORM). The cache layer is not yet wired into `default_http_client()`.

### Test fixtures

Checked-in JSON files under `src/providers/backends/youtube_api/` are the recorded API responses used by unit tests. `fixtures.yaml` maps URLs → file paths and backend names. Re-run `update_fixtures` to refresh them when the API schema changes.
