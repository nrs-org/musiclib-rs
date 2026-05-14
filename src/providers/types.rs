use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    sync::Arc,
};

use regex::Regex;

pub type OptionsId = u32;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::Mutex;

// dict of external identifier (mostly URLs), grouped by source (e.g. "wikidata", "spotify", etc.)
#[derive(Debug, Clone, Default)]
pub struct ExternalSources(pub HashMap<Cow<'static, str>, HashSet<String>>);

impl ExternalSources {
    pub fn get(&self, source: &str) -> Option<&HashSet<String>> {
        self.0.get(source)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the first identifier found across all sources, if any.
    pub fn first_identifier(&self) -> Option<&str> {
        self.0
            .values()
            .flat_map(|s| s.iter())
            .next()
            .map(|s| s.as_str())
    }
}

impl<T> From<T> for ExternalSources
where
    HashMap<Cow<'static, str>, HashSet<String>>: From<T>,
{
    fn from(sources: T) -> Self {
        ExternalSources(HashMap::from(sources))
    }
}

// track position within a release, with optional disc number (for multi-disc releases)
#[derive(Clone)]
pub struct TrackPosition {
    pub disc_no: Option<i32>,
    pub track_no: i32,
}

// specific data for different entry types
pub enum EntrySpecificData {
    Track {
        duration_ms: Option<i64>,
        positions: HashMap<String, TrackPosition>,
    },
    Release {
        release_type: Option<String>,
        num_discs: Option<i32>,
        num_tracks: Option<i32>,
    },
    ReleaseGroup {
        primary_type: Option<String>,
    },
    Artist,
}

// type of entry (artist, release group, release, track)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryType {
    Artist,
    ReleaseGroup,
    Release,
    #[default]
    Track,
}

// entry alias, with optional locale and extra metadata
#[derive(Default)]
pub struct Alias {
    pub name: String,
    pub source: String,
    pub locale: Option<String>,
    pub extra: serde_json::Value,
    // treat this with a grain of salt, an entry will have many "primary" aliases (each provider has at least one).
    pub primary: bool,
}

// reference to a child entry (e.g. track within a release), with optional name and position
#[derive(Default, Clone)]
pub struct ChildRef {
    pub entry_type: EntryType,
    /// Backend-specific type string (e.g. `"youtube:video"`, `"youtube:playlist"`).
    /// Empty string means unknown/unset.
    pub external_type: Cow<'static, str>,
    pub sources: ExternalSources,
    pub name: Option<String>,
    pub position: Option<TrackPosition>,
    pub contributions: Vec<Contribution>,
}

// contribution of an artist to an entry, with role (e.g. "main", "featured", "producer", etc.) and
// optional flag for main artist
#[derive(Default, Clone)]
pub struct Contribution {
    pub role: String,
    pub main_artist: bool,
    pub extra: serde_json::Value,
    pub source: Cow<'static, str>,
}

// result of fetching an entry, with optional release date, external sources, extra metadata,
// specific data for the entry type, child entries (e.g. tracks within a release), contributions
// (e.g. artists on a track), and aliases
pub struct EntityResult<T: Clone + Send + Sync + 'static = ChildFetchOptions> {
    pub release_date: Option<OffsetDateTime>,
    pub sources: ExternalSources,
    pub extra: serde_json::Value,
    pub specific_data: EntrySpecificData,
    pub children: Vec<Arc<CachedChildSource<T>>>,
    pub aliases: Vec<Alias>,
}

// result of canonicalizing an identifier, with canonical identifier, entry type, and external type
pub struct CanonicalizeResult {
    pub canonical_identifier: String,
    pub entry_type: EntryType,
    pub external_type: Cow<'static, str>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Invalid URL: {0}")]
    InvalidUrl(String),
    #[error("HTTP error: {0}")]
    Http(#[from] crate::http::Error),
    #[error("Invalid credentials: {0}")]
    InvalidCredentials(String),
    #[error("Missing credentials: {0}")]
    MissingCredentials(String),
    #[error("Invalid pattern: {0}")]
    InvalidPattern(String),
}

// Matchers based on entry metadata (requires fetching the entry)
#[derive(Clone, Serialize, Deserialize)]
pub enum EntryDataMatcher {
    // Generic — available for all backends
    EntryType(EntryType),
    ExternalType(String),
    NameRegex(String),
    DurationRange { min: Option<u64>, max: Option<u64> },
    HasSource(String),

