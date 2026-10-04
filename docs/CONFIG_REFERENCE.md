# Configuration Reference

`musiclib-rs` uses two YAML config files, a family of fetch-options YAML files,
and a directory of dedup barrier files. The canonical examples live in `config/`.

---

## `providers.yaml`

Controls which API backends are enabled and what credentials they use.

### Credential values

Every credential field accepts either a literal string or an environment-variable reference:

```yaml
api_key: "my-literal-value"
api_key: { env: "MY_ENV_VAR" }
```

When the environment variable is unset the credential resolves to `None`. Backends
with missing required credentials are skipped with a warning, not a hard error.

### Disabling a backend

Set any top-level backend key to `~` (YAML null):

```yaml
discogs: ~       # disabled
nicovideo: ~
```

Omitting the key entirely uses the default config (credentials read from the
standard environment variables listed below).

### Backend reference

#### `youtube_api`

| Field | Type | Default env var | Notes |
|---|---|---|---|
| `api_key` | Credential | `YOUTUBE_API_KEY` | Required |
| `ytmusicapi_server_url` | Credential? | `YTMUSICAPI_SERVER_URL` | Optional; enables YouTube Music metadata enrichment |

#### `spotify`

| Field | Type | Default env var | Notes |
|---|---|---|---|
| `client_id` | Credential | `SPOTIFY_CLIENT_ID` | Required |
| `client_secret` | Credential | `SPOTIFY_CLIENT_SECRET` | Required |

#### `musicbrainz`

| Field | Type | Default env var | Notes |
|---|---|---|---|
| `token` | Credential? | `MUSICBRAINZ_TOKEN` | Optional; raises rate limit |
| `base_url` | Credential? | `MUSICBRAINZ_BASE_URL` | Defaults to `https://musicbrainz.org/ws/2` |
| `mirror_db` | Credential? | `MUSICBRAINZ_MIRROR_DB` | Path to local `mb_mirror.db`; auto-detected from the data dir if omitted |
| `dump_base_url` | Credential? | `MUSICBRAINZ_DUMP_BASE_URL` | Used by `mb_extract_urls` only; defaults to the public MetaBrainz server |

If `mirror_db` is set (or found automatically), URL lookups skip the network for
URLs not in the mirror. Build the mirror with `mb_extract_urls` and keep it fresh
with `mb_sync_replication`.

#### `discogs`

| Field | Type | Default env var | Notes |
|---|---|---|---|
| `user_token` | Credential? | `DISCOGS_USER_TOKEN` | Optional; raises rate limit from 60 to 240 req/min |

#### `soundcloud`

| Field | Type | Default env var | Notes |
|---|---|---|---|
| `server_url` | Credential | `YTDLP_SERVER_URL` | URL of a running yt-dlp HTTP server |

#### `nicovideo`

Same fields as `soundcloud`. Both backends share the same yt-dlp server; you can
point them at the same URL.

| Field | Type | Default env var |
|---|---|---|
| `server_url` | Credential | `YTDLP_SERVER_URL` |

#### `local`

Resolves `local://` URIs to files on disk.

| Field | Type | Notes |
|---|---|---|
| `base_dir` | path? | Base directory for relative `local://` paths. Defaults to CWD. |

### Full example

See `config/providers.example.yaml`. A minimal providers file that reads
everything from environment variables is just `{}` (or an empty file).

---

## `http.yaml`

Controls the HTTP client stack: caching, per-domain scheduling, and retry
behaviour. The stack from innermost to outermost is:

```
DefaultHttpClient → DomainScheduler → DB cache (optional) → memory cache (optional)
```

Cache hits at an outer layer bypass all inner layers, including scheduling.

### Top-level fields

| Field | Type | Default | Notes |
|---|---|---|---|
| `memory_cache` | bool | `false` | Wrap the stack in an in-process memory cache (no TTL; lives for the duration of the process) |
| `db_cache` | object? | absent | Persistent SQLite cache; see below |
| `schedulers` | map | `{}` | Per-domain scheduler configs; see below |
| `coalescer_max_hold` | duration? | absent | Fire a partial batch once its oldest request has waited this long, even while other work is still running (e.g. `"30s"`). Absent = only fire partial batches when nothing else could add ids to them |

### `db_cache`

```yaml
db_cache:
  path: "sqlite:///path/to/http_cache.db"   # defaults to $CACHE_DIR/musiclib-rs/http_cache.db
  cache_policy:
    default_policy:
      status_rules: [...]
    domains:
      api.example.com:
        status_rules: [...]
```

