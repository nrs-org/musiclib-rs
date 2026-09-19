use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::io::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use rhai::{AST, Dynamic, Engine, ImmutableString, Map as RhaiMap, Scope};
use serde::Serialize;
use std::time::Instant;
use tracing::{info, warn};
use unicode_normalization::UnicodeNormalization as _;

use crate::pipeline::embedding::{
    EmbeddingAnn, EmbeddingCache, dynamic_to_serde, embed_stale_entries, register_http_fns,
    serde_to_dynamic,
};

use crate::http::{HeaderName, HeaderValue, HttpClient, Method, Request as HttpRequest};
use crate::musicdb::{
    AliasRow, ChildRow, ContribRow, IdentityJudgment, MusicDb, NewDedupFeedback,
    NewDedupSuggestion, SourceRow,
};
use crate::pipeline::dedup::DedupConfig;
use crate::pipeline::dedup_model::{DedupModel, ModelDecision};
use crate::providers::FetchProvider;

type Pair = (String, String);

/// All data needed about one entry for candidate scoring.
#[derive(Debug, Clone, Serialize)]
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
    /// For releases: entry IDs of tracks in the release. This is used only for
    /// bounded structural candidate retrieval; order/edition policy stays in
    /// the scorer.
    pub child_entry_ids: Vec<i64>,
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
        /// Dedup-v2 primitive-relation metadata (docs/dedup-v2.md), when the
        /// script called the 4-arg `relate(kind, conf, reason, metadata)`
        /// overload. `None` for every legacy `relate(kind, conf, reason)`
        /// call site that hasn't been migrated onto the new ontology.
        metadata: Option<serde_json::Value>,
    },
    Defer {
        confidence: f64,
        reason: String,
    },
    Separate {
        confidence: f64,
        reason: String,
    },
    Distinct,
}

pub struct SoftMatchConfig {
    pub script_path: String,
    /// Optional versioned logistic model. When set, it replaces Rhai verdict
    /// scoring; Rhai remains available for embedding and CSV diagnostics.
    pub model_path: Option<String>,
    /// Persist learned MERGE/DEFER candidates for an interactive review queue.
    pub persist_suggestions: bool,
    /// Write RELATE and MERGE decisions to the DB. Both are soft/reversible:
    /// RELATE writes an `entry_relation` row, MERGE writes a `same_identity`
    /// soft-identity assertion via `MusicDb::record_identity_feedback` — the
    /// same reversible path the player's manual "link" button uses. No
    /// softmatch verdict, from any backend (Rhai, learned model, or Jev),
    /// ever calls `MusicDb::merge_entries` (destructive, no undo path); that
    /// stays exclusively an import-time/dedup-barrier operation.
    pub apply_relates: bool,
    /// Entry types scored by TypeSafe's Jev model instead of the Rhai script
    /// or learned model (`pipeline::jev`). Empty by default. Per-type, not
    /// global, to allow a hybrid rollout (e.g. `{"track"}` while `release`
    /// stays on the existing heuristic).
    pub jev_entry_types: HashSet<String>,
    /// If set, write a CSV row for every candidate pair (including DISTINCT)
    /// to this path for manual quality review.
    pub csv_path: Option<String>,
    /// Path to the SQLite file used as the embedding cache.
    /// `None` disables semantic blocking entirely.
    pub embed_db_path: Option<String>,
    /// Stable identity of the model behind the local `embed()` hook. Learned
    /// artifacts use this to reject a mismatched embedding vector space.
    pub embed_model_id: Option<String>,
    /// Embedding vector dimension — must match the model used in the Rhai `embed()`
    /// function. Default script + default inference backend is 256 (Model2Vec);
    /// use 384 when the inference cdylib is built with `--features minilm`.
    pub embed_dim: usize,
    /// Number of semantic KNN neighbours per entry and type. Default: 20.
    pub embed_k: usize,
    /// Minimum cosine similarity to include a semantic pair as a blocking candidate.
    /// Default: 0.45, as calibrated by the entry-level retrieval PoC.
    pub embed_sim_threshold: f64,
    /// Maximum number of KNN pages to walk per entry type. One page is the
    /// calibrated k-neighbour channel; larger values are recall experiments.
    pub embed_max_pages: usize,
    /// Largest posting list expanded by a lexical/structural channel.
    pub candidate_max_block: usize,
    /// Per-entry character-trigram neighbors retained before channel union.
    pub candidate_ngram_k: usize,
    /// Print every non-distinct decision and its full source list to stdout.
    pub verbose_decisions: bool,
    /// Backing client for the script-facing `http_call(...)` primitive.
    /// `None` disables it (`http_call` returns `#{"error": ...}` rather than
    /// panicking) — e.g. no `http.yaml` configured. Uses the same
    /// `DomainScheduler`/`HttpCache` stack as every other backend; the script
    /// alone decides what, if anything, to call with it.
    pub http_client: Option<Arc<dyn HttpClient>>,
}

/// The `SoftMatchConfig` shared by every caller that runs online soft-dedup
/// after an ingest (the `import` CLI, the `server` binary's ingest job):
/// RELATE and soft MERGE (`same_identity`) decisions are both written
/// reversibly (tombstoned via `enabled`, never repoints `entry_source`) —
/// no softmatch verdict destructively merges entries; that stays an
/// import-time/dedup-barrier operation (docs/dedup-v2.md). No entry types
/// are routed to Jev by default; `http_client` is unset, so `http_call` is
/// disabled until a caller wires one in.
///
/// Returns `None` if `<config_dir>/match.rhai` doesn't exist, meaning
/// soft-dedup isn't configured and should be skipped entirely.
pub fn default_soft_match_config() -> Option<SoftMatchConfig> {
    let config_dir = crate::app_dirs::config_dir();
    let script_path = config_dir.join("match.rhai");
    if !script_path.exists() {
        return None;
    }
    let embed_db = crate::app_dirs::data_dir().join("embeddings.db");
    let model_path = config_dir.join("dedup-model.json");
    let persist_suggestions = model_path.exists();
    Some(SoftMatchConfig {
        script_path: script_path.display().to_string(),
        model_path: model_path
            .exists()
            .then(|| model_path.display().to_string()),
        persist_suggestions,
        apply_relates: true,
        jev_entry_types: Default::default(),
        csv_path: None,
        embed_db_path: Some(embed_db.display().to_string()),
        embed_model_id: None,
        embed_dim: 256,
        embed_k: 20,
        embed_sim_threshold: 0.45,
        embed_max_pages: 1,
        candidate_max_block: 50,
        candidate_ngram_k: 30,
        verbose_decisions: false,
        http_client: None,
    })
}

// ── Blocking helper ────────────────────────────────────────────────────────────

