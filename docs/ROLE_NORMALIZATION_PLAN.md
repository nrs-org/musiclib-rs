# Role Normalization — Implementation Plan

Status: **design settled, not yet implemented.**

## Goal

Give `Contribution.role` a coherent, cross-provider vocabulary so "which artist
did what on which song" is queryable without per-provider string-matching.

## Design (settled)

- `Contribution.role` is **either**:
  - a **bare canonical token** (`producer`, `vocal`, `uploader`, `listed_artist`), or
  - a **namespaced fallback** `backend::raw` (`discogs::composed by`, `musicbrainz::booking`)
    for anything not mapped.
- **Rule:** presence of `::` means "unmapped, raw preserved". This is the signal for a
  later "what still needs mapping?" query, and makes the system lossless by construction.
- Canonical vocabulary is **MusicBrainz-derived** (the most rigorous, machine-readable
  source) plus a couple of platform-native tokens (`uploader`, `listed_artist`).
- **Instrument / vocal / modifier detail is NOT baked into the role string.** It goes into
  `Contribution.extra` as a generic attributes array:
  ```jsonc
  { "attributes": ["lead vocals"] }      // vocal + which kind
  { "attributes": ["guitar", "solo"] }   // instrument + modifier
  { "attributes": ["additional"] }       // producer [additional], etc.
  ```
  MB's `attributes` array already carries instrument names, vocal types, AND modifiers
  (`additional`, `guest`, `solo`, `cover`, `live`), so capturing the whole array gets all
  of it for free. Discogs bracket modifiers (`Guitar [Lead Guitar]`) map onto the same
  shape: role `instrument`, `extra.attributes = ["lead guitar"]`.
- No DB/schema change — `contribution.role` stays a `String`.

## Canonical list scope

Derived from the three MB relationship-type pages (artist→recording, artist→release,
artist→work), deduplicated. Scope chosen: **musical + visual/credits** (~57 tokens).
Business/legal/copyright and misc/named-after MB types are intentionally left to the
`musicbrainz::<type>` fallback — preserved, just not unified.

Canonical token = MB link-type name lowercased, with spaces / `-` / `/` → `_`.

### Performance (8)
`performer` `instrument` `vocal` `performing_orchestra` `conductor` `chorus_master`
`concertmaster` `audio_director`

### Arrangement (4)
`arranger` `instrument_arranger` `orchestrator` `vocal_arranger`

### DJ / remix / compile (4)
`compiler` `mix_dj` `remixer` `samples_from_artist`

### Production / engineering (15)
`producer` `engineer` `audio` `mastering` `sound` `mix` `recording` `field_recordist`
`programming` `editor` `balance` `sound_effects` `transfer` `lacquer_cut`
`production_coordinator`

### Composition / writing (11)
`writer` `composer` `lyricist` `librettist` `translator` `authorship` `revised_by`
`scriptwriter` `reconstructed_by` `adapter` `previous_attribution`

### Visual / video / artwork (15)
`choreographer` `cinematographer` `video_director` `video_appearance` `animation`
`artwork` `design` `graphic_design` `illustration` `design_illustration` `art_direction`
`creative_direction` `photography` `booklet_editor` `liner_notes`

### Platform-native (2)
`uploader` `listed_artist`

### Intentionally → `musicbrainz::*` fallback (not canonical)
`legal_representation` `phonographic_copyright` `video_copyright` `copyright` `booking`
`artists_and_repertoire` `publishing` `licensor` `instrument_technician` `dedication`
`premiere` `commissioned` `misc` `named_after_artist` `named_after_work`

## Phases

### Phase 1 — Roles module (foundation)
File: `src/providers/std_values.rs` (grow the existing `StandardRoleNames`).

- Add the ~57 canonical consts above, grouped by category with a comment header per group.
- Keep existing `UPLOADER`, `LISTED_ARTIST`.
- Add `fn namespaced(backend: &str, raw: &str) -> String` → `format!("{backend}::{raw}")`.
- Add `CANONICAL: &[&str]` slice + `fn is_canonical(role: &str) -> bool` (tests + future
  "still namespaced?" queries).

Prerequisite for Phases 2 and 3. No DB change.

### Phase 2 — MusicBrainz: attribute capture + mapping
File: `src/providers/backends/musicbrainz/recording.rs`

1. Add to `RecordingRelation` (~line 41): `#[serde(default)] pub attributes: Vec<String>`.
2. Add private `fn map_mb_role(rel_type: &str) -> Cow<'static, str>` — near-1:1 table from
   MB type name → canonical const, falling back to `namespaced("musicbrainz", rel_type)`.
   (Drop-set names hit the fallback automatically.)
3. At the `Contribution` build (~line 118): `role: map_mb_role(&rel.relation_type)`, and
   `extra: { "attributes": rel.attributes }` (skip/empty when none).
4. Update existing test (~line 228) + add a case asserting an instrument/vocal rel yields a
   canonical role + populated `extra.attributes`. May need to refresh the recording fixture
   to include an attributed relation.

### Phase 3 — Discogs mapping
File: `src/providers/backends/discogs/release.rs` (shared `artists`→`ChildRef` builder, ~line 150)

1. Add `fn map_discogs_role(raw: &str) -> Cow<'static, str>` → canonical for common roles
   (`Composed By`→`composer`, `Written-By`→`writer`, `Lyrics By`→`lyricist`,
   `Arranged By`→`arranger`, `Producer`→`producer`, `Mixed By`→`mix`, `Mastered By`→`mastering`,
   `Vocals`→`vocal`, …), else `namespaced("discogs", raw)`.
2. Parse bracket modifiers `Foo [Bar]` → base role mapped, `Bar` → `extra.attributes`
   (parallels MB).
3. Keep the empty-role → `listed_artist` + `main_artist` branch.
4. Update `discogs/track.rs` tests (~lines 174–193): they currently assert raw `"Arranged By"`/
   `"Composed By"`; switch to canonical + namespaced expectations.

### Phase 4 — Audit remaining call sites (cheap)
Const-based sites are already canonical (confirm): `spotify/{track,album,playlist}`,
`soundcloud/{track,playlist}`, `nicovideo/{video,mylist,series}`, `youtube_api/video`.
`local/mod.rs` (~line 110) passes user YAML roles through verbatim — **recommend leave
as-is** (hand-authored, user owns their vocab).

### Phase 5 — Docs + verify
- Note the role convention in `CLAUDE.md` (contribution section) and, if roles surface there,
  `docs/CONFIG_REFERENCE.md`.
- `cargo test` + `cargo clippy`.

## Out of scope (follow-ups)

- **Work-rels for `composer`/`lyricist` from MB** — currently the recording fetch only
  requests `artist-rels` (`recording.rs` `PARTS = "artist-credits+artist-rels+url-rels"`).
  Those credits are work-level, reached via recording→work→artist. Needs `work-rels` in the
  `inc` + a work hop. Separate PR.
- Spotify / YouTube expose no per-role data upstream — nothing to map beyond `listed_artist` /
  `uploader`.
- A controlled vocab for the `attributes` **values** themselves (second-level normalization,
  e.g. canonical instrument list) — later; same `extra.attributes` machinery, one level down.

## Sequencing

`1 → 2 → 3` is the meaningful work; Phase 1 gates 2 and 3. Phases 4–5 are wrap-up.
