# Plan: filter cross-type links & move `entry_type` onto the `entry` table

## Problem

`flush.rs` runs union-find over pairs; every `Pair` carries its own `entry_type`
from `PairMetadata`, and a class can span pairs of *different* types — producing
an `entry` whose pairs disagree on type. Two observed cases:

- **Entry #10 (track + release):** MB recording `ab4266ab` and MB release
  `db6bd431` both list `youtube.com/watch?v=ndNWnIAdhZo` in their url-rels. Both
  backends insert that URL into `sources` (`recording.rs:132-140`,
  `release.rs:191-200`). Step 6 of the importer calls `import(youtube, …)` from
  both, creating `is_rel` edges from both to the same YouTube pair → union-find
  merges recording + release + video into one entry.
- **Entry #924 (artist + release):** Discogs artist `2158171` (Saitou Yuuma)
  has `nicovideo.jp/user/21254075/mylist/25488373` in its `urls`. The Discogs
  artist backend inserts it under `unknown_url` (`artist.rs:217-222`). Step 6
  canonicalizes it to `(nicovideo, mylist)`, fetched as a Release, and the
  `is_rel` chain links it into the artist entry.

## Root cause

The bad merges come entirely from the **`is_rel` edges built in `importer.rs`
step 6** (`importer.rs:166-181`):

```rust
for src in flatten_pairs(&sources) {
    if src != canonical {
        state.is_rel.lock().push((canonical.clone(), src.clone())); // unconditional
        subs.push(import(... src ...));
    }
}
```

`sources` mixes the entity's own canonical id (always same type) with **url-rels**
harvested from the API payload, which can point at entities of *any* type.
`is_rel` means "same entity", so linking a Release to a YouTube video (Track), or
an Artist to a Nicovideo mylist (Release), is what fuses the multi-type entries.

## Enabler

`CanonicalizeProvider::canonicalize` already returns `entry_type` and is a cheap,
network-free regex match — so we can learn a url-rel's type *before* linking,
without fetching it.

---

## Part A — Let each backend's `canonicalize` accept its own source key

The backends currently reject their native key
(`if source_key != UNKNOWN_URL { return None }`), but they *should* accept it.
This is a prerequisite for clean type resolution and also fixes
`provider_owns_any` (`importer.rs:257`), which today never returns `true` because
it calls `canonicalize("youtube", url)` etc. and hits the guard.

**Change.** In each `backends/*/canonicalize.rs`
(`youtube_api`, `spotify`, `musicbrainz`, `discogs`, `nicovideo`, `soundcloud`):

```rust
// before
if source_key != StandardProviderKeys::UNKNOWN_URL { return None; }
// after
if source_key != StandardProviderKeys::UNKNOWN_URL && source_key != SOURCE { return None; }
```

The match arms already operate on the URL form, and each backend's
`canonical_identifier` is that URL form, so `canonicalize(SOURCE, canonical_id)`
returns the same `CanonicalizeResult` (idempotent), including the correct
`entry_type`.

**Tests.** Per backend, a round-trip test: take a `canonical_identifier`, feed it
back as `canonicalize(SOURCE, id)`, assert the result equals canonicalizing the
original URL via `UNKNOWN_URL` (same canonical pair + `entry_type`). Verify short
forms too (e.g. YouTube `youtu.be/<id>` must be matched by `match_video_url`).

**Watch-out.** If any backend's canonical identifier is *not* a URL the match
functions accept (e.g. a bare id), that arm needs a small extension. The
round-trip test surfaces this immediately.

---

## Part B — Filter cross-type `is_rel` links in the importer

Resolve a pair's type via its native key (works after Part A).

**Add** to `importer.rs`:

```rust
async fn pair_entry_type(
    providers: &[Arc<dyn FetchProvider>],
    pair: &Pair,
) -> Option<EntryType> {
    for p in providers {
        if let Some(c) = p.canonicalize(&pair.0, &pair.1).await {
            return Some(c.entry_type);
        }
    }
    None
}
```

**Guard step 6** (`importer.rs:166-181`):

