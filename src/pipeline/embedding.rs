use std::collections::HashMap;
use std::sync::{Arc, Mutex, Once};

use anyhow::Context as _;
use hnsw::{Hnsw, Params as HnswParams};
use rand_pcg::Pcg64;
use rhai::{Dynamic, Engine, Map as RhaiMap};
use rusqlite::{Connection, params};
use space::{KnnPoints, Metric, Neighbor};
use tracing::{info, warn};

use crate::pipeline::softmatch::EntryInfo;

// ── sqlite-vec extension registration ────────────────────────────────────────

static VEC_EXT_REGISTERED: Once = Once::new();

fn ensure_vec_extension() {
    VEC_EXT_REGISTERED.call_once(|| unsafe {
        // sqlite_vec::sqlite3_vec_init is declared `unsafe extern "C" fn()` (no params)
        // but the actual symbol has the auto_extension signature — transmute is needed.
        let fn_ptr: rusqlite::auto_extension::RawAutoExtension =
            std::mem::transmute(sqlite_vec::sqlite3_vec_init as unsafe extern "C" fn());
        rusqlite::auto_extension::register_auto_extension(fn_ptr)
            .expect("register sqlite-vec auto extension");
    });
}

// ── EmbeddingCache ────────────────────────────────────────────────────────────

/// Entry types that get their own vec0 table. "unknown" is excluded.
const KNOWN_TYPES: &[&str] = &["track", "artist", "release", "release_group"];

fn table_for_type(entry_type: &str) -> Option<&'static str> {
    match entry_type {
        "track" => Some("vec_embeddings_track"),
        "artist" => Some("vec_embeddings_artist"),
        "release" => Some("vec_embeddings_release"),
        "release_group" => Some("vec_embeddings_release_group"),
        _ => None,
    }
}

/// In-process cache of multilingual entry embeddings backed by sqlite-vec.
///
/// Uses a separate rusqlite connection (possibly to the same on-disk file as the
/// main sea-orm connection; WAL mode allows safe concurrent access). All methods
/// are sync; call from async code inside `tokio::task::block_in_place`.
pub struct EmbeddingCache {
    conn: Arc<Mutex<Connection>>,
    pub dim: usize,
}

#[derive(Clone, Copy)]
struct SquaredL2;

impl Metric<Vec<f32>> for SquaredL2 {
    type Unit = u32;

    fn distance(&self, left: &Vec<f32>, right: &Vec<f32>) -> Self::Unit {
        left.iter()
            .zip(right)
            .map(|(a, b)| {
                let delta = a - b;
                delta * delta
            })
            .sum::<f32>()
            .to_bits()
    }
}

type TypeHnsw = Hnsw<SquaredL2, Vec<f32>, Pcg64, 24, 48>;

struct TypeAnn {
    index: TypeHnsw,
    entry_ids: Vec<i64>,
    index_by_entry: HashMap<i64, usize>,
}

/// Run-local approximate-nearest-neighbour index built from the durable SQLite
/// embedding cache. `vec0` remains the source of truth, while HNSW avoids an
/// O(N²) full-library query pattern at 100k-entry scale.
pub struct EmbeddingAnn {
    by_type: HashMap<String, TypeAnn>,
}

impl EmbeddingAnn {
    pub fn build(cache: &EmbeddingCache) -> anyhow::Result<Self> {
        let mut by_type = HashMap::new();
        for entry_type in KNOWN_TYPES {
            let rows = cache.all_vectors(entry_type)?;
            if rows.is_empty() {
                continue;
            }
            let mut index = TypeHnsw::new_params(SquaredL2, HnswParams::new().ef_construction(160));
            let mut entry_ids = Vec::with_capacity(rows.len());
            let mut index_by_entry = HashMap::with_capacity(rows.len());
            let mut searcher = hnsw::Searcher::default();
            for (entry_id, vector) in rows {
                let position = index.insert(vector, &mut searcher);
                if position != entry_ids.len() {
                    anyhow::bail!("HNSW returned a non-contiguous insertion index");
                }
                entry_ids.push(entry_id);
                index_by_entry.insert(entry_id, position);
            }
            info!(
                "Built {entry_type} HNSW index with {} vector(s)",
                entry_ids.len()
            );
            by_type.insert(
                (*entry_type).to_string(),
                TypeAnn {
                    index,
                    entry_ids,
                    index_by_entry,
                },
            );
        }
        Ok(Self { by_type })
    }