    // Backend-specific
    YouTube(YouTubeDataMatcher),
    // MusicBrainz(MusicBrainzDataMatcher),
    // Spotify(SpotifyDataMatcher),
}

// YouTube-specific entry data matchers
#[derive(Clone, Serialize, Deserialize)]
pub enum YouTubeDataMatcher {
    DescriptionRegex(String),
    CategoryId(String),
}

// Matchers based on parent-child relationship (no fetch needed)
#[derive(Clone, Serialize, Deserialize)]
pub enum RelationMatcher {
    IndexRange { min: Option<u32>, max: Option<u32> },
}

// Matchers based on children (requires fetching children)
#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum QuantifierMode {
    Count { min: Option<u32>, max: Option<u32> },
    Ratio { min: Option<f64>, max: Option<f64> },
}

#[derive(Clone, Serialize, Deserialize)]
pub enum ChildMatcher {
    Always,
    EntryData(EntryDataMatcher),
    Relation(RelationMatcher),
    ChildrenSatisfy {
        matcher: Box<ChildMatcherExpr>,
        mode: QuantifierMode,
    },
}

#[derive(Clone, Serialize, Deserialize)]
pub enum ChildMatcherExpr {
    Matcher(ChildMatcher),
    Not(Box<ChildMatcherExpr>),
    All(Vec<ChildMatcherExpr>),
    Any(Vec<ChildMatcherExpr>),
}

// --- Compiled matcher types ---
// These mirror the config types but with regexes pre-compiled and All/Any sub-expressions
// sorted by cost. Built once per filter_children call, evaluated on every item.

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tribool {
    True,
    False,
    Indeterminate,
}

impl From<bool> for Tribool {
    fn from(b: bool) -> Self {
        if b { Tribool::True } else { Tribool::False }
    }
}

impl Tribool {
    pub fn not(self) -> Self {
        match self {
            Tribool::True => Tribool::False,
            Tribool::False => Tribool::True,
            Tribool::Indeterminate => Tribool::Indeterminate,
        }
    }

    pub fn and(self, other: Self) -> Self {
        match (self, other) {
            (Tribool::False, _) | (_, Tribool::False) => Tribool::False,
            (Tribool::True, Tribool::True) => Tribool::True,
            _ => Tribool::Indeterminate,
        }
    }

    pub fn or(self, other: Self) -> Self {
        match (self, other) {
            (Tribool::True, _) | (_, Tribool::True) => Tribool::True,
            (Tribool::False, Tribool::False) => Tribool::False,
            _ => Tribool::Indeterminate,
        }
    }
}

#[derive(Clone)]
pub enum CompiledYouTubeDataMatcher {
    DescriptionRegex(Arc<Regex>),
    CategoryId(String),
}

#[derive(Clone)]
pub enum CompiledEntryDataMatcher {
    EntryType(EntryType),
    ExternalType(String),
    NameRegex(Arc<Regex>),
    DurationRange { min: Option<u64>, max: Option<u64> },
    HasSource(String),
    YouTube(CompiledYouTubeDataMatcher),
}

#[derive(Clone)]
pub enum CompiledChildMatcher {
    Always,
    Relation(RelationMatcher),
    EntryData(CompiledEntryDataMatcher),
    ChildrenSatisfy {
        matcher: Box<CompiledMatcherExpr>,
        mode: QuantifierMode,
    },
}

#[derive(Clone)]
pub enum CompiledMatcherExpr {
    Matcher(CompiledChildMatcher),
    Not(Box<CompiledMatcherExpr>),
    /// Sub-expressions sorted cheapest-first; short-circuits on first `false`.
    All(Vec<CompiledMatcherExpr>),
    /// Sub-expressions sorted cheapest-first; short-circuits on first `true`.
    Any(Vec<CompiledMatcherExpr>),
}

