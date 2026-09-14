# Embedded music/video player plan

## Goal

Build a self-hosted music player around `musiclib-rs`. Playback is delegated to
provider-hosted HTML players (YouTube iframe, Spotify embed, SoundCloud widget,
and similar integrations), while musiclib owns library navigation, queues,
source selection, metadata, and duplicate review.

The player also becomes the human-feedback surface for deduplication. It shows
likely duplicate entries in a sidebar, records `same`, `different`, and `unsure`
judgments, and supports correcting a previously accepted merge without losing
source records. Feedback is accumulated into a reproducible dataset before any
online model training is considered.

## Product principles

1. **Provider playback, local control plane.** Audio/video stays in provider
   embeds. The server never proxies or downloads provider media.
2. **One music identity, several playable sources.** A track may offer Spotify,
   YouTube, NicoNico, or SoundCloud sources. The player chooses one but exposes
   the alternatives.
3. **Capability-based controls.** Not every embed supports programmatic seek,
   pause, duration, or end events. The UI exposes only capabilities reported by
   the active adapter.
4. **Reversible deduplication.** UI feedback must not immediately call the
   destructive `merge_entries` path. Identity is first represented as
   provenance-bearing graph assertions.
5. **Offline learning first.** User decisions are immutable training evidence.
   Periodic versioned retraining is safer and easier to evaluate than updating
   model weights after every click.
6. **No new metadata APIs in the playback path.** Player and dedup views use
   data already stored by musiclib. Provider embeds may naturally contact their
   own provider to play media.

## Initial user experience

The desktop layout has three persistent regions:

- **Library/navigation:** search, artists, releases, playlists, and queue.
- **Now playing:** active provider embed, track metadata, playback controls,
  queue controls, and a source selector.
- **Review sidebar:** one potential duplicate at a time, evidence comparison,
  confidence and candidate channel, followed by `Same entity`, `Different`,
  `Unsure`, and `Not now` actions.

Selecting a track resolves its playable sources, chooses the highest-ranked
available adapter, and mounts exactly one active iframe. If the source cannot
play, the user can choose another source; automatic fallback can be added after
provider failure signals prove reliable.

The duplicate sidebar is non-blocking. Playback continues while judgments are
submitted, and the next suggestion is prefetched. Keyboard shortcuts should be
available, but destructive corrections require an explicit confirmation.

## Architecture

### Backend

Add a `player` binary that serves a local HTTP application:

- an Axum JSON API backed by the existing `MusicDb`/SeaORM connection;
- production frontend assets from an embedded or configured static directory;
- optional Server-Sent Events for library/import changes;
- no provider credentials or unrestricted source URLs exposed to the browser.

Keep API DTOs separate from database entities. This prevents the current
pair-centric storage schema from becoming a permanent public API contract.

### Frontend

Use a small TypeScript SPA built with Vite. Preact is a reasonable initial UI
layer for the player, queue, and sidebar state; provider integrations remain
plain TypeScript adapters so the component framework can be replaced later.

The frontend owns ephemeral playback state and queue ordering. The backend owns
library data, saved playlists, dedup judgments, and durable preferences. Persist
the current queue only after the basic playback loop is stable.

### Provider adapter boundary

Every provider implements a common best-effort interface:

```ts
interface EmbedAdapter {
  readonly provider: string;
  canHandle(source: PlayableSource): boolean;
  mount(host: HTMLElement, source: PlayableSource): Promise<PlayerCapabilities>;
  play?(): Promise<void>;
  pause?(): Promise<void>;
  seek?(seconds: number): Promise<void>;
  setVolume?(volume: number): Promise<void>;
  destroy(): void;
}
```

`PlayerCapabilities` reports which controls and events actually work. The first
adapters are:

1. YouTube iframe player;
2. Spotify track/album embed;
3. SoundCloud widget;
4. NicoNico iframe;
5. generic safe iframe only for explicitly allowlisted future providers.

Release and artist entries are not directly playable. They expand to ordered
track children, after which each track resolves its own source.

Embed URLs must be built from canonical provider identifiers, never copied from
arbitrary database text. Maintain a provider-origin allowlist in the server and
Content Security Policy. Validate each provider's current iframe parameters,
browser autoplay behavior, event support, and authentication requirements in a
small spike before considering its adapter complete.

## Playback source resolution

Return a normalized source list for a track:

