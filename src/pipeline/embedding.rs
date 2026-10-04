use std::collections::HashMap;
use std::sync::{Arc, Mutex, Once};

use anyhow::Context as _;
use rhai::{Dynamic, Engine, Map as RhaiMap};
use rusqlite::{Connection, params};
use tracing::{info, warn};

use crate::pipeline::softmatch::{EntryInfo, ScriptEntry};

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

/// Exact same-type nearest neighbours of every cached vector, computed once
/// per run (`build`) and then looked up (`knn`).
///
/// Brute force beats an approximate index at library scale: the per-type
/// all-pairs dot products (26.5k entries, 256-d: ~50 GFLOP for tracks) run
/// in about a second across all cores, versus ~75 s to build and query HNSW
/// graphs over dense vectors single-threaded, and the results are exact. The
/// work is tiled (a block of rows against a tile of columns that fits in L2)
/// so it stays compute-bound instead of streaming every vector from memory
/// once per row; memory is the vectors plus `k` neighbours per entry.
pub struct EmbeddingKnn {
    by_type: HashMap<String, TypeKnn>,
}

struct TypeKnn {
    index_by_entry: HashMap<i64, usize>,
    /// Per vector: up to `k` nearest others as `(entry_id, L2 distance)`,
    /// nearest first.
    neighbors: Vec<Vec<(i64, f64)>>,
}

/// Rows per unit of work, and columns per cache tile.
const KNN_ROWS: usize = 64;
const KNN_COLS: usize = 256;

fn dot(a: &[f32], b: &[f32]) -> f32 {
    // Eight independent accumulators so the compiler can vectorise the sum.
    let mut acc = [0f32; 8];
    let (ca, cb) = (a.chunks_exact(8), b.chunks_exact(8));
    let tail: f32 = ca
        .remainder()
        .iter()
        .zip(cb.remainder())
        .map(|(x, y)| x * y)
        .sum();
    for (x, y) in ca.zip(cb) {
        for k in 0..8 {
            acc[k] += x[k] * y[k];
        }
    }
    acc.iter().sum::<f32>() + tail
}

/// A row's nearest others so far: `(squared distance, row)`, ascending.
type Nearest = Vec<(f32, u32)>;

/// Insert `(d2, j)` into `best` (at most `k` long).
fn push_nearest(best: &mut Nearest, k: usize, d2: f32, j: u32) {
    if best.len() == k && (d2, j) >= best[k - 1] {
        return;
    }
    let at = best.partition_point(|&e| e < (d2, j));
    best.insert(at, (d2, j));
    best.truncate(k);
}

/// The `k` nearest other rows of every row of `flat` (`n` × `dim`).
fn all_nearest(flat: &[f32], dim: usize, k: usize) -> Vec<Nearest> {
    let n = flat.len() / dim;
    let norms: Vec<f32> = flat.chunks_exact(dim).map(|v| dot(v, v)).collect();
    let threads = std::thread::available_parallelism().map_or(1, |t| t.get());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut blocks: Vec<(usize, Vec<Nearest>)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                let (next, norms) = (&next, &norms);
                scope.spawn(move || {
                    let mut done = Vec::new();
                    loop {
                        let r0 = next.fetch_add(KNN_ROWS, std::sync::atomic::Ordering::Relaxed);
                        if r0 >= n {
                            break;
                        }
                        let r1 = (r0 + KNN_ROWS).min(n);
                        let mut best = vec![Vec::with_capacity(k + 1); r1 - r0];
                        for c0 in (0..n).step_by(KNN_COLS) {
                            let c1 = (c0 + KNN_COLS).min(n);
                            for i in r0..r1 {
                                let a = &flat[i * dim..(i + 1) * dim];
                                for j in c0..c1 {
                                    if j == i {
                                        continue;
                                    }
                                    let d2 = norms[i] + norms[j]
                                        - 2.0 * dot(a, &flat[j * dim..(j + 1) * dim]);
                                    push_nearest(&mut best[i - r0], k, d2.max(0.0), j as u32);
                                }
                            }
                        }
                        done.push((r0, best));
                    }
                    done
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().expect("KNN worker panicked"))
            .collect()
    });
    blocks.sort_by_key(|(r0, _)| *r0);
    blocks.into_iter().flat_map(|(_, b)| b).collect()
}