| Field | Type | Notes |
|---|---|---|
| `path` | string | SQLite connection string. Defaults to `$CACHE_DIR/musiclib-rs/http_cache.db` |
| `cache_policy` | object | Controls which responses are stored and for how long |

#### Cache policy

A `cache_policy` has a `default_policy` applied to all hosts not listed in
`domains`, and an optional per-host override map. Set a domain's value to `~` to
disable caching for that host entirely.

Each policy is a list of `status_rules`. Rules are evaluated in order; the first
matching rule wins.

```yaml
cache_policy:
  default_policy:
    status_rules:
      - status: 200
        policy: { ttl: "7d", swr: "1d" }
      - status: 404
        policy:
          ttl: { initial: "1h", multiplier: 2.0, max: "7d" }
          swr: "0s"
  domains:
    www.googleapis.com:
      status_rules:
        - status: 200
          policy: { ttl: "1h", swr: "6h" }
```

**`status`** — which HTTP status codes the rule applies to. Accepts:
- An exact code: `200`, `404`
- A class string: `"2xx"`, `"3xx"`, `"4xx"`, `"5xx"`
- The string `"any"` to match every response

**`policy`** — how to cache a matching response. Omit (or set to `~`) to
*not* cache responses matching this status.

| Field | Type | Notes |
|---|---|---|
| `ttl` | duration or backoff | How long the response is fresh. See duration formats below. |
| `swr` | duration | Stale-while-revalidate window added on top of `ttl`. Serve the stale entry and refresh in background. `"0s"` disables SWR. |

**`ttl` as a backoff** — lets the cache TTL grow each time the same URL returns
an error, reducing hammering on persistently failing endpoints:

```yaml
ttl:
  initial: "1h"
  multiplier: 2.0
  max: "7d"
```

The effective TTL is `initial × multiplier^(consecutive_error_count)`, capped at `max`.

#### Duration formats

Durations are strings: `"500ms"`, `"30s"`, `"5m"`, `"1h"`, `"7d"`.

### `schedulers`

A map from hostname to scheduler config. The special key `"*"` is the default
applied to any host not explicitly listed.

```yaml
schedulers:
  "*":
    max_concurrent: ~         # null = unlimited
    retry:
      retry_after_attempts: 5
      backoff_attempts: 3
      initial_backoff: "1s"
      backoff_multiplier: 2.0
      max_backoff: "1m"
    channel_capacity: 64

  musicbrainz.org:
    max_concurrent: 1
    retry:
      retry_after_attempts: 5
      backoff_attempts: 3
      initial_backoff: "1s"
      backoff_multiplier: 2.0
      max_backoff: "1m"
      rate_limit_statuses: [503]   # MB uses 503 instead of 429
```

#### `SchedulerConfig`

| Field | Type | Default | Notes |
|---|---|---|---|
| `max_concurrent` | int? | `~` (unlimited) | Maximum in-flight requests for this domain |
| `retry` | object? | see below | Retry-on-rate-limit config; set to `~` to disable retries |
| `channel_capacity` | int | `64` | Depth of the request queue for this domain |

#### `RetryConfig`

Retries use a two-phase strategy. Phase 1 honours the `Retry-After` response
header; if no header is present, or phase 1 is exhausted, phase 2 uses
exponential backoff.

| Field | Type | Default | Notes |
|---|---|---|---|
| `retry_after_attempts` | int | `5` | Max retries that use `Retry-After` (phase 1) |
| `backoff_attempts` | int | `3` | Max additional retries with exponential backoff (phase 2) |
| `initial_backoff` | duration | `"1s"` | Starting backoff for phase 2 |
| `backoff_multiplier` | float | `2.0` | Multiplier applied per phase-2 attempt |
| `max_backoff` | duration | `"1m"` | Upper bound on phase-2 backoff |
| `rate_limit_statuses` | list\<int\> | `[]` | Extra status codes treated as rate-limit responses, in addition to 429 |

---

## Fetch-options YAML

Fetch-options files control which **child entries** are fetched when importing a
parent entry, and with what options. The `import` binary takes a
`--fetch-options` argument pointing at one of these files.

Rules decide which children are **fetched**, not which are **recorded**:

- Fetching a non-artist entity (a release, release group or track) always
  records all of its children: an album's full tracklist, a track's credits
  and album. Children the rules don't fetch are stored as **stubs**: the pair,
  plus the type, name and duration the parent's listing gave it, with
  `entry_source.fetched_at` NULL. A later fetch fills the stub in.
- An artist's children are its discography, which is opt-in: only children
  the rules fetch are recorded.

An entity reached under several option sets in one run (say, an album reached
as a leaf from a track and with `main` from its artist) is processed under
each of them, so the result doesn't depend on which path arrives first.

Three ready-made files live in `config/fetch_options/`:

| File | Purpose |
|---|---|
| `fetch_discography.yaml` | Fetch an artist's releases and their tracks; each track also fetches its album(s), credited artists and original as leaves (`track`) |
| `vtuber_fetch_discography.yaml` | Like above but skip YouTube videos at channel level; whitelist/blacklist playlists by title |
| `no_fetch_discography.yaml` | Fetch the entry itself, follow nothing (its structural children become stubs) |

### File structure

A fetch-options file is a flat YAML map of **named option sets**. The reserved
key `main` is the root entry point used by the importer. All other keys are
reusable sets that rules can reference by name. Sets may reference themselves to
enable recursive traversal.

```yaml
main:
  child_rules:
    - match: { entry_type: artist }
      fetch: no_children          # reference a named set
    - match: { not: { entry_type: artist } }
      fetch: main                 # recurse

no_children:
  child_rules: []
```

### Cross-file references

A `fetch:` value may reference a named set from another file using the syntax
`./path/to/other.yaml::name`:

```yaml
- match: { entry_type: artist }
  fetch: ./no_fetch_discography.yaml::main
```

The path is resolved relative to the file containing the reference.

### Child rules

Each element of `child_rules` is a `{ match, fetch }` pair. Rules are evaluated
in order; the first match wins.

| Field | Notes |
|---|---|
| `match` | A matcher expression (see below) |
| `fetch` | Named set, cross-file ref, or `null` to not fetch the child (a non-artist parent still records it as a stub; an artist parent drops it). Omitting `fetch` uses the default (no child rules). |

### Matcher expressions

#### Leaf matchers

| Syntax | When it matches |
|---|---|
| `always` | Every child |
| `{ entry_type: <type> }` | `track`, `release`, `release_group`, or `artist` |
| `{ external_type: "youtube:video" }` | Exact `source:entity` type string (e.g. `"spotify:track"`, `"musicbrainz:recording"`) |
| `{ name_regex: "pattern" }` | Entry name matches the regex (requires fetching the entry) |
| `{ has_source: spotify }` | Entry has at least one identifier in the `spotify` source namespace |
| `{ appears_on: true }` | In an artist's discography: a release the artist only appears on (Spotify `appears_on`, Discogs `Appearance`/`TrackAppearance`), e.g. a various-artists compilation |
| `{ duration_range: { min: 60000, max: 1200000 } }` | Duration in milliseconds; both `min` and `max` are optional |
| `{ index_range: { min: 0, max: 50 } }` | Child's position in the parent's child list; both optional |

#### YouTube-specific matchers

These require fetching the YouTube entry and therefore have higher cost.

| Syntax | When it matches |
|---|---|
| `{ youtube: { description_regex: "pattern" } }` | YouTube video description matches the regex |
| `{ youtube: { category_id: "10" } }` | YouTube video category ID (e.g. `"10"` = Music) |

#### MusicBrainz-specific matchers

| Syntax | When it matches |
|---|---|
| `{ musicbrainz: { release_group_primary_type: "Album" } }` | Release-group primary type: `"Album"`, `"Single"`, `"EP"`, `"Broadcast"`, `"Other"` |
| `{ musicbrainz: { release_group_has_secondary_type: "Live" } }` | Release-group secondary types contain the value: `"Live"`, `"Compilation"`, `"Remix"`, etc. |
| `{ musicbrainz: { release_status: "Official" } }` | Release status: `"Official"`, `"Promotional"`, `"Bootleg"`, `"Pseudo-Release"` |
| `{ musicbrainz: { release_country: "JP" } }` | Release country (ISO 3166-1 alpha-2) |
| `{ musicbrainz: recording_is_video }` | MusicBrainz recording has `video: true` |

#### Logical combinators

```yaml
match: { not: <expr> }

match:
  all:
    - { entry_type: track }
    - { duration_range: { min: 60000 } }

match:
  any:
    - { entry_type: track }
    - { entry_type: release }
```