/// Default leaf evaluator: `Always` is unconditionally `True`; everything else is `Indeterminate`.
/// Use as a fallback arm in custom `static_eval_expr` closures.
pub fn default_eval_leaf(matcher: &CompiledChildMatcher) -> Tribool {
    match matcher {
        CompiledChildMatcher::Always => Tribool::True,
        _ => Tribool::Indeterminate,
    }
}

/// Propagates a per-leaf evaluator through a `CompiledMatcherExpr` using Kleene logic.
pub fn static_eval_expr(
    expr: &CompiledMatcherExpr,
    eval_leaf: &impl Fn(&CompiledChildMatcher) -> Tribool,
) -> Tribool {
    match expr {
        CompiledMatcherExpr::Matcher(m) => eval_leaf(m),
        CompiledMatcherExpr::Not(inner) => static_eval_expr(inner, eval_leaf).not(),
        CompiledMatcherExpr::All(exprs) => exprs.iter().fold(Tribool::True, |acc, e| {
            acc.and(static_eval_expr(e, eval_leaf))
        }),
        CompiledMatcherExpr::Any(exprs) => exprs.iter().fold(Tribool::False, |acc, e| {
            acc.or(static_eval_expr(e, eval_leaf))
        }),
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ChildRule {
    pub matcher: ChildMatcherExpr,
    /// Index into the accompanying `EntryFetchOptionsPool`.
    pub options_id: OptionsId,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct EntryFetchOptions {
    pub child_rules: Vec<ChildRule>,
}

/// Flat arena of `EntryFetchOptions` nodes. `ChildRule.options_id` indexes into this vec,
/// breaking the reference cycle that `Arc<EntryFetchOptions>` inside `ChildRule` would create.
///
/// Entry 0 is always `EntryFetchOptions::default()` and serves as the sentinel for "no rules".
#[derive(Clone, Serialize, Deserialize)]
pub struct EntryFetchOptionsPool {
    entries: Vec<EntryFetchOptions>,
}

impl Default for EntryFetchOptionsPool {
    fn default() -> Self {
        Self {
            entries: vec![EntryFetchOptions::default()],
        }
    }
}

impl EntryFetchOptionsPool {
    pub const DEFAULT_ID: OptionsId = 0;

    pub fn insert(&mut self, opts: EntryFetchOptions) -> OptionsId {
        let id = self.entries.len() as OptionsId;
        self.entries.push(opts);
        id
    }

    pub fn get(&self, id: OptionsId) -> &EntryFetchOptions {
        &self.entries[id as usize]
    }

    /// Overwrite an existing entry. Used by the YAML deserializer's two-pass algorithm:
    /// pre-allocate a slot (to get the id for self-referential named options), then patch
    /// it with the real content once all ids are known.
    pub fn patch(&mut self, id: OptionsId, opts: EntryFetchOptions) {
        self.entries[id as usize] = opts;
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (OptionsId, &EntryFetchOptions)> {
        self.entries
            .iter()
            .enumerate()
            .map(|(i, e)| (i as OptionsId, e))
    }
}

/// The per-child metadata yielded by a filtered `CachedChildSource`. Bundles the pool (shared)
/// with the `OptionsId` that selects which `EntryFetchOptions` to use when fetching that child.
#[derive(Clone)]
pub struct ChildFetchOptions {
    pub pool: Arc<EntryFetchOptionsPool>,
    pub id: OptionsId,
}

impl ChildFetchOptions {
    pub fn new(pool: Arc<EntryFetchOptionsPool>, id: OptionsId) -> Self {
        Self { pool, id }
    }

    pub fn get(&self) -> &EntryFetchOptions {
        self.pool.get(self.id)
    }
}

/// Async source of children, optionally annotated with metadata `T` per child.
#[async_trait::async_trait]
pub trait ChildSource<T: Clone + Send + Sync + 'static = ()>: Send {
    /// Return the next (child, metadata) pair, or `None` when exhausted.
    async fn next(&mut self) -> Result<Option<(ChildRef, T)>, Error>;

    /// Optional count hint for the remaining items: (lower_bound, upper_bound).
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }

    /// Static evaluation of a matcher expression against the remaining items in this source.
    /// Returns `True` if all remaining items will match, `False` if none will, or `Indeterminate`.
    fn evaluate_expr(&self, expr: &CompiledMatcherExpr) -> Tribool {
        let _ = expr;
        Tribool::Indeterminate
    }

    /// Combined static check using both `evaluate_expr` and `size_hint`.
    /// Returns the tribool from `evaluate_expr` alongside the upper bound of remaining items.
    fn static_check(&self, expr: &CompiledMatcherExpr) -> (Tribool, Option<usize>) {
        (self.evaluate_expr(expr), self.size_hint().1)
    }
}

/// Wraps a `Vec<ChildRef>` as a `ChildSource<()>`.
pub struct VecChildSource {
    children: std::vec::IntoIter<ChildRef>,
    remaining: usize,
}

impl VecChildSource {
    pub fn new(children: Vec<ChildRef>) -> Self {
        let remaining = children.len();
        Self {
            children: children.into_iter(),
            remaining,
        }
    }
}

#[async_trait::async_trait]
impl ChildSource for VecChildSource {
    async fn next(&mut self) -> Result<Option<(ChildRef, ())>, Error> {
        let item = self.children.next();
        if item.is_some() {
            self.remaining = self.remaining.saturating_sub(1);
        }
        Ok(item.map(|c| (c, ())))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }

    fn evaluate_expr(&self, expr: &CompiledMatcherExpr) -> Tribool {
        static_eval_expr(expr, &default_eval_leaf)
    }
}

/// A page of children returned by a page fetcher.
pub struct ChildPage {
    pub children: Vec<ChildRef>,
    pub next_page_token: Option<String>,
}

/// Async callback that fetches a page of children given an optional page token.
#[async_trait::async_trait]
pub trait PageFetcher: Send {
    async fn fetch_page(&mut self, page_token: Option<&str>) -> Result<ChildPage, Error>;
}

fn indeterminate_eval(_: &CompiledMatcherExpr) -> Tribool {
    Tribool::Indeterminate
}

/// `ChildSource` that lazily paginates through children using a `PageFetcher`.
pub struct PaginatedChildSource<
    F: Fn(&CompiledMatcherExpr) -> Tribool + Send = fn(&CompiledMatcherExpr) -> Tribool,
> {
    fetcher: Box<dyn PageFetcher>,
    buffer: std::vec::IntoIter<ChildRef>,
    next_page_token: Option<String>,
    exhausted: bool,
    static_eval: F,
}

impl PaginatedChildSource {
    pub fn new(fetcher: Box<dyn PageFetcher>) -> Self {
        Self {
            fetcher,
            buffer: Vec::new().into_iter(),
            next_page_token: None,
            exhausted: false,
            static_eval: indeterminate_eval,
        }
    }
}

impl<F: Fn(&CompiledMatcherExpr) -> Tribool + Send> PaginatedChildSource<F> {
    /// Attach a static evaluator that declares what this source's items look like.
    /// Used to enable early exit in quantifier matching without consuming items.
    pub fn with_static_eval<G: Fn(&CompiledMatcherExpr) -> Tribool + Send>(
        self,
        f: G,
    ) -> PaginatedChildSource<G> {
        PaginatedChildSource {
            fetcher: self.fetcher,
            buffer: self.buffer,
            next_page_token: self.next_page_token,
            exhausted: self.exhausted,
            static_eval: f,
        }
    }
}

#[async_trait::async_trait]
impl<F: Fn(&CompiledMatcherExpr) -> Tribool + Send> ChildSource for PaginatedChildSource<F> {
    async fn next(&mut self) -> Result<Option<(ChildRef, ())>, Error> {
        loop {
            if let Some(child) = self.buffer.next() {
                return Ok(Some((child, ())));
            }
            if self.exhausted {
                return Ok(None);
            }
            let page = self
                .fetcher
                .fetch_page(self.next_page_token.as_deref())
                .await?;
            self.buffer = page.children.into_iter();
            match page.next_page_token {
                Some(token) => self.next_page_token = Some(token),
                None => self.exhausted = true,
            }
        }
    }

    fn evaluate_expr(&self, expr: &CompiledMatcherExpr) -> Tribool {
        (self.static_eval)(expr)
    }
}

/// A read-through cache over a `ChildSource<T>`.
/// Buffers children as they're read; subsequent cursors replay from the buffer
/// and continue draining the live source where the previous cursor left off.
pub struct CachedChildSource<T: Clone + Send + Sync + 'static = ()> {
    state: Mutex<CachedState<T>>,
}

struct CachedState<T: Clone + Send + Sync + 'static> {
    buffer: Vec<(ChildRef, T)>,
    live: Option<Box<dyn ChildSource<T>>>,
}