impl EmbeddingKnn {
    /// Compute the `k` nearest same-type neighbours of every cached vector.
    pub fn build(cache: &EmbeddingCache, k: usize) -> anyhow::Result<Self> {
        let mut by_type = HashMap::new();
        for entry_type in KNOWN_TYPES {
            let rows = cache.all_vectors(entry_type)?;
            if rows.is_empty() || k == 0 {
                continue;
            }
            let started = std::time::Instant::now();
            let entry_ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
            let flat: Vec<f32> = rows.into_iter().flat_map(|(_, v)| v).collect();
            let neighbors = all_nearest(&flat, cache.dim, k)
                .into_iter()
                .map(|best| {
                    best.into_iter()
                        .map(|(d2, j)| (entry_ids[j as usize], (d2 as f64).sqrt()))
                        .collect()
                })
                .collect();
            info!(
                "Exact {k}-NN over {} {entry_type} vector(s) in {:.2?}",
                entry_ids.len(),
                started.elapsed()
            );
            by_type.insert(
                (*entry_type).to_string(),
                TypeKnn {
                    index_by_entry: entry_ids
                        .iter()
                        .enumerate()
                        .map(|(i, &id)| (id, i))
                        .collect(),
                    neighbors,
                },
            );
        }
        Ok(Self { by_type })
    }

