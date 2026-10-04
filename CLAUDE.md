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
RUST_LOG=musiclib_rs=info,import=info cargo run --bin import -- <url>... [--url-file urls.txt] \
  --fetch-options config/fetch_options/fetch_discography.yaml

# Re-apply dedup barrier to an existing DB (no new URLs ingested)
RUST_LOG=musiclib_rs=info,dedup=info cargo run --bin dedup

# MusicBrainz local mirror (URL table only)
cargo run --release --bin mb_extract_urls -- <mbdump.tar.bz2|-> -d mb_mirror.db
cargo run --bin mb_sync_replication -- -d mb_mirror.db   # incremental catch-up
cargo run --bin mb_apply_replication -- replication-<N>-v2.tar.bz2   # apply one packet
cargo run --bin mb_apply_replication -- --undo replication-<N>-v2.tar.bz2  # undo last packet
```

A `flake.nix` provides a dev shell with rustc/cargo/clippy/sqlite + pre-commit hooks (rustfmt, nixfmt, statix, EOL/whitespace fixers).

### App directories

Default paths follow XDG / OS conventions via the `directories` crate (`src/app_dirs.rs`):

| Purpose | Function | Typical Linux path |
|---|---|---|
| Config files | `app_dirs::config_dir()` | `~/.config/musiclib-rs/` |
| Cache | `app_dirs::cache_dir()` | `~/.cache/musiclib-rs/` |
| Data / DB | `app_dirs::data_dir()` | `~/.local/share/musiclib-rs/` |

Default paths for each binary:

| Binary | Default path |
|---|---|
| `import` / `dedup` — music DB | `<data_dir>/musiclib.db` |
| `mb_extract_urls` / `mb_apply_replication` — MB mirror | `<data_dir>/mb_mirror.db` |
| `providers.yaml` | `<config_dir>/providers.yaml` |
| `http.yaml` | `<config_dir>/http.yaml` |
| fetch-options (name only) | `<config_dir>/fetch_options/<name>` |
| dedup barriers (auto-loaded) | `<config_dir>/dedup_barriers/*.yaml` |

Copy `config/providers.example.yaml` → `<config_dir>/providers.yaml` and `config/http.example.yaml` → `<config_dir>/http.yaml`, then edit. The relevant env vars are: `YOUTUBE_API_KEY`, `SPOTIFY_CLIENT_ID`/`SPOTIFY_CLIENT_SECRET`, `MUSICBRAINZ_TOKEN`, `MUSICBRAINZ_BASE_URL`, `MUSICBRAINZ_MIRROR_DB`, `DISCOGS_USER_TOKEN`, `YTDLP_SERVER_URL`, `YTMUSICAPI_SERVER_URL`, plus `METABRAINZ_ACCESS_TOKEN` for replication.

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

The human-facing format is YAML, parsed in `providers/fetch_options_yaml.rs`: a flat map of named option sets where `main` is the root and other keys are reusable. A two-pass deserializer pre-allocates slot IDs so named sets can reference themselves before their body is parsed (the `EntryFetchOptionsPool::patch` API exists for this). See `config/fetch_options/fetch_discography.yaml` for a worked example, including the `./other.yaml::name` cross-file reference syntax.

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
DefaultHttpClient → DomainScheduler → Coalescer (opt) → DB cache (opt) → Memory cache (opt) → Activity layer
```

Cache hits at an outer layer bypass scheduling and batching.

`Coalescer` (`http/coalescer.rs`) merges single-entity requests matched by per-backend `CoalesceRule`s (YouTube `videos`/`channels`/`playlists.list`, Spotify tracks/albums/artists) into batched calls. Full batches fire immediately; partial batches fire only when `http::Activity` (`http/activity.rs`) reports the process idle, i.e. every piece of running work is parked in a coalescer queue. Traversal roots must be wrapped in `Activity::global().track(..)` (the pipeline does this) so CPU work between awaits isn't mistaken for idleness. `DomainScheduler` (`http/scheduler.rs`) enforces per-domain concurrency and a two-phase retry on 429 (and per-domain extra statuses like MusicBrainz's 503): phase 1 honours `Retry-After` for N attempts, phase 2 is exponential backoff. The special host key `"*"` is the default applied to unlisted domains.

`HttpCache` (`src/httpcache/`) has two impls: `MemoryHttpCache` (in-process) and `DbHttpCache` (SQLite/SeaORM). Cache keys default to `"{METHOD}:{url}"` but can be overridden per-request via `Request::cache_key`.

### Import pipeline (`src/pipeline/`)

Shared by the `import` and `dedup` binaries. Modules:

- `importer.rs` — structured-concurrency traversal (see below).
- `state.rs` — in-memory `State` accumulator; `claim` prevents duplicate processing.
- `flush.rs` — persists `State` into SeaORM/SQLite.
- `dedup.rs` — re-imports already-stored entities shallow (no new URLs) to re-apply the dedup barrier.
- `progress.rs` — wraps `HttpClient` in `indicatif` bars; routes `tracing` through `MultiProgressMakeWriter`.

#### Dedup barriers (`pipeline/dedup.rs`)

A barrier declares that two (or more) entities must never be merged into one DB entry even if the importer would normally unify them (e.g. two tracks sharing an ISRC). Configuration lives in `DedupConfig` (loaded from `<config_dir>/dedup_barriers/*.yaml`, or overridden via `--dedup-config`).

```yaml
groups:
  - name: "human label (logs only)"
    anchors:
      anchor_a:
        members:    # canonical pairs that define this entity
          - "spotify:track:abc"
          - "isrc:USRC12345678"
        claim:      # ambiguous pairs that belong exclusively to this anchor
          - "isrc:USRC12345678"
      anchor_b:
        members:
          - "spotify:track:xyz"
```

Each string is `source:identifier` (split on the first `:`). Pairs are canonicalized at load time so any URL form works. Two pairs assigned to different anchors within the same group can never share an entry; the `dedup` binary re-runs the barrier against an existing DB without ingesting new URLs.

### Importer (`src/pipeline/importer.rs`)

The importer is a structured-concurrency traversal that turns a starting URL into a populated `musiclib.db`. Identity throughout is the `Pair = (source_key, identifier)`. Per-pair operation:

1. Canonicalize via the first matching provider.
2. Record an `is_rel` edge from input pair → canonical pair if they differ.
3. **Claim** the canonical pair (`State::claim`); losing claimers exit early but the `is_rel` above is preserved.
4. `fetch_entry`.
5. Store `PairMetadata` (entry_type, release_date, extra, specific_data, aliases) on the canonical pair.
6. Fixed-point cross-link loop: ask every other provider to `resolve_external_source` until no new IDs are added. Providers that already own a namespace in `sources` are skipped. HTTP-layer caching makes repeated calls cheap.
7. Recurse into every other source in the result (linked via `is_rel`) and every child (linked via `has_rel`, primary child pair chosen from `flatten_pairs`, siblings linked via `is_rel`).
8. `join_all` on all spawned subtasks — no detached work.

`flush.rs` persists the collected `State` (metadata, `is_rel`, `has_rel`) into the SeaORM schema defined in `src/musicdb/mod.rs`. That schema is **pair-centric**: all metadata, aliases, child edges, and contributions are keyed by `(source, identifier)`; the `entry` table is just a grouping primitive whose `id` is referenced from `entry_source.entry_id`. A DB-level entry merge therefore only needs to re-point `entry_source` rows.

### Tests and fixtures

Each backend ships checked-in JSON fixtures next to its source (e.g. `youtube_api/channel_watame.json`). Tests use `test_utils::MockHttpClient` (`(Method, url) → Response` map) to replay these. `fixtures.yaml` is the manifest the `update_fixtures` binary uses to re-record from real APIs when an upstream schema changes.

## Notes

- `scripts/import.sh` is a convenience wrapper — it re-runs the importer against a fixed example URL with vtuber fetch options, relying on the default config directory for `http.yaml` and `providers.yaml`.
- `scripts/dedup.sh` and `scripts/clear_yt_cache.sh` are similar convenience wrappers.
- Edition: 2024.
- `docs/BATCHING_PROVIDERS.md` is the original design doc for the coalescer; its firing rule section is superseded (see the status note at its top).
- `docs/CONFIG_REFERENCE.md` has full annotated examples for `providers.yaml`, `http.yaml`, fetch-options files, and dedup barrier files.
- `docs/DEV.md` covers dev environment setup (Nix and non-Nix paths).
- GPU title encoder: `cargo build -p inference --release --lib --features vulkan` (inside the dev shell, which provides nixpkgs' `llama-cpp-vulkan`) runs semantic-blocking embeddings through llama.cpp on a discrete Vulkan GPU, using the bundle's `encoder/model-f16.gguf` (`train/learned-matcher/gguf_export.py`, called by `bundle.py`). Pair-feature vectors always stay on the exact CPU encoder: llama.cpp's tanh GELU/f16 kernels flip ~0.3% of parity verdicts. `MUSICLIB_ENCODER=cpu` disables the GPU.
- Learned soft-dedup: `config/match.learned.rhai` binds the `inference` cdylib's `matcher` feature (LightGBM + title encoder; bundle from `train/learned-matcher/bundle.py`) and keeps the policy (thresholds) in the script. Scripts get extra per-pair DB data from `pair_facts_json(...)` (`src/pipeline/pair_facts.rs`), not from new `EntryInfo` fields; entries reach scripts as a lazy `Entry` type. Plan + parity harness: `docs/plan-v15-runtime.md`.
