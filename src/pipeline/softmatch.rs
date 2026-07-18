use std::collections::{HashMap, HashSet, VecDeque};
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
    /// Canonical (source, identifier) pairs, sorted by `(source, identifier)`.
    pub pairs: Vec<Pair>,
    /// All known names, deduplicated case-insensitively, in canonical order.
    pub aliases: Vec<String>,
    /// Deterministic display/embedding title: the primary alias with the
    /// smallest canonical key if any, otherwise the first canonical alias.
    /// Derived purely from `sourced_aliases`; not used to drive merge verdicts
    /// (the matcher compares the full alias set instead).
    pub best_title: Option<String>,
    /// Every distinct per-source attribute, kept as a sorted set rather than
    /// collapsed to one value — an entry legitimately carries several (e.g. an
    /// MV cut vs an audio cut have different durations). Sorted + deduped so the
    /// DB-reload path and the in-memory merge path always agree.
    pub durations: Vec<i64>,
    pub release_dates: Vec<String>,
    pub release_types: Vec<String>,
    pub primary_types: Vec<String>,
    /// For tracks/releases: entry IDs of credited artists.
    /// For artists: entry IDs of items this artist is credited on.
    pub peer_entry_ids: Vec<i64>,
    /// For tracks: (release_entry_id, disc_no, track_no) from entry_child edges.
    pub track_positions: Vec<(i64, Option<i32>, Option<i32>)>,
    /// (source, alias_name, is_primary) triples in canonical order: clean
    /// sources before video sources, then by `(source, name)`. `pick_main_title`
    /// and `pick_markers` prefer authoritative names by reading this in order.
    pub sourced_aliases: Vec<(String, String, bool)>,
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

fn is_video_source(src: &str) -> bool {
    VIDEO_SOURCES.contains(&src)
}

// ── Canonical derivation helpers ───────────────────────────────────────────────
//
// An entry's derived collections are produced by these helpers, applied
// identically by `build_entry_infos` (DB-reload path) and `merge_entry_infos`
// (in-memory path). Because both paths emit the same canonical order, no merge
// verdict can depend on SQLite row order, which is what made soft-dedup
// non-idempotent before.

/// Sort and deduplicate a set-valued attribute (durations, release dates, …).
fn dedup_sorted<T: Ord>(mut v: Vec<T>) -> Vec<T> {
    v.sort();
    v.dedup();
    v
}

/// Collapse duplicate `(source, name)` aliases (OR-ing the primary flag) and
/// sort canonically: clean sources before video sources, then by source, then
/// primary aliases first, then by name.
fn canon_sourced_aliases(sa: Vec<(String, String, bool)>) -> Vec<(String, String, bool)> {
    let mut by_key: HashMap<(String, String), bool> = HashMap::new();
    for (src, name, primary) in sa {
        let e = by_key.entry((src, name)).or_insert(false);
        *e = *e || primary;
    }
    let mut out: Vec<(String, String, bool)> = by_key
        .into_iter()
        .map(|((src, name), primary)| (src, name, primary))
        .collect();
    out.sort_by(|a, b| {
        is_video_source(&a.0)
            .cmp(&is_video_source(&b.0))
            .then_with(|| a.0.cmp(&b.0))
            .then_with(|| (!a.2).cmp(&(!b.2)))
            .then_with(|| a.1.cmp(&b.1))
    });
    out
}

/// Flat, case-insensitively deduplicated alias list in canonical order.
fn derive_aliases(sourced: &[(String, String, bool)]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for (_, name, _) in sourced {
        if seen.insert(name.to_lowercase()) {
            out.push(name.clone());
        }
    }
    out
}