    /// Return approximate same-type neighbors as `(entry_id, L2 distance)`.
    pub fn knn(&self, entry_id: i64, k: usize, entry_type: &str) -> Vec<(i64, f64)> {
        let Some(type_index) = self.by_type.get(entry_type) else {
            return vec![];
        };
        let Some(&query_index) = type_index.index_by_entry.get(&entry_id) else {
            return vec![];
        };
        let query = type_index.index.get_point(query_index);
        let wanted = (k + 1).min(type_index.entry_ids.len());
        let mut neighbors = vec![
            Neighbor {
                index: usize::MAX,
                distance: u32::MAX,
            };
            wanted
        ];
        let mut searcher = hnsw::Searcher::default();
        let found = type_index.index.nearest(
            query,
            50.max(k.saturating_mul(3)),
            &mut searcher,
            &mut neighbors,
        );
        found
            .iter()
            .filter(|neighbor| neighbor.index != query_index)
            .take(k)
            .map(|neighbor| {
                (
                    type_index.entry_ids[neighbor.index],
                    f32::from_bits(neighbor.distance).sqrt() as f64,
                )
            })
            .collect()
    }
}

impl EmbeddingCache {
    /// Open (or create) the embedding DB at `path`. Registers the sqlite-vec
    /// extension on first call and creates the required tables.
    ///
    /// Each entry type gets its own vec0 table so KNN searches are type-local
    /// and all k slots go to same-type neighbours. If the old monolithic
    /// `vec_embeddings` table is detected the schema is migrated automatically
    /// (existing embeddings are cleared and will be re-computed on the next run).
    pub fn open(path: &str, dim: usize) -> anyhow::Result<Self> {
        ensure_vec_extension();
        let conn =
            Connection::open(path).with_context(|| format!("opening embedding db {path}"))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;

        // Create meta table first so migration can DELETE from it safely.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS entry_embedding_meta (\
                entry_id INTEGER PRIMARY KEY, \
                title    TEXT NOT NULL \
             );",
        )?;

        // Migration: old schema had a single mixed-type vec_embeddings table.
        // Drop it and clear meta so all entries are re-embedded into the new
        // per-type tables.
        let old_exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE name = 'vec_embeddings'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if old_exists {
            tracing::info!(
                "Migrating embedding DB to per-type tables; embeddings will be recomputed."
            );
            conn.execute_batch(
                "DROP TABLE IF EXISTS vec_embeddings; \
                 DELETE FROM entry_embedding_meta;",
            )?;
        }

        // One vec0 table per entry type. KNN on each table returns only
        // same-type neighbours, so k slots are never wasted on other types.
        // NOTE: vec0 uses L2 distance for FLOAT[n]; pre-normalising vectors
        // makes L2 order equivalent to cosine order.
        let mut batch = String::new();
        for t in KNOWN_TYPES {
            batch.push_str(&format!(
                "CREATE VIRTUAL TABLE IF NOT EXISTS vec_embeddings_{t} USING vec0(\
                    entry_id INTEGER PRIMARY KEY, \
                    embedding FLOAT[{dim}]\
                 );"
            ));
        }
        conn.execute_batch(&batch)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            dim,
        })
    }

    /// Returns `(entry_id, title, entry_type)` triples whose embedding is
    /// missing or whose cached title no longer matches the entry's current
    /// best_title. Entries with unknown or unrecognised types are skipped.
    pub fn stale_entries(&self, entries: &[EntryInfo]) -> Vec<(i64, String, String)> {
        let conn = self.conn.lock().unwrap();
        let mut stale = Vec::new();
        for e in entries {
            let Some(table) = table_for_type(&e.entry_type) else {
                continue;
            };
            let Some(title) = &e.best_title else {
                continue;
            };
            let cached: Option<String> = conn
                .query_row(
                    "SELECT title FROM entry_embedding_meta WHERE entry_id = ?",
                    params![e.entry_id],
                    |r| r.get(0),
                )
                .ok();
            // Title mismatch (or no meta row) → stale.
            // Title matches but embedding is absent from the correct type table
            // (e.g. entry_type changed between runs) → also stale.
            let in_correct_table = || {
                conn.query_row(
                    &format!("SELECT 1 FROM {table} WHERE entry_id = ?"),
                    params![e.entry_id],
                    |_| Ok(()),
                )
                .is_ok()
            };
            match cached {
                Some(ct) if ct == *title && in_correct_table() => {}
                _ => stale.push((e.entry_id, title.clone(), e.entry_type.clone())),
            }
        }
        stale
    }

    /// Store an embedding vector (as a JSON array string) for an entry.
    /// The vector should be unit-normalised so L2 ≡ cosine order.
    /// Entries with unrecognised types are silently skipped.
    pub fn upsert(
        &self,
        entry_id: i64,
        title: &str,
        entry_type: &str,
        vector: &[f64],
    ) -> anyhow::Result<()> {
        let Some(table) = table_for_type(entry_type) else {
            return Ok(());
        };
        let json = format!(
            "[{}]",
            vector
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO entry_embedding_meta (entry_id, title) VALUES (?, ?)",
            params![entry_id, title],
        )?;
        conn.execute(
            &format!("INSERT OR REPLACE INTO {table} (entry_id, embedding) VALUES (?, ?)"),
            params![entry_id, json],
        )?;
        Ok(())
    }

    fn all_vectors(&self, entry_type: &str) -> anyhow::Result<Vec<(i64, Vec<f32>)>> {
        let Some(table) = table_for_type(entry_type) else {
            return Ok(vec![]);
        };
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(&format!(
            "SELECT entry_id, embedding FROM {table} ORDER BY entry_id"
        ))?;
        let rows = statement.query_map([], |row| {
            let entry_id = row.get::<_, i64>(0)?;
            let bytes = row.get::<_, Vec<u8>>(1)?;
            Ok((entry_id, bytes))
        })?;
        let mut output = Vec::new();
        for row in rows {
            let (entry_id, bytes) = row?;
            if bytes.len() != self.dim * std::mem::size_of::<f32>() {
                anyhow::bail!(
                    "embedding {entry_id} has {} bytes, expected {}",
                    bytes.len(),
                    self.dim * std::mem::size_of::<f32>()
                );
            }
            let vector = bytes
                .chunks_exact(4)
                .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("four-byte chunk")))
                .collect();
            output.push((entry_id, vector));
        }
        Ok(output)
    }

    /// KNN search: up to `k` nearest neighbours of `entry_id` within the same
    /// entry type (self excluded). Returns `(neighbor_entry_id, l2_distance)`
    /// sorted by distance ascending. Returns an empty vec if `entry_id` has no
    /// stored embedding or its type is unrecognised.
    pub fn knn(
        &self,
        entry_id: i64,
        k: usize,
        entry_type: &str,
    ) -> anyhow::Result<Vec<(i64, f64)>> {
        let Some(table) = table_for_type(entry_type) else {
            return Ok(vec![]);
        };
        let conn = self.conn.lock().unwrap();
        // Request k+1 results to account for the query vector itself appearing
        // in the result set at distance 0.
        let mut stmt = conn.prepare(&format!(
            "SELECT entry_id, distance \
             FROM {table} \
             WHERE embedding MATCH (SELECT embedding FROM {table} WHERE entry_id = ?) \
               AND k = ? \
             ORDER BY distance",
        ))?;
        let results: Vec<(i64, f64)> = stmt
            .query_map(params![entry_id, (k + 1) as i64], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
            })?
            .filter_map(|r| r.ok())
            .filter(|(id, _)| *id != entry_id)
            .take(k)
            .collect();
        Ok(results)
    }

    /// Cosine similarity in [−1, 1] between two cached entries.
    /// Returns `None` if either entry has no stored embedding.
    /// Tries all per-type tables; both entries must be in the same table.
    pub fn cosine_similarity(&self, a: i64, b: i64) -> Option<f64> {
        let conn = self.conn.lock().unwrap();
        // vec_distance_cosine returns cosine distance in [0, 2]; convert to similarity.
        for t in KNOWN_TYPES {
            let table = format!("vec_embeddings_{t}");
            if let Ok(sim) = conn.query_row(
                &format!(
                    "SELECT 1.0 - vec_distance_cosine(\
                        (SELECT embedding FROM {table} WHERE entry_id = ?1), \
                        (SELECT embedding FROM {table} WHERE entry_id = ?2)\
                     )"
                ),
                params![a, b],
                |r| r.get::<_, f64>(0),
            ) {
                return Some(sim);
            }
        }
        None
    }
}