```json
{
  "entry_id": 42,
  "sources": [
    {
      "provider": "youtube",
      "identifier": "...",
      "embed_url": "...",
      "capabilities": ["play", "pause", "seek", "ended_event"]
    }
  ]
}
```

Rank sources deterministically using:

1. explicit user preference for this track;
2. provider availability and adapter support;
3. global provider preference;
4. official/licensed source classification when stored;
5. previous playback success;
6. stable provider/identifier ordering as the final tie-breaker.

Do not interpret a playback failure as identity evidence. Availability history
and dedup labels are separate data.

## Reversible identity and feedback storage

### Why the current merge path is insufficient

`merge_entries(loser, winner)` re-points `entry_source` rows and removes the old
grouping. A later “undedup” action cannot reliably reconstruct the previous
groups, especially after transitive merges. A disjoint-set representation also
cannot support edge deletion.

### New durable records

The soft-identity foundation now reuses the existing `entry_relation` table and
adds two focused records:

```text
entry_relation
  entry_a, entry_b
  kind: same_identity | different_identity
  confidence, origin, enabled, extra

dedup_suggestion
  entry_a, entry_b, model_version
  probability, decision, candidate_channels
  feature_snapshot_json, evidence_snapshot_json
  status, created_at, updated_at

dedup_feedback
  id, entry_a, entry_b
  judgment: same_identity | different_identity | unsure
  origin
  model_version?
  probability?
  candidate_channels?
  feature_snapshot_json
  evidence_snapshot_json
  note?
  created_at
  supersedes_feedback_id?
```

Suggestion and feedback rows retain the feature and complete entry evidence
shown at decision time, so later metadata changes do not silently rewrite the
training example. Endpoint order is normalized.

Use enabled `same_identity` assertions to derive virtual identity components.
Enabled `different_identity` assertions are hard cannot-link barriers. If a
proposed same edge would connect a component containing a cannot-link pair,
reject it and send the conflict to review.

### Undo behavior

- Undoing a user-created same assertion appends an `unsure` feedback row that
  supersedes the prior judgment, disables the identity edge, and recomputes
  affected components.
- Marking a suggested pair different appends a cannot-link assertion; it never
  deletes the model suggestion.
- Correcting an already materialized database merge requires a source-membership
  split operation. Record its complete before/after source partition in an
  audit event and execute it transactionally.
- Never infer a unique pairwise edge to remove from an arbitrary transitive
  component. Show the connecting evidence path and ask which assertion or
  source partition is wrong.

For the first release, keep grouping virtual and leave physical
`merge_entries` disabled. Materialization is an optimization to add only after
round-trip merge/split property tests exist.

## Deduplication review sidebar

Each card should show:

- both canonical titles and entity types;
- aliases grouped by provider;
- artist, duration, release date, release membership, and tracklist evidence;
- playable source buttons for A/B comparison;
- candidate channels and calibrated probability;
- whether either side already belongs to a virtual identity component;
- previous judgments and any cannot-link conflict.

Actions:

- **Same entity:** create a manual positive assertion.
- **Different:** create a manual cannot-link assertion.
- **Unsure:** preserve an explicit abstention for later sampling.
- **Not now:** change queue position without creating a training label.
- **Undo/correct:** supersede the prior assertion; never mutate its history.

The suggestion queue should mix several strata so feedback does not consist
only of model-boundary cases:

- high-value deferred candidates;
- high-confidence merge audits;
- suspected bad existing groups;
- new candidates introduced by imports;
- a small random sample of low-scored and ordinary candidates.

Record the sampling stratum, rank, candidate-set version, and whether the score
was visible. These fields are necessary to interpret selection bias later.

## Initial API surface

Read endpoints:

```text
GET /api/library/search?q=&type=&cursor=
GET /api/entries/:id
GET /api/entries/:id/playable-sources
GET /api/releases/:id/tracks
GET /api/dedup/suggestions?stratum=&cursor=
GET /api/dedup/history?entry_id=
GET /api/dedup/components/:entry_id
```

Write endpoints:

```text
POST /api/dedup/judgments
POST /api/dedup/judgments/:id/supersede
POST /api/dedup/suggestions/:id/snooze
POST /api/playback/source-preference
```

Every write request carries an idempotency key and the snapshot/content hashes
shown to the user. Return `409 Conflict` if the entries changed before the
judgment was submitted, then refresh the card.

