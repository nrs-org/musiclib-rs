# Plan: multi-song recordings (karaoke streams, medleys) and original anchors

Status: Part A implemented on 2026-10-05; the live DB repair has not been
run yet (see "Implementation notes"). Part B is exploratory.

## Problem

The cover-original import (`providers/backends/musicbrainz/recording.rs`, `work.rs`)
assumes that a cover has **one original song**. It writes every
`(original → derived)` edge as an `EDITION_KINDS` relation. Then
`build_edition_projection` unions those in a DSU, so every original of one
derived entry becomes **one edition group**.

A karaoke stream on MusicBrainz (MB) has one `performance (cover)` relation
for each song sung. Example: entry 24609, 4.4 h. It covers Shiny Smily Story,
HINOTORI, REDHEART, BLUE CLAPPER and more. All of those songs fall into one
edition group. Two streams that share one song then join their groups, and the
groups snowball. Two things amplify this:

- **Fan-out:** `find_original_performances` takes up to 20 untagged
  performances **per work** (`MAX_ORIGINALS`), in MB API order. For "Wannabe"
  (75 untagged) the first 20 were mostly DJ-mix fragments, a voiceover, the
  music video and compilation tracks. 18 of them were MB-only stubs. The worst
  stream (entry 28288) has 539 provider edges.
- **Learned bridges:** softmatch has no length or container sanity check.
  Example: a 33 s DDR megamix fragment (7398) and the 4.4 h stream (24609),
  learned `derived_from (cover)` at 0.525. Containers also pollute the
  `relation_facts` `shared_originals`/`derivatives` that the scripts read.

### Measured on the current `musiclib.db` (largest edition group, in identity roots)

| Edges kept | Largest group |
|---|---|
| all (today) | 2,087 |
| provider edges only | 804 |
| provider only, no edition edges from multi-song recordings | 33 |
| all, with that | 1,011 |
| + learned-edge guards (no multi-song/long-form endpoint, length ratio ≤ 3) | 240 |
| + learned confidence ≥ 0.6 | 124 |

"Multi-song" here uses a proxy (originals with more than one distinct
normalised title). Part A uses MB's work list. The ~240 that remain are
learned-matcher chains joined on title fragments ("Blue" ↔ "LILIUM 〜blue〜",
"Idol" ↔ "Your Idol"). They belong to `../learned-matcher` and are out of
scope here.

## The rule

> An **edition** means "the same song". An edition-kind relation is valid
> only between two items that each realise **exactly one song**.

Anything that realises two or more songs is a container: a karaoke stream, a
medley, a compilation or an album. It is never part of an edition group. A
compilation already obeys this, because it is a release and holds its tracks
through containment (`has_rel` / `entry_child`). A karaoke stream breaks the
rule only because it is typed as a single `Track`. It really is a
compilation in one file. Part B follows that idea.

No new relation kinds are added in either part.

## Part A — enforce the rule (do now)

### A1. Multi-song recordings get no edition edges (MB backend)

In `get_recording`, count the distinct works in `work_relations` (all
`performance` relations, whatever their attributes).

- **1 work:** same as today, with the anchor fix in A2.
- **2 or more works:** emit **no** original children (cover or arrangement)
  for it. Record the song count on the pair (see A3). The stream keeps its
  own row and its artist and other children.
- A `remix` relation (recording → recording) is unaffected. It is a 1:1 link
  by construction.

`medley` leaves `EDITION_KINDS`. A medley spans several songs by
definition, so a medley edge would break the rule.

Nothing is lost for good. The MB recording payloads stay in the HTTP DB
cache, so Part B can recover the per-song links later.

### A2. One anchor per work instead of the first 20 (`work.rs`)

1. The existing `work/<id>?inc=recording-rels` call gives the **untagged**
   set. Attributes are only visible here.
2. Browse `recording?work=<id>&inc=isrcs&limit=100`, paginated. This adds
   `first-release-date`, `isrcs` and `video` to each recording. It is 1–2
   extra requests per work, and they are HTTP-cached.
3. Filter the untagged set. Drop `video: true`. Drop a disambiguation that
   matches DJ-mix, "part of …", voiceover or live. Drop a title with a
   parenthesised version suffix (remix, dub, edit, …).
4. Rank by earliest `first-release-date`, then whether it has an ISRC, then
   MB order. Take the **top 1**. Also keep a second candidate only when it
   has the same date and a near-identical length (an MB duplicate of the
   same master).
