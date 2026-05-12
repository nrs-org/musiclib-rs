use std::sync::Arc;

use regex::Regex;
use tokio::sync::OnceCell;

use crate::providers::{
    FetchProvider,
    types::{
        CachedChildSource, ChildMatcher, ChildMatcherExpr, ChildRef, ChildRule, ChildSource,
        EntityResult, EntryDataMatcher, EntryFetchOptions, EntrySpecificData, Error,
        OwnedCachedChildCursor, QuantifierMode, RelationMatcher,
    },
};

/// Backend-specific matcher evaluation.
/// The engine delegates backend-specific `EntryDataMatcher` variants to this trait.
pub trait BackendMatcherEvaluator: Send + Sync {
    /// Evaluate a backend-specific matcher against an entity result.
    /// Returns `Some(bool)` if this evaluator handles the matcher, `None` otherwise.
    fn evaluate(&self, matcher: &EntryDataMatcher, entity: &EntityResult) -> Option<bool>;
}

/// No-op evaluator for when no backend-specific matchers are needed.
pub struct NoOpEvaluator;

impl BackendMatcherEvaluator for NoOpEvaluator {
    fn evaluate(&self, _matcher: &EntryDataMatcher, _entity: &EntityResult) -> Option<bool> {
        None
    }
}

/// Combines multiple backend evaluators — tries each in order, first `Some` wins.
pub struct CompositeEvaluator {
    evaluators: Vec<Box<dyn BackendMatcherEvaluator>>,
}

impl CompositeEvaluator {
    pub fn new(evaluators: Vec<Box<dyn BackendMatcherEvaluator>>) -> Self {
        Self { evaluators }
    }
}

impl BackendMatcherEvaluator for CompositeEvaluator {
    fn evaluate(&self, matcher: &EntryDataMatcher, entity: &EntityResult) -> Option<bool> {
        for evaluator in &self.evaluators {
            if let Some(result) = evaluator.evaluate(matcher, entity) {
                return Some(result);
            }
        }
        None
    }
}

/// Returns `false` when it can prove no item at `index` or beyond can ever match `expr`.
/// Conservative: unknown/fetch-required matchers return `true`.
fn can_future_items_match(expr: &ChildMatcherExpr, index: usize) -> bool {
    match expr {
        ChildMatcherExpr::Matcher(ChildMatcher::Relation(RelationMatcher::IndexRange {
            max: Some(max),
            ..
        })) => index < *max as usize,
        ChildMatcherExpr::All(exprs) => exprs.iter().all(|e| can_future_items_match(e, index)),
        ChildMatcherExpr::Any(exprs) => exprs.iter().any(|e| can_future_items_match(e, index)),
        _ => true,
    }
}

/// A lazy `ChildSource` that filters children through rules on-demand.
/// Each call to `next()` pulls from the underlying source until a match is found.
struct FilteringChildSource {
    cursor: OwnedCachedChildCursor,
    rules: Vec<ChildRule>,
    provider: Arc<dyn FetchProvider>,
    backend_evaluator: Arc<dyn BackendMatcherEvaluator>,
    index: usize,
}

#[async_trait::async_trait]
impl ChildSource<Arc<EntryFetchOptions>> for FilteringChildSource {
    async fn next(&mut self) -> Result<Option<(ChildRef, Arc<EntryFetchOptions>)>, Error> {
        loop {
            if !self
                .rules
                .iter()
                .any(|r| can_future_items_match(&r.matcher, self.index))
            {
                return Ok(None);
            }

            let Some((child, _)) = self.cursor.next().await? else {
                return Ok(None);
            };

            let entity_cell = OnceCell::new();
            let ctx = MatchContext {
                child: &child,
                child_index: self.index,
                entity_cell: &entity_cell,
            };
            self.index += 1;

            for rule in &self.rules {
                if evaluate_expr(
                    &rule.matcher,
                    &ctx,
                    self.provider.clone(),
                    self.backend_evaluator.as_ref(),
                )
                .await?
                {
                    return Ok(Some((child, rule.options.clone())));
                }
            }
        }
    }
}