// ── Rhai ↔ serde_json conversions ────────────────────────────────────────────

pub fn dynamic_to_serde(v: &Dynamic) -> serde_json::Value {
    if v.is_unit() {
        serde_json::Value::Null
    } else if let Ok(b) = v.as_bool() {
        serde_json::Value::Bool(b)
    } else if let Ok(i) = v.as_int() {
        serde_json::Value::Number(i.into())
    } else if let Ok(f) = v.as_float() {
        serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null)
    } else if let Some(s) = v.clone().try_cast::<String>() {
        serde_json::Value::String(s)
    } else if let Some(arr) = v.clone().try_cast::<Vec<Dynamic>>() {
        serde_json::Value::Array(arr.iter().map(dynamic_to_serde).collect())
    } else if let Some(map) = v.clone().try_cast::<RhaiMap>() {
        let mut obj = serde_json::Map::new();
        for (k, val) in map.iter() {
            obj.insert(k.to_string(), dynamic_to_serde(val));
        }
        serde_json::Value::Object(obj)
    } else {
        serde_json::Value::Null
    }
}

pub fn serde_to_dynamic(v: &serde_json::Value) -> Dynamic {
    match v {
        serde_json::Value::Null => Dynamic::UNIT,
        serde_json::Value::Bool(b) => Dynamic::from(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Dynamic::from(i)
            } else {
                Dynamic::from(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => Dynamic::from(s.clone()),
        serde_json::Value::Array(arr) => {
            Dynamic::from(arr.iter().map(serde_to_dynamic).collect::<Vec<Dynamic>>())
        }
        serde_json::Value::Object(obj) => {
            let map: RhaiMap = obj
                .iter()
                .map(|(k, v)| (k.clone().into(), serde_to_dynamic(v)))
                .collect();
            Dynamic::from_map(map)
        }
    }
}

// ── HTTP primitive for Rhai scripts ──────────────────────────────────────────

/// Register `http_post_json(url, body_map) -> map` on the engine.
///
/// Uses ureq (blocking, no tokio dependency) so it is safe to call from Rhai
/// script code running inside `tokio::task::block_in_place`. Errors are surfaced
/// as Rhai runtime errors.
pub fn register_http_fns(engine: &mut Engine) {
    engine.register_fn(
        "http_post_json",
        |url: String, body: RhaiMap| -> Result<Dynamic, Box<rhai::EvalAltResult>> {
            let body_val = dynamic_to_serde(&Dynamic::from_map(body));
            let mut resp = ureq::post(&url).send_json(&body_val).map_err(|e| {
                Box::new(rhai::EvalAltResult::ErrorRuntime(
                    format!("http_post_json({url}): {e}").into(),
                    rhai::Position::NONE,
                ))
            })?;
            let json: serde_json::Value = resp.body_mut().read_json().map_err(|e| {
                Box::new(rhai::EvalAltResult::ErrorRuntime(
                    format!("http_post_json({url}): bad JSON response: {e}").into(),
                    rhai::Position::NONE,
                ))
            })?;
            Ok(serde_to_dynamic(&json))
        },
    );
}

// ── Embedding phase ───────────────────────────────────────────────────────────

const EMBED_BATCH_SIZE: usize = 64;

/// Call the Rhai `embed_batch(texts)` (preferred) or `embed(text)` (fallback)
/// for every stale entry and store results.
///
/// Must be called from inside `tokio::task::block_in_place` (or a thread without
/// an active tokio context) because `http_post_json` uses blocking IO.
///
/// Returns `false` if neither `embed_batch` nor `embed` is defined in the script
/// (semantic blocking will be silently skipped).
pub fn embed_stale_entries(
    entries: &[EntryInfo],
    engine: &rhai::Engine,
    ast: &rhai::AST,
    base_scope: &rhai::Scope<'_>,
    user_ctx: &Dynamic,
    cache: &EmbeddingCache,
) -> bool {
    let stale = cache.stale_entries(entries);
    if stale.is_empty() {
        return true;
    }
    tracing::info!("Embedding {} stale entries...", stale.len());

    // Probe: call embed_batch(ctx, []) to check if the function exists without
    // doing real work (an empty batch returns immediately).
    // eval_ast=false: re-evaluating the AST would re-run the script's top-level
    // statements (and any `import`); we only want to invoke the function. The
    // context object from init() is threaded in as the first argument instead.
    let no_eval = rhai::CallFnOptions::new().eval_ast(false);
    let probe: Result<Dynamic, _> = engine.call_fn_with_options(
        no_eval,
        &mut base_scope.clone(),
        ast,
        "embed_batch",
        (user_ctx.clone(), Vec::<Dynamic>::new()),
    );
    let has_batch = !matches!(&probe, Err(e) if is_fn_not_found(e));

    // Track the densest vector produced this pass; a low maximum is the
    // signature of the naive token-hash fallback (see `report_embed_health`).
    let mut max_nonzero = 0usize;
    let ok = if has_batch {
        embed_via_batch(
            engine,
            ast,
            base_scope,
            user_ctx,
            cache,
            &stale,
            &mut max_nonzero,
        )
    } else {
        embed_via_single(
            engine,
            ast,
            base_scope,
            user_ctx,
            cache,
            &stale,
            &mut max_nonzero,
        )
    };
    report_embed_health(cache.dim, max_nonzero);
    ok
}

/// Number of vector components above a small magnitude epsilon.
fn nonzero_dims(arr: &[f64]) -> usize {
    arr.iter().filter(|x| x.abs() > 1e-6).count()
}

/// After an embedding pass, loudly flag the degenerate case where every vector
/// is sparse and near-orthogonal — the fingerprint of the token-hash naive
/// fallback (a handful of non-zero dims) rather than a real dense sentence
/// embedder. The example script's fallback only `print`s to stdout, which never
/// reaches `tracing`, so this is the signal operators actually see: it fires on
/// exactly the runs that populate the embedding cache with junk.
fn report_embed_health(dim: usize, max_nonzero: usize) {
    if dim == 0 {
        return;
    }
    // Real embedders (MiniLM / LaBSE) are essentially fully dense;
    // the naive histogram lights up only as many dims as a title has distinct
    // token buckets. Anything under 25% density across the *whole* batch means
    // no real model ran.
    if max_nonzero * 4 < dim {
        warn!(
            "Embedding backend produced only sparse, near-orthogonal vectors \
             (at most {max_nonzero}/{dim} non-zero dims) — this is the naive \
             token-hash fallback, NOT a real model, so semantic blocking is \
             effectively disabled. Build the inference cdylib and enable the \
             `ffi` feature (and make libinference.so loadable) so a real \
             embedder runs."
        );
    } else {
        info!("Embedding backend healthy: dense {dim}-d vectors.");
    }
}

fn embed_via_batch(
    engine: &rhai::Engine,
    ast: &rhai::AST,
    base_scope: &rhai::Scope<'_>,
    user_ctx: &Dynamic,
    cache: &EmbeddingCache,
    stale: &[(i64, String, String)],
    max_nonzero: &mut usize,
) -> bool {
    for chunk in stale.chunks(EMBED_BATCH_SIZE) {
        let texts: Vec<Dynamic> = chunk
            .iter()
            .map(|(_, t, _)| Dynamic::from(t.clone()))
            .collect();
        let result: Result<Dynamic, _> = engine.call_fn_with_options(
            rhai::CallFnOptions::new().eval_ast(false),
            &mut base_scope.clone(),
            ast,
            "embed_batch",
            (user_ctx.clone(), texts),
        );
        match result {
            Err(e) => {
                warn!("embed_batch error: {e}");
                continue;
            }
            Ok(val) => {
                let vectors = match val.try_cast::<Vec<Dynamic>>() {
                    Some(v) => v,
                    None => {
                        warn!("embed_batch did not return an array");
                        continue;
                    }
                };
                for (i, vec_dyn) in vectors.into_iter().enumerate() {
                    let Some((entry_id, title, entry_type)) = chunk.get(i) else {
                        break;
                    };
                    let arr: Vec<f64> = vec_dyn
                        .try_cast::<Vec<Dynamic>>()
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|d| d.as_float().ok())
                        .collect();
                    *max_nonzero = (*max_nonzero).max(nonzero_dims(&arr));
                    store_vec(cache, *entry_id, title, entry_type, &arr);
                }
            }
        }
    }
    true
}

fn embed_via_single(
    engine: &rhai::Engine,
    ast: &rhai::AST,
    base_scope: &rhai::Scope<'_>,
    user_ctx: &Dynamic,
    cache: &EmbeddingCache,
    stale: &[(i64, String, String)],
    max_nonzero: &mut usize,
) -> bool {
    for (entry_id, title, entry_type) in stale {
        let result: Result<Dynamic, _> = engine.call_fn_with_options(
            rhai::CallFnOptions::new().eval_ast(false),
            &mut base_scope.clone(),
            ast,
            "embed",
            (user_ctx.clone(), title.clone()),
        );
        match result {
            Err(e) if is_fn_not_found(&e) => {
                warn!("embed() not defined in Rhai script — semantic blocking disabled");
                return false;
            }
            Err(e) => warn!("embed({title:?}) error: {e}"),
            Ok(val) => {
                let arr: Vec<f64> = val
                    .try_cast::<Vec<Dynamic>>()
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|d| d.as_float().ok())
                    .collect();
                *max_nonzero = (*max_nonzero).max(nonzero_dims(&arr));
                store_vec(cache, *entry_id, title, entry_type, &arr);
            }
        }
    }
    true
}