5. Fallback: if the filters remove everything, take the earliest dated,
   else the first.

Checked live: for "Wannabe" this picks `640be329` (radio edit, 1996-07-03,
`GBAAA9600008`). Spotify, YouTube and the ISRC already merged into that
entry (6964). `MAX_ORIGINALS` goes away.

### A3. Where "multi-song" is stored

Softmatch and the migration need to know that a track is multi-song without
calling MB. Today there is no per-pair extra column. Options:

- **(a) Reuse `entry_source.num_tracks` on a `Track` pair** to mean "songs
  inside". This fits the "compilation in one file" framing, and a release's
  `num_tracks` already means the same thing. No schema change.
- **(b)** Add a nullable `song_count` column to `entry_source`.

Recommendation: (a). It also becomes the hook for Part B.

### A4. Softmatch guards (`pipeline/softmatch.rs`, host-side, so they hold for any script)

A heuristic or Jev edition-kind edge is never written when:

- either endpoint is **multi-song** (A3 count ≥ 2), or
- either endpoint is **long-form** (longer than about 20 min). This catches
  YouTube-only streams that have no MB data, or
- both sides have lengths and `max/min > 4`. Sides whose transformation is a
  degenerate marker (`DEGENERATE_MARKERS`: TV size, short, …) get a looser
  limit of about 8. Tune both limits against the labelled set.

`KnownRelations` leaves multi-song and long-form entries out of
`originals`/`derivatives`.

### A5. Repair the existing DB (one-shot bin, or a `dedup` flag)

1. Back up `musiclib.db`.
2. For each derived entry with provider `cover`/`arrangement` rows,
   re-fetch its MB recording (served from the HTTP cache). Then:
   - **multi-song:** delete its provider cover/arrangement rows and set A3.
   - **single-song:** keep the row to the A2 anchor and delete rows to
     non-anchor originals. These rows are provider-owned, so delete rather
     than tombstone. A tombstone means "the user rejected this".
   - Drop the matching cover → original `entry_child` rows. Containment is
     being misused to mean "derived from" there. FuwaMoco's cover currently
     "contains" 20 Wannabe recordings.
3. **Stub GC:** delete an original-import stub that has an MB-only source
   and nothing else referencing it (no other `entry_child` parent, not a run
   root, no manual relation).
4. Delete heuristic/Jev rows that break the A4 guards, then re-run
   softmatch.
5. Fix the one `entry_relation.extra` stored as a JSON **list**
   (`derived_entry` 5508), and find the merge path that wrote it.
6. Report the group-size distribution before and after.

### Tests (Part A)

- MB fixtures: a karaoke recording with 3 cover works, and a medley. Assert
  no original children, the song count is set, and the songs stay in
  separate edition groups.
- Anchor selection: a "Wannabe"-shaped work fixture. Assert that the
  dated ISRC row wins over DJ-mix, video and remix rows. Also test the
  fallback.
- Softmatch guards: multi-song, long-form, length ratio, and the
  degenerate-marker exemption.
- Migration: a flush-level test (the `MusicDb::new("sqlite::memory:")`
  pattern) for an over-fanned derived entry, a multi-song entry and the
  stub GC.

## Implementation notes (Part A, 2026-10-05)

Where the implementation differs from the plan above:

- **Songs, not works.** MB often credits a translated or alternate-lyrics
  cover with both the original work and its own version work ("Say So
  (Japanese Version)", "愛が止まらない 〜Turn It Into Love〜"). Counting works
  flagged those as multi-song. `work::group_into_songs` merges works linked by
  `other version` / `based on` / `revision of` / `translation` / `arrangement`
  / `orchestration` (never `medley` / `parts`). `EntrySpecificData::Track::songs`
  is `Vec<Vec<String>>` (songs → their works). `entry_source.track_extra` holds
  `{"songs": [...]}`, and `num_tracks` holds the song count.
- **Multi-song is decided per entry, not per recording.** MB often models a
  karaoke stream as one recording per song, each with a URL relation to the
  same YouTube video, so the importer merges all of them into the video's
  entry. Each recording is single-song, but the entry is a container. So
  `MusicDb::multi_song_entries`, `flush` step 10b, `EntryInfo::songs` and the
  repair all merge songs across an entry's pairs (`musicdb::merge_songs`).
  Those per-song recordings are in effect Part B's segments, collapsed into one
  entry.
- **No duration-ratio guard.** Measured on the DB, about 300 of the rows it
  would drop were legitimate: #shorts cuts, TV-size edits, rehearsal takes and
  extended mixes. `edition_guard` refuses only multi-song and long-form
  (> 20 min) tracks.
- **Repair:** `cargo run --release --bin repair_multi_work [-- --dry-run]`
  (`src/bin/repair_multi_work.rs`). It backs up to
  `musiclib.db.pre-multi-work-<unix ts>` with `VACUUM INTO` first. On a copy of
  the 2026-10-05 DB: 4,948 cover child edges removed and 15 anchors added
  (14 new stubs), 4,882 provider relations removed, 32 multi-song entries,
  2,531 MB-only stubs GC'd (none had a non-MB source or a release parent;
  10 Jev same-identity judgments go with them), 418 learned edition rows
  removed by the guard. The largest edition group went from 2,087 to 225
  identity roots, and from 804 to 18 for provider-only edges (a single song,
  Rudolph). What remains is learned-matcher title chains (e.g. the
  "… - Sing-Along" tracks), which belong to `../learned-matcher`.