impl<T: Clone + Send + Sync + 'static> CachedChildSource<T> {
    pub fn new(source: Box<dyn ChildSource<T>>) -> Self {
        Self {
            state: Mutex::new(CachedState {
                buffer: Vec::new(),
                live: Some(source),
            }),
        }
    }

    pub fn from_vec(children: Vec<(ChildRef, T)>) -> Self {
        Self {
            state: Mutex::new(CachedState {
                buffer: children,
                live: None,
            }),
        }
    }

    /// Create a cursor that starts at index 0.
    /// Reads cached children first, then drains the live source.
    pub fn cursor(&self) -> CachedChildCursor<'_, T> {
        CachedChildCursor {
            cache: self,
            index: 0,
        }
    }

    /// Create an owned cursor (requires `Arc<Self>`).
    pub fn owned_cursor(self: &Arc<Self>) -> OwnedCachedChildCursor<T> {
        OwnedCachedChildCursor {
            cache: Arc::clone(self),
            index: 0,
        }
    }
}

impl CachedChildSource<()> {
    pub fn from_children(children: Vec<ChildRef>) -> Self {
        Self::from_vec(children.into_iter().map(|c| (c, ())).collect())
    }
}

/// A cursor over `CachedChildSource<T>`. Implements `ChildSource<T>`.
pub struct CachedChildCursor<'a, T: Clone + Send + Sync + 'static = ()> {
    cache: &'a CachedChildSource<T>,
    index: usize,
}

