use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use futures::{Stream, StreamExt, stream::BoxStream};
use regex::Regex;

pub type OptionsId = u32;

use serde::{Deserialize, Serialize};
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
    /// True when position was synthesized from track index (unparseable Discogs position string).
    pub synthetic: bool,
}

// specific data for different entry types
pub enum EntrySpecificData {
    Track {
        /// All known durations for this track in milliseconds, sorted and
        /// deduped. Multiple values arise when different sources (e.g. MB
        /// release tracks) report slightly different lengths.
        duration_ms: Vec<i64>,
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

impl EntrySpecificData {
    pub fn entry_type(&self) -> EntryType {
        match self {
            EntrySpecificData::Track { .. } => EntryType::Track,
            EntrySpecificData::Release { .. } => EntryType::Release,
            EntrySpecificData::ReleaseGroup { .. } => EntryType::ReleaseGroup,
            EntrySpecificData::Artist => EntryType::Artist,
        }
    }
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
#[derive(Default, Clone)]
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
    /// Duration as given by the parent's listing (e.g. an album tracklist),
    /// when the listing payload carries one. Lets a child that's never fetched
    /// still be stored with a duration, and lets `duration_range` matchers
    /// decide without fetching the child.
    pub duration_ms: Option<i64>,
    /// Set on an artist's discography child when the artist only appears on
    /// that release (Spotify `album_group: appears_on`, Discogs role
    /// `Appearance`/`TrackAppearance`) rather than it being their own, e.g. a
    /// various-artists compilation. Matched by `EntryDataMatcher::AppearsOn`.
    pub appears_on: bool,
    pub position: Option<TrackPosition>,
    /// Contributions here represent the child's relationship to its parent
    /// (e.g. an artist's role on a release they contributed to).
    /// Note: there is no such contribution relationship from a track to its album/release,
    /// so this field is typically empty for track children.
    pub contributions: Vec<Contribution>,
    /// Set when this child is the *original* recording that the parent is a
    /// `kind`-transformation of (e.g. `"cover"`, `"remix"`, `"arrangement"`).
    /// Consumed by `pipeline::flush` to write a directed original→derived
    /// `entry_relation` row once both sides have resolved entry_ids. `None`
    /// for the overwhelming majority of children.
    pub original_relation_kind: Option<Cow<'static, str>>,
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
    pub release_date: Option<String>,
    pub sources: ExternalSources,
    pub extra: serde_json::Value,
    pub specific_data: EntrySpecificData,
    pub children: Vec<Arc<CachedChildSource<T>>>,
    pub aliases: Vec<Alias>,
}

// result of canonicalizing an identifier, with canonical source key, canonical identifier, entry
// type, and external type
#[derive(Debug, PartialEq)]
pub struct CanonicalizeResult {
    /// The provider that owns this identifier — e.g. `"youtube"`, `"musicbrainz"`,
    /// `"spotify"`. This is the provider's plain source key, not an entity-kind
    /// label. The pair `(canonical_source_key, canonical_identifier)` is the
    /// canonical identity used downstream (`entry_source.source` /
    /// `entry_source.identifier`).
    pub canonical_source_key: Cow<'static, str>,
    pub canonical_identifier: String,
    pub entry_type: EntryType,
    /// Backend-specific entity-kind label (e.g. `"youtube:video"`,
    /// `"musicbrainz:recording"`). Callers pass this as the `external_type`
    /// argument to `fetch_entry` / `raw_fetch` so the backend knows what kind
    /// of entity to fetch.
    pub external_type: Cow<'static, str>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Invalid URL: {0}")]
    InvalidUrl(String),
    #[error("Unsupported source key: {0}")]
    UnsupportedSourceKey(String),
    #[error("HTTP error: {0}")]
    Http(#[from] crate::http::Error),
    #[error("Invalid credentials: {0}")]
    InvalidCredentials(String),
    #[error("Authentication failed: {0}")]
    AuthenticationFailed(String),
    #[error("Missing credentials: {0}")]
    MissingCredentials(String),
    #[error("Invalid pattern: {0}")]
    InvalidPattern(String),
    #[error("Not found: {0}")]
    NotFound(String),
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
    AppearsOn(bool),

    // Backend-specific
    YouTube(YouTubeDataMatcher),
    MusicBrainz(MusicBrainzDataMatcher),
    // Spotify(SpotifyDataMatcher),
}

// YouTube-specific entry data matchers
#[derive(Clone, Serialize, Deserialize)]
pub enum YouTubeDataMatcher {
    DescriptionRegex(String),
    CategoryId(String),
}

// MusicBrainz-specific entry data matchers
#[derive(Clone, Serialize, Deserialize)]
pub enum MusicBrainzDataMatcher {
    /// release-group `primary-type`: "Album", "Single", "EP", "Broadcast", "Other"
    ReleaseGroupPrimaryType(String),
    /// release-group `secondary-types` contains the given value: "Live", "Compilation", "Remix", etc.
    ReleaseGroupHasSecondaryType(String),
    /// release `status`: "Official", "Promotional", "Bootleg", "Pseudo-Release"
    ReleaseStatus(String),
    /// release `country`: ISO 3166-1 alpha-2 code
    ReleaseCountry(String),
    /// recording `video` is true
    RecordingIsVideo,
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

impl std::ops::Not for Tribool {
    type Output = Self;

    fn not(self) -> Self {
        match self {
            Tribool::True => Tribool::False,
            Tribool::False => Tribool::True,
            Tribool::Indeterminate => Tribool::Indeterminate,
        }
    }
}

impl Tribool {
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
    AppearsOn(bool),
    YouTube(CompiledYouTubeDataMatcher),
    MusicBrainz(CompiledMusicBrainzDataMatcher),
}

#[derive(Clone)]
pub enum CompiledMusicBrainzDataMatcher {
    ReleaseGroupPrimaryType(String),
    ReleaseGroupHasSecondaryType(String),
    ReleaseStatus(String),
    ReleaseCountry(String),
    RecordingIsVideo,
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
        CompiledMatcherExpr::Not(inner) => !static_eval_expr(inner, eval_leaf),
        CompiledMatcherExpr::All(exprs) => {
            let mut result = Tribool::True;
            for e in exprs {
                result = result.and(static_eval_expr(e, eval_leaf));
                if result == Tribool::False {
                    return Tribool::False;
                }
            }
            result
        }
        CompiledMatcherExpr::Any(exprs) => {
            let mut result = Tribool::False;
            for e in exprs {
                result = result.or(static_eval_expr(e, eval_leaf));
                if result == Tribool::True {
                    return Tribool::True;
                }
            }
            result
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ChildRule {
    pub matcher: ChildMatcherExpr,
    /// Index into the accompanying `EntryFetchOptionsPool`, or `None` to skip this child entirely.
    pub options_id: Option<OptionsId>,
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

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
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
///
/// `id: None` means the child is listed but not fetched: the importer records
/// the edge and stores the child as a stub. Only non-artist parents yield
/// these (see `matcher::filter_children`).
#[derive(Clone)]
pub struct ChildFetchOptions {
    pub pool: Arc<EntryFetchOptionsPool>,
    pub id: Option<OptionsId>,
}

impl ChildFetchOptions {
    pub fn new(pool: Arc<EntryFetchOptionsPool>, id: OptionsId) -> Self {
        Self { pool, id: Some(id) }
    }

    /// Record the child without fetching it.
    pub fn stub(pool: Arc<EntryFetchOptionsPool>) -> Self {
        Self { pool, id: None }
    }

    pub fn get(&self) -> Option<&EntryFetchOptions> {
        self.id.map(|id| self.pool.get(id))
    }
}

/// Async source of children, optionally annotated with metadata `T` per child.
///
/// Extends [`Stream`] so that combinators like `buffer_unordered` work directly.
/// The `evaluate_expr` / `static_check` methods provide source-level static
/// analysis for the matcher system without consuming any items.
pub trait ChildSource<T: Clone + Send + Sync + 'static = ()>:
    Stream<Item = Result<(ChildRef, T), Error>> + Send + Unpin
{
    /// Static evaluation of a matcher expression against the remaining items in
    /// this source. Returns `True` if all will match, `False` if none will, or
    /// `Indeterminate` (the default).
    fn evaluate_expr(&self, expr: &CompiledMatcherExpr) -> Tribool {
        let _ = expr;
        Tribool::Indeterminate
    }

    /// Combined static check: tribool result + upper bound on remaining items.
    fn static_check(&self, expr: &CompiledMatcherExpr) -> (Tribool, Option<usize>) {
        let (_, upper) = self.size_hint();
        (self.evaluate_expr(expr), upper)
    }
}

/// Convenience wrapper: polls the next item and re-orders from `Option<Result<_>>` to
/// `Result<Option<_>>`, matching the old `async fn next` ergonomics at call sites.
pub async fn child_next<T: Clone + Send + Sync + 'static>(
    source: &mut (impl ChildSource<T> + ?Sized),
) -> Result<Option<(ChildRef, T)>, Error> {
    StreamExt::next(source).await.transpose()
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

impl Stream for VecChildSource {
    type Item = Result<(ChildRef, ()), Error>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.children.next() {
            Some(child) => {
                self.remaining = self.remaining.saturating_sub(1);
                Poll::Ready(Some(Ok((child, ()))))
            }
            None => Poll::Ready(None),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl Unpin for VecChildSource {}

impl ChildSource for VecChildSource {
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

/// `ChildSource` that lazily paginates through children using a `PageFetcher`.
///
/// The inner stream is built with `stream::unfold` so the `PageFetcher` moves
/// into the closure state — avoiding the self-referential borrow that would
/// arise from storing an in-progress `fetch_page` future alongside the fetcher.
pub struct PaginatedChildSource<
    F: Fn(&CompiledMatcherExpr) -> Tribool + Send = fn(&CompiledMatcherExpr) -> Tribool,
> {
    stream: BoxStream<'static, Result<(ChildRef, ()), Error>>,
    static_eval: F,
    /// Cleared once an item contradicts `static_eval`; from then on the source
    /// declares nothing (see `declaration_contradictions`).
    trusted: bool,
}

/// Ways `child` contradicts what `eval` declares about every item of its
/// listing. A declaration is hand-written next to the code that builds the
/// items, and nothing else ties the two together; a wrong one makes filtering
/// skip a listing it shouldn't (`matcher::filter_children`) or take a wrong
/// shortcut in quantifier matching, both silently. This compares each item
/// read against the facts declarations make (`entry_type`, `external_type`,
/// `appears_on`): a `True` claim the item doesn't satisfy, or a `False` claim
/// it does. `Indeterminate` claims are never wrong.
pub fn declaration_contradictions(
    eval: &dyn Fn(&CompiledMatcherExpr) -> Tribool,
    child: &ChildRef,
) -> Vec<String> {
    let leaf = |m: CompiledEntryDataMatcher| {
        CompiledMatcherExpr::Matcher(CompiledChildMatcher::EntryData(m))
    };
    let mut out = Vec::new();
    let mut check = |what: String, claim: Tribool, holds: bool| match claim {
        Tribool::True if !holds => out.push(format!("declares every item `{what}`")),
        Tribool::False if holds => out.push(format!("declares no item `{what}`")),
        _ => {}
    };
    for t in [
        EntryType::Artist,
        EntryType::ReleaseGroup,
        EntryType::Release,
        EntryType::Track,
    ] {
        let claim = eval(&leaf(CompiledEntryDataMatcher::EntryType(t)));
        check(format!("entry_type: {t:?}"), claim, child.entry_type == t);
    }
    // Equality-style declarations answer `False` for any type other than the
    // declared one, so checking the item's own type also catches a wrong
    // `True` for some other type.
    let ext = child.external_type.to_string();
    let claim = eval(&leaf(CompiledEntryDataMatcher::ExternalType(ext.clone())));
    check(format!("external_type: {ext:?}"), claim, true);
    for want in [true, false] {
        let claim = eval(&leaf(CompiledEntryDataMatcher::AppearsOn(want)));
        check(
            format!("appears_on: {want}"),
            claim,
            child.appears_on == want,
        );
    }
    out
}

fn indeterminate_eval(_: &CompiledMatcherExpr) -> Tribool {
    Tribool::Indeterminate
}

fn paginated_stream(
    fetcher: Box<dyn PageFetcher>,
) -> BoxStream<'static, Result<(ChildRef, ()), Error>> {
    // State: (fetcher, buffered items, next page token, exhausted flag)
    futures::stream::unfold(
        (
            fetcher,
            std::collections::VecDeque::<ChildRef>::new(),
            None::<String>,
            false,
        ),
        |(mut fetcher, mut buffer, mut page_token, mut exhausted)| async move {
            loop {
                if let Some(child) = buffer.pop_front() {
                    return Some((Ok((child, ())), (fetcher, buffer, page_token, exhausted)));
                }
                if exhausted {
                    return None;
                }
                match fetcher.fetch_page(page_token.as_deref()).await {
                    Ok(page) => {
                        buffer.extend(page.children);
                        match page.next_page_token {
                            Some(token) => page_token = Some(token),
                            None => exhausted = true,
                        }
                    }
                    Err(e) => {
                        // Signal the error then stop.
                        exhausted = true;
                        return Some((Err(e), (fetcher, buffer, page_token, exhausted)));
                    }
                }
            }
        },
    )
    .boxed()
}

impl PaginatedChildSource {
    pub fn new(fetcher: Box<dyn PageFetcher>) -> Self {
        Self {
            stream: paginated_stream(fetcher),
            static_eval: indeterminate_eval,
            trusted: true,
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
            stream: self.stream,
            static_eval: f,
            trusted: true,
        }
    }
}

impl<F: Fn(&CompiledMatcherExpr) -> Tribool + Send> Stream for PaginatedChildSource<F> {
    type Item = Result<(ChildRef, ()), Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let item = self.stream.as_mut().poll_next(cx);
        if self.trusted
            && let Poll::Ready(Some(Ok((child, ())))) = &item
        {
            let wrong = declaration_contradictions(&self.static_eval, child);
            if !wrong.is_empty() {
                // Tests read every fixture listing, so they catch a declaration
                // drifting from the code that builds its items.
                if cfg!(test) {
                    panic!(
                        "listing declaration contradicted by {:?}: {wrong:?}",
                        child.name
                    );
                }
                tracing::warn!(
                    "listing declaration contradicted by item {:?} ({:?}): {}; \
                     no longer trusting it for this listing",
                    child.name,
                    child.sources.first_identifier(),
                    wrong.join("; ")
                );
                self.trusted = false;
            }
        }
        item
    }
}

impl<F: Fn(&CompiledMatcherExpr) -> Tribool + Send> Unpin for PaginatedChildSource<F> {}

impl<F: Fn(&CompiledMatcherExpr) -> Tribool + Send> ChildSource for PaginatedChildSource<F> {
    fn evaluate_expr(&self, expr: &CompiledMatcherExpr) -> Tribool {
        if self.trusted {
            (self.static_eval)(expr)
        } else {
            Tribool::Indeterminate
        }
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
    live: Option<Box<dyn ChildSource<T> + Unpin>>,
}

impl<T: Clone + Send + Sync + 'static> CachedChildSource<T> {
    pub fn new(source: Box<dyn ChildSource<T> + Unpin>) -> Self {
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
            fut: None,
        }
    }

    /// Create an owned cursor (requires `Arc<Self>`).
    pub fn owned_cursor(self: &Arc<Self>) -> OwnedCachedChildCursor<T> {
        OwnedCachedChildCursor {
            cache: Arc::clone(self),
            index: 0,
            fut: None,
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
    /// In-progress advance future, persisted across polls. See [`CachedNextFut`].
    fut: Option<CachedNextFut<'a, T>>,
}

/// An owned cursor over `CachedChildSource<T>` (holds an `Arc`). Implements `ChildSource<T>`.
pub struct OwnedCachedChildCursor<T: Clone + Send + Sync + 'static = ()> {
    cache: Arc<CachedChildSource<T>>,
    index: usize,
    /// In-progress advance future, persisted across polls. See [`CachedNextFut`].
    fut: Option<CachedNextFut<'static, T>>,
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
            let in_range = min.is_none_or(|m| idx >= m) && max.is_none_or(|m| idx < m);
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
            CompiledEntryDataMatcher::AppearsOn(want) => (child.appears_on == *want).into(),
            CompiledEntryDataMatcher::ExternalType(t) => {
                (child.external_type.as_ref() == t.as_str()).into()
            }
            CompiledEntryDataMatcher::DurationRange { .. }
            | CompiledEntryDataMatcher::YouTube(_)
            | CompiledEntryDataMatcher::MusicBrainz(_) => Tribool::Indeterminate,
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
    let mut buf_all_match = true;
    let mut buf_none_match = true;
    for (i, (child, _)) in buf_slice.iter().enumerate() {
        let result = eval_expr_on_child_ref(expr, child, index + i);
        if result != Tribool::True {
            buf_all_match = false;
        }
        if result != Tribool::False {
            buf_none_match = false;
        }
        if !buf_all_match && !buf_none_match {
            break;
        }
    }

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

/// Returns `Some((item, new_index))` or `None` when exhausted.
/// Taking `index` by value (not `&mut`) avoids holding a borrow across the await.
async fn cached_next_owned<T: Clone + Send + Sync + 'static>(
    cache: &CachedChildSource<T>,
    index: usize,
) -> Option<(Result<(ChildRef, T), Error>, usize)> {
    let mut state = cache.state.lock().await;
    if index < state.buffer.len() {
        let item = state.buffer[index].clone();
        return Some((Ok(item), index + 1));
    }
    if let Some(live) = &mut state.live {
        match StreamExt::next(live.as_mut()).await {
            Some(Ok(item)) => {
                state.buffer.push(item.clone());
                return Some((Ok(item), index + 1));
            }
            Some(Err(e)) => return Some((Err(e), index)),
            None => state.live = None,
        }
    }
    None
}

/// Owned-Arc variant of [`cached_next_owned`], producing a `'static` future so
/// an [`OwnedCachedChildCursor`] can store it across polls.
async fn cached_next_arc<T: Clone + Send + Sync + 'static>(
    cache: Arc<CachedChildSource<T>>,
    index: usize,
) -> Option<(Result<(ChildRef, T), Error>, usize)> {
    cached_next_owned(&cache, index).await
}

/// In-progress `cached_next_*` future, persisted inside a cursor across polls.
///
/// Recreating this future on every `poll_next` would drop the pending
/// `state.lock()` waiter (de-registering this task from the mutex wait queue),
/// so a contended lock would never wake the task again — a hard deadlock.
type CachedNextFut<'a, T> =
    Pin<Box<dyn Future<Output = Option<(Result<(ChildRef, T), Error>, usize)>> + Send + 'a>>;

impl<T: Clone + Send + Sync + 'static> Stream for CachedChildCursor<'_, T> {
    type Item = Result<(ChildRef, T), Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        // Persist the advance future across polls; recreating it each poll would
        // drop the pending `state.lock()` waiter and deadlock under contention.
        let fut = this
            .fut
            .get_or_insert_with(|| Box::pin(cached_next_owned(this.cache, this.index)));
        match fut.as_mut().poll(cx) {
            Poll::Ready(Some((item, new_index))) => {
                this.index = new_index;
                this.fut = None;
                Poll::Ready(Some(item))
            }
            Poll::Ready(None) => {
                this.fut = None;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        cached_size_hint(self.cache, self.index)
    }
}

impl<T: Clone + Send + Sync + 'static> Unpin for CachedChildCursor<'_, T> {}

impl<T: Clone + Send + Sync + 'static> ChildSource<T> for CachedChildCursor<'_, T> {
    fn evaluate_expr(&self, expr: &CompiledMatcherExpr) -> Tribool {
        cached_evaluate_expr(self.cache, self.index, expr)
    }
}

impl<T: Clone + Send + Sync + 'static> Stream for OwnedCachedChildCursor<T> {
    type Item = Result<(ChildRef, T), Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        // Persist the advance future across polls; recreating it each poll would
        // drop the pending `state.lock()` waiter and deadlock under contention.
        let fut = this
            .fut
            .get_or_insert_with(|| Box::pin(cached_next_arc(Arc::clone(&this.cache), this.index)));
        match fut.as_mut().poll(cx) {
            Poll::Ready(Some((item, new_index))) => {
                this.index = new_index;
                this.fut = None;
                Poll::Ready(Some(item))
            }
            Poll::Ready(None) => {
                this.fut = None;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        cached_size_hint(&self.cache, self.index)
    }
}

impl<T: Clone + Send + Sync + 'static> Unpin for OwnedCachedChildCursor<T> {}

impl<T: Clone + Send + Sync + 'static> ChildSource<T> for OwnedCachedChildCursor<T> {
    fn evaluate_expr(&self, expr: &CompiledMatcherExpr) -> Tribool {
        cached_evaluate_expr(&self.cache, self.index, expr)
    }
}

#[cfg(test)]
mod cached_source_tests {
    use std::time::Duration;

    use super::*;

    /// Live source that yields `n` named children, sleeping 1ms before each to
    /// simulate async I/O. The sleep makes `poll_next` return `Pending` with the
    /// wake-up owned by a single `tokio::time::Sleep` waker slot — the condition
    /// that orphaned a cursor task when `poll_next` recreated (and dropped) its
    /// in-progress future every poll.
    struct SleepySource {
        stream: BoxStream<'static, Result<(ChildRef, ()), Error>>,
    }

    impl SleepySource {
        fn new(n: usize) -> Self {
            let stream = futures::stream::unfold(0usize, move |i| async move {
                if i >= n {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
                let child = ChildRef {
                    name: Some(i.to_string()),
                    ..Default::default()
                };
                Some((Ok((child, ())), i + 1))
            })
            .boxed();
            Self { stream }
        }
    }

    impl Stream for SleepySource {
        type Item = Result<(ChildRef, ()), Error>;
        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.stream.as_mut().poll_next(cx)
        }
    }
    impl Unpin for SleepySource {}
    impl ChildSource for SleepySource {}

    async fn drain(cache: Arc<CachedChildSource>) -> Vec<String> {
        let mut cursor = cache.owned_cursor();
        let mut out = Vec::new();
        while let Some(item) = StreamExt::next(&mut cursor).await {
            out.push(item.expect("no error").0.name.unwrap_or_default());
        }
        out
    }

    /// Regression test: two cursors over the *same* `CachedChildSource`, driven
    /// on separate tasks (so they have distinct wakers), must both fully drain.
    ///
    /// Before the fix, `CachedChildCursor::poll_next` rebuilt the advance future
    /// every poll and dropped it on `Pending`, discarding the pending
    /// `state.lock()` waiter. Once the shared `state` mutex was contended, the
    /// losing task was never woken again — a hard deadlock. With the future
    /// persisted across polls, both cursors complete.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_cursors_over_shared_source_dont_deadlock() {
        let cache = Arc::new(CachedChildSource::new(Box::new(SleepySource::new(8))));

        let a = tokio::spawn(drain(Arc::clone(&cache)));
        let b = tokio::spawn(drain(Arc::clone(&cache)));

        let joined = futures::future::try_join(a, b);
        let (a, b) = tokio::time::timeout(Duration::from_secs(10), joined)
            .await
            .expect("cursors deadlocked")
            .expect("tasks panicked");

        let expected: Vec<String> = (0..8).map(|i| i.to_string()).collect();
        assert_eq!(a, expected);
        // The second cursor replays the same buffered children from index 0.
        assert_eq!(b, expected);
    }

    fn video(entry_type: EntryType, external_type: &'static str, appears_on: bool) -> ChildRef {
        ChildRef {
            entry_type,
            external_type: external_type.into(),
            appears_on,
            ..Default::default()
        }
    }

    /// "Every item is a `youtube:video` track", like the YouTube uploads source.
    fn uploads_eval(expr: &CompiledMatcherExpr) -> Tribool {
        static_eval_expr(expr, &|m| match m {
            CompiledChildMatcher::EntryData(CompiledEntryDataMatcher::EntryType(t)) => {
                (*t == EntryType::Track).into()
            }
            CompiledChildMatcher::EntryData(CompiledEntryDataMatcher::ExternalType(t)) => {
                (t == "youtube:video").into()
            }
            _ => default_eval_leaf(m),
        })
    }

    #[test]
    fn declaration_contradictions_flags_wrong_claims_only() {
        assert!(
            declaration_contradictions(
                &uploads_eval,
                &video(EntryType::Track, "youtube:video", false)
            )
            .is_empty()
        );

        // An item of another external type contradicts "every item is a
        // youtube:video" (caught through the item's own type).
        let wrong = declaration_contradictions(
            &uploads_eval,
            &video(EntryType::Track, "youtube:playlist", false),
        );
        assert_eq!(wrong.len(), 1, "{wrong:?}");

        // A release contradicts both "every item is a track" and "no item is
        // a release".
        let wrong = declaration_contradictions(
            &uploads_eval,
            &video(EntryType::Release, "youtube:video", false),
        );
        assert_eq!(wrong.len(), 2, "{wrong:?}");

        // A declaration that says nothing is never wrong.
        assert!(
            declaration_contradictions(&indeterminate_eval, &video(EntryType::Artist, "", true))
                .is_empty()
        );
    }
}
