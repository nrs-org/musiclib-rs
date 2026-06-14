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

Three ready-made files live in `config/fetch_options/`:

| File | Purpose |
|---|---|
| `fetch_discography.yaml` | Recursively fetch all releases and tracks; visit artist children as leaves only |
| `vtuber_fetch_discography.yaml` | Like above but skip YouTube videos at channel level; whitelist/blacklist playlists by title |
| `no_fetch_discography.yaml` | Store the entry itself but fetch no children |

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
| `fetch` | Named set, cross-file ref, or `null` to skip the child entirely. Omitting `fetch` uses the default (no child rules). |

### Matcher expressions

#### Leaf matchers

| Syntax | When it matches |
|---|---|
| `always` | Every child |
| `{ entry_type: <type> }` | `track`, `release`, `release_group`, or `artist` |
| `{ external_type: "youtube:video" }` | Exact `source:entity` type string (e.g. `"spotify:track"`, `"musicbrainz:recording"`) |
| `{ name_regex: "pattern" }` | Entry name matches the regex (requires fetching the entry) |
| `{ has_source: spotify }` | Entry has at least one identifier in the `spotify` source namespace |
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