Keep the API local-only by default. Before listening on a non-loopback address,
add authentication, CSRF protection, origin checks, and explicit configuration.

## Training loop

### First stage: offline retraining

1. Export enabled human assertions plus their immutable feature/evidence
   snapshots to the existing annotation schema.
2. Deduplicate superseded judgments and preserve abstentions separately.
3. Split evaluation by identity component and time, not by independent pair.
4. Keep a frozen holdout that is never used for threshold selection.
5. Train and export a new versioned `runtime-model.json`.
6. Run the new model in shadow mode and compare suggestions before activation.
7. Activate by changing the configured model artifact; retain immediate
   rollback to the previous version.

Report candidate retrieval recall, merge precision/recall, defer rate, and
results by entity type and sampling stratum. Do not treat unlabeled suggestions
as negatives.

### Possible later stage: online learning

Only consider incremental updates after there is enough steady feedback to
maintain a representative rolling holdout. Online weights must still be
versioned, bounded, shadow-evaluated, and reversible. Thresholds should not
change after every judgment. Until those controls exist, “online labeling”
means immediate durable data collection, not immediate model mutation.

## Delivery phases

### Phase 0 — reversible identity foundation

- Use `entry_relation` for enabled same/cannot-link assertions.
- Persist model suggestions and append-only feedback with evidence snapshots.
- Implement graph evaluation, conflict detection, judgment supersession, and
  deterministic dataset-export operations.
- Keep all current automatic merges disabled.

Exit criterion: accepting and undoing assertions restores the same virtual
components in property and integration tests without losing any source record.

### Phase 1 — read-only library server

- Add the `player` binary and API DTO layer.
- Implement search, entry details, release tracks, and playable-source mapping.
- Serve a minimal frontend shell and add CSP/provider allowlists.

Exit criterion: browse the current library without provider API calls or DB
writes, and inspect the exact embed URL selected for every playable entry.

### Phase 2 — core player

- Implement YouTube and Spotify adapters.
- Add now-playing state, queue, previous/next, source switching, and errors.
- Ensure only one adapter can emit audio at a time.
- Persist per-track and global provider preferences.

Exit criterion: play a mixed YouTube/Spotify queue and recover manually from an
unavailable source without losing queue position.

### Phase 3 — provider breadth

- Add SoundCloud and NicoNico adapters after capability spikes.
- Add adapter contract tests and provider-specific fallback behavior.
- Add Media Session integration where browser support permits it.

### Phase 4 — review sidebar

- Add suggestion pagination and evidence cards.
- Support A/B playback without overlapping audio.
- Implement same/different/unsure/snooze and history/undo.
- Add component conflict explanations.

Exit criterion: every click produces an auditable, idempotent assertion tied to
the exact evidence and model version the user saw.

### Phase 5 — feedback-driven retraining

- Export a training ledger deterministically.
- Add temporal/component-safe evaluation and per-type reports.
- Shadow a challenger model in the sidebar before promoting it.
- Revisit online updates only after the offline loop is routine.

## Testing strategy

- Rust unit tests for identifier-to-embed mapping, source ranking, pagination,
  assertion conflicts, and merge/split reversibility.
- Database integration tests for concurrent/idempotent judgments and stale
  snapshot rejection.
- Frontend unit tests for adapter lifecycle and capability-driven controls.
- Browser tests with mocked iframe adapters; do not make ordinary CI depend on
  third-party players.
- A small manual provider matrix for desktop browsers, autoplay restrictions,
  login/cookie states, unavailable media, and mobile layout.
- Security tests ensuring arbitrary database URLs cannot become iframe origins.

## MVP boundary

The first usable release includes library search, track/release navigation, a
queue, YouTube and Spotify embeds, manual source switching, and a read/write
dedup sidebar backed by reversible assertions. It does **not** include media
proxying, downloads, recommendation, synchronized cross-provider seeking,
automatic destructive merges, or per-click online model training.

## Immediate implementation order

1. Write migrations and `MusicDb` methods for snapshots/assertions.
2. Add graph projection, cannot-link checks, and ledger export tests.
3. Scaffold the `player` server and read-only library endpoints.
4. Build the frontend shell and YouTube/Spotify adapter contract.
5. Add the queue and source selector.
6. Connect the existing dedup candidate scorer to suggestion persistence.
7. Add the review sidebar and correction history.
8. Run the app in read-only playback mode, then enable feedback writes.