/// Deterministic display/embedding title: the first primary alias in canonical
/// order, else the first alias.
fn pick_best_title(sourced: &[(String, String, bool)]) -> Option<String> {
    sourced
        .iter()
        .find(|(_, _, primary)| *primary)
        .or_else(|| sourced.first())
        .map(|(_, name, _)| name.clone())
}

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
    a_durations: Vec<i64>,
    b_durations: Vec<i64>,
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
            a_durations: a.durations.clone(),
            b_durations: b.durations.clone(),
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
        let known = !self.a_durations.is_empty() && !self.b_durations.is_empty();
        *self.c_dur_known.get_or_insert(known)
    }
    fn dur_delta_ms(&mut self) -> i64 {
        let a = &self.a_durations;
        let b = &self.b_durations;
        *self.c_dur_delta_ms.get_or_insert_with(|| {
            a.iter()
                .flat_map(|da| b.iter().map(move |db| (da - db).abs()))
                .min()
                .unwrap_or(0)
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
    // `title` is a deterministic convenience for logging/diagnostics; verdict
    // logic should compare the full alias set instead (see pick_main_title).
    m.insert(
        "title".into(),
        Dynamic::from(e.best_title.clone().unwrap_or_default()),
    );
    // Multi-valued per-source attributes, exposed as arrays — an entry can carry
    // several legitimate values (e.g. MV-cut vs audio-cut durations).
    let durations: Vec<Dynamic> = e.durations.iter().copied().map(Dynamic::from).collect();
    m.insert("durations".into(), Dynamic::from(durations));
    let release_dates: Vec<Dynamic> = e.release_dates.iter().cloned().map(Dynamic::from).collect();
    m.insert("release_dates".into(), Dynamic::from(release_dates));
    let release_types: Vec<Dynamic> = e.release_types.iter().cloned().map(Dynamic::from).collect();
    m.insert("release_types".into(), Dynamic::from(release_types));
    let primary_types: Vec<Dynamic> = e.primary_types.iter().cloned().map(Dynamic::from).collect();
    m.insert("primary_types".into(), Dynamic::from(primary_types));
    // sourced_aliases: array of #{source, name} maps, clean sources first.
    let sa: Vec<Dynamic> = e
        .sourced_aliases
        .iter()
        .map(|(src, name, _primary)| {
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
            kind: rhai_str("kind").unwrap_or_else(|| "variant".to_string()),
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

/// Semantic blocking for a single entry: walk up to `max_pages` KNN pages and
/// return all candidate pairs not yet in `already`. Pairs with entries absent
/// from `all_entries` (merge losers) are skipped. Because vec0 returns neighbours
/// sorted by ascending L2 distance, the first neighbour that exceeds the
/// threshold terminates the walk for all subsequent pages too.
fn generate_semantic_candidates_for_entry(
    entry: &EntryInfo,
    all_entries: &HashMap<i64, EntryInfo>,
    cache: &EmbeddingCache,
    base_k: usize,
    sim_threshold: f64,
    max_pages: usize,
    already: &mut HashSet<(i64, i64)>,
) -> Vec<(i64, i64)> {
    if entry.best_title.is_none() || entry.entry_type == "unknown" {
        return vec![];
    }
    // For unit vectors: L2² = 2(1 − cos_sim), so L2 = √(2(1 − cos_sim)).
    let l2_threshold = (2.0 * (1.0 - sim_threshold)).sqrt();
    let page_k = k_for_type(&entry.entry_type, base_k);
    let mut pairs = Vec::new();

    for page in 0..max_pages.max(1) {
        let want = (page + 1) * page_k;
        let neighbors = match cache.knn(entry.entry_id, want, &entry.entry_type) {
            Ok(n) => n,
            Err(err) => {
                warn!("semantic KNN failed for entry {}: {err}", entry.entry_id);
                break;
            }
        };
        let mut exhausted = false;
        for (neighbor_id, dist) in neighbors.into_iter().skip(page * page_k) {
            if dist > l2_threshold {
                exhausted = true;
                break;
            }
            if !all_entries.contains_key(&neighbor_id) {
                continue; // already merged away
            }
            let a = entry.entry_id.min(neighbor_id);
            let b = entry.entry_id.max(neighbor_id);
            if already.insert((a, b)) {
                pairs.push((a, b));
            }
        }
        if exhausted {
            break; // all further pages are strictly farther
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

async fn build_entry_infos(db: &MusicDb) -> anyhow::Result<HashMap<i64, EntryInfo>> {
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
        let mut pairs: Vec<Pair> = sources
            .iter()
            .map(|s| (s.source.clone(), s.identifier.clone()))
            .collect();
        pairs.sort();

        // Each per-source attribute is kept as a sorted set rather than collapsed
        // to one value, so the in-memory merge can reproduce it exactly.
        let durations = dedup_sorted(
            sources
                .iter()
                .flat_map(|s| s.duration_ms.iter().copied())
                .collect(),
        );
        let release_dates = dedup_sorted(
            sources
                .iter()
                .filter_map(|s| s.release_date.clone())
                .collect(),
        );
        let release_types = dedup_sorted(
            sources
                .iter()
                .filter_map(|s| s.release_type.clone())
                .collect(),
        );
        let primary_types = dedup_sorted(
            sources
                .iter()
                .filter_map(|s| s.primary_type.clone())
                .collect(),
        );

        // Collect every (source, name, primary) alias, then canonicalize. The
        // canonical order (clean sources first) is what lets pick_main_title /
        // pick_markers prefer authoritative names while staying reproducible.
        let mut sourced_aliases: Vec<(String, String, bool)> = Vec::new();
        for (src, id) in &pairs {
            if let Some(pair_aliases) = aliases_by_pair.get(&(src.clone(), id.clone())) {
                for a in pair_aliases {
                    sourced_aliases.push((src.clone(), a.name.clone(), a.primary_alias));
                }
            }
        }
        let sourced_aliases = canon_sourced_aliases(sourced_aliases);
        let aliases = derive_aliases(&sourced_aliases);
        let best_title = pick_best_title(&sourced_aliases);

        let mut peer_entry_ids: Vec<i64> = if e.entry_type == "artist" {
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
        peer_entry_ids.sort_unstable();

        let track_positions = positions_by_track.get(&e.id).cloned().unwrap_or_default();

        infos.push(EntryInfo {
            entry_id: e.id,
            entry_type: e.entry_type.clone(),
            pairs,
            aliases,
            best_title,
            durations,
            release_dates,
            release_types,
            primary_types,
            peer_entry_ids,
            track_positions,
            sourced_aliases,
        });
    }

    Ok(infos.into_iter().map(|e| (e.entry_id, e)).collect())
}

// ── In-memory entry merge ─────────────────────────────────────────────────────

/// Merge the loser's data into the winner in memory, mirroring what
/// `MusicDb::merge_entries` does at the DB level (re-pointing all loser pairs
/// to the winner). Called after the DB write so the winner's `EntryInfo` stays
/// in sync with the DB for the remainder of the scoring pass.
fn merge_entry_infos(winner: &mut EntryInfo, loser: &EntryInfo) {
    // Union every collection and re-canonicalize with the same helpers
    // build_entry_infos uses, so the merged winner is bit-identical to what a
    // fresh DB reload would produce for the combined entry.
    winner.pairs.extend(loser.pairs.iter().cloned());
    winner.pairs.sort();
    winner.pairs.dedup();

    let mut sourced = std::mem::take(&mut winner.sourced_aliases);
    sourced.extend(loser.sourced_aliases.iter().cloned());
    winner.sourced_aliases = canon_sourced_aliases(sourced);
    winner.aliases = derive_aliases(&winner.sourced_aliases);
    winner.best_title = pick_best_title(&winner.sourced_aliases);

    winner.durations = dedup_sorted(
        winner
            .durations
            .iter()
            .chain(&loser.durations)
            .copied()
            .collect(),
    );
    winner.release_dates = dedup_sorted(
        winner
            .release_dates
            .iter()
            .chain(&loser.release_dates)
            .cloned()
            .collect(),
    );
    winner.release_types = dedup_sorted(
        winner
            .release_types
            .iter()
            .chain(&loser.release_types)
            .cloned()
            .collect(),
    );
    winner.primary_types = dedup_sorted(
        winner
            .primary_types
            .iter()
            .chain(&loser.primary_types)
            .cloned()
            .collect(),
    );

    winner.peer_entry_ids = dedup_sorted(
        winner
            .peer_entry_ids
            .iter()
            .chain(&loser.peer_entry_ids)
            .copied()
            .collect(),
    );
    for &pos in &loser.track_positions {
        if !winner.track_positions.contains(&pos) {
            winner.track_positions.push(pos);
        }
    }
}

// ── Post-merge in-memory consistency helpers ──────────────────────────────────

/// After merging `loser_id` into `winner_id` at the DB level, update every
/// other entry's `peer_entry_ids` to replace `loser_id` with `winner_id`,
/// mirroring what a DB reload would produce. Returns the IDs of every entry
/// whose peer list was modified.
fn propagate_peer_remap(
    entries: &mut HashMap<i64, EntryInfo>,
    loser_id: i64,
    winner_id: i64,
) -> Vec<i64> {
    let affected: Vec<i64> = entries
        .values()
        .filter(|e| e.peer_entry_ids.contains(&loser_id))
        .map(|e| e.entry_id)
        .collect();
    for &id in &affected {
        if let Some(e) = entries.get_mut(&id) {
            e.peer_entry_ids.retain(|&p| p != loser_id);
            if !e.peer_entry_ids.contains(&winner_id) {
                e.peer_entry_ids.push(winner_id);
            }
        }
    }
    affected
}

/// Move every pair in `scored` that involves `entry_id` back onto `work_queue`
/// so it is re-scored after that entry was enriched with new data (duration,
/// peer coverage) that may flip a previous DISTINCT or RELATE verdict to MERGE.
fn drain_scored_for_entry(
    entry_id: i64,
    scored: &mut HashSet<(i64, i64)>,
    queued: &mut HashSet<(i64, i64)>,
    work_queue: &mut VecDeque<(i64, i64)>,
) {
    let to_revisit: Vec<(i64, i64)> = scored
        .iter()
        .copied()
        .filter(|(a, b)| *a == entry_id || *b == entry_id)
        .collect();
    for pair in to_revisit {
        scored.remove(&pair);
        queued.remove(&pair);
        work_queue.push_back(pair);
        // Re-insert into queued so generate_semantic won't schedule it a second time.
        queued.insert(pair);
    }
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

/// Score candidate pairs with cascade: after each merge, update the winner's
/// `EntryInfo` in memory, re-embed it if its title changed, and seed new KNN
/// candidates for it into the work queue. This converges to a fixed point within
/// a single call without re-scanning the full entry set.
///
/// `focus` restricts the initial seeding to a subset of entries (used by the
/// incremental import path); merge cascades from those seeds are unrestricted.
/// Returns `[merge, relate, distinct]` counts per entry type.
async fn score_candidates(
    db: &MusicDb,
    mut entries: HashMap<i64, EntryInfo>,
    focus: Option<&HashSet<i64>>,
    ctx: &ScriptCtx<'_>,
    barrier: &HashMap<Pair, crate::pipeline::dedup::AnchorId>,
    config: &SoftMatchConfig,
    embed_cache: Option<&EmbeddingCache>,
) -> anyhow::Result<HashMap<String, [usize; 3]>> {
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

    // `queued` tracks every pair ever added to `work_queue` or already decided,
    // preventing duplicates. `scored` is a subset of `queued` for pairs that
    // received a DISTINCT or RELATE verdict — they may be re-queued if one entry
    // gains new data (duration, peer coverage) that could flip the verdict to MERGE.
    // `loser_to_winner` lets us reroute a stale pair (A, loser) to (A, winner)
    // when the loser has already been merged away before the pair is processed.
    let mut queued: HashSet<(i64, i64)> = HashSet::new();
    let mut scored: HashSet<(i64, i64)> = HashSet::new();
    let mut loser_to_winner: HashMap<i64, i64> = HashMap::new();
    let mut work_queue: VecDeque<(i64, i64)> = VecDeque::new();

    // Seed the queue from the focused entries (or all entries for a full scan).
    let seed_ids: Vec<i64> = match focus {
        Some(f) => f.iter().copied().collect(),
        None => entries.keys().copied().collect(),
    };
    for id in seed_ids {
        if let Some(entry) = entries.get(&id) {
            let pairs = generate_semantic_candidates_for_entry(
                entry,
                &entries,
                cache,
                config.embed_k,
                config.embed_sim_threshold,
                config.embed_max_pages,
                &mut queued,
            );
            work_queue.extend(pairs);
        }
    }
    info!("Initial queue: {} candidate pair(s)", work_queue.len());

    let mut total_scored = 0usize;

    while let Some((orig_a, orig_b)) = work_queue.pop_front() {
        // Fix 4: when one of the original pair members was merged away, reroute
        // to its winner so the surviving partner is still compared against the
        // absorbing entry.
        let a = loser_to_winner.get(&orig_a).copied().unwrap_or(orig_a);
        let b = loser_to_winner.get(&orig_b).copied().unwrap_or(orig_b);
        if a == b {
            continue; // both ended up as the same entry after rerouting
        }
        let (id_a, id_b) = (a.min(b), a.max(b));
        // If rerouting changed either endpoint, the resulting pair is new and may
        // already be scheduled/decided — skip it if so. Original pairs (no rerouting)
        // are already in `queued` from seeding and must not be re-checked here, or
        // all of them would be skipped on the first `insert` returning false.
        if (id_a, id_b) != (orig_a, orig_b) && !queued.insert((id_a, id_b)) {
            continue;
        }

        let (ea, eb) = match (entries.get(&id_a), entries.get(&id_b)) {
            (Some(a), Some(b)) => (a.clone(), b.clone()),
            _ => continue,
        };

        if barrier_blocks(&ea.pairs, &eb.pairs, barrier) {
            if let Some(w) = &mut csv {
                write_csv_row(w, "BARRIER", "", 0.0, "", &ea, &eb, ctx)?;
            }
            continue;
        }

        total_scored += 1;
        let verdict = apply_candidate(db, &ea, &eb, ctx, config, &mut csv, &mut stats).await?;

        // Track DISTINCT and RELATE verdicts: either can flip to MERGE if the
        // entry gains new scoring-relevant data (duration, peer coverage) later.
        if matches!(verdict, Verdict::Distinct | Verdict::Relate { .. }) {
            scored.insert((id_a, id_b));
        }

        // On a real merge, update the winner in memory and cascade.
        if matches!(verdict, Verdict::Merge { .. }) && config.apply_relates {
            // apply_candidate calls merge_entries(ea, eb) → ea is loser, eb is winner.
            let loser_id = ea.entry_id;
            let winner_id = eb.entry_id;

            loser_to_winner.insert(loser_id, winner_id);

            if let Some(loser) = entries.remove(&loser_id) {
                let winner_snapshot = {
                    let winner = entries
                        .get_mut(&winner_id)
                        .expect("merge winner still in map");

                    // Capture pre-merge state for enrichment detection (Fix 1).
                    let old_duration_count = winner.durations.len();
                    let old_peer_count = winner.peer_entry_ids.len();

                    merge_entry_infos(winner, &loser);
                    // Re-embed if the winner's best_title changed (stale check is fast).
                    tokio::task::block_in_place(|| {
                        embed_stale_entries(
                            std::slice::from_ref(winner),
                            &ctx.engine,
                            &ctx.ast,
                            &ctx.base_scope,
                            &ctx.user_ctx,
                            cache,
                        );
                    });

                    // Fix 1+3: if the winner gained duration or peer coverage, pairs
                    // that previously scored DISTINCT or RELATE against the winner may
                    // now score MERGE — re-queue them for a fresh evaluation.
                    let gained_duration = winner.durations.len() > old_duration_count;
                    let gained_peers = winner.peer_entry_ids.len() > old_peer_count;
                    if gained_duration || gained_peers {
                        drain_scored_for_entry(
                            winner_id,
                            &mut scored,
                            &mut queued,
                            &mut work_queue,
                        );
                    }

                    winner.clone()
                };

                // Fix 2: the loser's entry_id is gone from the DB, but other entries
                // that listed it as a peer still hold the stale id. Remap them now
                // so the in-memory view matches what a fresh DB reload would produce.
                let affected = propagate_peer_remap(&mut entries, loser_id, winner_id);
                for &affected_id in &affected {
                    // Peer overlap changed → previously-scored pairs may now MERGE.
                    drain_scored_for_entry(affected_id, &mut scored, &mut queued, &mut work_queue);
                    // Seed new KNN candidates: embedding unchanged but peer data is richer.
                    if let Some(entry) = entries.get(&affected_id).cloned() {
                        let new_pairs = generate_semantic_candidates_for_entry(
                            &entry,
                            &entries,
                            cache,
                            config.embed_k,
                            config.embed_sim_threshold,
                            config.embed_max_pages,
                            &mut queued,
                        );
                        work_queue.extend(new_pairs);
                    }
                }

                // Seed new KNN candidates for the winner (its embedding may have changed).
                let new_pairs = generate_semantic_candidates_for_entry(
                    &winner_snapshot,
                    &entries,
                    cache,
                    config.embed_k,
                    config.embed_sim_threshold,
                    config.embed_max_pages,
                    &mut queued,
                );
                work_queue.extend(new_pairs);
            }
        }
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
    let entries_slice: Vec<EntryInfo> = entries.values().cloned().collect();
    let embed_cache: Option<Arc<EmbeddingCache>> = open_embed_cache(config, &entries_slice).await;
    let t_embed = t2.elapsed();
    info!("Embedding phase in {t_embed:.2?}");

    let t3 = Instant::now();
    let ctx = load_script(&config.script_path, embed_cache.clone()).await?;
    let t_script = t3.elapsed();
    info!("Script loaded in {t_script:.2?}");

    let t4 = Instant::now();
    let stats = score_candidates(
        db,
        entries,
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
    let entries_slice: Vec<EntryInfo> = entries.values().cloned().collect();
    let embed_cache: Option<Arc<EmbeddingCache>> = open_embed_cache(config, &entries_slice).await;
    let ctx = load_script(&config.script_path, embed_cache.clone()).await?;
    score_candidates(
        db,
        entries,
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

    fn blank_entry(id: i64, peers: Vec<i64>) -> EntryInfo {
        EntryInfo {
            entry_id: id,
            entry_type: "artist".to_string(),
            pairs: vec![("test".to_string(), id.to_string())],
            aliases: vec![],
            best_title: None,
            durations: vec![],
            release_dates: vec![],
            release_types: vec![],
            primary_types: vec![],
            peer_entry_ids: peers,
            track_positions: vec![],
            sourced_aliases: vec![],
        }
    }

    /// Build a release-group entry carrying a single primary alias `title` on
    /// source `src`, with the given durations.
    fn titled_entry(id: i64, title: &str, src: &str, durations: Vec<i64>) -> EntryInfo {
        let sourced_aliases = vec![(src.to_string(), title.to_string(), true)];
        EntryInfo {
            entry_id: id,
            entry_type: "release_group".to_string(),
            pairs: vec![(src.to_string(), id.to_string())],
            aliases: derive_aliases(&sourced_aliases),
            best_title: pick_best_title(&sourced_aliases),
            durations: dedup_sorted(durations),
            release_dates: vec![],
            release_types: vec![],
            primary_types: vec![],
            peer_entry_ids: vec![],
            track_positions: vec![],
            sourced_aliases,
        }
    }

    /// Fix 2: when a merge removes the loser, every other entry that listed it
    /// as a peer should have the loser replaced by the winner.
    #[test]
    fn test_propagate_peer_remap_basic() {
        let mut entries: HashMap<i64, EntryInfo> = HashMap::new();
        // Artists A=1 and B=2 both credit track T10 (loser). C=3 does not.
        entries.insert(1, blank_entry(1, vec![10, 30]));
        entries.insert(2, blank_entry(2, vec![10, 40]));
        entries.insert(3, blank_entry(3, vec![20, 30]));

        let affected = propagate_peer_remap(&mut entries, 10, 20);

        assert_eq!(affected.len(), 2);
        assert!(affected.contains(&1));
        assert!(affected.contains(&2));
        assert!(!affected.contains(&3));

        // A: T10 → T20; T20 was not previously listed → added.
        let a = &entries[&1].peer_entry_ids;
        assert!(!a.contains(&10));
        assert!(a.contains(&20));
        assert!(a.contains(&30));

        // B: T10 → T20; T40 untouched.
        let b = &entries[&2].peer_entry_ids;
        assert!(!b.contains(&10));
        assert!(b.contains(&20));
        assert!(b.contains(&40));

        // C: unchanged.
        assert_eq!(entries[&3].peer_entry_ids, vec![20, 30]);
    }

    /// Fix 2: when the winner is already in an entry's peer list, the loser
    /// must be removed without duplicating the winner.
    #[test]
    fn test_propagate_peer_remap_no_duplicate_winner() {
        let mut entries: HashMap<i64, EntryInfo> = HashMap::new();
        // Entry 1 already credits both loser (10) and winner (20).
        entries.insert(1, blank_entry(1, vec![10, 20]));

        propagate_peer_remap(&mut entries, 10, 20);

        let peers = &entries[&1].peer_entry_ids;
        assert!(!peers.contains(&10), "loser removed");
        assert_eq!(
            peers.iter().filter(|&&p| p == 20).count(),
            1,
            "winner not duplicated"
        );
    }

    /// Fixes 1+3: DISTINCT- and RELATE-scored pairs involving the enriched entry
    /// must be moved back onto the work queue for re-evaluation with the new data.
    #[test]
    fn test_drain_scored_for_entry_selective() {
        let mut queued: HashSet<(i64, i64)> = HashSet::new();
        let mut scored: HashSet<(i64, i64)> = HashSet::new();
        let mut work_queue: VecDeque<(i64, i64)> = VecDeque::new();

        let pair_13 = (1i64, 3i64);
        let pair_14 = (1i64, 4i64);
        let pair_25 = (2i64, 5i64); // unrelated — must not be touched
        for &p in &[pair_13, pair_14, pair_25] {
            queued.insert(p);
            scored.insert(p);
        }

        drain_scored_for_entry(1, &mut scored, &mut queued, &mut work_queue);

        // Only entry-1 pairs leave `scored`.
        assert!(!scored.contains(&pair_13));
        assert!(!scored.contains(&pair_14));
        assert!(scored.contains(&pair_25));

        // Those same pairs land in the work queue.
        let in_queue: Vec<_> = work_queue.iter().copied().collect();
        assert!(in_queue.contains(&pair_13));
        assert!(in_queue.contains(&pair_14));
        assert!(!in_queue.contains(&pair_25));

        // Pairs remain in `queued` so generate_semantic won't schedule them again.
        assert!(queued.contains(&pair_13));
        assert!(queued.contains(&pair_14));
        assert!(queued.contains(&pair_25));
    }

    /// merge_entry_infos recomputes best_title from the canonical alias set, so
    /// the result is independent of which entry was the winner — matching what
    /// build_entry_infos produces after a DB reload. With both aliases on the
    /// same source, the lexicographically smaller title name wins.
    #[test]
    fn test_merge_recomputes_best_title_canonically() {
        let mut winner = titled_entry(100, "わためのうた vol.1", "musicbrainz", vec![]);
        let loser = titled_entry(50, "わためのうた vol.2", "musicbrainz", vec![]);
        merge_entry_infos(&mut winner, &loser);
        assert_eq!(winner.best_title.as_deref(), Some("わためのうた vol.1"));

        // Swapping winner/loser yields the identical title — no orientation bias.
        let mut winner2 = titled_entry(50, "わためのうた vol.2", "musicbrainz", vec![]);
        let loser2 = titled_entry(100, "わためのうた vol.1", "musicbrainz", vec![]);
        merge_entry_infos(&mut winner2, &loser2);
        assert_eq!(winner2.best_title.as_deref(), winner.best_title.as_deref());
    }

    /// A clean-source alias must win the title slot over a video-source alias,
    /// regardless of merge orientation (canonical order puts clean sources first).
    #[test]
    fn test_merge_best_title_prefers_clean_source() {
        let mut winner = titled_entry(1, "Noisy MV Title", "youtube", vec![]);
        let loser = titled_entry(2, "Clean Title", "musicbrainz", vec![]);
        merge_entry_infos(&mut winner, &loser);
        assert_eq!(winner.best_title.as_deref(), Some("Clean Title"));
    }

    /// Durations are a sorted, deduplicated union — never collapsed to one value
    /// and never order-dependent.
    #[test]
    fn test_merge_unions_durations() {
        let mut winner = titled_entry(1, "T", "musicbrainz", vec![325000, 324000]);
        let loser = titled_entry(2, "T", "spotify", vec![308573, 324000]);
        merge_entry_infos(&mut winner, &loser);
        assert_eq!(winner.durations, vec![308573, 324000, 325000]);

        // Orientation-independent.
        let mut winner2 = titled_entry(2, "T", "spotify", vec![308573, 324000]);
        let loser2 = titled_entry(1, "T", "musicbrainz", vec![325000, 324000]);
        merge_entry_infos(&mut winner2, &loser2);
        assert_eq!(winner2.durations, winner.durations);
    }

    /// Fix 4: when a pair (A, loser) is popped but the loser was already merged
    /// away, the reroute logic in score_candidates maps loser→winner. Verify the
    /// normalisation arithmetic (min/max ordering, self-pair elimination).
    #[test]
    fn test_loser_reroute_normalisation() {
        // Simulate the reroute step inline so the logic is testable without a DB.
        let reroute = |id_a: i64, id_b: i64, loser: i64, winner: i64| -> Option<(i64, i64)> {
            let a = if id_a == loser { winner } else { id_a };
            let b = if id_b == loser { winner } else { id_b };
            if a == b {
                None // self-pair after reroute → skip
            } else {
                Some((a.min(b), a.max(b)))
            }
        };

        // (1, 5): 5 is the loser, 10 is the winner → reroute to (1, 10)
        assert_eq!(reroute(1, 5, 5, 10), Some((1, 10)));

        // (5, 20): 5 is loser, 10 is winner → reroute to (10, 20)
        assert_eq!(reroute(5, 20, 5, 10), Some((10, 20)));

        // (5, 10): 5 is loser, 10 is winner → both map to same entry → skip
        assert_eq!(reroute(5, 10, 5, 10), None);

        // No reroute needed (neither is a loser)
        assert_eq!(reroute(3, 7, 5, 10), Some((3, 7)));
    }

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
