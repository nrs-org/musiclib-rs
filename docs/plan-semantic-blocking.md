# Plan: Semantic Blocking for Soft-Dedup

## Problem

The current blocking step in `softmatch` groups candidates by `(entry_type, first_title_token)`.
This misses cross-language duplicates — "君の名は" and "Your Name" share no tokens and will
never be compared, even though they are the same work.

## Approach

Replace (or augment) first-token blocking with multilingual vector embeddings. Entries with
semantically similar titles land in the same candidate pool regardless of language or script.

## Components

### 1. Embedding service (Python)

A small HTTP server (same pattern as `ytmusicapi`/`yt-dlp` servers already in this project)
that loads a multilingual sentence embedding model and exposes:

```
POST /embed   { "text": "..." }  →  { "vector": [0.1, 0.2, ...] }
```

Recommended model: `paraphrase-multilingual-MiniLM-L12-v2` (384 dimensions, ~120 MB,
covers 50+ languages including Japanese, Korean, Chinese).

### 2. Rhai `embed` function

The Rhai match script implements `embed(text) -> array` however it wants:

```rhai
fn embed(text) {
    let res = http_post("http://localhost:8082/embed", #{text: text});
    res.vector
}
```

Rust calls `engine.call_fn("embed", title)` to populate the cache. The script owns the
entire contract — URL, request shape, response parsing, model. Switching backends (different
server, OpenAI API, local Ollama, etc.) requires only a Rhai change.

### 3. Embedding cache (SQLite, same `musiclib.db`)

Rust owns the cache. At 100k entries all-pairs cosine is ~5 billion comparisons and ~150 MB
of in-memory vectors — brute force is out. Use sqlite-vec's HNSW index for KNN queries.

Schema addition:

```sql
-- metadata for cache invalidation
CREATE TABLE IF NOT EXISTS entry_embedding_meta (
    entry_id  INTEGER PRIMARY KEY REFERENCES entry(id) ON DELETE CASCADE,
    title     TEXT    NOT NULL    -- recompute when this changes
);

-- sqlite-vec virtual table (HNSW index, 384-dim f32)
CREATE VIRTUAL TABLE IF NOT EXISTS vec_embeddings USING vec0(
    entry_id INTEGER PRIMARY KEY,
    embedding FLOAT[384]
);
```

KNN query to find candidate neighbors per entry:

```sql
SELECT entry_id, distance
FROM vec_embeddings
WHERE embedding MATCH ? AND k = 20
ORDER BY distance
```

Cache is invalidated per-entry when `best_title` changes.

### 4. Blocking strategy

Two passes, union of candidates:

1. **Token blocking** (existing) — fast, zero network calls, catches same-language variants.
2. **Semantic blocking** (new) — KNN query per entry (k=20) against sqlite-vec index,
   filtered by cosine distance threshold ~0.80, catches cross-language pairs.

Embeddings are computed once per entry and cached; subsequent softmatch runs hit only the
DB for the hot path.

### 5. `semantic_sim` as a Rhai scoring feature

Rust registers `semantic_sim(a_id, b_id) -> f64` that queries sqlite-vec for the distance
between two already-cached vectors (no HTTP call during scoring):

```rhai
fn decide_track(a, b) {
    // ...
    let sem = semantic_sim(a.entry_id, b.entry_id);
    if sem >= 0.90 && has_corroboration { return relate("alt_version", sem, "semantic title match"); }
    // ...
}
```

## Data flow

```
startup
  └─ load all entries from DB
  └─ for each entry missing/stale embedding:
       call engine.call_fn("embed", title)  ←  Rhai calls HTTP service
       store vector in sqlite-vec index

blocking
  └─ token blocking  →  candidate set A
  └─ semantic blocking: KNN per entry (k=20, threshold 0.80)  →  candidate set B
  └─ union A ∪ B, same-type filter  →  final candidates

scoring (per candidate pair)
  └─ Rhai decide_* called with EntryInfo structs
  └─ semantic_sim(a_id, b_id) queries sqlite-vec distance (no HTTP)
```

## What Rust hardcodes

- The shape of the cache table (`entry_embedding`).
- That `embed(text)` returns an array of numbers.
- The cosine distance function.
- The semantic blocking threshold (configurable, default 0.80).

Everything else — model, server URL, request format, which titles to embed — lives in the
Rhai script.

## Open questions

- Should `embed` receive `entry` (full object) instead of just `text`, so the script can
  concatenate title + artist name for better disambiguation?
- Semantic blocking threshold: 0.80 is a starting guess; needs tuning against labeled pairs.
- Whether to also expose `semantic_sim` as a Rhai-callable that hits the cache, or keep it
  Rust-internal.