#### Quantifier matcher

Matches a parent entry based on properties of its children. Requires fetching
children, so it is expensive.

```yaml
match:
  children_satisfy:
    matcher: { name_regex: "MV" }
    mode:
      ratio: { min: 0.75 }          # at least 75% of children match

# or using a count:
    mode:
      count: { min: 1, max: 100 }   # between 1 and 100 children match
```

Both `min` and `max` are optional in `ratio` and `count`.

---

## Dedup barrier files (`dedup_barriers/`)

Dedup barriers prevent distinct real-world entities from being merged into a
single DB entry when they share ambiguous identifiers (e.g. a URL that redirects
to multiple artists, or a Wikipedia page shared between a group and a solo act).

### Where the files live

The `dedup` binary auto-loads every `*.yaml` file from
`$CONFIG_DIR/musiclib-rs/dedup_barriers/` in sorted order. Pass one or more
explicit `--dedup-config <path>` arguments to override this and use specific
files instead.

### File structure

A barrier file is a list of **groups**. Each group contains two or more named
**anchors** — the distinct entities that must never be merged. Each anchor
declares all the pairs it owns under `members`.

```yaml
groups:
  - name: honeyworks-chico          # human label, used in logs only
    anchors:
      honeyworks:
        members:
          - musicbrainz:https://musicbrainz.org/artist/1dc670f7-...
          - unknown_url:http://www.honeyworks.jp/
          - unknown_url:https://ja.wikipedia.org/wiki/HoneyWorks
      chico:
        members:
          - musicbrainz:https://musicbrainz.org/artist/dbd0f795-...
          - unknown_url:http://www.chicoxxx.com/
```

### Fields

#### `groups[]`

| Field | Type | Notes |
|---|---|---|
| `name` | string | Optional human label, shown in log output |
| `anchors` | map\<string, Anchor\> | Two or more named anchors that define the barrier |

A barrier group with only one anchor is a no-op (there is nothing to separate).
In practice you will always have at least two anchors per group.

#### `Anchor`

| Field | Type | Notes |
|---|---|---|
| `members` | list\<pair\> | All pairs that belong to this anchor. Two effects: (1) positive — all listed pairs are force-merged together even if no URL cross-linking connects them; (2) negative — no listed pair may share a DB entry with a pair from a sibling anchor in the same group. |

#### Pair format

Every pair is a `source:identifier` string split on the **first** colon. Because
identifiers are often URLs (which contain colons), everything after the first
colon is kept verbatim:

```
musicbrainz:https://musicbrainz.org/artist/1dc670f7-1a43-4a71-973a-2ad181f4edd4
unknown_url:https://ja.wikipedia.org/wiki/HoneyWorks
spotify:https://open.spotify.com/artist/4Z8W4fKeB5YxbusRsdQVPb
```

All pairs are canonicalized through the provider registry before comparison, so
`unknown_url:https://youtu.be/dQw4w9WgXcQ` and
`unknown_url:https://www.youtube.com/watch?v=dQw4w9WgXcQ` resolve to the same
canonical pair.

### How the dedup barrier works

When `dedup` runs it:

1. Loads all barrier files and canonicalizes every declared pair.
2. For each file, checks whether the barrier is already satisfied in the DB
   (no single entry holds pairs from two different anchors in the same group).
3. Re-imports (shallow — no child traversal) only the entries that violate a
   barrier or whose config file has changed on disk.
4. Flushes the merged barrier: during flush, the `is_rel` graph is partitioned so
   pairs from different anchors land in separate DB entries.
5. Persists each file's mtime so unchanged, already-satisfied barriers are
   skipped on subsequent runs.

Re-imports are served from the HTTP cache where possible, so a no-change run is
cheap even for large libraries.

### When to add a barrier

Add a barrier group when two or more real-world entities share an identifier that
causes the importer to merge them. Typical cases:

- A shared website or Wikipedia article (e.g. a group and its lead vocalist).
- A URL that redirects differently depending on context.
- Two MusicBrainz artists that share a Discogs or SoundCloud page.

The `members` list should contain the most authoritative, unambiguous pairs for
each entity (MusicBrainz URLs are ideal), followed by any ambiguous pairs you
want attributed exclusively to that anchor. All pairs in `members` are treated
identically: they are force-merged together and kept apart from sibling anchors.

---

## `match.rhai` — soft-dedup match script

