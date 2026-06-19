use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use rhai::{AST, Dynamic, Engine, ImmutableString, Map as RhaiMap, Scope};
use std::time::Instant;
use tracing::{info, warn};

use crate::pipeline::embedding::{EmbeddingCache, embed_stale_entries, register_http_fns};

use crate::musicdb::{AliasRow, ChildRow, ContribRow, MusicDb, SourceRow};
use crate::pipeline::dedup::DedupConfig;
use crate::providers::FetchProvider;

type Pair = (String, String);

/// All data needed about one entry for candidate scoring.
#[derive(Debug, Clone)]
pub struct EntryInfo {
    pub entry_id: i64,
    pub entry_type: String,
    pub pairs: Vec<Pair>,
    /// All known names, deduplicated case-insensitively.
    pub aliases: Vec<String>,
    /// Best title: primary alias if one exists, otherwise first alias.
    pub best_title: Option<String>,
    /// Best non-null duration across all pairs.
    pub duration_ms: Option<i64>,
    pub release_date: Option<String>,
    pub release_type: Option<String>,
    pub primary_type: Option<String>,
    /// For tracks/releases: entry IDs of credited artists.
    /// For artists: entry IDs of items this artist is credited on.
    pub peer_entry_ids: Vec<i64>,
    /// For tracks: (release_entry_id, disc_no, track_no) from entry_child edges.
    pub track_positions: Vec<(i64, Option<i32>, Option<i32>)>,
    /// (source, alias_name) pairs. Clean sources appear before video sources so
    /// `pick_main_title` and `pick_markers` prefer authoritative names.
    pub sourced_aliases: Vec<(String, String)>,
}

/// The verdict the Rhai script returns for a pair.
#[derive(Debug, Clone)]
pub enum Verdict {
    Merge {
        confidence: f64,
        reason: String,
    },
    Relate {
        kind: String,
        confidence: f64,
        reason: String,
    },
    Distinct,
}

pub struct SoftMatchConfig {
    pub script_path: String,
    /// Write RELATE decisions to the DB.
    pub apply_relates: bool,
    /// If set, write a CSV row for every candidate pair (including DISTINCT)
    /// to this path for manual quality review.
    pub csv_path: Option<String>,
    /// Path to the SQLite file used as the embedding cache.
    /// `None` disables semantic blocking entirely.
    pub embed_db_path: Option<String>,
    /// Embedding vector dimension — must match the model used in the Rhai `embed()`
    /// function. Default script + default inference backend is 256 (Model2Vec);
    /// use 384 when the inference cdylib is built with `--features minilm`.
    pub embed_dim: usize,
    /// Base number of semantic KNN neighbours per entry for blocking (applies to
    /// tracks/releases; artists and release groups are capped lower via
    /// `k_for_type`). Default: 20.
    pub embed_k: usize,
    /// Minimum cosine similarity to include a semantic pair as a blocking candidate.
    /// Default: 0.5 (permissive — the Rhai script does the real filtering).
    pub embed_sim_threshold: f64,
    /// Maximum number of KNN pages to walk per entry type. Each page widens the
    /// neighbour window by one `k_for_type` step; paging stops early once a type's
    /// per-page merge rate falls below `embed_page_merge_rate`. Default: 4.
    pub embed_max_pages: usize,
    /// Per-type page merge rate (merges / scored) required to fetch the next page.
    /// A high first-page merge rate suggests the `k` window is too small and more
    /// neighbours are worth scoring. Default: 0.5.
    pub embed_page_merge_rate: f64,
}

// ── Source classification ─────────────────────────────────────────────────────

/// Sources whose titles are noisy (artist prefix, "Music Video" suffix, etc.).
/// Clean sources (spotify, musicbrainz, discogs, …) use standard music title
/// formatting: "Main Title (Marker1) [Marker2]".
const VIDEO_SOURCES: &[&str] = &["youtube", "nicovideo", "soundcloud"];

// ── Title normalization ───────────────────────────────────────────────────────

