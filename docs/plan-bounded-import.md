# Plan: bounded import — fetched means complete, everything else is a stub

## Problem

Importing the Hololive discography takes days, mostly because of Spotify
quota and 429 blocks (see the `api.spotify.com` note in `http.yaml`). The walk
is not bounded by the discography:

- Every non-artist child recurses with `main` (`vtuber_fetch_discography.yaml`:
  `not artist → main`), including the album a track points *up* to. One cover
  on a 30-artist compilation pulls in the whole compilation, then each of
  those tracks' albums, and so on.
- In `musiclib.db`, 644 of 1,402 Spotify albums were reached **only** through
  a track→album edge, never from an artist discography. They add 4,841 of the
  6,826 Spotify tracks (~70%).

A narrower config alone can't fix this without losing data, because of how
fetch options interact with storage:

- `fetch: null` drops the child **and the edge** (`matcher.rs:218`), so
  "track is on album X" is lost.
- A leaf fetch (`no_fetch_discography.yaml`) pays for the album fetch and
  then records **none** of its tracklist.
- `State::claim` keys on the pair only. The first path to reach a pair picks
  its options, so the result depends on arrival order. With several roots
  (`--url-file`), member B reached as a leaf from a collab track never gets
  their discography, and nothing is logged.

Correctness problems found along the way:

- `contribution` has 432,800 rows for 81,979 distinct rows (~5× duplicates).
  Every re-import appends another copy. NRS scores artists as Σ(rated
  tracks × contribution factor), so this skews scores now.
- A child-cursor error mid-pagination `break`s (`importer.rs` step 7) and
  keeps the partial child list. An album with 12 of 30 tracks looks complete.

## Model

**Completeness is not stored.** It is whatever the current policy's walk
reaches. Changing fetch options means re-running; nothing stored depends on
the policy, so nothing goes stale because of it.

**A fetched entity is complete.** Fetching an entity records **all** of its
structural edges: an album's full tracklist, a track's credits and album(s), a
cover's original. Neighbors that are not fetched are stored as stubs.

**A stub** is a pair known only because something referenced it. Its own
record was never fetched.

| | Stub | Fetched |
|---|---|---|
| pair + `entry_id` | yes | yes |
| type, name | from the parent's listing (`ChildRef`) | from its own record |
| incoming edges | yes | yes |
| own metadata (date, aliases, duration) | no | yes |
| own outgoing edges | no | yes, all structural ones |
| cross-linked to other platforms | no | yes |
| `fetched_at` | NULL | set |

A later fetch promotes a stub in place (same pair, same entry).

**Artists have a second level: discography.** Artist→release and
channel→upload/playlist listings are big and paginated. Recording them is
opt-in, and fetch-options filtering decides which items are fetched and
recorded. Filtered-out discography items are **not** recorded.

**Policy decides which stubs to fetch, never which structural edges to
record.** Concretely:

- children of a non-artist parent: always recorded; the matched rule decides
  whether the child is fetched (`fetch: null` → stub)
- children of an artist parent: recorded only if fetched

**Entry IDs must stay stable.** NRS references entries by `store_id` =
`entry.id` (`nrs/store-protocol/specification.md` §5). Everything below
updates the DB incrementally and never rebuilds it.

## Phase 1: no duplicate edges

Re-importing must not append copies. (Removing edges that disappeared
upstream is phase 5: it is only safe after phases 2–4.)

1. Migration `user_version` 2 (`CONTRIBUTION_UNIQUE_MIGRATION` in
   `musicdb/mod.rs`): normalize NULL `extra` to `'null'`, delete exact
   duplicates keeping the lowest `id`, and add a unique index on (source,
   identifier, artist_source, artist_identifier, role, main_artist, extra).
2. `insert_contribution` uses `ON CONFLICT DO NOTHING` on that index, like
   `insert_child_edge` does on its primary key. The existing row keeps its
   original `run_id`, so a rollback never deletes it.