```rust
for src in flatten_pairs(&sources) {
    if src == canonical { continue; }
    if let Some(t) = pair_entry_type(&providers, &src).await
        && t != entry_type
    {
        debug!("skip cross-type source {}:{} ({:?} != {:?})", src.0, src.1, t, entry_type);
        continue;            // drop link AND skip recursion
    }
    state.is_rel.lock().unwrap().push((canonical.clone(), src.clone()));
    subs.push(import(/* … src … */));
}
```

- Known mismatched type → drop link and don't recurse (matches "don't link in
  the first place"). Fixes entries #10 and #924.
- Unknown type (`None`, unrecognized domain) → keep; it can never carry a typed
  `PairMetadata`, so it can't create a conflict.
- Optional: apply the same guard to the sibling `is_rel` in step 7
  (`importer.rs:195-201`) against `child_ref.entry_type`. Lower risk; defer
  unless observed.

---

## Part C — Move `entry_type` from the pair (`entry_source`) onto the `entry` table

With Part B making each equivalence class type-homogeneous, the type is genuinely
an entry-level property. Per-pair `specific_data` columns (`duration_ms`,
`num_tracks`, …) **stay** on `entry_source` — those legitimately vary per source.

Note: the `entry_source.entry_type` column is currently **write-only** across the
whole repo (nothing `SELECT`s it outside `musicdb/mod.rs`), so moving it is safe
on the read side.

**Schema (`musicdb/mod.rs`).**
- Add `pub entry_type: String` to the `entry` model (default `"unknown"`).
- Remove `pub entry_type: String` from the `entry_source` model.

**DB methods.**
- `insert_entry(entry_type: Option<EntryType>)` → sets the column (`"unknown"`
  when `None`). Add `set_entry_type(entry_id, EntryType)` to update an
  existing/merged entry.
- `upsert_pair(...)`: drop the `entry_type` param and the column from the
  insert/on-conflict update. Keep `EntryType` in `PairMetadata` (still needed
  in-memory for class reconciliation).
- `insert_stub_pair(...)`: drop the `entry_type = "unknown"` field.
- `merge_entries(loser, winner)`: unchanged structurally (still only re-points
  `entry_source`); the caller reconciles the type afterward.

**flush.rs reconciliation (step 5, `flush.rs:135-154`).** Compute each class's
type from members' fresh metadata:

```rust
fn reconcile_type(members: &[Pair], metadata: &HashMap<Pair, PairMetadata>) -> Option<EntryType> {
    let types: HashSet<EntryType> =
        members.iter().filter_map(|p| metadata.get(p).map(|m| m.entry_type)).collect();
    match types.len() {
        0 => None,                                   // stub-only class
        1 => types.into_iter().next(),
        _ => { warn!("class spans types {:?} — filtering gap", types); /* deterministic pick */ }
    }
}
```

Then:
- New class → `insert_entry(class_type)`.
- Existing winner → after `merge_entries`, call `set_entry_type(winner, t)` only
  when `class_type` is `Some` (never overwrite a real type with `"unknown"`).

A `len() > 1` warning now signals that Part B missed a cross-type link — a useful
regression canary.

**Migration.** `import.sh` deletes `musiclib.db` and `MusicDb::new` runs
`schema_registry().sync()`, so a fresh schema is created automatically — no manual
migration for the dev loop. For any persisted DB, `sync()` adds
`entry.entry_type`; the now-unused `entry_source.entry_type` column can be left in
place (SQLite won't drop it via sync) or dropped manually.

---

## Tests (end-to-end)

- **Entry #10:** fixtures for an MB recording (Track) and MB release (Release)
  sharing one YouTube url-rel. Assert `is_rel` contains `recording→youtube` but
  not `release→youtube`, and flush yields two `entry` rows typed `track` and
  `release` (not one fused entry).
- **Entry #924:** Discogs artist (Artist) with a nicovideo-mylist url under
  `unknown_url`. Assert no `artist→mylist` edge; the artist entry stays `artist`.
- **Reconciliation:** a class with a fetched pair + a stub pair → entry takes the
  fetched type; the stub doesn't force `"unknown"`.
- Part A round-trip tests as above.

## Suggested order

A → B → C. Part A unblocks reliable type resolution; B stops the bad links; C
makes the schema reflect the now-guaranteed one-type-per-entry invariant and adds
a built-in conflict canary.