/// Filter a `CachedChildSource` through `EntryFetchOptions.child_rules`.
/// Returns a `CachedChildSource<Arc<EntryFetchOptions>>` that lazily evaluates rules
/// as children are consumed.
pub fn filter_children(
    source: Arc<CachedChildSource>,
    options: &EntryFetchOptions,
    provider: Arc<dyn FetchProvider>,
    backend_evaluator: Arc<dyn BackendMatcherEvaluator>,
) -> CachedChildSource<Arc<EntryFetchOptions>> {
    let filtering = FilteringChildSource {
        cursor: source.owned_cursor(),
        rules: options.child_rules.clone(),
        provider,
        backend_evaluator,
        index: 0,
    };
    CachedChildSource::new(Box::new(filtering))
}

struct MatchContext<'a> {
    child: &'a ChildRef,
    child_index: usize,
    entity_cell: &'a OnceCell<EntityResult>,
}

impl MatchContext<'_> {
    async fn get_entity(&self, provider: Arc<dyn FetchProvider>) -> Result<&EntityResult, Error> {
        self.entity_cell
            .get_or_try_init(|| async {
                let identifier =
                    self.child.sources.first_identifier().ok_or_else(|| {
                        Error::InvalidUrl("child has no source identifiers".into())
                    })?;
                provider
                    .fetch_entry(identifier, EntryFetchOptions::default())
                    .await
            })
            .await
    }
}