fn store_vec(cache: &EmbeddingCache, entry_id: i64, title: &str, entry_type: &str, arr: &[f64]) {
    if arr.len() != cache.dim {
        warn!(
            "embed({title:?}) returned {} values, expected {}; skipping",
            arr.len(),
            cache.dim
        );
        return;
    }
    if let Err(e) = cache.upsert(entry_id, title, entry_type, arr) {
        warn!("Failed to store embedding for entry {entry_id}: {e}");
    }
}

fn is_fn_not_found(e: &rhai::EvalAltResult) -> bool {
    matches!(e, rhai::EvalAltResult::ErrorFunctionNotFound(..))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hnsw_reads_cached_vectors_and_returns_nearest_entry() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("embeddings.db");
        let cache = EmbeddingCache::open(path.to_str().unwrap(), 2).unwrap();
        cache.upsert(1, "one", "track", &[1.0, 0.0]).unwrap();
        cache.upsert(2, "two", "track", &[0.99, 0.01]).unwrap();
        cache.upsert(3, "three", "track", &[0.0, 1.0]).unwrap();

        let index = EmbeddingAnn::build(&cache).unwrap();
        let neighbors = index.knn(1, 1, "track");
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].0, 2);
        assert!(neighbors[0].1 < 0.02);
    }
}
