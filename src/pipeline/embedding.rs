use std::sync::{Arc, Mutex, Once};

use anyhow::Context as _;
use rhai::{Dynamic, Engine, Map as RhaiMap};
use rusqlite::{Connection, params};
use tracing::warn;

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
            if table_for_type(&e.entry_type).is_none() {
                continue;
            }
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
            match cached {
                Some(ct) if ct == *title => {}
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

    if has_batch {
        embed_via_batch(engine, ast, base_scope, user_ctx, cache, &stale)
    } else {
        embed_via_single(engine, ast, base_scope, user_ctx, cache, &stale)
    }
}

fn embed_via_batch(
    engine: &rhai::Engine,
    ast: &rhai::AST,
    base_scope: &rhai::Scope<'_>,
    user_ctx: &Dynamic,
    cache: &EmbeddingCache,
    stale: &[(i64, String, String)],
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