The `softmatch` binary evaluates candidate entry pairs using a
[Rhai](https://rhai.rs/) script. Copy `config/match.example.rhai` to
`<config_dir>/match.rhai` and tune thresholds as needed.

Passing `--model <runtime-model.json>` replaces `decide()` with the native,
versioned logistic scorer. Candidate generation and the local `embed()` hook
still come from the same pipeline; there are no scoring API calls. Scores in
the learned defer band are emitted as `DEFER`, while low scores are `SEPARATE`.
The richer research `model.json` is intentionally rejected because Rust cannot
reproduce all of its enrichment features. If the runtime artifact names an
embedding model, `--embedding-model-id` must name that exact local model.
The batch and import paths also discover `<config_dir>/dedup-model.json`
automatically. A learned MERGE decision is always soft (a `same_identity`
assertion, not a destructive merge — see below); installing the model does
not change that. Imports persist learned DEFER candidates for review; batch
runs opt into this with `--persist-suggestions`.

Soft identity uses enabled `entry_relation` rows with kinds `same_identity` and
`different_identity`. The former creates virtual connected components; the
latter is a cannot-link barrier and wins over a conflicting same-identity path.
Corrections tombstone relation state and append to `dedup_feedback`; model
candidates live in `dedup_suggestion` with their model, feature, and evidence
snapshots.

### Lifecycle

The host calls these entry points in order; all but `decide` are optional:

| Function | Signature | Called when |
|---|---|---|
| `init` | `fn init() -> Dynamic` | Once after the script is loaded |
| `prepare` | `fn prepare(ctx, entries) -> array` | In batches of 512 entries before scoring; each returned value is that entry's `a.prepared` |
| `decide` | `fn decide(ctx, a, b) -> verdict` | For each candidate pair, on every core at once |
| `refine` | `fn refine(ctx, items) -> array` | After `decide`, in chunks of 256 non-DISTINCT pairs |
| `destroy` | `fn destroy(ctx)` | Once when the engine shuts down |

`refine` is where slow work belongs (network calls): `decide` runs in parallel
and must stay CPU-bound. Each item is `#{a, b, verdict}` (`verdict` is
`decide`'s map); return one verdict per item, `item.verdict` or `()` to keep
it. A chunk that errors keeps its verdicts.

`init()` returns an arbitrary **context object** (`ctx`). The host holds it for
the whole run and passes it back as the first argument of every other hook. Use
it to carry state that must persist across calls — e.g. an FFI library handle,
a pre-compiled regex, or cached config values. Returns `()` when absent.

Rhai function scopes are completely isolated (no top-level `let`/`const`/`global::`
is visible inside a function), so the context object is the only supported way
to share state between calls.

### Entity fields (`a`, `b`)

`a` and `b` are `Entry` values (`type_of(a) == "Entry"`): fields are converted
to Rhai values only when the script reads them, as `a.durations` or
`a["durations"]`. Data beyond these fields comes from `pair_facts_json` (below),
not from new entry fields.

| Field | Type | Notes |
|---|---|---|
| `entry_type` | string | `"track"` \| `"release"` \| `"release_group"` \| `"artist"` |
| `entry_id` | int | Opaque DB entry ID; used with `semantic_sim` |
| `title` | string | Best known name (raw, unprocessed) |
| `durations` | array of int | Every distinct per-source duration (ms) |
| `release_dates` / `release_types` / `primary_types` | array of strings | Distinct per-source values |
| `pairs` | array of `#{source, identifier}` | Canonical DB pairs |
| `aliases` | array of strings | All known names (case-deduplicated) |
| `sourced_aliases` | array of `#{source, name}` | Authoritative sources first (non-video before video) |
| `peer_ids` | array of int | For tracks: credited-artist entry IDs. For artists: credited-track entry IDs |
| `track_positions` | array of `#{release_id, disc_no, track_no}` | Placement in releases; `disc_no`/`track_no` are `()` when unknown |
| `child_ids` | array of int | For releases: child track entry IDs used by bounded tracklist retrieval |

### Host-provided functions

#### String similarity

| Function | Returns | Notes |
|---|---|---|
| `normalize(s)` | string | Lowercase, alphanumeric+CJK only, whitespace collapsed |
| `jaccard(a, b)` | f64 | Token Jaccard on normalized strings |
| `levenshtein(a, b)` | f64 | Normalized Levenshtein |
| `jaro_winkler(a, b)` | f64 | Jaro-Winkler |
| `str_sim(a, b)` | f64 | `max(levenshtein, jaccard)` |

#### Regex

| Function | Returns | Notes |
|---|---|---|
| `compile_re(pattern)` | Regex | Pre-compile a pattern; store in `ctx` |
| `re_is_match(re_or_pat, text)` | bool | |
| `re_replace_all(re_or_pat, repl, text)` | string | `repl` is literal — `$` is not special |
| `re_captures_all(re_or_pat, text)` | `[[string]]` | Each inner array: `[full_match, group1, …]`; unmatched groups are `""` |

Both `re_*` functions accept either a pre-compiled `Regex` or a pattern `String`.

#### Verdict constructors

```rhai
merge(conf, reason)             // conf: f64 confidence in [0,1] -- soft: asserts same_identity, never destructively merges
relate(kind, conf, reason)      // kind: see below
relate(kind, conf, reason, metadata)  // + dedup-v2 metadata, e.g. #{transformation, derived_side: "a"|"b"}
defer(conf, reason)             // undecided: counted and written to the CSV as DEFER; refine() can settle it
distinct()
```

A verdict map may also carry `origin` (default `"heuristic"`) and
`model_version`, recorded on the `dedup_feedback` / `entry_relation` rows it
writes — e.g. `v.origin = "jev"` for a verdict an external model made.

Valid `relate` kinds: `alt_version`, `live`, `remix`, `instrumental`, `cover`,
`medley`, `release_variant`, `in_release_group`, `same_artist`.

#### Pair facts

```rhai
pair_facts_json(source, identifier)        // → JSON string: one musiclib-pair-facts/1 object
pair_facts_json([[source, identifier], …]) // → JSON string: array of them (#{source, identifier} also accepted)
```

Per-pair data read lazily from the DB: names, durations, release date/types,
contributions (with artist entry id and best name), parent and child links
(with names), credited items (`credited` ids, `credited_names` alongside).
Keyed by `(source, identifier)`, so facts stay valid when entries merge.
Returns `()` when the run has no file-backed DB. Field list and ordering:
`docs/plan-v15-runtime.md` §2b (`parents[].name` and `credited_names` are later
additions). `config/match.learned.rhai` uses it to feed the learned matcher,
and `config/jev.rhai` to build Jev's evidence view.

#### Misc

```rhai
env_var(name)     // → string ("" when unset)
cache_dir()       // → <cache_dir> (app_dirs::cache_dir), for script-owned caches
url_decode(s)     // → percent-decoded string (input unchanged if not valid UTF-8)
read_text(path)   // → contents of a text file, path relative to the script's directory
to_json(v) / parse_json(s)
```

`print(...)` and `debug(...)` go to the log (`info` / `debug` level), not stdout.

#### Semantic similarity

```rhai
semantic_sim(a.entry_id, b.entry_id)  // → f64 cosine similarity [0,1]; 0.0 if either entry lacks an embedding
```

Returns the pre-computed value from the embedding cache — no HTTP call at scoring time.

#### Feature detection

```rhai
ffi_available()   // → bool: true when musiclib was built with --features ffi
```

### Semantic embedding hooks

If the script defines `embed_batch(ctx, texts) -> array-of-arrays`, the host
calls it in chunks of 512 at startup to embed every entry with a missing or stale
vector. Falls back to `embed(ctx, text) -> array` if `embed_batch` is not defined.
Vectors are stored in the sqlite-vec embedding cache
(`<data_dir>/embeddings.db`). A full run computes exact per-type top-k
neighbours in memory (tiled brute force, all cores) for candidate KNN;
SQLite is only the durable store of the vectors.

Two optional hooks control what is cached:

- `embed_text(ctx, entry) -> string` — the text an entry is embedded from
  (default: its best title; `()` skips the entry). An entry is re-embedded when
  this text changes. `match.learned.rhai` returns `title [A] primary artist` for
  tracks, the title encoder's training input.
- `embedding_model_id(ctx) -> string` — names the model (default `""`). The
  cache records it; when it changes, a full `softmatch` run drops every cached
  vector and re-embeds, so two scripts can share one `--embed-db` without mixing
  vector spaces. An import-time (focused) run seeing a different id skips
  semantic blocking instead of rebuilding the whole cache.

The embedding dimension must match the `--embed-dim` flag (default `256`).

### FFI module (`ffi::`) and the `inference` cdylib

When musiclib is built with `--features ffi`, the `ffi` Rhai module is available.
It lets the script bind and call functions in any C-ABI shared library:

```rhai
let lib = ffi::open("./libinference.so");   // path relative to the script file
let f   = lib.func("my_fn", "i32", ["ptr", "u64"]);
let rc  = f.invoke([some_ptr, 42]);
```

Supported types: `"void"`, `"i32"`, `"u32"`, `"i64"`, `"u64"`, `"f32"`, `"f64"`,
`"ptr"`. Memory helpers: `ffi::malloc(n)`, `ffi::free(p)`, `ffi::cstr(s)`,
`ffi::read_cstr(p)`, `ffi::read_i64(p, n)`, `ffi::read_f32(p, n)`,
`ffi::read_ptr(p, i)`, `ffi::write_ptr(p, i, v)`.

Relative paths (containing a `/` or `\`) are resolved relative to the **script
file**, not the process working directory.

#### `config/inference/` cdylib

The workspace member `config/inference` (package name `inference`) builds a
`libinference.so` cdylib providing romaji detection and LangID; real semantic
embeddings (MiniLM, 384-d) need `--features minilm`:

```bash
cargo build -p inference --release --lib
# artifact: target/release/libinference.so
```

`config/match.example.rhai`'s `open_inference()` function tries three locations
in order: a `libinference.so` symlink next to the script (e.g.
`~/.config/musiclib-rs/libinference.so → <repo>/target/release/libinference.so`),
the repo build tree (`../target/release/libinference.so`), then the system loader
search path. The recommended setup:

```bash
ln -s /path/to/repo/target/release/libinference.so ~/.config/musiclib-rs/libinference.so
```

### Jev refinement (`config/jev.rhai`)

`config/match.learned.rhai` sends every pair the learned model DEFERs to
TypeSafe's Jev from its `refine` hook and uses the answer instead. The
questions are prompt v2.1 (`train/learned-matcher/jev_client.py`), aligned
with the learned matcher's ontology:

| Jev answer | Verdict |
|---|---|
| `same_identity` (incl. a full MV of the recording) | MERGE (soft `same_identity`) |
| `derived` (tracks: cut/TV size, other MV version, live, remix, instrumental, cover, arrangement) | RELATE `derived_from`; `kind` → `transformation`, `direction` → `derived_side` |
| `sibling` (two versions of one song, neither made from the other) | DISTINCT (no direct edge) |
| `unrelated` / `different_identity` | DISTINCT |
| `unsure` | stays DEFER |

Rows written from these verdicts carry `origin = "jev"`,
`model_version = "typesafe-jev/v2.1"`.

The questions live in `config/jev/<entry type>.json` and are sent verbatim,
with their key order kept (Rhai maps would sort it, so `jev.rhai` builds
requests as JSON text). `config/jev.rhai` (a Rhai module) holds the evidence
view and the answer mapping. Keep `jev.rhai` and `jev/` next to `match.rhai`.
Transport, retry, concurrency and the response cache are the inference
cdylib's `inference_typesafe_*` API, so build it with the `typesafe` feature:

```bash
cargo build -p inference --release --lib --features matcher,typesafe
```

| Env var | Default | Notes |
|---|---|---|
| `TYPESAFE_API_KEY` | unset | Turns Jev on; without it DEFER verdicts stay deferred |
| `MUSICLIB_JEV_CONCURRENCY` | `12` | Concurrent API calls per batch |
| `MUSICLIB_JEV_CACHE` | `<cache_dir>/typesafe.db` | Response cache (SQLite), keyed by the SHA-256 of the request |
| `TYPESAFE_BASE_URL` | the TypeSafe API | Override, e.g. a local mock |

Because the cache key is the request itself, unchanged evidence is never paid
for twice and any edit to the questions or the view is a fresh request. Jev
bills per input token: about 1.9k for a track request and 1.1k for the other
types, i.e. roughly $0.08 / $0.05 per thousand pairs at $0.042 per million. Each
`refine` chunk logs one line (`jev: N pair(s): C cached, A asked, F failed ->
…`), and `init()` logs whether Jev is on and, if not, why.

The library handle is opened once in `init()` and held for the whole run via the
context object, so the model loads only once regardless of how many embedding
batches are processed.

The `ffi` feature adds a build-time dependency on `libffi` (compiled from source
via autotools); all other features are off by default.