## Part B — segments (explore later)

Model a multi-song recording exactly like a compilation. The container
`has_rel` its **segments**, one single-song synthetic track per song. Each
segment is an ordinary track, so `segment ↔ original` is a legitimate edition
edge ("Gura sang Wannabe in this stream"). The container itself never
touches an edition group.

```
stream (container, song count = N)
 ├─ has_rel → segment "Wannabe"   ──cover──▶ Wannabe anchor
 ├─ has_rel → segment "HINOTORI"  ──cover──▶ HINOTORI anchor
 └─ …
```

### The hard parts

1. **Identity.** Segments come from no API, so their identifiers are
   synthetic and must be stable:
   - from MB: `<recording mbid>#work=<work mbid>` (stable, but no order or
     time);
   - from YouTube chapters or timestamps: `<videoId>#t=<start>` (ordered and
     timed, but it shifts when a chapter is edited).

   One stream usually has both an MB recording and a YouTube video
   (`same_identity`), which gives **two segment sets for the same songs**.
   They need matching inside the container (title + order + MB work).
   That is a small, constrained softmatch, much easier than the global one.
2. **Fetching.** A segment is complete from its parent's payload. It is
   emitted as a child with all data inline, and fetching it alone is not
   supported. That fits the bounded-import model (fetched = complete), but
   the provider needs a "derived-only" pair kind that `canonicalize` /
   `fetch_entry` reject cleanly. It also needs a new `source_key`
   (e.g. `segment`). Pick the name at the start of B.
3. **Timestamps.** MB has none. Candidates:
   - yt-dlp `chapters` (`ytdlp.rs` already exists);
   - parsing the YouTube description for `mm:ss Title` lines;
   - **Holodex**, which keeps per-song timestamps for hololive streams and
     suits this library well. That would be a new backend.

   Without timestamps, segments still give correct grouping and
   "performed in" links. They just can't play.
4. **Storage.** `entry_child` has `track_no` for order but no offset. Add
   `start_ms`/`end_ms` to `entry_child` (offsets are a property of the
   containment), or put them on the segment pair. The segment's duration is
   `end - start`.
5. **Playback.** The YouTube embed supports `start`/`end` and the player
   already has `seek`. So `playable-sources` for a segment means "the
   container's embed + an offset". The player gets a small extension, not a
   new source type.
6. **Softmatch.** Segments are good matcher inputs: a song-length duration,
   the singer from the stream's channel, and a clean title. The container is
   excluded by A4.

### Suggested exploration order

1. Prototype MB-only segments (no time): emit `#work=` segments from A1's
   multi-song branch, plus cover edges from segment to anchor. Check the
   group sizes and the entry pages.
2. Add timestamps from one source (yt-dlp chapters or Holodex) and
   cross-match MB and YouTube segment sets inside a container.
3. Player offsets.

Each step can be done on its own, and Part A is the base for all of them.

## Open questions

- **Q1:** For A3, reuse `num_tracks` (a) or add a new column (b)?
  Recommended: (a).
- **Q2:** Store work ids as facts on MB pairs to enable a work
  cannot-link for learned edges? This relaxes the earlier "Work is a
  transient hop only" decision a little. It is a follow-up and not needed
  for A.
- **Q3:** Confirm the stub GC (delete non-anchor stubs that nothing else
  references). Recommended: yes.
- **Q4 (B):** Segment source key name, and whether offsets live on
  `entry_child` or on the segment.