/// Runs `f` (synchronous, CPU/FFI-bound work — Rhai script execution, an
/// embedding model) off the async task without stalling the runtime, on
/// runtimes that support it. `tokio::task::block_in_place` requires a
/// multi-threaded runtime (it hands this task's worker thread to another
/// waiting task while `f` runs) and panics on a current-thread one — which
/// the `server` binary uses deliberately, since its dedup scoring pipeline
/// holds a `rhai::Engine` (not `Send`) across `.await` points and needs
/// `spawn_local` (see `src/bin/server/jobs.rs`). On a current-thread runtime
/// there's no worker pool to hand off to anyway, so `f` just runs in place —
/// blocking the one thread, same as every other request handler already does
/// while `db`/HTTP calls are in flight there.
fn run_blocking<R>(f: impl FnOnce() -> R) -> R {
    if tokio::runtime::Handle::current().runtime_flavor()
        == tokio::runtime::RuntimeFlavor::MultiThread
    {
        tokio::task::block_in_place(f)
    } else {
        f()
    }
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
// An entry's derived collections are produced by these helpers, applied by
// `build_entry_infos`/`entry_infos_by_ids` — the only path that constructs an
// `EntryInfo` (no softmatch verdict merges entries in memory any more; MERGE
// is soft — see `SoftMatchConfig::apply_relates`). Canonical, sorted-set order
// means no verdict can depend on SQLite row order, which is what made
// soft-dedup non-idempotent before.

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

/// Compatibility-normalize, lowercase, keep alphanumerics, collapse whitespace.
/// This matches the normalization contract used to calibrate the Python PoC.
fn normalize(s: &str) -> String {
    s.nfkc()
        .flat_map(char::to_lowercase)
        .map(|c| {
            if c.is_alphabetic() || c.is_numeric() {
                c
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

const VERSION_MARKERS: &[&str] = &[
    "acoustic",
    "arrange",
    "arranged",
    "bootleg",
    "cover",
    "demo",
    "edit",
    "instrumental",
    "karaoke",
    "live",
    "mix",
    "remaster",
    "remastered",
    "remix",
    "reprise",
    "spedup",
    "version",
    "ver",
    "radio",
    "unplugged",
    "アコースティック",
    "アレンジ",
    "インスト",
    "カバー",
    "ライブ",
    "リミックス",
];

const RUNTIME_FEATURE_NAMES: &[&str] = &[
    "name_exact",
    "name_similarity",
    "token_jaccard",
    "ngram_jaccard",
    "identifier_overlap",
    "artist_jaccard",
    "tracklist_jaccard",
    "tracklist_length_similarity",
    "date_exact",
    "duration_similarity",
    "version_conflict",
    "base_title_exact",
    "qualifier_jaccard",
    "qualifier_conflict",
    "primary_type_match",
    "primary_type_conflict",
    "track_position_match",
    "internal_mixedness",
    "empty_side",
    "semantic_similarity",
];

fn set_jaccard<T: Eq + std::hash::Hash>(a: &HashSet<T>, b: &HashSet<T>) -> f64 {
    let union = a.union(b).count();
    if union == 0 {
        0.0
    } else {
        a.intersection(b).count() as f64 / union as f64
    }
}

fn title_tokens(names: &[String]) -> HashSet<String> {
    names
        .iter()
        .flat_map(|name| {
            normalize(name)
                .split_whitespace()
                .filter(|token| token.chars().count() > 1)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}

fn title_grams(names: &[String]) -> HashSet<String> {
    names.iter().flat_map(|name| char_trigrams(name)).collect()
}

// difflib.SequenceMatcher without junk handling. Title strings are far below
// Python's 200-item autojunk cutoff, so this reproduces the PoC ratio.
fn sequence_match_ratio(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    fn matching(a: &[char], b: &[char]) -> usize {
        let mut best = (0, 0, 0);
        let mut previous = vec![0usize; b.len() + 1];
        for i in 0..a.len() {
            let mut current = vec![0usize; b.len() + 1];
            for j in 0..b.len() {
                if a[i] == b[j] {
                    let size = previous[j] + 1;
                    current[j + 1] = size;
                    let start = (i + 1 - size, j + 1 - size);
                    if size > best.2 || (size == best.2 && (start.0, start.1) < (best.0, best.1)) {
                        best = (start.0, start.1, size);
                    }
                }
            }
            previous = current;
        }
        if best.2 == 0 {
            0
        } else {
            matching(&a[..best.0], &b[..best.1])
                + best.2
                + matching(&a[best.0 + best.2..], &b[best.1 + best.2..])
        }
    }
    2.0 * matching(&a, &b) as f64 / (a.len() + b.len()) as f64
}

fn normalized_names(entry: &EntryInfo) -> Vec<String> {
    entry
        .aliases
        .iter()
        .map(|name| normalize(name))
        .filter(|name| !name.is_empty())
        .collect()
}

fn best_name_similarity(a: &[String], b: &[String]) -> f64 {
    a.iter()
        .flat_map(|left| b.iter().map(move |right| sequence_match_ratio(left, right)))
        .fold(0.0, f64::max)
}

fn base_title(value: &str) -> String {
    let mut depth = 0usize;
    let mut out = String::new();
    for ch in value.chars() {
        if matches!(ch, '(' | '[' | '【' | '（') {
            depth += 1;
            out.push(' ');
        } else if matches!(ch, ')' | ']' | '】' | '）') && depth > 0 {
            depth -= 1;
            out.push(' ');
        } else if depth == 0 {
            out.push(ch);
        }
    }
    normalize(&out)
}

fn qualifiers(value: &str) -> HashSet<String> {
    let mut depth = 0usize;
    let mut grouped = String::new();
    for ch in value.chars() {
        if matches!(ch, '(' | '[' | '【' | '（') {
            depth += 1;
        } else if matches!(ch, ')' | ']' | '】' | '）') {
            depth = depth.saturating_sub(1);
        } else if depth > 0 {
            grouped.push(ch);
        } else {
            grouped.push(' ');
        }
    }
    let markers: HashSet<&str> = VERSION_MARKERS.iter().copied().collect();
    let mut result: HashSet<String> = normalize(&grouped)
        .split_whitespace()
        .filter(|token| token.chars().count() > 1)
        .map(str::to_owned)
        .collect();
    result.extend(
        normalize(value)
            .split_whitespace()
            .filter(|token| markers.contains(token))
            .map(str::to_owned),
    );
    result.remove("ver");
    result.remove("version");
    result
}

fn is_external_identifier(source: &str, value: &str) -> bool {
    let source = source.to_lowercase();
    if matches!(source.as_str(), "barcode" | "upc" | "isrc") {
        return true;
    }
    let chars: Vec<char> = value.chars().collect();
    (chars.len() == 12
        && chars[..2].iter().all(|c| c.is_ascii_alphabetic())
        && chars[2..5].iter().all(|c| c.is_ascii_alphanumeric())
        && chars[5..].iter().all(|c| c.is_ascii_digit()))
        || ((12..=14).contains(&chars.len()) && chars.iter().all(|c| c.is_ascii_digit()))
}

fn learned_features(
    a: &EntryInfo,
    b: &EntryInfo,
    embeddings: Option<&EmbeddingCache>,
) -> HashMap<String, f64> {
    let an = normalized_names(a);
    let bn = normalized_names(b);
    let ac: HashSet<String> = an.iter().map(|name| name.replace(' ', "")).collect();
    let bc: HashSet<String> = bn.iter().map(|name| name.replace(' ', "")).collect();
    let at = title_tokens(&a.aliases);
    let bt = title_tokens(&b.aliases);
    let ag = title_grams(&a.aliases);
    let bg = title_grams(&b.aliases);
    let ai: HashSet<String> = a
        .pairs
        .iter()
        .filter(|(s, i)| is_external_identifier(s, i))
        .map(|(_, i)| normalize(i))
        .collect();
    let bi: HashSet<String> = b
        .pairs
        .iter()
        .filter(|(s, i)| is_external_identifier(s, i))
        .map(|(_, i)| normalize(i))
        .collect();
    let abase: HashSet<String> = a
        .aliases
        .iter()
        .map(|name| base_title(name))
        .filter(|x| !x.is_empty())
        .collect();
    let bbase: HashSet<String> = b
        .aliases
        .iter()
        .map(|name| base_title(name))
        .filter(|x| !x.is_empty())
        .collect();
    let aq: HashSet<String> = a.aliases.iter().flat_map(|name| qualifiers(name)).collect();
    let bq: HashSet<String> = b.aliases.iter().flat_map(|name| qualifiers(name)).collect();
    let am: HashSet<String> = at
        .iter()
        .filter(|x| VERSION_MARKERS.contains(&x.as_str()))
        .cloned()
        .collect();
    let bm: HashSet<String> = bt
        .iter()
        .filter(|x| VERSION_MARKERS.contains(&x.as_str()))
        .cloned()
        .collect();
    let ap: HashSet<String> = a.primary_types.iter().map(|x| normalize(x)).collect();
    let bp: HashSet<String> = b.primary_types.iter().map(|x| normalize(x)).collect();
    let apos: HashSet<(Option<i32>, Option<i32>)> =
        a.track_positions.iter().map(|(_, d, t)| (*d, *t)).collect();
    let bpos: HashSet<(Option<i32>, Option<i32>)> =
        b.track_positions.iter().map(|(_, d, t)| (*d, *t)).collect();
    let duration_similarity = if a.durations.is_empty() || b.durations.is_empty() {
        0.0
    } else {
        let delta = a
            .durations
            .iter()
            .flat_map(|x| b.durations.iter().map(move |y| x.abs_diff(*y)))
            .min()
            .unwrap();
        (1.0 - delta as f64 / 30_000.0).max(0.0)
    };
    let tracklist_length_similarity =
        if a.child_entry_ids.is_empty() || b.child_entry_ids.is_empty() {
            0.0
        } else {
            a.child_entry_ids.len().min(b.child_entry_ids.len()) as f64
                / a.child_entry_ids.len().max(b.child_entry_ids.len()) as f64
        };
    let mixedness = |entry: &EntryInfo| {
        let primary: Vec<String> = entry
            .sourced_aliases
            .iter()
            .filter(|(_, _, p)| *p)
            .map(|(_, name, _)| normalize(name))
            .filter(|x| !x.is_empty())
            .collect();
        if primary.len() < 2 {
            0.0
        } else {
            let minimum = primary
                .iter()
                .enumerate()
                .flat_map(|(i, x)| {
                    primary[i + 1..]
                        .iter()
                        .map(move |y| sequence_match_ratio(x, y))
                })
                .fold(1.0, f64::min);
            1.0 - minimum
        }
    };
    let base_exact = !abase.is_disjoint(&bbase);
    HashMap::from([
        ("name_exact".into(), (!ac.is_disjoint(&bc)) as u8 as f64),
        ("name_similarity".into(), best_name_similarity(&an, &bn)),
        ("token_jaccard".into(), set_jaccard(&at, &bt)),
        ("ngram_jaccard".into(), set_jaccard(&ag, &bg)),
        (
            "identifier_overlap".into(),
            (!ai.is_disjoint(&bi)) as u8 as f64,
        ),
        (
            "artist_jaccard".into(),
            jaccard_i64(&a.peer_entry_ids, &b.peer_entry_ids),
        ),
        (
            "tracklist_jaccard".into(),
            jaccard_i64(&a.child_entry_ids, &b.child_entry_ids),
        ),
        (
            "tracklist_length_similarity".into(),
            tracklist_length_similarity,
        ),
        (
            "date_exact".into(),
            a.release_dates.iter().any(|x| {
                b.release_dates
                    .iter()
                    .any(|y| x.split_whitespace().next() == y.split_whitespace().next())
            }) as u8 as f64,
        ),
        ("duration_similarity".into(), duration_similarity),
        (
            "version_conflict".into(),
            (am != bm && (!am.is_empty() || !bm.is_empty())) as u8 as f64,
        ),
        ("base_title_exact".into(), base_exact as u8 as f64),
        ("qualifier_jaccard".into(), set_jaccard(&aq, &bq)),
        (
            "qualifier_conflict".into(),
            (base_exact && aq != bq && (!aq.is_empty() || !bq.is_empty())) as u8 as f64,
        ),
        (
            "primary_type_match".into(),
            (!ap.is_disjoint(&bp)) as u8 as f64,
        ),
        (
            "primary_type_conflict".into(),
            (!ap.is_empty() && !bp.is_empty() && ap.is_disjoint(&bp)) as u8 as f64,
        ),
        (
            "track_position_match".into(),
            (!apos.is_disjoint(&bpos)) as u8 as f64,
        ),
        ("internal_mixedness".into(), mixedness(a).max(mixedness(b))),
        (
            "empty_side".into(),
            (an.is_empty() || bn.is_empty()) as u8 as f64,
        ),
        (
            "semantic_similarity".into(),
            embeddings
                .and_then(|cache| cache.cosine_similarity(a.entry_id, b.entry_id))
                .unwrap_or(0.0),
        ),
    ])
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
    http_client: Option<Arc<dyn HttpClient>>,
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
    // Overload carrying dedup-v2 primitive-relation metadata (e.g.
    // `#{"transformation": "instrumental"}`) alongside the legacy
    // kind/confidence/reason shape, dispatched by arity. Kept as an add-on
    // rather than replacing the 3-arg form so every existing `relate(...)`
    // call site that hasn't been deliberately validated against the new
    // ontology keeps behaving exactly as before.
    engine.register_fn(
        "relate",
        |kind: String, conf: f64, reason: String, metadata: RhaiMap| -> RhaiMap {
            let mut m = RhaiMap::new();
            m.insert("verdict".into(), Dynamic::from("relate".to_string()));
            m.insert("kind".into(), Dynamic::from(kind));
            m.insert("confidence".into(), Dynamic::from(conf));
            m.insert("reason".into(), Dynamic::from(reason));
            m.insert("metadata".into(), Dynamic::from_map(metadata));
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

    // ── Generic HTTP + JSON primitives ────────────────────────────────────────
    // Deliberately opinion-free: Rust knows how to make an HTTP request and
    // convert JSON, nothing more. What gets called, why, with what evidence,
    // and how the answer changes a verdict is entirely script logic (e.g. a
    // script wiring up an external identity-review model) — see the
    // conversation this was designed in for why that split matters: policy
    // that "different people may disagree about" belongs in the editable
    // `.rhai` file, not compiled into this binary.
    {
        let client = http_client.clone();
        engine.register_fn("http_call", move |request: RhaiMap| -> RhaiMap {
            http_call_impl(client.as_ref(), request)
        });
    }
    engine.register_fn("env_var", |name: String| -> String {
        std::env::var(&name).unwrap_or_default()
    });
    engine.register_fn("to_json", |value: Dynamic| -> String {
        serde_json::to_string(&dynamic_to_serde(&value)).unwrap_or_default()
    });
    engine.register_fn("parse_json", |text: String| -> Dynamic {
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => serde_to_dynamic(&v),
            Err(e) => {
                warn!("parse_json: invalid JSON ({e}); returning unit");
                Dynamic::UNIT
            }
        }
    });

    engine
}

/// `http_call(request: Map) -> Map` implementation. `request` is
/// `#{method, url, headers?, body?, cache_key?}` (all strings; `headers` is a
/// string→string map); returns `#{status, body}` on completion or
/// `#{error: "..."}` (optionally alongside a `status` if one was received) on
/// failure — never panics or throws into the script, so a script can always
/// check `result.status`/`result` before deciding what to do.
///
/// Goes through the same `HttpClient` stack (`DomainScheduler` concurrency,
/// `HttpCache`) every other backend in this codebase uses; `cache_key`, if
/// given, is passed straight through to that cache layer, opaque to Rust —
/// the script decides what makes two calls "the same" (e.g. hashing whatever
/// evidence it put in the body).
///
/// `register_fn` closures are synchronous; this bridges into the async
/// `HttpClient` the same way `softmatch.rs`'s merge-cascade re-embed step
/// already does (`tokio::task::block_in_place` + `Handle::current().block_on`).
fn http_call_impl(client: Option<&Arc<dyn HttpClient>>, request: RhaiMap) -> RhaiMap {
    let mut out = RhaiMap::new();
    let err = |out: &mut RhaiMap, msg: String| {
        out.insert("error".into(), Dynamic::from(msg));
    };

    let Some(client) = client else {
        err(
            &mut out,
            "http_call: no HTTP client configured for this run".to_string(),
        );
        return out;
    };

    let get_str = |key: &str| -> Option<String> {
        request
            .get(key)
            .and_then(|v| v.clone().try_cast::<ImmutableString>())
            .map(|s| s.to_string())
    };

    let Some(method_str) = get_str("method") else {
        err(&mut out, "http_call: missing 'method'".to_string());
        return out;
    };
    let Some(url) = get_str("url") else {
        err(&mut out, "http_call: missing 'url'".to_string());
        return out;
    };
    let method = match method_str.parse::<Method>() {
        Ok(m) => m,
        Err(e) => {
            err(
                &mut out,
                format!("http_call: bad method {method_str:?}: {e}"),
            );
            return out;
        }
    };

    let mut headers = Vec::new();
    if let Some(h) = request
        .get("headers")
        .and_then(|v| v.clone().try_cast::<RhaiMap>())
    {
        for (k, v) in h.iter() {
            let name = match HeaderName::from_bytes(k.as_bytes()) {
                Ok(n) => n,
                Err(e) => {
                    err(&mut out, format!("http_call: bad header name {k:?}: {e}"));
                    return out;
                }
            };
            let val_str = v
                .clone()
                .try_cast::<ImmutableString>()
                .map(|s| s.to_string())
                .unwrap_or_default();
            let value = match HeaderValue::from_str(&val_str) {
                Ok(v) => v,
                Err(e) => {
                    err(
                        &mut out,
                        format!("http_call: bad header value for {k:?}: {e}"),
                    );
                    return out;
                }
            };
            headers.push((name, value));
        }
    }

    let body = get_str("body").map(bytes::Bytes::from);
    let cache_key = get_str("cache_key");

    let req = HttpRequest {
        method,
        url,
        headers,
        body,
        cache_key,
        ..Default::default()
    };

    let result = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let resp = client
                .make_request(req, crate::http::text_body_extractor().into())
                .await?;
            let status = resp.status.as_u16();
            let body = resp.text().await?.to_string();
            Ok::<_, crate::http::Error>((status, body))
        })
    });

    match result {
        Ok((status, body)) => {
            out.insert("status".into(), Dynamic::from(status as i64));
            out.insert("body".into(), Dynamic::from(body));
        }
        Err(e) => {
            err(&mut out, format!("http_call: {e}"));
        }
    }

    out
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
    let child_ids: Vec<Dynamic> = e
        .child_entry_ids
        .iter()
        .copied()
        .map(Dynamic::from)
        .collect();
    m.insert("child_ids".into(), Dynamic::from(child_ids));
    m
}

/// Convert a Rhai `Dynamic` to a `serde_json::Value`, for turning a script's
/// dedup-v2 relation metadata map (flat string/number/bool fields, plus
/// nested arrays/maps if a script ever needs them) into JSON for the
/// `entry_relation.extra` column. Not exhaustive — falls back to `Null` for
/// types metadata maps aren't expected to carry (closures, blobs, etc.).
fn rhai_dynamic_to_json(d: &Dynamic) -> serde_json::Value {
    if d.is_unit() {
        return serde_json::Value::Null;
    }
    if let Some(b) = d.clone().try_cast::<bool>() {
        return serde_json::Value::Bool(b);
    }
    if let Some(i) = d.clone().try_cast::<i64>() {
        return serde_json::Value::Number(i.into());
    }
    if let Some(f) = d.clone().try_cast::<f64>() {
        return serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null);
    }
    if let Some(s) = d.clone().try_cast::<ImmutableString>() {
        return serde_json::Value::String(s.to_string());
    }
    if let Some(arr) = d.clone().try_cast::<rhai::Array>() {
        return serde_json::Value::Array(arr.iter().map(rhai_dynamic_to_json).collect());
    }
    if let Some(map) = d.clone().try_cast::<RhaiMap>() {
        return serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| (k.to_string(), rhai_dynamic_to_json(v)))
                .collect(),
        );
    }
    serde_json::Value::Null
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
            metadata: map.get("metadata").map(rhai_dynamic_to_json),
        },
        _ => Verdict::Distinct,
    })
}