/// An owned cursor over `CachedChildSource<T>` (holds an `Arc`). Implements `ChildSource<T>`.
pub struct OwnedCachedChildCursor<T: Clone + Send + Sync + 'static = ()> {
    cache: Arc<CachedChildSource<T>>,
    index: usize,
}

fn cached_size_hint<T: Clone + Send + Sync + 'static>(
    cache: &CachedChildSource<T>,
    index: usize,
) -> (usize, Option<usize>) {
    let Ok(state) = cache.state.try_lock() else {
        return (0, None);
    };
    let buffered = state.buffer.len().saturating_sub(index);
    let (live_lower, live_upper) = state
        .live
        .as_ref()
        .map(|l| l.size_hint())
        .unwrap_or((0, Some(0)));
    (buffered + live_lower, live_upper.map(|u| buffered + u))
}

/// Evaluate `expr` against a single `ChildRef` at a given index, without fetching the entity.
/// Returns `Indeterminate` for matchers that require entity data (duration, YouTube metadata, etc.).
fn eval_expr_on_child_ref(expr: &CompiledMatcherExpr, child: &ChildRef, index: usize) -> Tribool {
    static_eval_expr(expr, &|matcher| match matcher {
        CompiledChildMatcher::Always => Tribool::True,
        CompiledChildMatcher::Relation(RelationMatcher::IndexRange { min, max }) => {
            let idx = index as u32;
            let in_range = min.map_or(true, |m| idx >= m) && max.map_or(true, |m| idx < m);
            if in_range {
                Tribool::True
            } else {
                Tribool::False
            }
        }
        CompiledChildMatcher::EntryData(data) => match data {
            CompiledEntryDataMatcher::EntryType(t) => {
                if child.entry_type == *t {
                    Tribool::True
                } else {
                    Tribool::False
                }
            }
            CompiledEntryDataMatcher::NameRegex(regex) => match child.name.as_deref() {
                Some(n) => {
                    if regex.is_match(n) {
                        Tribool::True
                    } else {
                        Tribool::False
                    }
                }
                None => Tribool::False,
            },
            CompiledEntryDataMatcher::HasSource(source) => {
                if child.sources.get(source).is_some() {
                    Tribool::True
                } else {
                    Tribool::False
                }
            }
            CompiledEntryDataMatcher::ExternalType(t) => {
                (child.external_type.as_ref() == t.as_str()).into()
            }
            CompiledEntryDataMatcher::DurationRange { .. }
            | CompiledEntryDataMatcher::YouTube(_) => Tribool::Indeterminate,
        },
        CompiledChildMatcher::ChildrenSatisfy { .. } => Tribool::Indeterminate,
    })
}