No in-memory dedup in `flush` is needed: the DB key also covers edges pushed
once per pass under phase 4's accumulated claims, and edges that only
collide after flush canonicalizes pairs.

On a copy of the real DB: 432,800 → 81,979 rows, in 2.8 s.

## Phase 2: no truncated structural listings

In `importer.rs`, buffer a parent's child edges locally and push them, along
with its metadata (step 5), only once every child cursor has finished
cleanly.

- **Non-artist parent, cursor error:** treat as a fetch failure (log with
  `fetch_failed`). Push no metadata and no edges, so the pair stays a stub.
  Children already spawned still import normally, since they're real entities.
- **Artist parent, cursor error:** keep the metadata and the edges seen so
  far. A discography is policy-filtered anyway, so a partial listing claims
  nothing.

Only **listing** errors (the parent's pagination, `child_next` on the
underlying cursor) count. A **matcher** error on one child (e.g.
`duration_range` fetching a deleted video) used to surface through the same
stream and end the listing too. Now `filtering_stream` logs it and treats that
child as unmatched, without trying lower-priority rules, so a failed VOD filter
can't fall through to a catch-all. Phase 4 turns that skip into a stub edge for
non-artist parents.

The buffering is per pass (see phase 4's per-(pair, options) claims). A
failed pass discards only its own buffered edges; data from an earlier pass
over the same pair that succeeded is kept.

## Phase 3: real stubs

0. **Aliases are idempotent first** (migration v3). They had the same
   append-on-every-import bug as contributions (404,739 rows for 65,745
   distinct), and stubs would make it worse: a stub's listing name is seen once
   per reference. Duplicates are collapsed, a unique index goes on
   (pair, name, `COALESCE(locale, '')`, `COALESCE(extra, '')`, primary) since
   `locale` is mostly NULL, `insert_aliases_for_pair` uses a targetless
   `ON CONFLICT DO NOTHING`, and the FTS index is rebuilt.
1. `ChildRef.duration_ms`, filled where the listing payload has it:
   - Spotify album tracklist and playlist items
   - MB release media tracks (track `length`, falling back to the
     recording's) and both artist recording listings
   - Discogs release tracklist (via `parse_duration_ms`)
   - not YouTube playlist items (the API doesn't return durations there);
     local gets `None`

   `duration_range` uses `ChildRef.duration_ms` when present and only fetches
   the child otherwise.
2. `State.stubs: HashMap<Pair, StubInfo { entry_type, name, duration_ms }>`,
   recorded by the importer for every listed child (first non-empty name and
   duration win). Flush drains and canonicalizes it with everything else, and
   uses it only for pairs without fresh metadata. Stub info can reach a
   periodic flush before its edge (edges wait for the listing to finish, see
   phase 2), so info for pairs not written yet goes back to `State` until the
   final flush.
3. Writes:
   - `upsert_stub_pair`: a new stub row has `fetched_at = NULL` and the
     listing's duration. On conflict it never touches a fetched row or
     `entry_id`; an existing stub with no duration takes one.
   - `insert_stub_alias`: the listing name as a **non-primary** alias, only
     while the pair is still a stub. The player's title picker prefers a
     primary alias, so a later fetch's own name wins.
   - A class with no fetched members gets its entry type from the stubs, for
     new entries only (an existing entry's type came from a fetch).
   - Caveat: MB listings append the disambiguation to the name
     (`"Title (dis)"`), so those stub aliases include it.
4. Migration v4: `entry_source` is rebuilt from its own DDL with
   `fetched_at` nullable (SQLite can't drop NOT NULL in place) and its indexes
   re-created. Existing stubs are then marked: rows with no metadata, aliases,
   outgoing edges or contributions. On a copy of the real DB that's 56,150 of
   99,439 rows, almost all never-fetched namespaces (`unknown_url`, Apple
   Music, ISRC, Deezer, VGMdb…) plus a few hundred failed fetches.
5. `upsert_pair` (a real fetch) promotes a stub in place: same pair, same
   entry, `fetched_at` set.

`SourceRow` exposes `fetched_at`, so the player can tell stubs apart.

On a copy of the real DB, migrations v2–v4 together take ~16 s.

## Phase 4: fetch-options semantics and accumulated claims

1. **Claims per (pair, options).** `State.claimed` becomes
   `HashMap<Pair, HashSet<OptionsId>>`. Reaching `pair` with `opts`:

   ```
   if opts ∈ claimed[pair]                         → stop
   if opts has no child rules and pair is claimed  → stop   (a leaf adds nothing)
   insert opts; run import() for pair under opts
   ```

   Each pair ends up processed under the **union** of every option set that
   reached it, from any path. Option sets are never merged into new rule
   lists: rules are first-match-wins and each names its own target, so `A ++ B`
   and `B ++ A` differ. Each set is evaluated on its own, and a child gets the
   option IDs from every rule that matched it, across all passes.

   Properties:
   - **The result doesn't depend on arrival order.** The same roots and config
     always walk the same graph. This replaces today's first-claimer-wins.
   - **Only the difference does work.** When A was processed under X and is
     later reached under Y, A's fetch repeats (memory LRU, then DB cache; real
     requests only if both missed or expired). Children that Y sends to
     options they already have stop at the claim check. Only children Y
     follows with new options recurse. X and Y don't need to be ordered.
   - **It propagates through sources.** Step 6 passes the current options to
     the entity's other-platform pairs. A YouTube channel reached with `main`
     upgrades its Spotify and MB artist pairs too, even if they were first
     claimed as leaves.
   - **It terminates:** at most #pairs × #option sets passes, and a config has
     only a handful of option sets.
   - **Edges and metadata may be pushed once per pass.** Metadata overwrites,
     `insert_metadata` subtracts the replaced entry's size from
     `approx_bytes`, and duplicate edges are dropped in flush (phase 1).
   - **Concurrent passes:** a pass that arrives while another is still in
     flight may request A again. The coalescer merges identical queued ids on
     batch endpoints; elsewhere it's at most one extra request.
   - Claims are per run (`State`). A later run walks everything again, mostly
     from cache. Avoiding that is the "Resume" item under Later.

   Not done: keeping each pair's `CachedChildSource` in `State` would let a
   repeat pass replay listings with zero HTTP calls, but it holds every
   listing in memory for the whole run. Only worth it if repeat fetches turn
   out to matter.

   Implemented as `State::claim(pair, options, leaf)`; it replaced the
   earlier per-pair `bool` draft.
2. **Filtering** (`matcher::filter_children` takes the parent's
   `EntryType`; every backend passes `result.specific_data.entry_type()`):
   - non-artist parent: the listing is walked to the end (no
     `can_future_items_match` early exit) and every child is yielded.
     Children not to fetch (`fetch: null`, no rule matched, or the matcher
     failed) come with `ChildFetchOptions::stub(pool)`, i.e. `id: None`.
   - artist parent: unchanged. Unmatched children are dropped, and paging
     stops early when no rule can match further.

   The importer records the edge and stub info for every yielded child and
   only calls `import` when `id` is `Some`. So a leaf pass over an album (empty
   rules) records its whole tracklist as stubs.
3. **Configs.** `no_fetch_discography.yaml::main` (empty rules) means "fetch
   this entity on its own, completely, follow nothing".
   `fetch_discography.yaml` and `vtuber_fetch_discography.yaml` send tracks to
   a `track` set (`always → leaf`), and `playlist_all` does the same for its
   tracks. Their header comments describe the record/fetch split, and so does
   `docs/CONFIG_REFERENCE.md`. A test loads all three shipped files.

   The copies in `~/.config/musiclib-rs/fetch_options/` are older versions of
   the repo files (the vtuber one predates the FF7/VOD filters). They are
   only used when a bare name is passed to `--fetch-options`. `import.sh`
   uses the repo path.

What this gives, per root artist:

- releases and their tracks: fetched (complete)
- each track's album, credited artists and original: fetched, with their own
  edges recorded (the album's other tracks, the original's credits) as stubs
- nothing further

## Phase 5: replace stale edges

A fetch is a full snapshot of a non-artist parent's structural edges, so
edges that disappeared upstream (a track dropped from an album, a credit
removed) should be deleted when the parent is fetched again.

This only works once the earlier phases hold. Doing it earlier would delete
good data:

- before phase 2, a truncated listing would replace a full one
- before phase 4, a leaf pass (which records no edges today) or a
  `fetch: null` (which drops the edge) would delete edges recorded earlier
- artist parents are excluded: their discography edges depend on policy, so
  a narrower config must not delete what a broader one recorded

Mechanism: `upsert_pair` deliberately does not stamp `run_id` on existing
rows, and `insert_child_edge`/`insert_contribution` keep the existing row on
conflict. So the DB can't tell which old rows the run re-confirmed. Instead:

1. `State.run_edges`, kept for the whole run (flush never drains it, and it's
   not counted in `approx_bytes`), filled by `flush` after canonicalization:
   - every non-artist pair with fresh metadata (fetched completely: phase 2
     guarantees no truncated listing got this far)
   - every edge seen, as child pairs and contribution keys (artist pair, role,
     main_artist, extra as stored) per parent pair

   The two are tracked separately because a parent's metadata and its edges
   can land in different flushes.
2. `MusicDb::commit_import_run(run_id, &state.take_replacements())`: in **the
   same transaction** as the status update, delete each re-fetched parent's
   `entry_child` and `contribution` rows that aren't in its current set (an
   empty set for a parent that listed nothing). A crash or rollback before
   commit deletes nothing.

Not handled (left for later if they matter):
- a child edge whose position changed keeps its old `disc_no`/`track_no`
  (`insert_child_edge` keeps the existing row)
- a stub only the removed edge referenced stays as an orphan stub row
- `entry_relation` rows from `original_relation_kind` aren't replaced
- the `dedup` binary flushes without a run, so it never replaces

## Verification

- Unit tests:
  - claims: (pair, X) then (pair, Y) both proceed; (pair, X) again stops; a
    leaf set on an already-claimed pair stops; a leaf then an expanding set
    both proceed
  - order independence: two roots whose walks overlap with different options
    produce the same `State` (pairs, edges) in either order
  - a truncated child cursor on a release leaves no metadata and no edges
  - `fetch: null` under a release records a stub edge; under an artist it
    records nothing
  - re-inserting a contribution keeps one row; the v2 migration collapses
    duplicates (phase 1, done)
  - phase 5: a re-fetched album that lost a track and a track that lost a
    credit drop them at commit; a rolled-back run keeps them; an artist parent
    keeps its old discography edges (done)
- `cargo test`, `cargo clippy`.
- Run the import against a copy of `musiclib.db`, for one member (Calliope),
  before and after. Compare:
  - Spotify request count (from the progress output / HTTP cache rows)
  - Spotify albums reached only through a track (currently 644)
  - `contribution` total vs distinct

## Found during the Calliope run

**Appearances are leaves.** Spotify's artist listing requests
`include_groups=album,single,compilation,appears_on`, and Discogs lists
`Appearance`/`TrackAppearance` releases, so various-artists compilations were
part of the "discography" and every track on them was fetched (34 Spotify
appearances = 357 tracks for Calliope, mostly unrelated). `ChildRef.appears_on`
now carries that (Spotify `album_group`, Discogs `role`), a `{ appears_on: true }`
matcher reads it, and both discography configs send those releases to the
leaf set: the album is fetched with its tracklist as stubs.

**Hang: abandoned prefetched requests.** When a listing failed partway
(here: the channel's YouTube Music discography, with the ytmusicapi server
down), the importer stopped reading it, but `buffer_unordered` in
`filtering_stream` had already re-polled the listing and started the failed
request again. That request sat in a stream kept alive (via
`EntityResult.children`) until the parent finished, holding an
`http::Activity` guard, so the coalescer never saw the process idle and every
parked request waited forever. `HEAD` has the same leak (it stalled ~2.5 min
before something unblocked it); with this branch it hung permanently. Fixed
twice over:
- `filtering_stream` ends after yielding a listing error (no re-request)
- the importer drops an entity's child listings before awaiting its children,
  cancelling anything they still had in flight

**Result** (Calliope's channel, vtuber config, against a copy of the real DB,
`--skip-dedup --skip-softmatch`): finished in ~40 s; 3,112 pairs flushed,
+1,628 entries, 1,447 new fetched pairs and 2,086 new stubs (Spotify: 142
tracks, 70 albums, 44 artists). Compilations stay leaves (e.g. *HARAJUKU BUZZ
TRACKS - Anime Lovers*: album fetched, 72 tracks recorded, 0 fetched). Entry
3869 (Calliope) kept its id. The Sep 21 attempt made ~5,900 Spotify calls.

## Skipping listings statically

Listings are lazy (a YouTube channel's uploads are paged on demand, a
Niconico account's uploads come from a second yt-dlp call made on first
read), and each can declare what all of its items are without fetching them
(`ChildSource::evaluate_expr`, e.g. YouTube uploads: "every item is a
`youtube:video` track"). `filter_children` didn't use that: it always read the
listing, then dropped every item under `youtube:video → null`.

Now, for an **artist** parent, `drops_every_item` walks the rules in order
against the source's declaration: a rule that matches no item is passed over;
the first rule that matches every item settles it (skip iff `fetch: null`);
anything undecidable (`Indeterminate`, the default for undeclared facts) means
the listing is read. No rule matching anything also skips. A skipped listing
is never polled, so its lazy request never happens. Non-artist listings are
always read (their children become stubs). The Niconico user uploads source
now declares `nicovideo:video` tracks (`upload_eval`), next to YouTube's
existing declarations; `vtuber_fetch_discography.yaml` drops `nicovideo:video`
at artist level.

Measured on the holomem batch: uploads paging was 252 of 461 YouTube units
(Baelz alone: 26 pages), now skipped. Roboco's re-import went from 539 s
(stalled on a 14,000+ video Niconico upload listing) to 5 s with no yt-dlp
call for that account.

**Risk and guards.** A skip trusts the listing's hand-written declaration,
which nothing tied to the code building its items. A wrong `True` on a
`fetch: null` rule, or a wrong `False` on a fetching rule, skips the listing
and silently loses items. One was already wrong: the Discogs artist listing
(releases mixed with masters) declared *every* item both a release and a
release group. Current configs never asked, so it never fired, but quantifier
shortcuts could already misbehave on it. Guards:
- `declaration_contradictions`: `PaginatedChildSource` (where all
  declarations are attached) checks every item it yields against its
  declaration (`entry_type`, `external_type`, `appears_on`). On a mismatch it
  warns and stops trusting the declaration for that listing; under `cfg(test)`
  it panics, so the backend fixture tests, which read every listing, catch
  drift. It caught the Discogs bug on the first item. Fixed: those two types
  are now `Indeterminate`.
- Non-leaf skips log at `info`, and each fetch runs in a `fetch{entity=…}`
  tracing span, so every skip names the entity and the rule that caused it.

## Later (not in this plan)

- **Resume.** A crash or rollback of a multi-day import currently purges the
  whole run (`purge_run_rows`), which works against resumability. Direction:
  shorter runs (commit per root, or every N roots), plus letting the walk
  pass through recently fetched pairs by reading their edges from the DB
  instead of refetching. That needs its own design.
- **Leaf artists across platforms.** Each fetched credited artist still
  cross-links on every platform. You want the identity merge for NRS; a
  cheaper path is resolving through MB and skipping per-platform artist
  fetches.
- **Entry merges vs NRS `store_id`.** Dedup merging two entries retires an
  id that NRS may hold. That needs a redirect, but it's a separate problem.
- **Stubs in the player:** album pages show them; whether library browsing
  and search do is a player decision. Rating a stub in NRS should trigger a
  fetch first, since a stub has no credits.