// ── Candidate blocking ────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Default)]
struct ChannelMask(u16);

impl ChannelMask {
    const CHANNELS: [(&'static str, u16); 7] = [
        ("exact_name", 1 << 0),
        ("token", 1 << 1),
        ("char_ngram", 1 << 2),
        ("duration_credit", 1 << 3),
        ("tracklist", 1 << 4),
        ("tracklist_overlap", 1 << 5),
        ("semantic_ann", 1 << 6),
    ];

    fn insert(&mut self, channel: &'static str) {
        let bit = Self::CHANNELS
            .iter()
            .find_map(|(name, bit)| (*name == channel).then_some(*bit))
            .expect("registered candidate channel");
        self.0 |= bit;
    }

    #[cfg(test)]
    fn contains(&self, channel: &str) -> bool {
        Self::CHANNELS
            .iter()
            .any(|(name, bit)| *name == channel && self.0 & bit != 0)
    }

    fn csv(self) -> String {
        Self::CHANNELS
            .iter()
            .filter_map(|(name, bit)| (self.0 & bit != 0).then_some(*name))
            .collect::<Vec<_>>()
            .join(";")
    }
}

type CandidateChannels = HashMap<(i64, i64), ChannelMask>;

fn candidate_pair(a: i64, b: i64) -> (i64, i64) {
    (a.min(b), a.max(b))
}

fn add_candidate(
    output: &mut CandidateChannels,
    a: i64,
    b: i64,
    channel: &'static str,
    focus: Option<&HashSet<i64>>,
) {
    if a == b || focus.is_some_and(|ids| !ids.contains(&a) && !ids.contains(&b)) {
        return;
    }
    output
        .entry(candidate_pair(a, b))
        .or_default()
        .insert(channel);
}

fn compact_normalized(value: &str) -> String {
    normalize(value).replace(' ', "")
}

fn char_trigrams(value: &str) -> BTreeSet<String> {
    let chars: Vec<char> = compact_normalized(value).chars().collect();
    if chars.is_empty() {
        return BTreeSet::new();
    }
    if chars.len() <= 3 {
        return [chars.into_iter().collect()].into_iter().collect();
    }
    (0..=chars.len() - 3)
        .map(|index| chars[index..index + 3].iter().collect())
        .collect()
}

fn emit_blocks<K: Eq + std::hash::Hash>(
    postings: HashMap<K, Vec<i64>>,
    channel: &'static str,
    max_block: usize,
    focus: Option<&HashSet<i64>>,
    output: &mut CandidateChannels,
) {
    for members in postings.into_values() {
        let members = dedup_sorted(members);
        if !(2..=max_block).contains(&members.len()) {
            continue;
        }
        for (index, &left) in members.iter().enumerate() {
            for &right in &members[index + 1..] {
                add_candidate(output, left, right, channel, focus);
            }
        }
    }
}

/// Bounded, entry-level lexical and structural retrieval. Every posting is
/// capped before pair expansion, and fuzzy trigrams retain only `ngram_k`
/// neighbors per entry. Channels are unioned instead of competing for one
/// global top-k, preserving complementary recall without an all-pairs scan.
fn generate_bounded_candidates(
    entries: &HashMap<i64, EntryInfo>,
    focus: Option<&HashSet<i64>>,
    max_block: usize,
    ngram_k: usize,
) -> CandidateChannels {
    let mut exact: HashMap<(String, String), Vec<i64>> = HashMap::new();
    let mut tokens: HashMap<(String, String), Vec<i64>> = HashMap::new();
    let mut grams: HashMap<(String, String), Vec<i64>> = HashMap::new();
    let mut duration_credit: HashMap<(i64, i64), Vec<i64>> = HashMap::new();
    let mut tracklists: HashMap<Vec<i64>, Vec<i64>> = HashMap::new();
    let mut tracks: HashMap<i64, Vec<i64>> = HashMap::new();

    for entry in entries.values() {
        if entry.entry_type == "unknown" {
            continue;
        }
        for alias in &entry.aliases {
            let normalized = normalize(alias);
            let compact = normalized.replace(' ', "");
            if compact.chars().count() >= 2 {
                exact
                    .entry((entry.entry_type.clone(), compact))
                    .or_default()
                    .push(entry.entry_id);
            }
            for token in normalized
                .split_whitespace()
                .filter(|token| token.chars().count() > 1)
            {
                tokens
                    .entry((entry.entry_type.clone(), token.to_string()))
                    .or_default()
                    .push(entry.entry_id);
            }
            for gram in char_trigrams(alias) {
                grams
                    .entry((entry.entry_type.clone(), gram))
                    .or_default()
                    .push(entry.entry_id);
            }
        }
        if entry.entry_type == "track" {
            for &peer in &entry.peer_entry_ids {
                for &duration in &entry.durations {
                    let bucket = duration / 5_000;
                    for nearby in [bucket - 1, bucket, bucket + 1] {
                        duration_credit
                            .entry((peer, nearby))
                            .or_default()
                            .push(entry.entry_id);
                    }
                }
            }
        }
        if entry.entry_type == "release" && !entry.child_entry_ids.is_empty() {
            let fingerprint = dedup_sorted(entry.child_entry_ids.clone());
            tracklists
                .entry(fingerprint.clone())
                .or_default()
                .push(entry.entry_id);
            for child in fingerprint {
                tracks.entry(child).or_default().push(entry.entry_id);
            }
        }
    }

    let mut output = CandidateChannels::new();
    emit_blocks(exact, "exact_name", max_block, focus, &mut output);
    emit_blocks(tokens, "token", max_block, focus, &mut output);
    emit_blocks(
        duration_credit,
        "duration_credit",
        max_block,
        focus,
        &mut output,
    );
    emit_blocks(tracklists, "tracklist", max_block, focus, &mut output);

    let mut gram_overlap: HashMap<i64, HashMap<i64, usize>> = HashMap::new();
    for members in grams.into_values() {
        let members = dedup_sorted(members);
        if !(2..=max_block).contains(&members.len()) {
            continue;
        }
        for (index, &left) in members.iter().enumerate() {
            for &right in &members[index + 1..] {
                *gram_overlap
                    .entry(left)
                    .or_default()
                    .entry(right)
                    .or_default() += 1;
                *gram_overlap
                    .entry(right)
                    .or_default()
                    .entry(left)
                    .or_default() += 1;
            }
        }
    }
    for (left, neighbors) in gram_overlap {
        let mut ranked: Vec<(usize, i64)> = neighbors.into_iter().map(|(id, n)| (n, id)).collect();
        ranked.sort_unstable_by(|a, b| b.cmp(a));
        for (shared, right) in ranked.into_iter().take(ngram_k) {
            if shared >= 2 {
                add_candidate(&mut output, left, right, "char_ngram", focus);
            }
        }
    }

    let mut track_overlap: HashMap<(i64, i64), usize> = HashMap::new();
    for members in tracks.into_values() {
        let members = dedup_sorted(members);
        if !(2..=max_block).contains(&members.len()) {
            continue;
        }
        for (index, &left) in members.iter().enumerate() {
            for &right in &members[index + 1..] {
                *track_overlap
                    .entry(candidate_pair(left, right))
                    .or_default() += 1;
            }
        }
    }
    for ((left, right), shared) in track_overlap {
        if shared < 2 {
            continue;
        }
        let a: HashSet<i64> = entries[&left].child_entry_ids.iter().copied().collect();
        let b: HashSet<i64> = entries[&right].child_entry_ids.iter().copied().collect();
        let union = a.union(&b).count();
        if union > 0 && shared as f64 / union as f64 >= 0.18 {
            add_candidate(&mut output, left, right, "tracklist_overlap", focus);
        }
    }
    output
}

/// Semantic blocking for a single entry: walk up to `max_pages` KNN pages and
/// return all candidate pairs not yet in `already`. Pairs with entries absent
/// from `all_entries` (merge losers) are skipped. Because vec0 returns neighbours
/// sorted by ascending L2 distance, the first neighbour that exceeds the
/// threshold terminates the walk for all subsequent pages too.
trait SemanticKnn {
    fn semantic_knn(
        &self,
        entry_id: i64,
        k: usize,
        entry_type: &str,
    ) -> anyhow::Result<Vec<(i64, f64)>>;
}

impl SemanticKnn for EmbeddingCache {
    fn semantic_knn(
        &self,
        entry_id: i64,
        k: usize,
        entry_type: &str,
    ) -> anyhow::Result<Vec<(i64, f64)>> {
        self.knn(entry_id, k, entry_type)
    }
}

impl SemanticKnn for EmbeddingAnn {
    fn semantic_knn(
        &self,
        entry_id: i64,
        k: usize,
        entry_type: &str,
    ) -> anyhow::Result<Vec<(i64, f64)>> {
        Ok(self.knn(entry_id, k, entry_type))
    }
}

fn generate_semantic_candidates_for_entry(
    entry: &EntryInfo,
    all_entries: &HashMap<i64, EntryInfo>,
    search: &impl SemanticKnn,
    base_k: usize,
    sim_threshold: f64,
    max_pages: usize,
    already: &mut HashSet<(i64, i64)>,
    channels: &mut CandidateChannels,
) -> Vec<(i64, i64)> {
    if entry.best_title.is_none() || entry.entry_type == "unknown" {
        return vec![];
    }
    // For unit vectors: L2² = 2(1 − cos_sim), so L2 = √(2(1 − cos_sim)).
    let l2_threshold = (2.0 * (1.0 - sim_threshold)).sqrt();
    let page_k = base_k;
    let mut pairs = Vec::new();

    for page in 0..max_pages.max(1) {
        let want = (page + 1) * page_k;
        let neighbors = match search.semantic_knn(entry.entry_id, want, &entry.entry_type) {
            Ok(neighbors) => neighbors,
            Err(error) => {
                warn!("semantic KNN failed for entry {}: {error}", entry.entry_id);
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
            channels.entry((a, b)).or_default().insert("semantic_ann");
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
    let mut children_by_release: HashMap<i64, Vec<i64>> = HashMap::new();
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
            children_by_release
                .entry(parent_id)
                .or_default()
                .push(child_id);
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
        let child_entry_ids =
            dedup_sorted(children_by_release.get(&e.id).cloned().unwrap_or_default());

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
            child_entry_ids,
            sourced_aliases,
        });
    }

    Ok(infos.into_iter().map(|e| (e.entry_id, e)).collect())
}

/// Scoped counterpart to `build_entry_infos`: assemble `EntryInfo` for exactly
/// `ids`, touching only those entries' own pairs and their immediate graph
/// neighbors (credited artists/tracks, parent/child releases) — never a full
/// table scan. Ids that no longer exist (e.g. a stale embedding-cache row
/// pointing at a merged-away entry) are silently omitted from the result.
pub async fn entry_infos_by_ids(
    db: &MusicDb,
    ids: &[i64],
) -> anyhow::Result<HashMap<i64, EntryInfo>> {
    let ids = dedup_sorted(ids.to_vec());
    if ids.is_empty() {
        return Ok(HashMap::new());
    }

    let entry_rows = db.entry_rows_by_ids(&ids).await?;
    let source_rows = db.source_rows_by_entry_ids(&ids).await?;

    let own_pairs: Vec<Pair> = dedup_sorted(
        source_rows
            .iter()
            .map(|s| (s.source.clone(), s.identifier.clone()))
            .collect(),
    );

    let alias_rows = db.alias_rows_for_pairs(&own_pairs).await?;
    let contrib_as_track = db.contrib_rows_for_track_pairs(&own_pairs).await?;
    let contrib_as_artist = db.contrib_rows_for_artist_pairs(&own_pairs).await?;
    let child_as_parent = db.child_rows_for_parent_pairs(&own_pairs).await?;
    let child_as_child = db.child_rows_for_child_pairs(&own_pairs).await?;

    // Resolve every pair these rows reference but that isn't already one of
    // our own pairs (the "other side" of a credit or a child edge) to its
    // entry id, so peer/child ids can be filled in without needing the whole
    // library's entry_source table in memory.
    let own_pair_set: HashSet<&Pair> = own_pairs.iter().collect();
    let mut referenced_pairs: Vec<Pair> = Vec::new();
    for c in &contrib_as_track {
        referenced_pairs.push((c.artist_source.clone(), c.artist_identifier.clone()));
    }
    for c in &contrib_as_artist {
        referenced_pairs.push((c.source.clone(), c.identifier.clone()));
    }
    for c in &child_as_parent {
        referenced_pairs.push((c.child_source.clone(), c.child_identifier.clone()));
    }
    for c in &child_as_child {
        referenced_pairs.push((c.parent_source.clone(), c.parent_identifier.clone()));
    }
    let extra_pairs: Vec<Pair> = dedup_sorted(referenced_pairs)
        .into_iter()
        .filter(|p| !own_pair_set.contains(p))
        .collect();
    let extra_sources = db.source_rows_for_pairs(&extra_pairs).await?;

    let mut pair_to_entry: HashMap<Pair, i64> = source_rows
        .iter()
        .map(|s| ((s.source.clone(), s.identifier.clone()), s.entry_id))
        .collect();
    for s in &extra_sources {
        pair_to_entry.insert((s.source.clone(), s.identifier.clone()), s.entry_id);
    }

    let mut sources_by_entry: HashMap<i64, Vec<&SourceRow>> = HashMap::new();
    for s in &source_rows {
        sources_by_entry.entry(s.entry_id).or_default().push(s);
    }
    let mut aliases_by_pair: HashMap<Pair, Vec<&AliasRow>> = HashMap::new();
    for a in &alias_rows {
        aliases_by_pair
            .entry((a.source.clone(), a.identifier.clone()))
            .or_default()
            .push(a);
    }

    // A release and one of its tracks can both be in `ids`, in which case the
    // same underlying child_entry row comes back from both the parent-side and
    // child-side queries — dedupe before folding into the position/children maps.
    let mut seen_child_rows: HashSet<(String, String, String, String)> = HashSet::new();
    #[allow(clippy::type_complexity)]
    let mut positions_by_track: HashMap<i64, Vec<(i64, Option<i32>, Option<i32>)>> = HashMap::new();
    let mut children_by_release: HashMap<i64, Vec<i64>> = HashMap::new();
    for c in child_as_parent.iter().chain(child_as_child.iter()) {
        let key = (
            c.parent_source.clone(),
            c.parent_identifier.clone(),
            c.child_source.clone(),
            c.child_identifier.clone(),
        );
        if !seen_child_rows.insert(key) {
            continue;
        }
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
            children_by_release
                .entry(parent_id)
                .or_default()
                .push(child_id);
        }
    }