/// Lowercase, keep only alphanumeric + CJK, collapse whitespace.
/// Good enough for blocking and Jaccard without external unicode crates.
fn normalize(s: &str) -> String {
    s.chars()
        .map(|c| {
            // Keep letters, digits, CJK/kana/hangul ranges, collapse the rest to spaces.
            if c.is_alphabetic() || c.is_numeric() {
                c.to_lowercase().next().unwrap_or(c)
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn token_jaccard(a: &str, b: &str) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let a_tokens: HashSet<&str> = a.split_whitespace().collect();
    let b_tokens: HashSet<&str> = b.split_whitespace().collect();
    let inter = a_tokens.intersection(&b_tokens).count();
    let union = a_tokens.union(&b_tokens).count();
    if union == 0 {
        1.0
    } else {
        inter as f64 / union as f64
    }
}

fn jaccard_i64(a: &[i64], b: &[i64]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let a_set: HashSet<i64> = a.iter().copied().collect();
    let b_set: HashSet<i64> = b.iter().copied().collect();
    let inter = a_set.intersection(&b_set).count();
    let union = a_set.union(&b_set).count();
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

// ── Lazy features (internal — used only for CSV diagnostic output) ────────────

#[derive(Clone)]
struct LazyFeatures {
    a_duration: Option<i64>,
    b_duration: Option<i64>,
    a_peers: Vec<i64>,
    b_peers: Vec<i64>,
    a_positions: Vec<(i64, Option<i32>, Option<i32>)>,
    b_positions: Vec<(i64, Option<i32>, Option<i32>)>,
    c_dur_known: Option<bool>,
    c_dur_delta_ms: Option<i64>,
    c_artist_overlap: Option<f64>,
    c_same_release: Option<bool>,
    c_same_release_position: Option<bool>,
}

impl LazyFeatures {
    fn new(a: &EntryInfo, b: &EntryInfo) -> Self {
        Self {
            a_duration: a.duration_ms,
            b_duration: b.duration_ms,
            a_peers: a.peer_entry_ids.clone(),
            b_peers: b.peer_entry_ids.clone(),
            a_positions: a.track_positions.clone(),
            b_positions: b.track_positions.clone(),
            c_dur_known: None,
            c_dur_delta_ms: None,
            c_artist_overlap: None,
            c_same_release: None,
            c_same_release_position: None,
        }
    }

    fn dur_known(&mut self) -> bool {
        *self
            .c_dur_known
            .get_or_insert_with(|| self.a_duration.is_some() && self.b_duration.is_some())
    }
    fn dur_delta_ms(&mut self) -> i64 {
        *self
            .c_dur_delta_ms
            .get_or_insert_with(|| match (self.a_duration, self.b_duration) {
                (Some(da), Some(db)) => (da - db).abs(),
                _ => 0,
            })
    }
    fn artist_overlap(&mut self) -> f64 {
        *self
            .c_artist_overlap
            .get_or_insert_with(|| jaccard_i64(&self.a_peers, &self.b_peers))
    }
    fn same_release(&mut self) -> bool {
        *self.c_same_release.get_or_insert_with(|| {
            let a_releases: HashSet<i64> = self.a_positions.iter().map(|(r, _, _)| *r).collect();
            self.b_positions
                .iter()
                .any(|(r, _, _)| a_releases.contains(r))
        })
    }
    fn same_release_position(&mut self) -> bool {
        *self.c_same_release_position.get_or_insert_with(|| {
            let a_set: HashSet<(i64, Option<i32>, Option<i32>)> = self
                .a_positions
                .iter()
                .filter(|(_, _, t)| t.is_some())
                .copied()
                .collect();
            self.b_positions
                .iter()
                .filter(|(_, _, t)| t.is_some())
                .any(|pos| a_set.contains(pos))
        })
    }
}

// ── Rhai engine ───────────────────────────────────────────────────────────────

type RegexCache = Arc<Mutex<HashMap<String, regex::Regex>>>;

fn get_or_compile(
    cache: &Mutex<HashMap<String, regex::Regex>>,
    pattern: &str,
) -> Result<regex::Regex, regex::Error> {
    let mut c = cache.lock().unwrap();
    if let Some(re) = c.get(pattern) {
        return Ok(re.clone());
    }
    let re = regex::Regex::new(pattern)?;
    c.insert(pattern.to_string(), re.clone());
    Ok(re)
}

fn build_rhai_engine(
    script_dir: &Path,
    regex_cache: RegexCache,
    embed_cache: Option<Arc<EmbeddingCache>>,
) -> Engine {
    let mut engine = Engine::new();
    engine.set_max_expr_depths(0, 0); // no limit on expression or function-body nesting depth

    // Generic FFI (feature `ffi`): the script binds any cdylib's C symbols
    // itself via `ffi::open(...)` / `.func(...)` / `.invoke(...)` (see
    // `pipeline::ffi`). Registered as a STATIC module so it's visible in every
    // call context, including `call_fn` from Rust (which builds a fresh
    // GlobalRuntimeState without dynamic imports). All values cross as plain C
    // types, so there's no rhai-version/TypeId coupling between host and plugin.
    #[cfg(feature = "ffi")]
    engine.register_static_module(
        "ffi",
        crate::pipeline::ffi::module(script_dir.to_path_buf()).into(),
    );

    // Let scripts detect at runtime whether the `ffi` module is available, so a
    // script can pick a backend (e.g. real embeddings vs a naive fallback)
    // without being edited per build. Pairs with a `try { ffi::open(...) }` guard
    // for the library-actually-loads case.
    let ffi_on = cfg!(feature = "ffi");
    engine.register_fn("ffi_available", move || ffi_on);

    // Enable `import` for plain `.rhai` modules, resolved relative to the
    // script's directory (not the process CWD).
    engine.set_module_resolver(rhai::module_resolvers::FileModuleResolver::new_with_path(
        script_dir,
    ));

    // Verdict constructors — the script calls these to return its decision.
    engine.register_fn("merge", |conf: f64, reason: String| -> RhaiMap {
        let mut m = RhaiMap::new();
        m.insert("verdict".into(), Dynamic::from("merge".to_string()));
        m.insert("confidence".into(), Dynamic::from(conf));
        m.insert("reason".into(), Dynamic::from(reason));
        m
    });
    engine.register_fn(
        "relate",
        |kind: String, conf: f64, reason: String| -> RhaiMap {
            let mut m = RhaiMap::new();
            m.insert("verdict".into(), Dynamic::from("relate".to_string()));
            m.insert("kind".into(), Dynamic::from(kind));
            m.insert("confidence".into(), Dynamic::from(conf));
            m.insert("reason".into(), Dynamic::from(reason));
            m
        },
    );
    engine.register_fn("distinct", || -> RhaiMap {
        let mut m = RhaiMap::new();
        m.insert("verdict".into(), Dynamic::from("distinct".to_string()));
        m
    });

    // ── Low-level string primitives ───────────────────────────────────────────
    engine.register_fn("normalize", |s: String| -> String { normalize(&s) });
    engine.register_fn("jaccard", |a: String, b: String| -> f64 {
        token_jaccard(&a, &b)
    });
    engine.register_fn("levenshtein", |a: String, b: String| -> f64 {
        strsim::normalized_levenshtein(&a, &b)
    });
    engine.register_fn("jaro_winkler", |a: String, b: String| -> f64 {
        strsim::jaro_winkler(&a, &b)
    });
    engine.register_fn("str_sim", |a: String, b: String| -> f64 {
        strsim::normalized_levenshtein(&a, &b).max(token_jaccard(&a, &b))
    });
    engine.register_fn("join", |arr: Vec<Dynamic>, sep: String| -> String {
        arr.iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join(&sep)
    });
    engine.register_fn(
        "any",
        |arr: Vec<Dynamic>, f: rhai::FnPtr| -> Result<bool, Box<rhai::EvalAltResult>> {
            Ok(arr.into_iter().any(|x| {
                f.call::<bool>(&Engine::new(), &AST::empty(), (x,))
                    .unwrap_or(false)
            }))
        },
    );
    engine.register_fn(
        "all",
        |arr: Vec<Dynamic>, f: rhai::FnPtr| -> Result<bool, Box<rhai::EvalAltResult>> {
            Ok(arr.into_iter().all(|x| {
                f.call::<bool>(&Engine::new(), &AST::empty(), (x,))
                    .unwrap_or(false)
            }))
        },
    );

    // HTTP primitive — used by the Rhai embed() function to call the embedding server.
    register_http_fns(&mut engine);

    // semantic_sim(a_entry_id, b_entry_id) → f64
    // Returns cosine similarity [0, 1] between two cached embeddings.
    // Returns 0.0 when either entry has no stored embedding or no cache is configured.
    if let Some(cache) = embed_cache {
        engine.register_fn("semantic_sim", move |a: i64, b: i64| -> f64 {
            cache.cosine_similarity(a, b).unwrap_or(0.0).max(0.0)
        });
    } else {
        engine.register_fn("semantic_sim", |_a: i64, _b: i64| -> f64 { 0.0 });
    }

    // ── Regex primitives ──────────────────────────────────────────────────────
    // `compile_re(pattern)` — compile once, store in a variable, reuse in decide().
    // The three re_* functions are overloaded: pass either a pre-compiled Regex or
    // a pattern String (compiled on first use and cached for the engine lifetime).
    engine.register_type_with_name::<regex::Regex>("Regex");
    engine.register_fn(
        "compile_re",
        |pattern: String| -> Result<regex::Regex, Box<rhai::EvalAltResult>> {
            regex::Regex::new(&pattern).map_err(|e| {
                Box::new(rhai::EvalAltResult::ErrorRuntime(
                    format!("compile_re: bad pattern {pattern:?}: {e}").into(),
                    rhai::Position::NONE,
                ))
            })
        },
    );

    // Overloads that accept a pre-compiled Regex:
    engine.register_fn("re_is_match", |re: regex::Regex, text: String| -> bool {
        re.is_match(&text)
    });
    engine.register_fn(
        "re_replace_all",
        |re: regex::Regex, replacement: String, text: String| -> String {
            re.replace_all(&text, regex::NoExpand(replacement.as_str()))
                .into_owned()
        },
    );
    engine.register_fn(
        "re_captures_all",
        |re: regex::Regex, text: String| -> Vec<Dynamic> {
            re.captures_iter(&text)
                .map(|caps| {
                    let arr: Vec<Dynamic> = caps
                        .iter()
                        .map(|m| Dynamic::from(m.map_or("", |m| m.as_str()).to_string()))
                        .collect();
                    Dynamic::from(arr)
                })
                .collect()
        },
    );

    // Overloads that accept a pattern String (compiled lazily, cached per engine):
    {
        let c = regex_cache.clone();
        engine.register_fn(
            "re_is_match",
            move |pattern: String, text: String| -> bool {
                match get_or_compile(&c, &pattern) {
                    Ok(re) => re.is_match(&text),
                    Err(e) => {
                        warn!("re_is_match: bad pattern {pattern:?}: {e}");
                        false
                    }
                }
            },
        );
    }
    {
        let c = regex_cache.clone();
        engine.register_fn(
            "re_replace_all",
            move |pattern: String, replacement: String, text: String| -> String {
                match get_or_compile(&c, &pattern) {
                    Ok(re) => re
                        .replace_all(&text, regex::NoExpand(replacement.as_str()))
                        .into_owned(),
                    Err(e) => {
                        warn!("re_replace_all: bad pattern {pattern:?}: {e}");
                        text
                    }
                }
            },
        );
    }
    {
        let c = regex_cache;
        // Returns [[full_match, group1, group2, …], …]. Unmatched optional groups are "".
        engine.register_fn(
            "re_captures_all",
            move |pattern: String, text: String| -> Vec<Dynamic> {
                match get_or_compile(&c, &pattern) {
                    Err(e) => {
                        warn!("re_captures_all: bad pattern {pattern:?}: {e}");
                        vec![]
                    }
                    Ok(re) => re
                        .captures_iter(&text)
                        .map(|caps| {
                            let arr: Vec<Dynamic> = caps
                                .iter()
                                .map(|m| Dynamic::from(m.map_or("", |m| m.as_str()).to_string()))
                                .collect();
                            Dynamic::from(arr)
                        })
                        .collect(),
                }
            },
        );
    }

    engine
}

fn entry_to_rhai(e: &EntryInfo) -> RhaiMap {
    let mut m = RhaiMap::new();
    m.insert("entry_type".into(), Dynamic::from(e.entry_type.clone()));
    m.insert(
        "title".into(),
        Dynamic::from(e.best_title.clone().unwrap_or_default()),
    );
    m.insert(
        "duration_ms".into(),
        e.duration_ms.map_or(Dynamic::UNIT, Dynamic::from),
    );
    m.insert(
        "release_date".into(),
        e.release_date.clone().map_or(Dynamic::UNIT, Dynamic::from),
    );
    // sourced_aliases: array of #{source, name} maps, clean sources first.
    let sa: Vec<Dynamic> = e
        .sourced_aliases
        .iter()
        .map(|(src, name)| {
            let mut mm = RhaiMap::new();
            mm.insert("source".into(), Dynamic::from(src.clone()));
            mm.insert("name".into(), Dynamic::from(name.clone()));
            Dynamic::from_map(mm)
        })
        .collect();
    m.insert("sourced_aliases".into(), Dynamic::from(sa));
    // pairs: [{source, identifier}, ...] — raw canonical DB pairs.
    let pairs_dyn: Vec<Dynamic> = e
        .pairs
        .iter()
        .map(|(src, id)| {
            let mut pm = RhaiMap::new();
            pm.insert("source".into(), Dynamic::from(src.clone()));
            pm.insert("identifier".into(), Dynamic::from(id.clone()));
            Dynamic::from_map(pm)
        })
        .collect();
    m.insert("pairs".into(), Dynamic::from(pairs_dyn));
    // aliases: flat deduplicated list of all known names.
    let aliases_dyn: Vec<Dynamic> = e.aliases.iter().cloned().map(Dynamic::from).collect();
    m.insert("aliases".into(), Dynamic::from(aliases_dyn));
    // peer_ids: credited-artist entry IDs for tracks/releases; credited-track
    // entry IDs for artists. Opaque integers — useful for set overlap checks.
    let peer_ids_dyn: Vec<Dynamic> = e
        .peer_entry_ids
        .iter()
        .copied()
        .map(Dynamic::from)
        .collect();
    m.insert("peer_ids".into(), Dynamic::from(peer_ids_dyn));
    // track_positions: [{release_id, disc_no, track_no}, …]. disc_no/track_no
    // are () when unknown.
    let pos_dyn: Vec<Dynamic> = e
        .track_positions
        .iter()
        .map(|(r, d, t)| {
            let mut pm = RhaiMap::new();
            pm.insert("release_id".into(), Dynamic::from(*r));
            pm.insert("disc_no".into(), d.map_or(Dynamic::UNIT, Dynamic::from));
            pm.insert("track_no".into(), t.map_or(Dynamic::UNIT, Dynamic::from));
            Dynamic::from_map(pm)
        })
        .collect();
    m.insert("track_positions".into(), Dynamic::from(pos_dyn));
    m
}

fn call_script(
    engine: &Engine,
    ast: &AST,
    base_scope: &Scope,
    user_ctx: &Dynamic,
    a: &EntryInfo,
    b: &EntryInfo,
) -> anyhow::Result<Verdict> {
    let mut scope = base_scope.clone();
    let a_dyn = Dynamic::from_map(entry_to_rhai(a));
    let b_dyn = Dynamic::from_map(entry_to_rhai(b));

    let result: Dynamic = engine
        .call_fn(&mut scope, ast, "decide", (user_ctx.clone(), a_dyn, b_dyn))
        .unwrap_or_else(|e| {
            warn!("Rhai decide() error: {e}");
            Dynamic::from_map(RhaiMap::new())
        });

    let map = match result.try_cast::<RhaiMap>() {
        Some(m) => m,
        None => return Ok(Verdict::Distinct),
    };

    let rhai_str = |key: &str| -> Option<String> {
        map.get(key)
            .and_then(|v| v.clone().try_cast::<ImmutableString>())
            .map(|s| s.to_string())
    };
    let rhai_f64 = |key: &str| -> f64 {
        map.get(key)
            .and_then(|v| v.clone().try_cast::<f64>())
            .unwrap_or(0.0)
    };

    Ok(match rhai_str("verdict").as_deref() {
        Some("merge") => Verdict::Merge {
            confidence: rhai_f64("confidence"),
            reason: rhai_str("reason").unwrap_or_default(),
        },
        Some("relate") => Verdict::Relate {
            kind: rhai_str("kind").unwrap_or_else(|| "alt_version".to_string()),
            confidence: rhai_f64("confidence"),
            reason: rhai_str("reason").unwrap_or_default(),
        },
        _ => Verdict::Distinct,
    })
}

// ── Candidate blocking ────────────────────────────────────────────────────────

#[allow(dead_code)]
fn blocking_key(entry: &EntryInfo) -> Option<String> {
    let title = entry.best_title.as_deref()?;
    let norm = normalize(title);
    let first = norm.split_whitespace().next()?;
    if first.len() < 2 {
        return None;
    }
    Some(first.to_string())
}

/// If `focus` is Some, only emit pairs where at least one entry_id is in the set.
/// This lets callers do incremental comparisons (new entries vs. everything in
/// their blocking bucket) without re-scanning already-compared pairs.
#[allow(dead_code)]
fn generate_candidates(entries: &[EntryInfo], focus: Option<&HashSet<i64>>) -> Vec<(i64, i64)> {
    // Group by (entry_type, first_title_token). Only same-type pairs.
    let mut blocks: HashMap<(String, String), Vec<i64>> = HashMap::new();
    for e in entries {
        if e.entry_type == "unknown" {
            continue;
        }
        if let Some(key) = blocking_key(e) {
            blocks
                .entry((e.entry_type.clone(), key))
                .or_default()
                .push(e.entry_id);
        }
    }

    let mut pairs: Vec<(i64, i64)> = Vec::new();
    for ((et, _key), group) in &blocks {
        if group.len() > 200 {
            warn!(
                "Skipping oversized block ({} entries, type={et}) — too large to compare O(n²)",
                group.len()
            );
            continue;
        }
        for i in 0..group.len() {
            for j in (i + 1)..group.len() {
                if focus.is_some_and(|f| !f.contains(&group[i]) && !f.contains(&group[j])) {
                    continue;
                }
                let a = group[i].min(group[j]);
                let b = group[i].max(group[j]);
                pairs.push((a, b));
            }
        }
    }

    pairs.sort_unstable();
    pairs.dedup();
    pairs
}

/// Per-entry-type KNN fan-out for semantic blocking.
///
/// Higher-cardinality, more-distinctive types (tracks, releases) tolerate a large
/// neighbour count, but coarse types (artists, release groups) have few true
/// duplicates and a large `k` only floods the scorer with noise, so they are
/// capped well below the base. `base_k` is the configured `embed_k` and applies
/// to tracks/releases and any unrecognised type.
fn k_for_type(entry_type: &str, base_k: usize) -> usize {
    match entry_type {
        "artist" => 3.min(base_k),
        "release_group" => 5.min(base_k),
        _ => base_k,
    }
}

/// Semantic blocking, one KNN *page* at a time. A page covers neighbour ranks
/// `[page * page_k, (page + 1) * page_k)` for each entry, where `page_k =
/// k_for_type(type, base_k)`. Only entries whose type is in `active` are walked,
/// and pairs already emitted on an earlier page (tracked in `already`) are
/// skipped, so callers can keep requesting pages until merge rate drops off.
///
/// `sim_threshold` is a cosine *similarity* lower bound (≥ 0); distance in vec0 is
/// L2 on unit vectors, which has the same ordering as cosine distance. Because
/// neighbours come back sorted by distance, the first one past the threshold ends
/// the walk for that entry — any further (and any later page's) neighbours are
/// strictly farther.
#[allow(clippy::too_many_arguments)]
fn generate_semantic_candidates(
    entries: &[EntryInfo],
    cache: &EmbeddingCache,
    focus: Option<&HashSet<i64>>,
    base_k: usize,
    sim_threshold: f64,
    page: usize,
    active: &HashSet<String>,
    already: &mut HashSet<(i64, i64)>,
) -> Vec<(i64, i64)> {
    // Convert cosine similarity threshold → L2 distance threshold.
    // For unit vectors: L2² = 2(1 − cos_sim), so L2 = √(2(1 − cos_sim)).
    let l2_threshold = (2.0 * (1.0 - sim_threshold)).sqrt();

    let mut pairs: Vec<(i64, i64)> = Vec::new();
    for e in entries {
        if e.best_title.is_none() {
            continue;
        }
        if !active.contains(&e.entry_type) {
            continue;
        }
        if focus.is_some_and(|f| !f.contains(&e.entry_id)) {
            continue;
        }
        let page_k = k_for_type(&e.entry_type, base_k);
        let want = (page + 1) * page_k;
        // KNN is now type-local: all k slots go to same-type neighbours.
        let neighbors = match cache.knn(e.entry_id, want, &e.entry_type) {
            Ok(n) => n,
            Err(err) => {
                warn!("semantic KNN failed for entry {}: {err}", e.entry_id);
                continue;
            }
        };
        // Skip the ranks already revealed on earlier pages.
        for (neighbor_id, dist) in neighbors.into_iter().skip(page * page_k) {
            if dist > l2_threshold {
                break;
            }
            let a = e.entry_id.min(neighbor_id);
            let b = e.entry_id.max(neighbor_id);
            if already.insert((a, b)) {
                pairs.push((a, b));
            }
        }
    }
    pairs
}

// ── Barrier veto check ────────────────────────────────────────────────────────

fn barrier_blocks(
    pairs_a: &[Pair],
    pairs_b: &[Pair],
    barrier: &HashMap<Pair, crate::pipeline::dedup::AnchorId>,
) -> bool {
    let anchor_a = pairs_a.iter().find_map(|p| barrier.get(p));
    let anchor_b = pairs_b.iter().find_map(|p| barrier.get(p));
    matches!((anchor_a, anchor_b), (Some(a), Some(b)) if a != b)
}

// ── Data loading ──────────────────────────────────────────────────────────────

async fn build_entry_infos(db: &MusicDb) -> anyhow::Result<Vec<EntryInfo>> {
    let entry_rows = db.all_entry_rows().await?;
    let source_rows: Vec<SourceRow> = db.all_source_rows().await?;
    let alias_rows: Vec<AliasRow> = db.all_alias_rows().await?;
    let contrib_rows: Vec<ContribRow> = db.all_contrib_rows().await?;
    let child_rows: Vec<ChildRow> = db.all_child_rows().await?;

    // pair → entry_id index
    let pair_to_entry: HashMap<Pair, i64> = source_rows
        .iter()
        .map(|s| ((s.source.clone(), s.identifier.clone()), s.entry_id))
        .collect();

    // entry_id → sources
    let mut sources_by_entry: HashMap<i64, Vec<&SourceRow>> = HashMap::new();
    for s in &source_rows {
        sources_by_entry.entry(s.entry_id).or_default().push(s);
    }

    // pair → aliases
    let mut aliases_by_pair: HashMap<Pair, Vec<&AliasRow>> = HashMap::new();
    for a in &alias_rows {
        aliases_by_pair
            .entry((a.source.clone(), a.identifier.clone()))
            .or_default()
            .push(a);
    }

    // track positions: child_entry_id → [(release_entry_id, disc_no, track_no)]
    #[allow(clippy::type_complexity)]
    let mut positions_by_track: HashMap<i64, Vec<(i64, Option<i32>, Option<i32>)>> = HashMap::new();
    for c in &child_rows {
        let parent_key = (c.parent_source.clone(), c.parent_identifier.clone());
        let child_key = (c.child_source.clone(), c.child_identifier.clone());
        if let (Some(&parent_id), Some(&child_id)) = (
            pair_to_entry.get(&parent_key),
            pair_to_entry.get(&child_key),
        ) {
            positions_by_track
                .entry(child_id)
                .or_default()
                .push((parent_id, c.disc_no, c.track_no));
        }
    }

    // credited-artist relationships
    let mut track_to_artists: HashMap<i64, HashSet<i64>> = HashMap::new();
    let mut artist_to_tracks: HashMap<i64, HashSet<i64>> = HashMap::new();
    for c in &contrib_rows {
        let track_key = (c.source.clone(), c.identifier.clone());
        let artist_key = (c.artist_source.clone(), c.artist_identifier.clone());
        if let (Some(&te), Some(&ae)) = (
            pair_to_entry.get(&track_key),
            pair_to_entry.get(&artist_key),
        ) {
            track_to_artists.entry(te).or_default().insert(ae);
            artist_to_tracks.entry(ae).or_default().insert(te);
        }
    }

    let mut infos = Vec::with_capacity(entry_rows.len());
    for e in &entry_rows {
        let sources = sources_by_entry
            .get(&e.id)
            .map_or(&[][..], |v| v.as_slice());
        let pairs: Vec<Pair> = sources
            .iter()
            .map(|s| (s.source.clone(), s.identifier.clone()))
            .collect();

        let duration_ms = sources.iter().find_map(|s| s.duration_ms);
        let release_date = sources.iter().find_map(|s| s.release_date.clone());
        let release_type = sources.iter().find_map(|s| s.release_type.clone());
        let primary_type = sources.iter().find_map(|s| s.primary_type.clone());

        // Collect aliases: prefer primary, deduplicate case-insensitively.
        // sourced_aliases puts clean sources before video sources so that
        // pick_main_title / pick_markers always see authoritative names first.
        let mut seen_lower: HashSet<String> = HashSet::new();
        let mut primary_alias: Option<String> = None;
        let mut aliases: Vec<String> = Vec::new();
        let mut sourced_aliases: Vec<(String, String)> = Vec::new();
        for pass in 0..2usize {
            for (src, id) in &pairs {
                let is_video = VIDEO_SOURCES.contains(&src.as_str());
                if (pass == 0) == is_video {
                    continue; // pass 0 = clean sources, pass 1 = video sources
                }
                if let Some(pair_aliases) = aliases_by_pair.get(&(src.clone(), id.clone())) {
                    let mut sorted: Vec<&&AliasRow> = pair_aliases.iter().collect();
                    sorted.sort_by_key(|a| !a.primary_alias);
                    for a in sorted {
                        if primary_alias.is_none() && a.primary_alias && pass == 0 {
                            primary_alias = Some(a.name.clone());
                        }
                        if seen_lower.insert(a.name.to_lowercase()) {
                            aliases.push(a.name.clone());
                        }
                        sourced_aliases.push((src.clone(), a.name.clone()));
                    }
                }
            }
        }
        let best_title = primary_alias.or_else(|| aliases.first().cloned());

        let peer_entry_ids: Vec<i64> = if e.entry_type == "artist" {
            artist_to_tracks
                .get(&e.id)
                .map(|s| s.iter().copied().collect())
                .unwrap_or_default()
        } else {
            track_to_artists
                .get(&e.id)
                .map(|s| s.iter().copied().collect())
                .unwrap_or_default()
        };

        let track_positions = positions_by_track.get(&e.id).cloned().unwrap_or_default();

        infos.push(EntryInfo {
            entry_id: e.id,
            entry_type: e.entry_type.clone(),
            pairs,
            aliases,
            best_title,
            duration_ms,
            release_date,
            release_type,
            primary_type,
            peer_entry_ids,
            track_positions,
            sourced_aliases,
        });
    }

    Ok(infos)
}

// ── Report helpers ────────────────────────────────────────────────────────────

fn fmt_pairs(pairs: &[Pair]) -> String {
    pairs
        .iter()
        .map(|(s, id)| format!("{s}:{id}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Wrap a string value in CSV double-quotes, escaping any internal quotes.
fn csv_field(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn fmt_pairs_csv(pairs: &[Pair]) -> String {
    pairs
        .iter()
        .map(|(s, id)| format!("{s}:{id}"))
        .collect::<Vec<_>>()
        .join(" | ")
}

// ── Shared scoring loop ───────────────────────────────────────────────────────

struct ScriptCtx<'a> {
    engine: Engine,
    ast: AST,
    base_scope: Scope<'a>,
    /// Opaque context object returned by the script's `init()` and threaded back
    /// as the first argument of every script entry point (`decide`, `embed_batch`,
    /// …). The script decides what it holds — e.g. a handle to an `ffi`-opened
    /// inference library — and keeping it alive here keeps that state alive for
    /// the whole run. `()` when the script defines no `init()`.
    user_ctx: Dynamic,
}

impl Drop for ScriptCtx<'_> {
    fn drop(&mut self) {
        // Call optional destroy(ctx) hook; silently ignore "function not found".
        let ctx = self.user_ctx.clone();
        let _ = self
            .engine
            .call_fn::<Dynamic>(&mut self.base_scope, &self.ast, "destroy", (ctx,));
    }
}

async fn load_script(
    path: &str,
    embed_cache: Option<Arc<EmbeddingCache>>,
) -> anyhow::Result<ScriptCtx<'static>> {
    let script = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("reading script {path}"))?;
    let script_dir = Path::new(path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_owned();
    let regex_cache: RegexCache = Arc::new(Mutex::new(HashMap::new()));
    let engine = build_rhai_engine(&script_dir, regex_cache, embed_cache);
    let ast = engine
        .compile(&script)
        .map_err(|e| anyhow::anyhow!("Rhai compile error in {path}: {e}"))?;
    let mut base_scope = Scope::new();
    engine
        .run_ast_with_scope(&mut base_scope, &ast)
        .map_err(|e| anyhow::anyhow!("Script init error in {path}: {e}"))?;
    // Call optional init() hook; its return value is the opaque context object
    // threaded into every entry point. Missing/erroring init → unit context.
    let user_ctx = engine
        .call_fn::<Dynamic>(&mut base_scope, &ast, "init", ())
        .unwrap_or(Dynamic::UNIT);
    Ok(ScriptCtx {
        engine,
        ast,
        base_scope,
        user_ctx,
    })
}

/// Score every candidate pair produced by `generate_candidates` with the given
/// focus restriction. Prints each non-distinct verdict; applies DB writes when
/// `config.apply_relates` is true. Returns `[merge, relate, distinct]` counts
/// per entry type.
async fn score_candidates(
    db: &MusicDb,
    entries: &[EntryInfo],
    focus: Option<&HashSet<i64>>,
    ctx: &ScriptCtx<'_>,
    barrier: &HashMap<Pair, crate::pipeline::dedup::AnchorId>,
    config: &SoftMatchConfig,
    embed_cache: Option<&EmbeddingCache>,
) -> anyhow::Result<HashMap<String, [usize; 3]>> {
    let entry_map: HashMap<i64, &EntryInfo> = entries.iter().map(|e| (e.entry_id, e)).collect();

    // Optional CSV output for manual quality review.
    let mut csv: Option<std::io::BufWriter<std::fs::File>> = if let Some(path) = &config.csv_path {
        let f = std::fs::File::create(path).with_context(|| format!("creating CSV file {path}"))?;
        let mut w = std::io::BufWriter::new(f);
        writeln!(
            w,
            "verdict,kind,confidence,reason,type,\
             entry_a,title_a,sources_a,\
             entry_b,title_b,sources_b,\
             main_title_sim,markers_conflict,\
             dur_known,dur_delta_ms,\
             same_release,same_release_position,artist_overlap"
        )?;
        Some(w)
    } else {
        None
    };

    let mut stats: HashMap<String, [usize; 3]> = HashMap::new();

    let Some(cache) = embed_cache else {
        info!("Semantic blocking disabled (no embedding cache); no candidates to score.");
        return Ok(stats);
    };

    // Types still worth paging deeper into. Seeded with every concrete type
    // present; a type drops out once a page's merge rate falls below the
    // configured floor (or the page yields nothing new).
    let mut active: HashSet<String> = entries
        .iter()
        .map(|e| e.entry_type.clone())
        .filter(|t| t != "unknown")
        .collect();

    let mut already: HashSet<(i64, i64)> = HashSet::new();
    let mut total_scored = 0usize;

    for page in 0..config.embed_max_pages.max(1) {
        if active.is_empty() {
            break;
        }
        let page_pairs = generate_semantic_candidates(
            entries,
            cache,
            focus,
            config.embed_k,
            config.embed_sim_threshold,
            page,
            &active,
            &mut already,
        );
        if page_pairs.is_empty() {
            break;
        }
        info!(
            "Semantic page {page}: scoring {} candidate pair(s) across {} active type(s)",
            page_pairs.len(),
            active.len()
        );
        total_scored += page_pairs.len();

        // Per-type (merges, scored) for this page only — drives whether the type
        // is paged further.
        let mut page_rate: HashMap<String, (usize, usize)> = HashMap::new();

        for (id_a, id_b) in page_pairs {
            let (Some(ea), Some(eb)) = (entry_map.get(&id_a), entry_map.get(&id_b)) else {
                continue;
            };

            // Barrier-separated pairs: no RELATE between deliberately distinct entities.
            if barrier_blocks(&ea.pairs, &eb.pairs, barrier) {
                if let Some(w) = &mut csv {
                    write_csv_row(w, "BARRIER", "", 0.0, "", ea, eb, ctx)?;
                }
                continue;
            }

            let verdict = apply_candidate(db, ea, eb, ctx, config, &mut csv, &mut stats).await?;
            let rate = page_rate.entry(ea.entry_type.clone()).or_insert((0, 0));
            rate.1 += 1;
            if matches!(verdict, Verdict::Merge { .. }) {
                rate.0 += 1;
            }
        }

        // Continue paging only the types whose merge rate this page held up.
        active.retain(|t| match page_rate.get(t) {
            Some(&(merges, scored)) if scored > 0 => {
                merges as f64 / scored as f64 >= config.embed_page_merge_rate
            }
            _ => false,
        });
    }

    info!("Scored {total_scored} candidate pair(s)");
    Ok(stats)
}

/// Score one candidate pair with the Rhai script, emit its CSV row and console
/// log, apply the DB write when `apply_relates` is set, and bump `stats`. Returns
/// the verdict so the caller can measure per-page merge rate for adaptive paging.
async fn apply_candidate(
    db: &MusicDb,
    ea: &EntryInfo,
    eb: &EntryInfo,
    ctx: &ScriptCtx<'_>,
    config: &SoftMatchConfig,
    csv: &mut Option<std::io::BufWriter<std::fs::File>>,
    stats: &mut HashMap<String, [usize; 3]>,
) -> anyhow::Result<Verdict> {
    let verdict = call_script(
        &ctx.engine,
        &ctx.ast,
        &ctx.base_scope,
        &ctx.user_ctx,
        ea,
        eb,
    )?;

    if let Some(w) = csv {
        let (vname, kind, conf, reason) = match &verdict {
            Verdict::Merge { confidence, reason } => ("MERGE", "", *confidence, reason.as_str()),
            Verdict::Relate {
                kind,
                confidence,
                reason,
            } => ("RELATE", kind.as_str(), *confidence, reason.as_str()),
            Verdict::Distinct => ("DISTINCT", "", 0.0, ""),
        };
        write_csv_row(w, vname, kind, conf, reason, ea, eb, ctx)?;
    }

    let counters = stats.entry(ea.entry_type.clone()).or_insert([0; 3]);

    match &verdict {
        Verdict::Distinct => {
            counters[2] += 1;
        }
        Verdict::Merge { confidence, reason } => {
            counters[0] += 1;
            println!("[MERGE] conf={:.2}  type={}", confidence, ea.entry_type);
            println!(
                "  A: {:?} (entry {})\n     {}",
                ea.best_title.as_deref().unwrap_or("?"),
                ea.entry_id,
                fmt_pairs(&ea.pairs)
            );
            println!(
                "  B: {:?} (entry {})\n     {}",
                eb.best_title.as_deref().unwrap_or("?"),
                eb.entry_id,
                fmt_pairs(&eb.pairs)
            );
            println!("  reason: {reason}\n");

            if config.apply_relates {
                db.merge_entries(ea.entry_id, eb.entry_id)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                info!("Merged entry {} into {}", ea.entry_id, eb.entry_id);
            }
        }
        Verdict::Relate {
            kind,
            confidence,
            reason,
        } => {
            counters[1] += 1;
            println!(
                "[RELATE {}] conf={:.2}  type={}",
                kind, confidence, ea.entry_type
            );
            println!(
                "  A: {:?} (entry {})\n     {}",
                ea.best_title.as_deref().unwrap_or("?"),
                ea.entry_id,
                fmt_pairs(&ea.pairs)
            );
            println!(
                "  B: {:?} (entry {})\n     {}",
                eb.best_title.as_deref().unwrap_or("?"),
                eb.entry_id,
                fmt_pairs(&eb.pairs)
            );
            println!("  reason: {reason}\n");

            if config.apply_relates {
                db.upsert_relation(
                    ea.entry_id,
                    eb.entry_id,
                    kind,
                    *confidence,
                    "heuristic",
                    None,
                )
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            }
        }
    }

    Ok(verdict)
}

#[allow(clippy::too_many_arguments)]
fn write_csv_row(
    w: &mut impl std::io::Write,
    verdict: &str,
    kind: &str,
    confidence: f64,
    reason: &str,
    ea: &EntryInfo,
    eb: &EntryInfo,
    ctx: &ScriptCtx,
) -> anyhow::Result<()> {
    let mut f = LazyFeatures::new(ea, eb);
    let dur_known = f.dur_known();
    let dur_delta = f.dur_delta_ms();
    let same_rel = f.same_release();
    let same_pos = f.same_release_position();
    let art_ov = f.artist_overlap();

    // Policy-level diagnostics delegate to the script so no logic is duplicated.
    let a_dyn = Dynamic::from_map(entry_to_rhai(ea));
    let b_dyn = Dynamic::from_map(entry_to_rhai(eb));
    let uc = ctx.user_ctx.clone();
    let main_sim: f64 = ctx
        .engine
        .call_fn::<f64>(
            &mut ctx.base_scope.clone(),
            &ctx.ast,
            "main_title_sim",
            (uc.clone(), a_dyn.clone(), b_dyn.clone()),
        )
        .unwrap_or(0.0);
    let markers_conf: bool = ctx
        .engine
        .call_fn::<bool>(
            &mut ctx.base_scope.clone(),
            &ctx.ast,
            "markers_conflict",
            (uc, a_dyn, b_dyn),
        )
        .unwrap_or(false);

    writeln!(
        w,
        "{},{},{:.4},{},{},{},{},{},{},{},{},{:.4},{},{},{},{},{},{:.4}",
        verdict,
        kind,
        confidence,
        csv_field(reason),
        ea.entry_type,
        ea.entry_id,
        csv_field(ea.best_title.as_deref().unwrap_or("")),
        csv_field(&fmt_pairs_csv(&ea.pairs)),
        eb.entry_id,
        csv_field(eb.best_title.as_deref().unwrap_or("")),
        csv_field(&fmt_pairs_csv(&eb.pairs)),
        main_sim,
        markers_conf,
        dur_known,
        dur_delta,
        same_rel,
        same_pos,
        art_ov,
    )?;
    Ok(())
}

// ── Public entry points ───────────────────────────────────────────────────────

/// Full scan: compare every candidate pair in the DB. Used by the `softmatch`
/// binary for batch dry-runs and bulk apply passes.
pub async fn match_db(
    db: &MusicDb,
    dedup: &DedupConfig,
    providers: &[Arc<dyn FetchProvider>],
    config: &SoftMatchConfig,
) -> anyhow::Result<()> {
    let t0 = Instant::now();

    info!("Loading entry data from DB...");
    let entries = build_entry_infos(db).await?;
    let t_load = t0.elapsed();
    info!("Loaded {} entries in {t_load:.2?}", entries.len());

    let t1 = Instant::now();
    let (barrier, _) = dedup.compile(providers).await;
    let t_dedup = t1.elapsed();
    info!("Compiled dedup barrier in {t_dedup:.2?}");

    let t2 = Instant::now();
    let embed_cache: Option<Arc<EmbeddingCache>> = open_embed_cache(config, &entries).await;
    let t_embed = t2.elapsed();
    info!("Embedding phase in {t_embed:.2?}");

    let t3 = Instant::now();
    let ctx = load_script(&config.script_path, embed_cache.clone()).await?;
    let t_script = t3.elapsed();
    info!("Script loaded in {t_script:.2?}");

    let t4 = Instant::now();
    let stats = score_candidates(
        db,
        &entries,
        None,
        &ctx,
        &barrier,
        config,
        embed_cache.as_deref(),
    )
    .await?;
    let t_score = t4.elapsed();
    info!("Scoring in {t_score:.2?}");

    let t_total = t0.elapsed();
    println!("=== Timing ===");
    println!("  load entries : {t_load:.2?}");
    println!("  dedup barrier: {t_dedup:.2?}");
    println!("  embed phase  : {t_embed:.2?}");
    println!("  script load  : {t_script:.2?}");
    println!("  scoring      : {t_score:.2?}");
    println!("  total        : {t_total:.2?}");
    println!("=== Summary ===");
    for t in ["track", "release", "release_group", "artist"] {
        if let Some(c) = stats.get(t) {
            println!("  {t:14}  merge={} relate={} distinct={}", c[0], c[1], c[2]);
        }
    }
    if config.apply_relates {
        info!("Decisions written to DB.");
    } else {
        info!("Dry-run complete. Pass --apply to write decisions to the DB.");
    }

    Ok(())
}

/// Incremental scan: only compare pairs involving one of `new_entry_ids`.
/// Called automatically by the `import` binary after each flush so new entries
/// are soft-matched against the existing library online.
pub async fn match_new_entries(
    db: &MusicDb,
    new_entry_ids: &HashSet<i64>,
    dedup: &DedupConfig,
    providers: &[Arc<dyn FetchProvider>],
    config: &SoftMatchConfig,
) -> anyhow::Result<()> {
    if new_entry_ids.is_empty() {
        return Ok(());
    }
    let entries = build_entry_infos(db).await?;
    let (barrier, _) = dedup.compile(providers).await;
    let embed_cache: Option<Arc<EmbeddingCache>> = open_embed_cache(config, &entries).await;
    let ctx = load_script(&config.script_path, embed_cache.clone()).await?;
    score_candidates(
        db,
        &entries,
        Some(new_entry_ids),
        &ctx,
        &barrier,
        config,
        embed_cache.as_deref(),
    )
    .await?;
    Ok(())
}

// ── Embedding cache helper ────────────────────────────────────────────────────

/// Open the embedding cache (if configured), run `embed()` for stale entries,
/// and return an `Arc<EmbeddingCache>` ready for KNN queries.
/// Returns `None` if embedding is disabled or fails to open.
async fn open_embed_cache(
    config: &SoftMatchConfig,
    entries: &[EntryInfo],
) -> Option<Arc<EmbeddingCache>> {
    let path = config.embed_db_path.as_deref()?;
    let cache = match EmbeddingCache::open(path, config.embed_dim) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            warn!("Failed to open embedding cache at {path}: {e} — skipping semantic blocking");
            return None;
        }
    };

    // Load or compile the Rhai script just to call embed() — we build a temporary
    // engine with HTTP support. The main script is re-loaded afterward with the
    // populated cache bound into semantic_sim.
    let embed_script = match tokio::fs::read_to_string(&config.script_path).await {
        Ok(s) => s,
        Err(e) => {
            warn!("Could not read script for embed phase: {e}");
            return Some(cache);
        }
    };
    let embed_script_dir = Path::new(&config.script_path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_owned();
    let tmp_engine = build_rhai_engine(
        &embed_script_dir,
        Arc::new(Mutex::new(HashMap::new())),
        None,
    );
    let tmp_ast = match tmp_engine.compile(&embed_script) {
        Ok(a) => a,
        Err(e) => {
            warn!("Script compile error (embed phase): {e}");
            return Some(cache);
        }
    };
    let mut tmp_scope = Scope::new();
    if let Err(e) = tmp_engine.run_ast_with_scope(&mut tmp_scope, &tmp_ast) {
        warn!("Script init error (embed phase): {e}");
    }
    // init()'s return is the context object threaded into embed_batch/embed; the
    // embed phase holds it for the whole pass so an ffi-opened library (if any)
    // stays mapped and loads its model only once.
    let user_ctx = tmp_engine
        .call_fn::<Dynamic>(&mut tmp_scope, &tmp_ast, "init", ())
        .unwrap_or(Dynamic::UNIT);

    let cache_clone = cache.clone();
    let entries_ref = entries;
    tokio::task::block_in_place(|| {
        embed_stale_entries(
            entries_ref,
            &tmp_engine,
            &tmp_ast,
            &tmp_scope,
            &user_ctx,
            &cache_clone,
        );
    });

    Some(cache)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped example script must compile under the real engine and run its
    /// dependency-free naive embedding (no `ffi` feature, no inference plugin).
    #[test]
    fn example_script_compiles_and_embeds() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/config/match.example.rhai");
        let script = std::fs::read_to_string(path).expect("read example script");
        let dir = Path::new(path).parent().unwrap();

        let engine = build_rhai_engine(dir, Arc::new(Mutex::new(HashMap::new())), None);
        let ast = engine.compile(&script).expect("example script compiles");

        let mut scope = Scope::new();
        // init() returns the context threaded into embed_batch; with no ffi here
        // it's the unit context, exercising the naive fallback path.
        let user_ctx: Dynamic = engine
            .call_fn(&mut scope, &ast, "init", ())
            .unwrap_or(Dynamic::UNIT);
        let texts: rhai::Array = vec![Dynamic::from("hello world"), Dynamic::from("hello world")];
        let rows: rhai::Array = engine
            .call_fn(&mut scope, &ast, "embed_batch", (user_ctx, texts))
            .expect("embed_batch runs");

        assert_eq!(rows.len(), 2);
        let v0: rhai::Array = rows[0].clone().cast();
        assert_eq!(
            v0.len(),
            256,
            "embedding dimension (must match SoftMatchConfig.embed_dim)"
        );
        // L2-normalized → sum of squares ≈ 1 for a non-empty title.
        let ss: f64 = v0.iter().map(|d| d.as_float().unwrap().powi(2)).sum();
        assert!(
            (ss - 1.0).abs() < 1e-6,
            "embedding should be L2-normalized, ss={ss}"
        );
    }
}