fn evaluate_expr<'a>(
    expr: &'a ChildMatcherExpr,
    ctx: &'a MatchContext<'a>,
    provider: Arc<dyn FetchProvider>,
    backend_evaluator: &'a dyn BackendMatcherEvaluator,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, Error>> + Send + 'a>> {
    Box::pin(async move {
        match expr {
            ChildMatcherExpr::Matcher(m) => {
                evaluate_matcher(m, ctx, provider, backend_evaluator).await
            }
            ChildMatcherExpr::Not(inner) => {
                Ok(!evaluate_expr(inner, ctx, provider, backend_evaluator).await?)
            }
            ChildMatcherExpr::All(exprs) => {
                for e in exprs.iter() {
                    if !evaluate_expr(e, ctx, provider.clone(), backend_evaluator).await? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            ChildMatcherExpr::Any(exprs) => {
                for e in exprs.iter() {
                    if evaluate_expr(e, ctx, provider.clone(), backend_evaluator).await? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        }
    })
}

async fn evaluate_matcher(
    matcher: &ChildMatcher,
    ctx: &MatchContext<'_>,
    provider: Arc<dyn FetchProvider>,
    backend_evaluator: &dyn BackendMatcherEvaluator,
) -> Result<bool, Error> {
    match matcher {
        ChildMatcher::Always => Ok(true),
        ChildMatcher::Relation(rel) => Ok(evaluate_relation(rel, ctx)),
        ChildMatcher::EntryData(data) => {
            evaluate_entry_data(data, ctx, provider, backend_evaluator).await
        }
        ChildMatcher::ChildrenSatisfy { matcher, mode } => {
            evaluate_children_satisfy(matcher, mode, ctx, provider, backend_evaluator).await
        }
    }
}

fn evaluate_relation(rel: &RelationMatcher, ctx: &MatchContext<'_>) -> bool {
    match rel {
        RelationMatcher::IndexRange { min, max } => {
            let idx = ctx.child_index as u32;
            if let Some(min) = min
                && idx < *min
            {
                return false;
            }
            if let Some(max) = max
                && idx >= *max
            {
                return false;
            }
            true
        }
    }
}

async fn evaluate_entry_data(
    data: &EntryDataMatcher,
    ctx: &MatchContext<'_>,
    provider: Arc<dyn FetchProvider>,
    backend_evaluator: &dyn BackendMatcherEvaluator,
) -> Result<bool, Error> {
    match data {
        // These can be evaluated from ChildRef alone — no fetch needed
        EntryDataMatcher::EntryType(t) => Ok(ctx.child.entry_type == *t),
        EntryDataMatcher::NameRegex(pattern) => {
            let Some(name) = &ctx.child.name else {
                return Ok(false);
            };
            Ok(Regex::new(pattern)
                .map(|r| r.is_match(name))
                .unwrap_or(false))
        }
        EntryDataMatcher::HasSource(source) => Ok(ctx.child.sources.get(source).is_some()),

        // These need the full entity
        EntryDataMatcher::DurationRange { min, max } => {
            let entity = ctx.get_entity(provider).await?;
            let duration = match &entity.specific_data {
                EntrySpecificData::Track { duration_ms, .. } => duration_ms.map(|d| d as u64),
                _ => None,
            };
            let Some(duration) = duration else {
                return Ok(false);
            };
            if let Some(min) = min
                && duration < *min
            {
                return Ok(false);
            }
            if let Some(max) = max
                && duration >= *max
            {
                return Ok(false);
            }
            Ok(true)
        }

        // Backend-specific — delegate
        _ => {
            let entity = ctx.get_entity(provider).await?;
            Ok(backend_evaluator.evaluate(data, entity).unwrap_or(false))
        }
    }
}

async fn evaluate_children_satisfy(
    matcher: &ChildMatcherExpr,
    mode: &QuantifierMode,
    ctx: &MatchContext<'_>,
    provider: Arc<dyn FetchProvider>,
    backend_evaluator: &dyn BackendMatcherEvaluator,
) -> Result<bool, Error> {
    let entity = ctx.get_entity(provider.clone()).await?;
    let mut cursor = entity.children.cursor();

    let mut match_count: u32 = 0;
    let mut total: u32 = 0;
    let mut i: usize = 0;

    while let Some((child, _)) = cursor.next().await? {
        total += 1;
        let child_entity_cell = OnceCell::new();
        let child_ctx = MatchContext {
            child: &child,
            child_index: i,
            entity_cell: &child_entity_cell,
        };
        if evaluate_expr(matcher, &child_ctx, provider.clone(), backend_evaluator).await? {
            match_count += 1;
        }
        i += 1;

        // Early exit when outcome is already determined regardless of remaining children.
        match mode {
            // Count: exceeded max — no future items can bring it back down
            QuantifierMode::Count { max: Some(max), .. } if match_count > *max => {
                return Ok(false);
            }
            _ => {}
        }

        // Size-hint-based early exit: check whether even the best remaining outcome
        // can still satisfy min, or the worst can still satisfy max.
        if let (_, Some(remaining)) = cursor.size_hint() {
            let remaining = remaining as u32;
            let early_fail = match mode {
                QuantifierMode::Count { min, max } => {
                    min.is_some_and(|min| match_count + remaining < min)
                        || max.is_some_and(|max| match_count > max)
                }
                QuantifierMode::Ratio { min, max } => {
                    let denom = (total + remaining) as f64;
                    min.is_some_and(|min| (match_count + remaining) as f64 / denom < min)
                        || max.is_some_and(|max| match_count as f64 / denom > max)
                }
            };
            if early_fail {
                return Ok(false);
            }
        }
    }

    if total == 0 {
        return match mode {
            QuantifierMode::Count { min, .. } => Ok(min.unwrap_or(0) == 0),
            QuantifierMode::Ratio { .. } => Ok(false),
        };
    }

    Ok(match mode {
        QuantifierMode::Count { min, max } => {
            if let Some(min) = min
                && match_count < *min
            {
                return Ok(false);
            }
            if let Some(max) = max
                && match_count > *max
            {
                return Ok(false);
            }
            true
        }
        QuantifierMode::Ratio { min, max } => {
            let ratio = match_count as f64 / total as f64;
            if let Some(min) = min
                && ratio < *min
            {
                return Ok(false);
            }
            if let Some(max) = max
                && ratio > *max
            {
                return Ok(false);
            }
            true
        }
    })
}

#[cfg(test)]
mod tests {
    use std::{borrow::Cow, collections::HashSet, sync::Arc};

    use crate::providers::types::*;

    use super::*;

    fn make_child(entry_type: EntryType, name: &str, source: &'static str) -> ChildRef {
        ChildRef {
            entry_type,
            name: Some(name.to_string()),
            sources: [(Cow::Borrowed(source), HashSet::from([name.to_string()]))].into(),
            ..Default::default()
        }
    }

    fn always_rule(options: EntryFetchOptions) -> ChildRule {
        ChildRule {
            matcher: ChildMatcherExpr::Matcher(ChildMatcher::Always),
            options: Arc::new(options),
        }
    }

    fn entry_type_rule(t: EntryType, options: EntryFetchOptions) -> ChildRule {
        ChildRule {
            matcher: ChildMatcherExpr::Matcher(ChildMatcher::EntryData(
                EntryDataMatcher::EntryType(t),
            )),
            options: Arc::new(options),
        }
    }

    fn name_regex_rule(pattern: &str, options: EntryFetchOptions) -> ChildRule {
        ChildRule {
            matcher: ChildMatcherExpr::Matcher(ChildMatcher::EntryData(
                EntryDataMatcher::NameRegex(pattern.to_string()),
            )),
            options: Arc::new(options),
        }
    }

    fn index_range_rule(
        min: Option<u32>,
        max: Option<u32>,
        options: EntryFetchOptions,
    ) -> ChildRule {
        ChildRule {
            matcher: ChildMatcherExpr::Matcher(ChildMatcher::Relation(
                RelationMatcher::IndexRange { min, max },
            )),
            options: Arc::new(options),
        }
    }

    // Dummy provider that panics if called — for tests that only use ChildRef-level matchers
    struct PanicProvider;

    #[async_trait::async_trait]
    impl crate::providers::CanonicalizeProvider for PanicProvider {
        async fn canonicalize(&self, _url: &str) -> Option<CanonicalizeResult> {
            panic!("should not be called")
        }
    }

    #[async_trait::async_trait]
    impl FetchProvider for PanicProvider {
        async fn fetch_entry(
            self: Arc<Self>,
            _identifier: &str,
            _fetch_options: EntryFetchOptions,
        ) -> Result<EntityResult, Error> {
            panic!("should not be called — test only uses ChildRef-level matchers")
        }
    }

    /// Drain a `CachedChildSource<Arc<EntryFetchOptions>>` into a vec of `(ChildRef, Arc<EntryFetchOptions>)`.
    async fn drain(
        source: CachedChildSource<Arc<EntryFetchOptions>>,
    ) -> Vec<(ChildRef, Arc<EntryFetchOptions>)> {
        let mut cursor = source.cursor();
        let mut out = Vec::new();
        while let Some(item) = cursor.next().await.unwrap() {
            out.push(item);
        }
        out
    }

    fn run_filter(
        children: Vec<ChildRef>,
        rules: Vec<ChildRule>,
    ) -> impl std::future::Future<Output = Vec<(ChildRef, Arc<EntryFetchOptions>)>> {
        let source = Arc::new(CachedChildSource::from_children(children));
        let options = EntryFetchOptions { child_rules: rules };
        let filtered = filter_children(
            source,
            &options,
            Arc::new(PanicProvider),
            Arc::new(NoOpEvaluator),
        );
        drain(filtered)
    }

    #[tokio::test]
    async fn test_always_matches_all() {
        let children = vec![
            make_child(EntryType::Track, "Song A", "youtube"),
            make_child(EntryType::Artist, "Artist B", "spotify"),
        ];
        let result = run_filter(children, vec![always_rule(EntryFetchOptions::default())]).await;
        assert_eq!(result.len(), 2);
    }

    #[tokio::test]
    async fn test_entry_type_filter() {
        let children = vec![
            make_child(EntryType::Track, "Song", "youtube"),
            make_child(EntryType::Artist, "Artist", "youtube"),
            make_child(EntryType::Track, "Another Song", "youtube"),
        ];
        let result = run_filter(
            children,
            vec![entry_type_rule(
                EntryType::Track,
                EntryFetchOptions::default(),
            )],
        )
        .await;
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0.name.as_deref(), Some("Song"));
        assert_eq!(result[1].0.name.as_deref(), Some("Another Song"));
    }

    #[tokio::test]
    async fn test_name_regex_filter() {
        let children = vec![
            make_child(EntryType::Track, "Original Song【MV】", "youtube"),
            make_child(EntryType::Track, "Cover Song", "youtube"),
            make_child(EntryType::Track, "Another Original【MV】", "youtube"),
        ];
        let result = run_filter(
            children,
            vec![name_regex_rule("【MV】", EntryFetchOptions::default())],
        )
        .await;
        assert_eq!(result.len(), 2);
    }

    #[tokio::test]
    async fn test_index_range() {
        let children = vec![
            make_child(EntryType::Track, "A", "yt"),
            make_child(EntryType::Track, "B", "yt"),
            make_child(EntryType::Track, "C", "yt"),
            make_child(EntryType::Track, "D", "yt"),
        ];
        // Only match children at index 1..3 (B and C)
        let result = run_filter(
            children,
            vec![index_range_rule(
                Some(1),
                Some(3),
                EntryFetchOptions::default(),
            )],
        )
        .await;
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0.name.as_deref(), Some("B"));
        assert_eq!(result[1].0.name.as_deref(), Some("C"));
    }

    #[tokio::test]
    async fn test_first_match_wins() {
        let children = vec![make_child(EntryType::Track, "Original Song", "youtube")];
        let opts_a = EntryFetchOptions {
            child_rules: vec![always_rule(EntryFetchOptions::default())],
        };
        let opts_b = EntryFetchOptions::default();
        let result = run_filter(
            children,
            vec![name_regex_rule("Original", opts_a), always_rule(opts_b)],
        )
        .await;
        assert_eq!(result.len(), 1);
        // Should match the first rule (has child_rules) not the second (empty)
        assert_eq!(result[0].1.child_rules.len(), 1);
    }

    #[tokio::test]
    async fn test_no_match_excluded() {
        let children = vec![make_child(EntryType::Artist, "Artist", "youtube")];
        let result = run_filter(
            children,
            vec![entry_type_rule(
                EntryType::Track,
                EntryFetchOptions::default(),
            )],
        )
        .await;
        assert_eq!(result.len(), 0);
    }

    #[tokio::test]
    async fn test_not_combinator() {
        let children = vec![
            make_child(EntryType::Track, "Song", "youtube"),
            make_child(EntryType::Artist, "Artist", "youtube"),
        ];
        let result = run_filter(
            children,
            vec![ChildRule {
                matcher: ChildMatcherExpr::Not(Box::new(ChildMatcherExpr::Matcher(
                    ChildMatcher::EntryData(EntryDataMatcher::EntryType(EntryType::Artist)),
                ))),
                options: Arc::new(EntryFetchOptions::default()),
            }],
        )
        .await;
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0.entry_type, EntryType::Track);
    }

    #[tokio::test]
    async fn test_all_combinator() {
        let children = vec![
            make_child(EntryType::Track, "Original Song", "youtube"),
            make_child(EntryType::Track, "Cover Song", "youtube"),
            make_child(EntryType::Artist, "Original Artist", "youtube"),
        ];
        // Track AND name contains "Original"
        let result = run_filter(
            children,
            vec![ChildRule {
                matcher: ChildMatcherExpr::All(vec![
                    ChildMatcherExpr::Matcher(ChildMatcher::EntryData(
                        EntryDataMatcher::EntryType(EntryType::Track),
                    )),
                    ChildMatcherExpr::Matcher(ChildMatcher::EntryData(
                        EntryDataMatcher::NameRegex("Original".to_string()),
                    )),
                ]),
                options: Arc::new(EntryFetchOptions::default()),
            }],
        )
        .await;
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0.name.as_deref(), Some("Original Song"));
    }

    // --- IndexRange: no early exit without optimization ---

    /// Lazy source that generates `total` items on demand, counting every `next()` call.
    struct GenerativeChildSource {
        current: u64,
        total: u64,
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ChildSource for GenerativeChildSource {
        async fn next(&mut self) -> Result<Option<(ChildRef, ())>, Error> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            if self.current >= self.total {
                return Ok(None);
            }
            let child = make_child(EntryType::Track, "track", "youtube");
            self.current += 1;
            Ok(Some((child, ())))
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            let remaining = (self.total - self.current) as usize;
            (remaining, Some(remaining))
        }
    }

    // IndexRange { max: 100 } on a 200_000-item source.
    // Without an optimization that detects "no further items can match after index 99",
    // the filter exhausts all 200_000 items looking for more matches.
    // This test demonstrates the gap: it asserts only 101 source calls (100 matches + 1
    // trailing None), but currently makes 200_001.
    #[tokio::test]
    async fn test_index_range_does_not_over_consume() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let source = GenerativeChildSource {
            current: 0,
            total: 200_000,
            call_count: Arc::clone(&call_count),
        };
        let cached = Arc::new(CachedChildSource::new(Box::new(source)));
        let options = EntryFetchOptions {
            child_rules: vec![index_range_rule(
                None,
                Some(100),
                EntryFetchOptions::default(),
            )],
        };
        let filtered = filter_children(
            cached,
            &options,
            Arc::new(PanicProvider),
            Arc::new(NoOpEvaluator),
        );
        let result = drain(filtered).await;

        assert_eq!(result.len(), 100);
        // Exactly 100 source calls: the guard fires before fetching item at index 100.
        assert_eq!(call_count.load(Ordering::SeqCst), 100);
    }

    // --- ChildrenSatisfy with lazy source ---

    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;

    /// A lazy `ChildSource<Arc<EntryFetchOptions>>` that counts every `next()` call.
    struct CountingSource {
        items: std::vec::IntoIter<ChildRef>,
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ChildSource<Arc<EntryFetchOptions>> for CountingSource {
        async fn next(&mut self) -> Result<Option<(ChildRef, Arc<EntryFetchOptions>)>, Error> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .items
                .next()
                .map(|c| (c, Arc::new(EntryFetchOptions::default()))))
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            self.items.size_hint()
        }
    }

    /// Provider that returns a pre-built `EntityResult` exactly once for a given identifier.
    struct SingleResultProvider {
        identifier: String,
        result: Mutex<Option<EntityResult>>,
    }

    impl SingleResultProvider {
        fn new(identifier: impl Into<String>, result: EntityResult) -> Self {
            Self {
                identifier: identifier.into(),
                result: Mutex::new(Some(result)),
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::providers::CanonicalizeProvider for SingleResultProvider {
        async fn canonicalize(&self, _url: &str) -> Option<CanonicalizeResult> {
            None
        }
    }

    #[async_trait::async_trait]
    impl FetchProvider for SingleResultProvider {
        async fn fetch_entry(
            self: Arc<Self>,
            identifier: &str,
            _fetch_options: EntryFetchOptions,
        ) -> Result<EntityResult, Error> {
            assert_eq!(
                identifier, self.identifier,
                "unexpected identifier passed to fetch_entry"
            );
            self.result
                .lock()
                .await
                .take()
                .ok_or_else(|| Error::InvalidUrl("fetch_entry called more than once".into()))
        }
    }

    fn make_playlist_entity(songs: Vec<ChildRef>, call_count: Arc<AtomicUsize>) -> EntityResult {
        let source = CountingSource {
            items: songs.into_iter(),
            call_count,
        };
        EntityResult {
            release_date: None,
            sources: Default::default(),
            extra: Default::default(),
            specific_data: EntrySpecificData::Release {
                release_type: Some("playlist".into()),
                num_discs: None,
                num_tracks: None,
            },
            children: Arc::new(CachedChildSource::new(Box::new(source))),
            aliases: vec![],
        }
    }

    fn make_playlist_ref(url: &'static str) -> ChildRef {
        ChildRef {
            entry_type: EntryType::Release,
            name: Some("Playlist B".to_string()),
            sources: [(Cow::Borrowed("youtube"), HashSet::from([url.to_string()]))].into(),
            ..Default::default()
        }
    }

    fn children_satisfy_rule(
        pattern: &str,
        min_ratio: f64,
        options: EntryFetchOptions,
    ) -> ChildRule {
        ChildRule {
            matcher: ChildMatcherExpr::Matcher(ChildMatcher::ChildrenSatisfy {
                matcher: Box::new(ChildMatcherExpr::Matcher(ChildMatcher::EntryData(
                    EntryDataMatcher::NameRegex(pattern.to_string()),
                ))),
                mode: QuantifierMode::Ratio {
                    min: Some(min_ratio),
                    max: None,
                },
            }),
            options: Arc::new(options),
        }
    }

    // ratio = 1/2 = 0.5, required ≥ 0.75 → no match
    #[tokio::test]
    async fn test_children_satisfy_ratio_fails() {
        const PLAYLIST_URL: &str = "https://www.youtube.com/playlist?list=TEST";
        let call_count = Arc::new(AtomicUsize::new(0));

        let song1 = make_child(EntryType::Track, "Song A", "youtube"); // no MV
        let song2 = make_child(EntryType::Track, "Song B【MV】", "youtube"); // has MV

        let playlist = make_playlist_entity(vec![song1, song2], Arc::clone(&call_count));
        let provider = Arc::new(SingleResultProvider::new(PLAYLIST_URL, playlist));

        // Before filtering, children of playlist B have not been touched
        assert_eq!(call_count.load(Ordering::SeqCst), 0);

        let result = {
            let source = Arc::new(CachedChildSource::from_children(vec![make_playlist_ref(
                PLAYLIST_URL,
            )]));
            let options = EntryFetchOptions {
                child_rules: vec![children_satisfy_rule(
                    "MV",
                    0.75,
                    EntryFetchOptions::default(),
                )],
            };
            let filtered = filter_children(source, &options, provider, Arc::new(NoOpEvaluator));
            drain(filtered).await
        };

        // Playlist B matched 1/2 = 0.5 < 0.75 — excluded
        assert_eq!(result.len(), 0);

        // After song1 (no match): best possible = (0+1)/(1+1) = 0.5 < 0.75 → exit early.
        // Song2 is never fetched.
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    // ratio = 2/2 = 1.0, required ≥ 0.75 → match
    #[tokio::test]
    async fn test_children_satisfy_ratio_passes() {
        const PLAYLIST_URL: &str = "https://www.youtube.com/playlist?list=TEST2";
        let call_count = Arc::new(AtomicUsize::new(0));

        let song1 = make_child(EntryType::Track, "Song A【MV】", "youtube");
        let song2 = make_child(EntryType::Track, "Song B【MV】", "youtube");

        let playlist = make_playlist_entity(vec![song1, song2], Arc::clone(&call_count));
        let provider = Arc::new(SingleResultProvider::new(PLAYLIST_URL, playlist));

        let result = {
            let source = Arc::new(CachedChildSource::from_children(vec![make_playlist_ref(
                PLAYLIST_URL,
            )]));
            let options = EntryFetchOptions {
                child_rules: vec![children_satisfy_rule(
                    "MV",
                    0.75,
                    EntryFetchOptions::default(),
                )],
            };
            let filtered = filter_children(source, &options, provider, Arc::new(NoOpEvaluator));
            drain(filtered).await
        };

        // Playlist B matched 2/2 = 1.0 ≥ 0.75 — included
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0.name.as_deref(), Some("Playlist B"));

        // Both songs consumed lazily
        assert_eq!(call_count.load(Ordering::SeqCst), 3);
    }
}