    // Same duplication risk for a track/artist pair both being in `ids`.
    let mut seen_contrib_rows: HashSet<(String, String, String, String)> = HashSet::new();
    let mut track_to_artists: HashMap<i64, HashSet<i64>> = HashMap::new();
    let mut artist_to_tracks: HashMap<i64, HashSet<i64>> = HashMap::new();
    for c in contrib_as_track.iter().chain(contrib_as_artist.iter()) {
        let key = (
            c.source.clone(),
            c.identifier.clone(),
            c.artist_source.clone(),
            c.artist_identifier.clone(),
        );
        if !seen_contrib_rows.insert(key) {
            continue;
        }
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

    let mut infos = HashMap::with_capacity(entry_rows.len());
    for e in &entry_rows {
        let sources = sources_by_entry
            .get(&e.id)
            .map_or(&[][..], |v| v.as_slice());
        let mut pairs: Vec<Pair> = sources
            .iter()
            .map(|s| (s.source.clone(), s.identifier.clone()))
            .collect();
        pairs.sort();

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
        let child_entry_ids =
            dedup_sorted(children_by_release.get(&e.id).cloned().unwrap_or_default());

        infos.insert(
            e.id,
            EntryInfo {
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
                child_entry_ids,
                sourced_aliases,
            },
        );
    }

    Ok(infos)
}

/// Persisted-index equivalent of the per-entry postings `generate_bounded_candidates`
/// builds in memory: every key this entry would insert itself under, across all
/// channels. Symmetric with lookup — an entry is discoverable by anything that
/// shares at least one of these keys via `dedup_block_key`.
fn block_keys_for_entry(entry: &EntryInfo) -> Vec<String> {
    let mut keys = Vec::new();
    if entry.entry_type == "unknown" {
        return keys;
    }
    for alias in &entry.aliases {
        let compact = compact_normalized(alias);
        if compact.chars().count() >= 2 {
            keys.push(format!("exact|{}|{}", entry.entry_type, compact));
        }
        let normalized = normalize(alias);
        for token in normalized
            .split_whitespace()
            .filter(|token| token.chars().count() > 1)
        {
            keys.push(format!("token|{}|{}", entry.entry_type, token));
        }
        for gram in char_trigrams(alias) {
            keys.push(format!("gram|{}|{}", entry.entry_type, gram));
        }
    }
    if entry.entry_type == "track" {
        for &peer in &entry.peer_entry_ids {
            for &duration in &entry.durations {
                let bucket = duration / 5_000;
                for nearby in [bucket - 1, bucket, bucket + 1] {
                    keys.push(format!("dur|{peer}|{nearby}"));
                }
            }
        }
    }
    if entry.entry_type == "release" && !entry.child_entry_ids.is_empty() {
        let fingerprint = dedup_sorted(entry.child_entry_ids.clone());
        let fp_key = fingerprint
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        keys.push(format!("tracklistfp|{fp_key}"));
        for &child in &fingerprint {
            keys.push(format!("trackchild|{child}"));
        }
    }
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// Recompute and persist block keys for `entries`. Call before querying the
/// index against these entries so newly-created or newly-enriched entries are
/// immediately discoverable, and so an existing entry's stale keys (from an
/// alias/credit/tracklist change) don't linger.
async fn reindex_block_keys(db: &MusicDb, entries: &HashMap<i64, EntryInfo>) -> anyhow::Result<()> {
    for entry in entries.values() {
        let keys = block_keys_for_entry(entry);
        db.replace_block_keys(entry.entry_id, &keys).await?;
    }
    Ok(())
}

/// Catch the persisted block-key index up with entries it has never seen — a
/// library that predates this index, or one upgraded from before it existed.
/// Pages through unindexed entries in bounded batches so peak memory stays
/// flat regardless of library size. Safe to call repeatedly: it's a cheap
/// no-op once caught up, since `match_new_entries` and the full-scan path
/// keep every entry they touch reindexed on their own from then on. Intended
/// to be run once (e.g. from the `dedup` maintenance binary) after adopting
/// the focused/online dedup path against an existing library.
pub async fn backfill_block_key_index(db: &MusicDb) -> anyhow::Result<usize> {
    const BATCH: usize = 500;
    let mut total = 0usize;
    loop {
        let ids = db.unindexed_entry_ids(BATCH).await?;
        if ids.is_empty() {
            break;
        }
        let batch_len = ids.len();
        let batch_entries = entry_infos_by_ids(db, &ids).await?;
        reindex_block_keys(db, &batch_entries).await?;
        total += batch_entries.len();
        info!("Block-key index backfill: {total} entr(y/ies) indexed so far");
        if batch_len < BATCH {
            break;
        }
    }
    Ok(total)
}

/// Entry ids sharing `key` with the caller, or empty if the block is larger
/// than `max_block` (a hub — too generic to be useful, matching the in-memory
/// blocker's cutoff in `emit_blocks`).
async fn capped_block_members(
    db: &MusicDb,
    key: &str,
    max_block: usize,
) -> anyhow::Result<Vec<i64>> {
    let members = db.block_key_members(key, max_block).await?;
    if members.len() > max_block {
        Ok(Vec::new())
    } else {
        Ok(members)
    }
}

#[allow(clippy::too_many_arguments)]
async fn add_block_candidates(
    db: &MusicDb,
    key: &str,
    max_block: usize,
    from_id: i64,
    channel: &'static str,
    focus_set: &HashSet<i64>,
    output: &mut CandidateChannels,
    candidate_ids: &mut HashSet<i64>,
) -> anyhow::Result<()> {
    for member in capped_block_members(db, key, max_block).await? {
        if member == from_id {
            continue;
        }
        add_candidate(output, from_id, member, channel, Some(focus_set));
        candidate_ids.insert(member);
    }
    Ok(())
}

/// DB-driven candidate retrieval for the online/focused soft-match pass: looks
/// up each focus entry's own block keys against the persisted `dedup_block_key`
/// index (reindexing the focus entries first so their keys are current), plus
/// one semantic KNN query per focus entry when embeddings are configured.
/// Never loads the full library into memory or builds the in-RAM HNSW index —
/// cost is O(focus entries × candidates found), not O(library).
#[allow(clippy::too_many_arguments)]
async fn generate_focused_candidates(
    db: &MusicDb,
    focus_entries: &HashMap<i64, EntryInfo>,
    max_block: usize,
    ngram_k: usize,
    embed_cache: Option<&EmbeddingCache>,
    embed_k: usize,
    embed_sim_threshold: f64,
    embed_max_pages: usize,
) -> anyhow::Result<(CandidateChannels, HashMap<i64, EntryInfo>)> {
    if focus_entries.is_empty() {
        return Ok((CandidateChannels::new(), HashMap::new()));
    }
    let focus_set: HashSet<i64> = focus_entries.keys().copied().collect();
    reindex_block_keys(db, focus_entries).await?;

    let mut output = CandidateChannels::new();
    let mut candidate_ids: HashSet<i64> = focus_set.clone();

    // exact_name / token / duration_credit / tracklist(fingerprint): plain
    // membership lookups, capped the same way `emit_blocks` caps in-memory blocks.
    for entry in focus_entries.values() {
        if entry.entry_type == "unknown" {
            continue;
        }
        for alias in &entry.aliases {
            let compact = compact_normalized(alias);
            if compact.chars().count() >= 2 {
                let key = format!("exact|{}|{}", entry.entry_type, compact);
                add_block_candidates(
                    db,
                    &key,
                    max_block,
                    entry.entry_id,
                    "exact_name",
                    &focus_set,
                    &mut output,
                    &mut candidate_ids,
                )
                .await?;
            }
            let normalized = normalize(alias);
            for token in normalized
                .split_whitespace()
                .filter(|token| token.chars().count() > 1)
            {
                let key = format!("token|{}|{}", entry.entry_type, token);
                add_block_candidates(
                    db,
                    &key,
                    max_block,
                    entry.entry_id,
                    "token",
                    &focus_set,
                    &mut output,
                    &mut candidate_ids,
                )
                .await?;
            }
        }
        if entry.entry_type == "track" {
            for &peer in &entry.peer_entry_ids {
                for &duration in &entry.durations {
                    let bucket = duration / 5_000;
                    for nearby in [bucket - 1, bucket, bucket + 1] {
                        let key = format!("dur|{peer}|{nearby}");
                        add_block_candidates(
                            db,
                            &key,
                            max_block,
                            entry.entry_id,
                            "duration_credit",
                            &focus_set,
                            &mut output,
                            &mut candidate_ids,
                        )
                        .await?;
                    }
                }
            }
        }
        if entry.entry_type == "release" && !entry.child_entry_ids.is_empty() {
            let fingerprint = dedup_sorted(entry.child_entry_ids.clone());
            let fp_key = fingerprint
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let key = format!("tracklistfp|{fp_key}");
            add_block_candidates(
                db,
                &key,
                max_block,
                entry.entry_id,
                "tracklist",
                &focus_set,
                &mut output,
                &mut candidate_ids,
            )
            .await?;
        }
    }

    // char_ngram: rank by shared-trigram count, same as the in-memory blocker,
    // but only among entries that actually share a trigram with this one.
    for entry in focus_entries.values() {
        if entry.entry_type == "unknown" {
            continue;
        }
        let mut grams: HashSet<String> = HashSet::new();
        for alias in &entry.aliases {
            grams.extend(char_trigrams(alias));
        }
        let mut shared: HashMap<i64, usize> = HashMap::new();
        for gram in &grams {
            let key = format!("gram|{}|{}", entry.entry_type, gram);
            for member in capped_block_members(db, &key, max_block).await? {
                if member == entry.entry_id {
                    continue;
                }
                *shared.entry(member).or_default() += 1;
            }
        }
        let mut ranked: Vec<(usize, i64)> = shared.into_iter().map(|(id, n)| (n, id)).collect();
        ranked.sort_unstable_by(|a, b| b.cmp(a));
        for (count, member) in ranked.into_iter().take(ngram_k) {
            if count >= 2 {
                add_candidate(
                    &mut output,
                    entry.entry_id,
                    member,
                    "char_ngram",
                    Some(&focus_set),
                );
                candidate_ids.insert(member);
            }
        }
    }

    // tracklist_overlap: candidates found via shared child tracks, confirmed by
    // Jaccard ratio once full child lists are available (after the batched fetch below).
    let mut overlap_pending: Vec<(i64, i64, usize)> = Vec::new();
    for entry in focus_entries.values() {
        if entry.entry_type != "release" || entry.child_entry_ids.is_empty() {
            continue;
        }
        let mut shared: HashMap<i64, usize> = HashMap::new();
        for &child in &entry.child_entry_ids {
            let key = format!("trackchild|{child}");
            for member in capped_block_members(db, &key, max_block).await? {
                if member == entry.entry_id {
                    continue;
                }
                *shared.entry(member).or_default() += 1;
            }
        }
        for (member, count) in shared {
            if count >= 2 {
                candidate_ids.insert(member);
                overlap_pending.push((entry.entry_id, member, count));
            }
        }
    }

    // semantic_ann: direct indexed KNN per focus entry — no in-RAM HNSW build.
    let mut semantic_pending: Vec<(i64, i64)> = Vec::new();
    if let Some(cache) = embed_cache {
        let l2_threshold = (2.0 * (1.0 - embed_sim_threshold)).sqrt();
        for entry in focus_entries.values() {
            if entry.best_title.is_none() || entry.entry_type == "unknown" {
                continue;
            }
            let mut exhausted = false;
            for page in 0..embed_max_pages.max(1) {
                if exhausted {
                    break;
                }
                let want = (page + 1) * embed_k;
                let neighbors = match cache.knn(entry.entry_id, want, &entry.entry_type) {
                    Ok(n) => n,
                    Err(error) => {
                        warn!("semantic KNN failed for entry {}: {error}", entry.entry_id);
                        break;
                    }
                };
                for (neighbor_id, dist) in neighbors.into_iter().skip(page * embed_k) {
                    if dist > l2_threshold {
                        exhausted = true;
                        break;
                    }
                    candidate_ids.insert(neighbor_id);
                    semantic_pending.push((entry.entry_id, neighbor_id));
                }
            }
        }
    }

    let all_ids: Vec<i64> = candidate_ids.into_iter().collect();
    let entries = entry_infos_by_ids(db, &all_ids).await?;

    for (a, b, shared) in overlap_pending {
        let (Some(ea), Some(eb)) = (entries.get(&a), entries.get(&b)) else {
            continue;
        };
        let a_children: HashSet<i64> = ea.child_entry_ids.iter().copied().collect();
        let b_children: HashSet<i64> = eb.child_entry_ids.iter().copied().collect();
        let union = a_children.union(&b_children).count();
        if union > 0 && shared as f64 / union as f64 >= 0.18 {
            add_candidate(&mut output, a, b, "tracklist_overlap", Some(&focus_set));
        }
    }

    for (a, b) in semantic_pending {
        if entries.contains_key(&a) && entries.contains_key(&b) {
            add_candidate(&mut output, a, b, "semantic_ann", Some(&focus_set));
        }
    }

    Ok((output, entries))
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
    http_client: Option<Arc<dyn HttpClient>>,
) -> anyhow::Result<ScriptCtx<'static>> {
    let script = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("reading script {path}"))?;
    let script_dir = Path::new(path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_owned();
    let regex_cache: RegexCache = Arc::new(Mutex::new(HashMap::new()));
    let engine = build_rhai_engine(&script_dir, regex_cache, embed_cache, http_client);
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
///
/// `precomputed`, when set, is the candidate/channel map the caller already
/// retrieved from the DB (see `generate_focused_candidates`) — skips the
/// in-memory `generate_bounded_candidates` scan and `EmbeddingAnn` build
/// entirely, since both would otherwise materialize the full library that
/// `entries` (deliberately just focus ∪ candidates for this path) doesn't have.
/// Returns `[merge, relate, distinct, defer]` counts per entry type.
#[allow(clippy::too_many_arguments)]
async fn score_candidates(
    db: &MusicDb,
    entries: HashMap<i64, EntryInfo>,
    focus: Option<&HashSet<i64>>,
    ctx: &ScriptCtx<'_>,
    barrier: &HashMap<Pair, crate::pipeline::dedup::AnchorId>,
    config: &SoftMatchConfig,
    embed_cache: Option<&EmbeddingCache>,
    precomputed: Option<CandidateChannels>,
) -> anyhow::Result<HashMap<String, [usize; 4]>> {
    // Optional CSV output for manual quality review.
    let mut csv: Option<std::io::BufWriter<std::fs::File>> = if let Some(path) = &config.csv_path {
        let f = std::fs::File::create(path).with_context(|| format!("creating CSV file {path}"))?;
        let mut w = std::io::BufWriter::new(f);
        writeln!(
            w,
            "verdict,kind,confidence,reason,candidate_channels,type,\
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

    let learned_model = config
        .model_path
        .as_deref()
        .map(Path::new)
        .map(DedupModel::load)
        .transpose()?;
    if let Some(model) = &learned_model {
        let supported: HashSet<&str> = RUNTIME_FEATURE_NAMES.iter().copied().collect();
        let unsupported: Vec<&str> = model
            .feature_names()
            .filter(|name| !supported.contains(name))
            .collect();
        if !unsupported.is_empty() {
            anyhow::bail!(
                "dedup model requires unsupported runtime features: {}",
                unsupported.join(", ")
            );
        }
        if let Some(required) = model.required_embedding_model() {
            if embed_cache.is_none() {
                anyhow::bail!(
                    "dedup model requires local embeddings from {required:?}, but embeddings are disabled"
                );
            }
            if config.embed_model_id.as_deref() != Some(required) {
                anyhow::bail!(
                    "dedup model requires embedding model {required:?}; pass --embedding-model-id {required:?} only when the configured local embed() hook uses it"
                );
            }
        }
    }
    if let Some(path) = &config.model_path {
        info!("Loaded learned dedup scorer from {path}");
    }
    let soft_identity = db.soft_identity_projection().await?;
    if !soft_identity.conflicts.is_empty() {
        warn!(
            conflicts = soft_identity.conflicts.len(),
            "soft identity graph contains conflicting assertions; cannot-link edges won"
        );
    }
    let mut stats: HashMap<String, [usize; 4]> = HashMap::new();

    // `queued` tracks every pair ever added to `work_queue` or already decided,
    // preventing duplicates. No softmatch verdict physically removes an entry
    // from `entries` any more (MERGE is soft — see `SoftMatchConfig::apply_relates`),
    // so unlike the entry-merging era there is no rerouting/re-queueing to do here.
    // The focused/online path already did retrieval against the DB (see
    // `generate_focused_candidates`) and hands us the finished channel map —
    // recomputing it here in memory would silently fall back to seeing only
    // `entries` (focus ∪ discovered candidates) instead of the full library,
    // which would look like it worked while actually missing most matches.
    let used_precomputed = precomputed.is_some();
    let mut candidate_channels = match precomputed {
        Some(channels) => channels,
        None => generate_bounded_candidates(
            &entries,
            focus,
            config.candidate_max_block,
            config.candidate_ngram_k,
        ),
    };
    let mut queued: HashSet<(i64, i64)> = candidate_channels.keys().copied().collect();
    let mut lexical_pairs: Vec<(i64, i64)> = queued.iter().copied().collect();
    lexical_pairs.sort_unstable();
    let mut work_queue: VecDeque<(i64, i64)> = lexical_pairs.into();
    info!(
        "Bounded lexical/structural retrieval: {} candidate pair(s)",
        work_queue.len()
    );

    // Seed the queue from the focused entries (or all entries for a full scan).
    // The precomputed path already ran semantic retrieval per focus entry via
    // direct indexed KNN queries (see `generate_focused_candidates`), so an
    // in-RAM `EmbeddingAnn` — which rebuilds all four type-local HNSW graphs
    // from every vector in the library — is only worth its build cost for a
    // full scan that's going to touch most of the library anyway.
    let seed_ids: Vec<i64> = match focus {
        Some(f) => f.iter().copied().collect(),
        None => entries.keys().copied().collect(),
    };
    if !used_precomputed {
        let ann_index = embed_cache.map(EmbeddingAnn::build).transpose()?;
        if let Some(ann) = &ann_index {
            for id in seed_ids {
                if let Some(entry) = entries.get(&id) {
                    let pairs = generate_semantic_candidates_for_entry(
                        entry,
                        &entries,
                        ann,
                        config.embed_k,
                        config.embed_sim_threshold,
                        config.embed_max_pages,
                        &mut queued,
                        &mut candidate_channels,
                    );
                    work_queue.extend(pairs);
                }
            }
        } else {
            info!("Semantic blocking disabled; using lexical/structural candidates only.");
        }
    }
    info!("Initial queue: {} candidate pair(s)", work_queue.len());

    let mut total_scored = 0usize;

    while let Some((id_a, id_b)) = work_queue.pop_front() {
        let (ea, eb) = match (entries.get(&id_a), entries.get(&id_b)) {
            (Some(a), Some(b)) => (a.clone(), b.clone()),
            _ => continue,
        };

        if soft_identity.are_same(id_a, id_b) {
            if let Some(w) = &mut csv {
                let channels = candidate_channels.get(&(id_a, id_b));
                write_csv_row(w, "SOFT_SAME", "", 1.0, "", channels, &ea, &eb, ctx)?;
            }
            continue;
        }
        if soft_identity.are_different(id_a, id_b) {
            if let Some(w) = &mut csv {
                let channels = candidate_channels.get(&(id_a, id_b));
                write_csv_row(w, "SOFT_BARRIER", "", 0.0, "", channels, &ea, &eb, ctx)?;
            }
            continue;
        }

        if barrier_blocks(&ea.pairs, &eb.pairs, barrier) {
            if let Some(w) = &mut csv {
                let channels = candidate_channels.get(&(id_a, id_b));
                write_csv_row(w, "BARRIER", "", 0.0, "", channels, &ea, &eb, ctx)?;
            }
            continue;
        }

        total_scored += 1;
        let channels = candidate_channels.get(&(id_a, id_b));
        apply_candidate(
            db,
            &ea,
            &eb,
            channels,
            ctx,
            learned_model.as_ref(),
            embed_cache,
            config,
            &mut csv,
            &mut stats,
        )
        .await?;
    }

    info!("Scored {total_scored} candidate pair(s)");
    Ok(stats)
}

/// Resolve a `derived_from` script's script-relative `"derived_side": "a"|"b"`
/// metadata into the two absolute entry ids the direction is actually about
/// (`derived_entry`/`source_entry`), and drop the transient `"a"`/`"b"` label
/// -- it would be ambiguous to a later reader once `entry_a`/`entry_b` have
/// been canonicalized to `(min(id), max(id))` by `upsert_relation_on`, which
/// has nothing to do with which side is the original vs. the transformation.
fn resolve_derived_side(extra: &mut serde_json::Value, ea_id: i64, eb_id: i64) {
    let Some(obj) = extra.as_object_mut() else {
        return;
    };
    let Some(side) = obj.remove("derived_side") else {
        return;
    };
    let (derived, source) = match side.as_str() {
        Some("a") => (ea_id, eb_id),
        Some("b") => (eb_id, ea_id),
        _ => return,
    };
    obj.insert("derived_entry".to_string(), serde_json::json!(derived));
    obj.insert("source_entry".to_string(), serde_json::json!(source));
}

/// Score one candidate pair with the Rhai script, emit its CSV row and console
/// log, apply the DB write when `apply_relates` is set, and bump `stats`. Returns
/// the verdict so the caller can measure per-page merge rate for adaptive paging.
async fn apply_candidate(
    db: &MusicDb,
    ea: &EntryInfo,
    eb: &EntryInfo,
    channels: Option<&ChannelMask>,
    ctx: &ScriptCtx<'_>,
    learned_model: Option<&DedupModel>,
    embed_cache: Option<&EmbeddingCache>,
    config: &SoftMatchConfig,
    csv: &mut Option<std::io::BufWriter<std::fs::File>>,
    stats: &mut HashMap<String, [usize; 4]>,
) -> anyhow::Result<Verdict> {
    let (verdict, origin, model_version): (Verdict, &'static str, Option<String>) =
        if config.jev_entry_types.contains(&ea.entry_type) {
            let verdict = crate::pipeline::jev::score_pair(db, ea, eb, config).await?;
            (
                verdict,
                "jev",
                Some(crate::pipeline::jev::MODEL_VERSION.to_string()),
            )
        } else if let Some(model) = learned_model {
            let features = learned_features(ea, eb, embed_cache);
            let probability = model.probability(&ea.entry_type, &features)?;
            let decision = model.decide(probability);
            if config.persist_suggestions && decision != ModelDecision::Separate {
                let sorted_features: BTreeMap<&str, f64> = features
                    .iter()
                    .map(|(name, value)| (name.as_str(), *value))
                    .collect();
                db.upsert_dedup_suggestion(NewDedupSuggestion {
                    entry_a: ea.entry_id,
                    entry_b: eb.entry_id,
                    model_version: model.version().to_owned(),
                    probability,
                    decision: match decision {
                        ModelDecision::Merge => "merge",
                        ModelDecision::Defer => "defer",
                        ModelDecision::Separate => unreachable!(),
                    }
                    .to_owned(),
                    candidate_channels: channels.copied().map(ChannelMask::csv).unwrap_or_default(),
                    features: serde_json::to_string(&sorted_features)?,
                    evidence: serde_json::to_string(&serde_json::json!({
                        "left": ea,
                        "right": eb,
                    }))?,
                })
                .await
                .map_err(|error| anyhow::anyhow!("persisting dedup suggestion: {error}"))?;
            }
            let reason = "learned musiclib-entry-info/1 scorer".to_owned();
            let verdict = match decision {
                ModelDecision::Merge => Verdict::Merge {
                    confidence: probability,
                    reason,
                },
                ModelDecision::Separate => Verdict::Separate {
                    confidence: probability,
                    reason,
                },
                ModelDecision::Defer => Verdict::Defer {
                    confidence: probability,
                    reason,
                },
            };
            (verdict, "heuristic", Some(model.version().to_owned()))
        } else {
            let verdict = call_script(
                &ctx.engine,
                &ctx.ast,
                &ctx.base_scope,
                &ctx.user_ctx,
                ea,
                eb,
            )?;
            (verdict, "heuristic", None)
        };

    if let Some(w) = csv {
        let (vname, kind, conf, reason) = match &verdict {
            Verdict::Merge { confidence, reason } => ("MERGE", "", *confidence, reason.as_str()),
            Verdict::Relate {
                kind,
                confidence,
                reason,
                ..
            } => ("RELATE", kind.as_str(), *confidence, reason.as_str()),
            Verdict::Defer { confidence, reason } => ("DEFER", "", *confidence, reason.as_str()),
            Verdict::Separate { confidence, reason } => {
                ("SEPARATE", "", *confidence, reason.as_str())
            }
            Verdict::Distinct => ("DISTINCT", "", 0.0, ""),
        };
        write_csv_row(w, vname, kind, conf, reason, channels, ea, eb, ctx)?;
    }

    let counters = stats.entry(ea.entry_type.clone()).or_insert([0; 4]);

    match &verdict {
        Verdict::Distinct | Verdict::Separate { .. } => {
            counters[2] += 1;
        }
        Verdict::Defer { .. } => {
            counters[3] += 1;
        }
        Verdict::Merge { confidence, reason } => {
            counters[0] += 1;
            if config.verbose_decisions {
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
            }

            // MERGE is soft: it asserts `same_identity` via the same reversible
            // path the player's manual "link" button uses
            // (`MusicDb::record_identity_feedback`), never `merge_entries`
            // (destructive, no undo path). Hard merge stays exclusively an
            // import-time/dedup-barrier operation — no softmatch verdict, from
            // any backend, triggers it.
            if config.apply_relates {
                db.record_identity_feedback(NewDedupFeedback {
                    entry_a: ea.entry_id,
                    entry_b: eb.entry_id,
                    judgment: IdentityJudgment::Same,
                    origin: origin.to_string(),
                    model_version: model_version.clone(),
                    probability: Some(*confidence),
                    candidate_channels: channels.copied().map(ChannelMask::csv),
                    features: None,
                    evidence: Some(reason.clone()),
                    note: None,
                    supersedes_id: None,
                })
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
                info!(
                    "Soft-merged (same_identity) entry {} and {} [{origin}]",
                    ea.entry_id, eb.entry_id
                );
            }
        }
        Verdict::Relate {
            kind,
            confidence,
            reason,
            metadata,
        } => {
            counters[1] += 1;
            if config.verbose_decisions {
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
            }

            if config.apply_relates {
                // `reason` is always present; dedup-v2 metadata (if the script
                // used the 4-arg `relate(...)` overload) is folded in under
                // its own fields so `extra` stays a flat, queryable object
                // rather than nesting a `metadata` object one level deep.
                let mut extra = serde_json::json!({ "reason": reason });
                if let (Some(obj), Some(serde_json::Value::Object(meta))) =
                    (extra.as_object_mut(), metadata)
                {
                    obj.extend(meta.clone());
                }
                // `entry_relation` is undirected in storage (`upsert_relation_on`
                // always canonicalizes to `(min(id), max(id))`), so a script's
                // "a"/"b"-relative direction is meaningless once persisted --
                // resolve it into absolute entry ids now, while `ea`/`eb` (the
                // script's actual a/b) are still in scope.
                resolve_derived_side(&mut extra, ea.entry_id, eb.entry_id);
                let extra_json = serde_json::to_string(&extra)?;

                db.upsert_relation(
                    ea.entry_id,
                    eb.entry_id,
                    kind,
                    *confidence,
                    origin,
                    Some(&extra_json),
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
    channels: Option<&ChannelMask>,
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
        "{},{},{:.4},{},{},{},{},{},{},{},{},{},{:.4},{},{},{},{},{},{:.4}",
        verdict,
        kind,
        confidence,
        csv_field(reason),
        csv_field(&channels.copied().map(ChannelMask::csv).unwrap_or_default(),),
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
    let ctx = load_script(
        &config.script_path,
        embed_cache.clone(),
        config.http_client.clone(),
    )
    .await?;
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
        None,
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
            println!(
                "  {t:14}  merge={} relate={} distinct={} defer={}",
                c[0], c[1], c[2], c[3]
            );
        }
    }
    if config.apply_relates {
        info!("RELATE and soft MERGE (same_identity) decisions written to DB.");
    } else {
        info!("Dry-run complete. Pass --apply to write decisions to the DB.");
    }

    Ok(())
}

/// Incremental scan: only compare pairs involving one of `new_entry_ids`.
/// Called automatically by the `import` binary after each flush so new entries
/// are soft-matched against the existing library online.
///
/// Unlike the full-scan path, this never materializes the library: it loads
/// `EntryInfo` for exactly `new_entry_ids` and whatever candidates the
/// persisted block-key index / embedding KNN turn up for them (see
/// `generate_focused_candidates`), so cost scales with candidates found, not
/// library size.
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
    let focus_ids: Vec<i64> = new_entry_ids.iter().copied().collect();
    let focus_entries = entry_infos_by_ids(db, &focus_ids).await?;
    let (barrier, _) = dedup.compile(providers).await;
    let entries_slice: Vec<EntryInfo> = focus_entries.values().cloned().collect();
    let embed_cache: Option<Arc<EmbeddingCache>> = open_embed_cache(config, &entries_slice).await;
    let ctx = load_script(
        &config.script_path,
        embed_cache.clone(),
        config.http_client.clone(),
    )
    .await?;
    let (candidate_channels, entries) = generate_focused_candidates(
        db,
        &focus_entries,
        config.candidate_max_block,
        config.candidate_ngram_k,
        embed_cache.as_deref(),
        config.embed_k,
        config.embed_sim_threshold,
        config.embed_max_pages,
    )
    .await?;
    score_candidates(
        db,
        entries,
        Some(new_entry_ids),
        &ctx,
        &barrier,
        config,
        embed_cache.as_deref(),
        Some(candidate_channels),
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
    run_blocking(|| {
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
            child_entry_ids: vec![],
            sourced_aliases,
        }
    }

    #[test]
    fn bounded_candidates_union_exact_and_fuzzy_channels() {
        let mut a = titled_entry(1, "Shared Name", "musicbrainz", vec![]);
        let mut b = titled_entry(2, "shared-name", "discogs", vec![]);
        a.entry_type = "artist".into();
        b.entry_type = "artist".into();
        let entries = [(1, a), (2, b)].into_iter().collect();
        let candidates = generate_bounded_candidates(&entries, None, 50, 30);
        let channels = &candidates[&(1, 2)];
        assert!(channels.contains("exact_name"));
        assert!(channels.contains("char_ngram"));
        assert!(channels.contains("token"));
    }

    #[test]
    fn normalization_uses_nfkc_for_calibration_parity() {
        assert_eq!(normalize("ＡＢＣ—Live!"), "abc live");
    }

    async fn insert_track(
        db: &MusicDb,
        source: &str,
        identifier: &str,
        title: &str,
        duration_ms: i64,
    ) -> i64 {
        use crate::providers::types::{Alias, EntrySpecificData, EntryType};

        let entry_id = db.insert_entry(Some(EntryType::Track)).await.unwrap();
        db.upsert_pair(
            source,
            identifier,
            entry_id,
            None,
            &EntrySpecificData::Track {
                duration_ms: vec![duration_ms],
                positions: Default::default(),
            },
        )
        .await
        .unwrap();
        db.insert_aliases_for_pair(
            source,
            identifier,
            &[Alias {
                name: title.to_string(),
                source: source.to_string(),
                locale: None,
                extra: serde_json::Value::Null,
                primary: true,
            }],
        )
        .await
        .unwrap();
        entry_id
    }

    /// The focused/online path must find a pre-existing library entry as a
    /// candidate for a newly-imported one purely through the persisted
    /// `dedup_block_key` index — no full-library scan involved — and
    /// `entry_infos_by_ids` must assemble the same `EntryInfo` a full
    /// `build_entry_infos` load would have produced for the touched entries.
    #[tokio::test]
    async fn focused_candidates_find_exact_name_match_via_db_index() {
        let db = MusicDb::new("sqlite::memory:").await.unwrap();

        let existing_id = insert_track(&db, "youtube", "vid1", "Shiny Song", 180_000).await;
        // An unrelated entry must never surface as a candidate.
        let unrelated_id = insert_track(&db, "youtube", "vid2", "Totally Different", 42_000).await;
        // Simulate both having gone through a prior online pass, which is what
        // actually indexes an entry (the focused path only reindexes what it
        // touches — see `backfill_block_key_index` for the pre-existing-library
        // migration case this test intentionally isn't exercising).
        let prior = entry_infos_by_ids(&db, &[existing_id, unrelated_id])
            .await
            .unwrap();
        reindex_block_keys(&db, &prior).await.unwrap();

        let new_id = insert_track(&db, "spotify", "trk1", "Shiny Song", 180_500).await;
        let focus_entries = entry_infos_by_ids(&db, &[new_id]).await.unwrap();
        assert_eq!(focus_entries.len(), 1);
        assert_eq!(
            focus_entries[&new_id].aliases,
            vec!["Shiny Song".to_string()]
        );
        assert_eq!(focus_entries[&new_id].durations, vec![180_500]);

        let (channels, entries) =
            generate_focused_candidates(&db, &focus_entries, 50, 30, None, 20, 0.45, 1)
                .await
                .unwrap();

        assert!(entries.contains_key(&existing_id));
        let pair = (new_id.min(existing_id), new_id.max(existing_id));
        let mask = channels
            .get(&pair)
            .expect("pre-existing entry found as a candidate via the block-key index");
        assert!(mask.contains("exact_name"));
        assert!(!channels.contains_key(&(unrelated_id.min(new_id), unrelated_id.max(new_id))));
    }

    /// A library that predates the block-key index (or one where the index
    /// somehow drifted) must be recoverable by `backfill_block_key_index`
    /// without the caller doing anything else.
    #[tokio::test]
    async fn backfill_indexes_every_entry_exactly_once() {
        let db = MusicDb::new("sqlite::memory:").await.unwrap();
        let mut ids = Vec::new();
        for i in 0..1203 {
            ids.push(
                insert_track(
                    &db,
                    "youtube",
                    &format!("v{i}"),
                    &format!("Song {i}"),
                    1_000,
                )
                .await,
            );
        }

        let indexed = backfill_block_key_index(&db).await.unwrap();
        assert_eq!(indexed, ids.len());
        assert!(db.unindexed_entry_ids(10).await.unwrap().is_empty());

        // Idempotent: nothing left to do on a second run.
        assert_eq!(backfill_block_key_index(&db).await.unwrap(), 0);
    }

    #[test]
    fn learned_features_cover_runtime_model_contract() {
        let mut a = titled_entry(1, "Song (Live)", "isrc", vec![180_000]);
        let mut b = titled_entry(2, "Song [Live]", "spotify", vec![181_000]);
        a.entry_type = "track".into();
        b.entry_type = "track".into();
        a.pairs = vec![("isrc".into(), "JPABC1234567".into())];
        b.pairs = vec![("spotify".into(), "JPABC1234567".into())];
        a.peer_entry_ids = vec![7];
        b.peer_entry_ids = vec![7];
        let row = learned_features(&a, &b, None);
        let expected: HashSet<&str> = [
            "name_exact",
            "name_similarity",
            "token_jaccard",
            "ngram_jaccard",
            "identifier_overlap",
            "artist_jaccard",
            "tracklist_jaccard",
            "tracklist_length_similarity",
            "date_exact",
            "duration_similarity",
            "version_conflict",
            "base_title_exact",
            "qualifier_jaccard",
            "qualifier_conflict",
            "primary_type_match",
            "primary_type_conflict",
            "track_position_match",
            "internal_mixedness",
            "empty_side",
            "semantic_similarity",
        ]
        .into_iter()
        .collect();
        assert_eq!(
            row.keys().map(String::as_str).collect::<HashSet<_>>(),
            expected
        );
        assert_eq!(row["identifier_overlap"], 1.0);
        assert_eq!(row["artist_jaccard"], 1.0);
        assert_eq!(row["base_title_exact"], 1.0);
        assert_eq!(row["qualifier_jaccard"], 1.0);
        assert!((row["duration_similarity"] - (29.0 / 30.0)).abs() < 1e-12);
    }

    #[test]
    fn bounded_candidates_use_track_duration_and_credit() {
        let mut a = titled_entry(1, "異なる題", "musicbrainz", vec![180_000]);
        let mut b = titled_entry(2, "Different title", "spotify", vec![183_000]);
        a.entry_type = "track".into();
        b.entry_type = "track".into();
        a.peer_entry_ids = vec![99];
        b.peer_entry_ids = vec![99];
        let entries = [(1, a), (2, b)].into_iter().collect();
        let candidates = generate_bounded_candidates(&entries, None, 50, 30);
        assert!(candidates[&(1, 2)].contains("duration_credit"));
    }

    #[test]
    fn bounded_candidates_use_partial_release_tracklists() {
        let mut a = titled_entry(1, "Edition A", "musicbrainz", vec![]);
        let mut b = titled_entry(2, "Edition B", "discogs", vec![]);
        a.entry_type = "release".into();
        b.entry_type = "release".into();
        a.child_entry_ids = vec![10, 11, 12];
        b.child_entry_ids = vec![10, 11, 13];
        let entries = [(1, a), (2, b)].into_iter().collect();
        let candidates = generate_bounded_candidates(&entries, None, 50, 30);
        assert!(candidates[&(1, 2)].contains("tracklist_overlap"));
    }

    #[test]
    fn bounded_candidates_honor_incremental_focus() {
        let mut a = titled_entry(1, "Shared", "musicbrainz", vec![]);
        let mut b = titled_entry(2, "Shared", "discogs", vec![]);
        a.entry_type = "artist".into();
        b.entry_type = "artist".into();
        let entries = [(1, a), (2, b)].into_iter().collect();
        let absent = HashSet::from([3]);
        assert!(generate_bounded_candidates(&entries, Some(&absent), 50, 30).is_empty());
        let focused = HashSet::from([1]);
        assert!(
            generate_bounded_candidates(&entries, Some(&focused), 50, 30).contains_key(&(1, 2))
        );
    }

    /// The shipped example script must compile under the real engine and run its
    /// dependency-free naive embedding (no `ffi` feature, no inference plugin).
    #[test]
    fn example_script_compiles_and_embeds() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/config/match.example.rhai");
        let script = std::fs::read_to_string(path).expect("read example script");
        let dir = Path::new(path).parent().unwrap();

        let engine = build_rhai_engine(dir, Arc::new(Mutex::new(HashMap::new())), None, None);
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

    /// `to_json`/`parse_json` must round-trip an arbitrary Rhai value through a
    /// JSON string and back, since a script builds its own request bodies and
    /// parses its own responses with these (Rust never sees the shape).
    #[test]
    fn to_json_and_parse_json_round_trip() {
        let engine = build_rhai_engine(
            Path::new("."),
            Arc::new(Mutex::new(HashMap::new())),
            None,
            None,
        );
        let script = r#"
            let original = #{
                "str": "hello",
                "int": 42,
                "float": 1.5,
                "bool": true,
                "null": (),
                "arr": [1, "two", 3.0],
                "nested": #{"inner": "value"},
            };
            let json = to_json(original);
            let parsed = parse_json(json);
            parsed
        "#;
        let ast = engine.compile(script).expect("round-trip script compiles");
        let mut scope = Scope::new();
        let result: RhaiMap = engine
            .eval_ast_with_scope(&mut scope, &ast)
            .expect("round-trip script runs");

        assert_eq!(result["str"].clone().cast::<String>(), "hello");
        assert_eq!(result["int"].clone().as_int().unwrap(), 42);
        assert!((result["float"].clone().as_float().unwrap() - 1.5).abs() < 1e-9);
        assert!(result["bool"].clone().as_bool().unwrap());
        assert!(result["null"].is_unit());
        let arr = result["arr"].clone().cast::<rhai::Array>();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[1].clone().cast::<String>(), "two");
        let nested = result["nested"].clone().cast::<RhaiMap>();
        assert_eq!(nested["inner"].clone().cast::<String>(), "value");
    }

    /// `http_call` must never panic the script engine, even when nothing is
    /// wired up to actually make the request — it should hand back an `error`
    /// field the script can branch on. A live-network exercise of the real
    /// `HttpClient` path isn't done here (no tokio runtime in a plain `#[test]`
    /// for `block_in_place` to hand off to, and no live dependency in CI); the
    /// `None`-client short circuit is what every caller gets when no
    /// `http.yaml`/`--http-config` is configured, so it's the one guaranteed to
    /// run in practice.
    #[test]
    fn http_call_without_a_client_reports_an_error_not_a_panic() {
        let engine = build_rhai_engine(
            Path::new("."),
            Arc::new(Mutex::new(HashMap::new())),
            None,
            None, // no HttpClient configured
        );
        let script = r#"
            http_call(#{
                "method": "GET",
                "url": "https://example.invalid/",
            })
        "#;
        let ast = engine.compile(script).expect("http_call script compiles");
        let mut scope = Scope::new();
        let result: RhaiMap = engine
            .eval_ast_with_scope(&mut scope, &ast)
            .expect("http_call does not throw");

        assert!(
            result.contains_key("error"),
            "expected an error field, got {result:?}"
        );
        assert!(!result.contains_key("status"));
    }
}