fn cached_evaluate_expr<T: Clone + Send + Sync + 'static>(
    cache: &CachedChildSource<T>,
    index: usize,
    expr: &CompiledMatcherExpr,
) -> Tribool {
    let Ok(state) = cache.state.try_lock() else {
        return Tribool::Indeterminate;
    };

    let buf_start = index.min(state.buffer.len());
    let buf_slice = &state.buffer[buf_start..];

    // Check whether all / none of the buffered remaining items match.
    // Both are vacuously true for an empty slice.
    let buf_all_match = buf_slice
        .iter()
        .enumerate()
        .all(|(i, (child, _))| eval_expr_on_child_ref(expr, child, index + i) == Tribool::True);
    let buf_none_match = buf_slice
        .iter()
        .enumerate()
        .all(|(i, (child, _))| eval_expr_on_child_ref(expr, child, index + i) == Tribool::False);

    // Ask the live source for its portion.
    let live = state
        .live
        .as_ref()
        .map(|l| l.evaluate_expr(expr))
        .unwrap_or(Tribool::True); // exhausted live source: vacuously all match

    // All remaining items match iff both segments agree all match.
    // No remaining items match iff both segments agree none match.
    if buf_all_match && live == Tribool::True {
        Tribool::True
    } else if buf_none_match && live == Tribool::False {
        Tribool::False
    } else {
        Tribool::Indeterminate
    }
}

#[async_trait::async_trait]
impl<T: Clone + Send + Sync + 'static> ChildSource<T> for CachedChildCursor<'_, T> {
    async fn next(&mut self) -> Result<Option<(ChildRef, T)>, Error> {
        let mut state = self.cache.state.lock().await;
        if self.index < state.buffer.len() {
            let item = state.buffer[self.index].clone();
            self.index += 1;
            return Ok(Some(item));
        }
        if let Some(live) = &mut state.live {
            if let Some(item) = live.next().await? {
                state.buffer.push(item.clone());
                self.index += 1;
                return Ok(Some(item));
            }
            state.live = None;
        }
        Ok(None)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        cached_size_hint(self.cache, self.index)
    }

    fn evaluate_expr(&self, expr: &CompiledMatcherExpr) -> Tribool {
        cached_evaluate_expr(self.cache, self.index, expr)
    }
}

#[async_trait::async_trait]
impl<T: Clone + Send + Sync + 'static> ChildSource<T> for OwnedCachedChildCursor<T> {
    async fn next(&mut self) -> Result<Option<(ChildRef, T)>, Error> {
        let mut state = self.cache.state.lock().await;
        if self.index < state.buffer.len() {
            let item = state.buffer[self.index].clone();
            self.index += 1;
            return Ok(Some(item));
        }
        if let Some(live) = &mut state.live {
            if let Some(item) = live.next().await? {
                state.buffer.push(item.clone());
                self.index += 1;
                return Ok(Some(item));
            }
            state.live = None;
        }
        Ok(None)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        cached_size_hint(&self.cache, self.index)
    }

    fn evaluate_expr(&self, expr: &CompiledMatcherExpr) -> Tribool {
        cached_evaluate_expr(&self.cache, self.index, expr)
    }
}
