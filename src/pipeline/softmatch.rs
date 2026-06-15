use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use rhai::{AST, Dynamic, Engine, ImmutableString, Map as RhaiMap, Scope};
use tracing::{info, warn};

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
            let a_set: HashSet<(i64, Option<i32>, Option<i32>)> =
                self.a_positions.iter().copied().collect();
            self.b_positions.iter().any(|pos| a_set.contains(pos))
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

fn build_rhai_engine(regex_cache: RegexCache) -> Engine {
    let mut engine = Engine::new();
    engine.set_max_expr_depths(0, 0); // no limit on expression or function-body nesting depth

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

    // TODO: load dylib extensions from a plugin directory so callers can register
    // additional Rhai functions (e.g. ML-based embedding similarity) without
    // touching this file.

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
    a: &EntryInfo,
    b: &EntryInfo,
) -> anyhow::Result<Verdict> {
    let mut scope = base_scope.clone();
    let a_dyn = Dynamic::from_map(entry_to_rhai(a));
    let b_dyn = Dynamic::from_map(entry_to_rhai(b));

    let result: Dynamic = engine
        .call_fn(&mut scope, ast, "decide", (a_dyn, b_dyn))
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
        .map(|(s, id)| {
            let short_id = if id.len() > 40 {
                &id[..40]
            } else {
                id.as_str()
            };
            format!("{s}:{short_id}")
        })
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
        .map(|(s, id)| {
            let short_id = if id.len() > 32 {
                &id[..32]
            } else {
                id.as_str()
            };
            format!("{s}:{short_id}")
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

// ── Shared scoring loop ───────────────────────────────────────────────────────

struct ScriptCtx<'a> {
    engine: Engine,
    ast: AST,
    base_scope: Scope<'a>,
}

impl Drop for ScriptCtx<'_> {
    fn drop(&mut self) {
        // Call optional destroy() hook; silently ignore "function not found".
        let _ = self
            .engine
            .call_fn::<Dynamic>(&mut self.base_scope, &self.ast, "destroy", ());
    }
}

async fn load_script(path: &str) -> anyhow::Result<ScriptCtx<'static>> {
    let script = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("reading script {path}"))?;
    let regex_cache: RegexCache = Arc::new(Mutex::new(HashMap::new()));
    let engine = build_rhai_engine(regex_cache);
    let ast = engine
        .compile(&script)
        .map_err(|e| anyhow::anyhow!("Rhai compile error in {path}: {e}"))?;
    let mut base_scope = Scope::new();
    engine
        .run_ast_with_scope(&mut base_scope, &ast)
        .map_err(|e| anyhow::anyhow!("Script init error in {path}: {e}"))?;
    // Call optional init() hook; silently ignore "function not found".
    let _ = engine.call_fn::<Dynamic>(&mut base_scope, &ast, "init", ());
    Ok(ScriptCtx {
        engine,
        ast,
        base_scope,
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
) -> anyhow::Result<HashMap<String, [usize; 3]>> {
    let entry_map: HashMap<i64, &EntryInfo> = entries.iter().map(|e| (e.entry_id, e)).collect();
    let candidates = generate_candidates(entries, focus);
    info!("Scoring {} candidate pair(s)", candidates.len());

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

    for (id_a, id_b) in &candidates {
        let ea = match entry_map.get(id_a) {
            Some(e) => e,
            None => continue,
        };
        let eb = match entry_map.get(id_b) {
            Some(e) => e,
            None => continue,
        };

        // Barrier-separated pairs: no RELATE between deliberately distinct entities.
        if barrier_blocks(&ea.pairs, &eb.pairs, barrier) {
            if let Some(w) = &mut csv {
                write_csv_row(w, "BARRIER", "", 0.0, "", ea, eb, ctx)?;
            }
            continue;
        }

        let verdict = call_script(&ctx.engine, &ctx.ast, &ctx.base_scope, ea, eb)?;

        if let Some(w) = &mut csv {
            let (vname, kind, conf, reason) = match &verdict {
                Verdict::Merge { confidence, reason } => {
                    ("MERGE", "", *confidence, reason.as_str())
                }
                Verdict::Relate {
                    kind,
                    confidence,
                    reason,
                } => ("RELATE", kind.as_str(), *confidence, reason.as_str()),
                Verdict::Distinct => ("DISTINCT", "", 0.0, ""),
            };
            write_csv_row(w, vname, kind, conf, reason, ea, eb, ctx)?;
        }

        let et = ea.entry_type.clone();
        let counters = stats.entry(et).or_insert([0; 3]);

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
    }

    Ok(stats)
}

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
    let main_sim: f64 = ctx
        .engine
        .call_fn::<f64>(
            &mut ctx.base_scope.clone(),
            &ctx.ast,
            "main_title_sim",
            (a_dyn.clone(), b_dyn.clone()),
        )
        .unwrap_or(0.0);
    let markers_conf: bool = ctx
        .engine
        .call_fn::<bool>(
            &mut ctx.base_scope.clone(),
            &ctx.ast,
            "markers_conflict",
            (a_dyn, b_dyn),
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
    info!("Loading entry data from DB...");
    let entries = build_entry_infos(db).await?;
    info!("Loaded {} entries", entries.len());

    let ctx = load_script(&config.script_path).await?;
    let (barrier, _) = dedup.compile(providers).await;

    let stats = score_candidates(db, &entries, None, &ctx, &barrier, config).await?;

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
    let ctx = load_script(&config.script_path).await?;
    let (barrier, _) = dedup.compile(providers).await;
    score_candidates(db, &entries, Some(new_entry_ids), &ctx, &barrier, config).await?;
    Ok(())
}
