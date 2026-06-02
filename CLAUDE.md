# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
# Build / test / lint
cargo build
cargo test
cargo test <test_name>     # run a single test
cargo clippy

# Refresh checked-in API fixtures from real APIs (needs creds in .env)
cargo run --bin update_fixtures
cargo run --bin update_fixtures -- fixtures.yaml <filter>   # filter by path/url/backend

# End-to-end import (see import.sh for the canonical invocation)
RUST_LOG=musiclib_rs=info,import=info cargo run --bin import -- <url> \
  --db musiclib.db \
  --fetch-options fetch_options/fetch_discography.yaml \
  --http-config .temp/http.yaml \
  --registry-config .temp/providers.yaml

# MusicBrainz local mirror (URL table only)
cargo run --release --bin mb_extract_urls -- <mbdump.tar.bz2|-> -d mb_mirror.db
cargo run --bin mb_sync_replication -- -d mb_mirror.db   # incremental catch-up
```

A `flake.nix` provides a dev shell with rustc/cargo/clippy/sqlite + pre-commit hooks (rustfmt, nixfmt, statix, EOL/whitespace fixers).

Credentials live in `.env` (gitignored). The relevant keys are read via the `Credential::Env` defaults in `src/providers/registry.rs`: `YOUTUBE_API_KEY`, `SPOTIFY_CLIENT_ID`/`SPOTIFY_CLIENT_SECRET`, `MUSICBRAINZ_TOKEN`, `MUSICBRAINZ_BASE_URL`, `MUSICBRAINZ_MIRROR_DB`, `DISCOGS_USER_TOKEN`, `YTDLP_SERVER_URL`, `YTMUSICAPI_SERVER_URL`, plus `METABRAINZ_ACCESS_TOKEN` for replication.

## Architecture

`musiclib-rs` aggregates music metadata from multiple external sources (YouTube Data API, Spotify, MusicBrainz, Discogs, SoundCloud, NicoVideo, local files, yt-dlp) into a unified type system, and imports it into a SQLite "music library" DB. The library half lives under `src/` (consumed via `lib.rs`); driver binaries are in `src/bin/` (declared in `Cargo.toml` — `autobins = false`).

### Provider model (`src/providers/`)

The central abstractions in `providers/mod.rs`:

- **`CanonicalizeProvider`** — given `(source_key, identifier)`, returns `Option<CanonicalizeResult>` with a canonical `(source_key, identifier)` plus an `external_type` label (e.g. `"youtube:video"`, `"musicbrainz:recording"`). Backends return `None` for shapes they don't own. Note: `source_key` describes the *format* (`"unknown_url"`, `"isrc"`), distinct from `external_type` which names the entity kind.
- **`FetchProvider: CanonicalizeProvider`** — `fetch_entry` returns an `EntityResult`; `resolve_external_source` lets a backend enrich an `ExternalSources` map with IDs in its own namespace (used for cross-provider linking, e.g. ISRC → Spotify).
- **`RawFetchProvider`** — used only by `update_fixtures` to capture raw JSON payloads.

`EntityResult` (`providers/types.rs`) holds release date, `ExternalSources` (`HashMap<source, HashSet<identifier>>`), `EntrySpecificData` (Track/Release/ReleaseGroup/Artist), aliases, and **child sources**.

#### Children are streams, not vectors

Children are exposed as `ChildSource<T>: Stream<Item = Result<(ChildRef, T), Error>>`. The trait extends `futures::Stream` so combinators (`buffer_unordered`, etc.) work directly. Three concrete impls in `types.rs`:

- `VecChildSource` — adapts a `Vec<ChildRef>`.
- `PaginatedChildSource` — lazily walks API pagination via a `PageFetcher`.
- `CachedChildSource` — read-through buffer over another source, allowing multiple cursors to replay/extend the same stream.

`ChildSource` also exposes `evaluate_expr(&CompiledMatcherExpr) -> Tribool` for **static analysis** of matcher expressions against pending items without consuming them — used by the matcher system for early-exit in quantifier checks.

#### Fetch-options DAG

`EntryFetchOptions` is a list of `ChildRule { matcher, options_id }`. Options live in a flat arena `EntryFetchOptionsPool` and reference each other by `OptionsId`, so rules can recurse (e.g. `main → main`) without `Arc` cycles. Entry 0 is the no-rules sentinel.

The human-facing format is YAML, parsed in `providers/fetch_options_yaml.rs`: a flat map of named option sets where `main` is the root and other keys are reusable. A two-pass deserializer pre-allocates slot IDs so named sets can reference themselves before their body is parsed (the `EntryFetchOptionsPool::patch` API exists for this). See `fetch_options/fetch_discography.yaml` for a worked example, including the `./other.yaml::name` cross-file reference syntax.

Matchers (`ChildMatcher` / `ChildMatcherExpr`) are config-shaped; before evaluation they're compiled to `CompiledChildMatcher` / `CompiledMatcherExpr` with regexes pre-compiled and `All`/`Any` arms sorted cheapest-first. Evaluation uses **Tribool** (True/False/Indeterminate) Kleene logic so matchers that depend on un-fetched entity data (duration, YouTube description) can still drive early-exit decisions.

### Backends (`src/providers/backends/`)

Each backend module mirrors the API entity it wraps (e.g. `musicbrainz/{artist,recording,release,release_group,url,isrc}.rs`). All seven backends — `youtube_api`, `spotify`, `musicbrainz`, `discogs`, `soundcloud`, `nicovideo`, `local` — are wired into the registry; `ytdlp.rs` is a single-file helper used by SoundCloud/NicoVideo for stream URL resolution.

**MusicBrainz mirror.** `MusicBrainzConfig::mirror_db` points at a SQLite file produced by `mb_extract_urls` (initial bootstrap from an `mbdump.tar.bz2`) and kept fresh by `mb_sync_replication` (which calls into `src/replication.rs` to apply dbmirror v2 packets to the local `musicbrainz.url` table). When set, `lookup_url` skips the network round-trip for URLs the mirror doesn't know about — the common no-match case.

### Registry (`src/providers/registry.rs`)

`RegistryConfig` is the YAML config consumed by `build_providers(&config, http) -> Vec<Arc<dyn FetchProvider>>`. Backends are `Option<Config>`: omit → defaults (reads from env), `~` → disabled. `Credential` is either a literal string or `{ env: "VAR_NAME" }`. Backends with missing credentials are skipped with a warning, not an error.

`canonicalize(raw)` and `normalize(raw)` are top-level helpers that try every backend canonicalizer in priority order (see `all_canonicalize_providers()`); used by the importer to seed `(source_key, identifier)` pairs from arbitrary URLs.

### HTTP stack (`src/http/`)

`HttpClient::make_request(req, body_extractor)` is the single transport entry point. `BodyExtractor` decouples deserialization from transport (`bytes`/`text`/`json<T>`/`auto`).

`HttpClientConfig::build()` composes a layered client, innermost → outermost:

```
DefaultHttpClient → DomainScheduler → DB cache (opt) → Memory cache (opt)
```

Cache hits at an outer layer bypass scheduling. `DomainScheduler` (`http/scheduler.rs`) enforces per-domain concurrency and a two-phase retry on 429 (and per-domain extra statuses like MusicBrainz's 503): phase 1 honours `Retry-After` for N attempts, phase 2 is exponential backoff. The special host key `"*"` is the default applied to unlisted domains.

`HttpCache` (`src/httpcache/`) has two impls: `MemoryHttpCache` (in-process) and `DbHttpCache` (SQLite/SeaORM). Cache keys default to `"{METHOD}:{url}"` but can be overridden per-request via `Request::cache_key`.

### Importer (`src/bin/import/`)

The importer is a structured-concurrency traversal that turns a starting URL into a populated `musiclib.db`. Identity throughout is the `Pair = (source_key, identifier)`. Per-pair operation in `importer.rs::import`:

1. Canonicalize via the first matching provider.
2. Record an `is_rel` edge from input pair → canonical pair if they differ.
3. **Claim** the canonical pair (`State::claim`); losing claimers exit early but the `is_rel` above is preserved.
4. `fetch_entry`.
5. Store `PairMetadata` (entry_type, release_date, extra, specific_data, aliases) on the canonical pair.
6. Fixed-point cross-link loop: ask every other provider to `resolve_external_source` until no new IDs are added. Providers that already own a namespace in `sources` are skipped. HTTP-layer caching makes repeated calls cheap.
7. Recurse into every other source in the result (linked via `is_rel`) and every child (linked via `has_rel`, primary child pair chosen from `flatten_pairs`, siblings linked via `is_rel`).
8. `join_all` on all spawned subtasks — no detached work.

`flush.rs` persists the collected `State` (metadata, `is_rel`, `has_rel`) into the SeaORM schema defined in `src/musicdb/mod.rs`. That schema is **pair-centric**: all metadata, aliases, child edges, and contributions are keyed by `(source, identifier)`; the `entry` table is just a grouping primitive whose `id` is referenced from `entry_source.entry_id`. A DB-level entry merge therefore only needs to re-point `entry_source` rows.

`progress.rs` wraps the `HttpClient` in an `indicatif` progress bar and routes `tracing` output through `MultiProgressMakeWriter` so log lines don't tear the bars.

### Tests and fixtures

Each backend ships checked-in JSON fixtures next to its source (e.g. `youtube_api/channel_watame.json`). Tests use `test_utils::MockHttpClient` (`(Method, url) → Response` map) to replay these. `fixtures.yaml` is the manifest the `update_fixtures` binary uses to re-record from real APIs when an upstream schema changes.

## Notes

- The repo currently has uncommitted state: `CLAUDE.md` is staged for deletion and there's an untracked `OLD_CLAUDE.md` retained as a reference snapshot.
- `import.sh` is a convenience wrapper — it deletes `musiclib.db` and re-runs the importer against a fixed MusicBrainz artist with `.temp/http.yaml` + `.temp/providers.yaml` config paths.
- Edition: 2024.