    /// Up to `k` same-type neighbours as `(entry_id, L2 distance)`, nearest
    /// first (at most the `k` given to `build`).
    pub fn knn(&self, entry_id: i64, k: usize, entry_type: &str) -> Vec<(i64, f64)> {
        let Some(t) = self.by_type.get(entry_type) else {
            return vec![];
        };
        let Some(&i) = t.index_by_entry.get(&entry_id) else {
            return vec![];
        };
        t.neighbors[i].iter().take(k).copied().collect()
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
        // `title` is the text the entry was embedded from (see `embed_text`).
        // `embedding_cache_meta.model_id` names the model every vector came
        // from; a cache written before it was recorded counts as model "".
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS entry_embedding_meta (\
                entry_id INTEGER PRIMARY KEY, \
                title    TEXT NOT NULL \
             ); \
             CREATE TABLE IF NOT EXISTS embedding_cache_meta (\
                key   TEXT PRIMARY KEY, \
                value TEXT NOT NULL \
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

    /// The model id the cached vectors were made with ("" when none was recorded).
    pub fn model_id(&self) -> anyhow::Result<String> {
        let conn = self.conn.lock().unwrap();
        let id: Option<String> = conn
            .query_row(
                "SELECT value FROM embedding_cache_meta WHERE key = 'model_id'",
                [],
                |r| r.get(0),
            )
            .ok();
        Ok(id.unwrap_or_default())
    }

    /// Drop every cached vector and record `model_id` as the source of the
    /// vectors stored from now on.
    pub fn reset_for_model(&self, model_id: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let mut batch = String::from("BEGIN; DELETE FROM entry_embedding_meta;");
        for t in KNOWN_TYPES {
            batch.push_str(&format!(" DELETE FROM vec_embeddings_{t};"));
        }
        batch.push_str(" COMMIT;");
        conn.execute_batch(&batch)?;
        conn.execute(
            "INSERT OR REPLACE INTO embedding_cache_meta (key, value) VALUES ('model_id', ?)",
            params![model_id],
        )?;
        Ok(())
    }

    /// The `(entry_id, text, entry_type)` items whose embedding is missing or
    /// was made from a different text. Unknown entry types are skipped.
    pub fn stale_entries(&self, items: &[(i64, String, String)]) -> Vec<(i64, String, String)> {
        let conn = self.conn.lock().unwrap();
        // Two bulk reads instead of two lookups per entry.
        // (entry id → embedded text, vector table → entry ids)
        type Stored = (
            HashMap<i64, String>,
            HashMap<&'static str, std::collections::HashSet<i64>>,
        );
        let read = || -> rusqlite::Result<Stored> {
            let texts = conn
                .prepare("SELECT entry_id, title FROM entry_embedding_meta")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let mut stored = HashMap::new();
            for t in KNOWN_TYPES {
                let table = table_for_type(t).expect("known type");
                let ids = conn
                    .prepare(&format!("SELECT entry_id FROM {table}"))?
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                stored.insert(table, ids);
            }
            Ok((texts, stored))
        };
        let (texts, stored) = match read() {
            Ok(r) => r,
            Err(e) => {
                warn!("Reading the embedding cache failed: {e}; re-embedding everything");
                Default::default()
            }
        };
        items
            .iter()
            .filter(|(entry_id, text, entry_type)| {
                let Some(table) = table_for_type(entry_type) else {
                    return false;
                };
                // Text mismatch (or no meta row) → stale. Text matches but the
                // vector is absent from the type's table (e.g. entry_type
                // changed between runs) → also stale.
                texts.get(entry_id) != Some(text)
                    || !stored.get(table).is_some_and(|ids| ids.contains(entry_id))
            })
            .cloned()
            .collect()
    }

    /// Store an embedding vector for an entry.
    /// The vector should be unit-normalised so L2 ≡ cosine order.
    /// Entries with unrecognised types are silently skipped.
    pub fn upsert(
        &self,
        entry_id: i64,
        title: &str,
        entry_type: &str,
        vector: &[f64],
    ) -> anyhow::Result<()> {
        self.upsert_many(&[(entry_id, title, entry_type, vector)])
    }

    /// `upsert` for many entries in one transaction. Vectors go in as raw
    /// little-endian f32 blobs (sqlite-vec's native format), not JSON text.
    pub fn upsert_many(&self, rows: &[(i64, &str, &str, &[f64])]) -> anyhow::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        for &(entry_id, title, entry_type, vector) in rows {
            let Some(table) = table_for_type(entry_type) else {
                continue;
            };
            let blob: Vec<u8> = vector
                .iter()
                .flat_map(|&f| (f as f32).to_le_bytes())
                .collect();
            tx.prepare_cached(
                "INSERT OR REPLACE INTO entry_embedding_meta (entry_id, title) VALUES (?, ?)",
            )?
            .execute(params![entry_id, title])?;
            tx.prepare_cached(&format!(
                "INSERT OR REPLACE INTO {table} (entry_id, embedding) VALUES (?, ?)"
            ))?
            .execute(params![entry_id, blob])?;
        }
        tx.commit()?;
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

/// Texts per `embed_batch` call. Large, so a model that length-sorts its
/// input (the title encoder does) pads little and runs few forward passes.
const EMBED_BATCH_SIZE: usize = 512;

/// Call the Rhai `embed_batch(texts)` (preferred) or `embed(text)` (fallback)
/// for every stale entry and store results.
///
/// Must be called from inside `tokio::task::block_in_place` (or a thread without
/// an active tokio context) because `http_post_json` uses blocking IO.
///
/// The script's optional `embedding_model_id(ctx)` names its model; when that
/// differs from the cache's, every cached vector is dropped and re-embedded
/// (`full`), or, for a run that only embeds some entries (`!full`), left alone
/// and `false` returned so the caller skips semantic blocking rather than
/// mixing two models' vectors. Optional `embed_text(ctx, entry)` picks the text
/// each entry is embedded from (default: its best title).
///
/// Returns whether the cache holds this script's vectors and may be used for
/// semantic blocking.
pub fn embed_stale_entries(
    entries: &[EntryInfo],
    engine: &rhai::Engine,
    ast: &rhai::AST,
    base_scope: &rhai::Scope<'_>,
    user_ctx: &Dynamic,
    cache: &EmbeddingCache,
    full: bool,
) -> bool {
    // eval_ast=false: re-evaluating the AST would re-run the script's top-level
    // statements (and any `import`); we only want to invoke the function. The
    // context object from init() is threaded in as the first argument instead.
    let no_eval = || rhai::CallFnOptions::new().eval_ast(false);

    let model_id = match engine.call_fn_with_options::<Dynamic>(
        no_eval(),
        &mut base_scope.clone(),
        ast,
        "embedding_model_id",
        (user_ctx.clone(),),
    ) {
        Ok(v) => v
            .into_immutable_string()
            .map(|s| s.to_string())
            .unwrap_or_default(),
        Err(e) if is_fn_not_found(&e) => String::new(),
        Err(e) => {
            warn!("embedding_model_id() error: {e}; semantic blocking disabled");
            return false;
        }
    };
    let cached_model = match cache.model_id() {
        Ok(id) => id,
        Err(e) => {
            warn!("Reading the embedding cache's model id failed: {e}; semantic blocking disabled");
            return false;
        }
    };
    if cached_model != model_id {
        if !full {
            warn!(
                "Embedding cache holds {cached_model:?} vectors but the script embeds with \
                 {model_id:?}; semantic blocking disabled until a full softmatch run re-embeds it"
            );
            return false;
        }
        info!(
            "Embedding model changed ({cached_model:?} → {model_id:?}); re-embedding every entry"
        );
        if let Err(e) = cache.reset_for_model(&model_id) {
            warn!("Resetting the embedding cache failed: {e}; semantic blocking disabled");
            return false;
        }
    }

    let items = entry_texts(entries, engine, ast, base_scope, user_ctx);
    let stale = cache.stale_entries(&items);
    if stale.is_empty() {
        return true;
    }
    tracing::info!("Embedding {} stale entries...", stale.len());

    // Probe: call embed_batch(ctx, []) to check if the function exists without
    // doing real work (an empty batch returns immediately).
    let probe: Result<Dynamic, _> = engine.call_fn_with_options(
        no_eval(),
        &mut base_scope.clone(),
        ast,
        "embed_batch",
        (user_ctx.clone(), Vec::<Dynamic>::new()),
    );
    let has_batch = !matches!(&probe, Err(e) if is_fn_not_found(e));

    // Track the densest vector produced this pass; a low maximum is the
    // signature of the naive token-hash fallback (see `report_embed_health`).
    let mut max_nonzero = 0usize;
    let embedded = if has_batch {
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
    if embedded {
        report_embed_health(cache.dim, max_nonzero);
    }
    // A script without embed hooks leaves the cache as an earlier run stored it.
    true
}

/// `(entry_id, text, entry_type)` for every embeddable entry: the text is the
/// script's `embed_text(ctx, entry)` when defined (`()` skips the entry),
/// otherwise the entry's best title.
fn entry_texts(
    entries: &[EntryInfo],
    engine: &rhai::Engine,
    ast: &rhai::AST,
    base_scope: &rhai::Scope<'_>,
    user_ctx: &Dynamic,
) -> Vec<(i64, String, String)> {
    let mut has_hook = true;
    let mut failures = 0usize;
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        if table_for_type(&e.entry_type).is_none() {
            continue;
        }
        let Some(title) = &e.best_title else {
            continue;
        };
        if has_hook {
            let result = engine.call_fn_with_options::<Dynamic>(
                rhai::CallFnOptions::new().eval_ast(false),
                &mut base_scope.clone(),
                ast,
                "embed_text",
                (
                    user_ctx.clone(),
                    Dynamic::from(ScriptEntry(Arc::new(e.clone()), Dynamic::UNIT)),
                ),
            );
            match result {
                Ok(v) if v.is_unit() => continue,
                Ok(v) => match v.into_immutable_string() {
                    Ok(text) => {
                        out.push((e.entry_id, text.to_string(), e.entry_type.clone()));
                        continue;
                    }
                    Err(t) => {
                        failures += 1;
                        if failures <= 5 {
                            warn!(
                                "embed_text() returned {t} for entry {}; using its title",
                                e.entry_id
                            );
                        }
                    }
                },
                Err(err) if is_fn_not_found(&err) => has_hook = false,
                Err(err) => {
                    failures += 1;
                    if failures <= 5 {
                        warn!(
                            "embed_text() error for entry {}: {err}; using its title",
                            e.entry_id
                        );
                    }
                }
            }
        }
        out.push((e.entry_id, title.clone(), e.entry_type.clone()));
    }
    if failures > 5 {
        warn!("embed_text() failed for {failures} entries in total");
    }
    out
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
                let arrays: Vec<Vec<f64>> = vectors
                    .into_iter()
                    .map(|vec_dyn| {
                        vec_dyn
                            .try_cast::<Vec<Dynamic>>()
                            .unwrap_or_default()
                            .iter()
                            .filter_map(|d| d.as_float().ok())
                            .collect()
                    })
                    .collect();
                let mut rows = Vec::with_capacity(arrays.len());
                for ((entry_id, title, entry_type), arr) in chunk.iter().zip(&arrays) {
                    *max_nonzero = (*max_nonzero).max(nonzero_dims(arr));
                    if checked_dim(cache, title, arr) {
                        rows.push((
                            *entry_id,
                            title.as_str(),
                            entry_type.as_str(),
                            arr.as_slice(),
                        ));
                    }
                }
                if let Err(e) = cache.upsert_many(&rows) {
                    warn!("Failed to store {} embeddings: {e}", rows.len());
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

/// Whether `arr` has the cache's dimension (warning when it doesn't).
fn checked_dim(cache: &EmbeddingCache, title: &str, arr: &[f64]) -> bool {
    if arr.len() != cache.dim {
        warn!(
            "embed({title:?}) returned {} values, expected {}; skipping",
            arr.len(),
            cache.dim
        );
        return false;
    }
    true
}

fn store_vec(cache: &EmbeddingCache, entry_id: i64, title: &str, entry_type: &str, arr: &[f64]) {
    if !checked_dim(cache, title, arr) {
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
    fn knn_reads_cached_vectors_and_returns_nearest_entry() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("embeddings.db");
        let cache = EmbeddingCache::open(path.to_str().unwrap(), 2).unwrap();
        cache.upsert(1, "one", "track", &[1.0, 0.0]).unwrap();
        cache.upsert(2, "two", "track", &[0.99, 0.01]).unwrap();
        cache.upsert(3, "three", "track", &[0.0, 1.0]).unwrap();

        let index = EmbeddingKnn::build(&cache, 2).unwrap();
        let neighbors = index.knn(1, 1, "track");
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].0, 2);
        assert!(neighbors[0].1 < 0.02);
    }
}
