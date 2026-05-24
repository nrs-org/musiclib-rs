use std::sync::Arc;

use regex::Regex;
use tokio::sync::OnceCell;

use crate::providers::{
    FetchProvider,
    backends::{
        musicbrainz::matcher::evaluate as evaluate_musicbrainz,
        youtube_api::matcher::evaluate as evaluate_youtube,
    },
    types::{
        CachedChildSource, ChildFetchOptions, ChildMatcher, ChildMatcherExpr, ChildRef,
        ChildSource, CompiledChildMatcher, CompiledEntryDataMatcher, CompiledMatcherExpr,
        CompiledMusicBrainzDataMatcher, CompiledYouTubeDataMatcher, EntityResult, EntryDataMatcher,
        EntryFetchOptionsPool, EntrySpecificData, Error, MusicBrainzDataMatcher, OptionsId,
        OwnedCachedChildCursor, QuantifierMode, RelationMatcher, Tribool, YouTubeDataMatcher,
    },
};

pub struct CompiledChildRule {
    pub matcher: CompiledMatcherExpr,
    /// `None` means skip this child entirely; `Some(id)` means fetch with those options.
    pub options_id: Option<OptionsId>,
}

// --- Compilation ---

fn compiled_expr_cost(expr: &CompiledMatcherExpr) -> u32 {
    match expr {
        CompiledMatcherExpr::Matcher(m) => match m {
            CompiledChildMatcher::Always => 0,
            CompiledChildMatcher::Relation(_) => 1,
            CompiledChildMatcher::EntryData(d) => match d {
                CompiledEntryDataMatcher::EntryType(_) => 2,
                CompiledEntryDataMatcher::ExternalType(_) => 2,
                CompiledEntryDataMatcher::HasSource(_) => 2,
                CompiledEntryDataMatcher::NameRegex(_) => 5,
                CompiledEntryDataMatcher::DurationRange { .. } => 20,
                CompiledEntryDataMatcher::YouTube(_) => 20,
                CompiledEntryDataMatcher::MusicBrainz(_) => 20,
            },
            CompiledChildMatcher::ChildrenSatisfy { .. } => 100,
        },
        CompiledMatcherExpr::Not(inner) => compiled_expr_cost(inner),
        // Cost of All/Any is their cheapest child — short-circuit means we may only
        // pay that minimum.
        CompiledMatcherExpr::All(exprs) | CompiledMatcherExpr::Any(exprs) => {
            exprs.iter().map(compiled_expr_cost).min().unwrap_or(0)
        }
    }
}

/// Compile a `ChildMatcherExpr` into a `CompiledMatcherExpr`: pre-compiles regexes and
/// sorts `All`/`Any` sub-expressions by ascending cost (post-order, so children are sorted
/// before their cost contributes to the parent's sort key).
pub fn compile_expr(expr: &ChildMatcherExpr) -> Result<CompiledMatcherExpr, Error> {
    Ok(match expr {
        ChildMatcherExpr::Matcher(m) => CompiledMatcherExpr::Matcher(compile_matcher(m)?),
        ChildMatcherExpr::Not(inner) => CompiledMatcherExpr::Not(Box::new(compile_expr(inner)?)),
        ChildMatcherExpr::All(exprs) => {
            let mut compiled: Vec<CompiledMatcherExpr> =
                exprs.iter().map(compile_expr).collect::<Result<_, _>>()?;
            compiled.sort_by_key(compiled_expr_cost);
            CompiledMatcherExpr::All(compiled)
        }
        ChildMatcherExpr::Any(exprs) => {
            let mut compiled: Vec<CompiledMatcherExpr> =
                exprs.iter().map(compile_expr).collect::<Result<_, _>>()?;
            compiled.sort_by_key(compiled_expr_cost);
            CompiledMatcherExpr::Any(compiled)
        }
    })
}

fn compile_matcher(m: &ChildMatcher) -> Result<CompiledChildMatcher, Error> {
    Ok(match m {
        ChildMatcher::Always => CompiledChildMatcher::Always,
        ChildMatcher::Relation(r) => CompiledChildMatcher::Relation(r.clone()),
        ChildMatcher::EntryData(d) => CompiledChildMatcher::EntryData(compile_entry_data(d)?),
        ChildMatcher::ChildrenSatisfy { matcher, mode } => CompiledChildMatcher::ChildrenSatisfy {
            matcher: Box::new(compile_expr(matcher)?),
            mode: *mode,
        },
    })
}

fn compile_entry_data(d: &EntryDataMatcher) -> Result<CompiledEntryDataMatcher, Error> {
    Ok(match d {
        EntryDataMatcher::EntryType(t) => CompiledEntryDataMatcher::EntryType(*t),
        EntryDataMatcher::ExternalType(s) => CompiledEntryDataMatcher::ExternalType(s.clone()),
        EntryDataMatcher::NameRegex(p) => CompiledEntryDataMatcher::NameRegex(Arc::new(
            Regex::new(p).map_err(|e| Error::InvalidPattern(e.to_string()))?,
        )),
        EntryDataMatcher::DurationRange { min, max } => CompiledEntryDataMatcher::DurationRange {
            min: *min,
            max: *max,
        },
        EntryDataMatcher::HasSource(s) => CompiledEntryDataMatcher::HasSource(s.clone()),
        EntryDataMatcher::YouTube(yt) => CompiledEntryDataMatcher::YouTube(match yt {
            YouTubeDataMatcher::DescriptionRegex(p) => {
                CompiledYouTubeDataMatcher::DescriptionRegex(Arc::new(
                    Regex::new(p).map_err(|e| Error::InvalidPattern(e.to_string()))?,
                ))
            }
            YouTubeDataMatcher::CategoryId(id) => {
                CompiledYouTubeDataMatcher::CategoryId(id.clone())
            }
        }),
        EntryDataMatcher::MusicBrainz(mb) => CompiledEntryDataMatcher::MusicBrainz(match mb {
            MusicBrainzDataMatcher::ReleaseGroupPrimaryType(t) => {
                CompiledMusicBrainzDataMatcher::ReleaseGroupPrimaryType(t.clone())
            }
            MusicBrainzDataMatcher::ReleaseGroupHasSecondaryType(t) => {
                CompiledMusicBrainzDataMatcher::ReleaseGroupHasSecondaryType(t.clone())
            }
            MusicBrainzDataMatcher::ReleaseStatus(s) => {
                CompiledMusicBrainzDataMatcher::ReleaseStatus(s.clone())
            }
            MusicBrainzDataMatcher::ReleaseCountry(c) => {
                CompiledMusicBrainzDataMatcher::ReleaseCountry(c.clone())
            }
            MusicBrainzDataMatcher::RecordingIsVideo => {
                CompiledMusicBrainzDataMatcher::RecordingIsVideo
            }
        }),
    })
}

// --- Future-match pruning ---

/// Returns `false` when it can prove no item at `index` or beyond can ever match `expr`.
/// Conservative: unknown/fetch-required matchers return `true`.
fn can_future_items_match(expr: &CompiledMatcherExpr, index: usize) -> bool {
    match expr {
        CompiledMatcherExpr::Matcher(CompiledChildMatcher::Relation(
            RelationMatcher::IndexRange { max: Some(max), .. },
        )) => index < *max as usize,
        CompiledMatcherExpr::All(exprs) => exprs.iter().all(|e| can_future_items_match(e, index)),
        CompiledMatcherExpr::Any(exprs) => exprs.iter().any(|e| can_future_items_match(e, index)),
        _ => true,
    }
}

// --- Filtering child source ---

struct FilteringChildSource {
    cursor: OwnedCachedChildCursor,
    rules: Vec<CompiledChildRule>,
    pool: Arc<EntryFetchOptionsPool>,
    provider: Arc<dyn FetchProvider>,
    index: usize,
}

#[async_trait::async_trait]
impl ChildSource<ChildFetchOptions> for FilteringChildSource {
    async fn next(&mut self) -> Result<Option<(ChildRef, ChildFetchOptions)>, Error> {
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
                if evaluate_expr(&rule.matcher, &ctx, self.provider.clone()).await? {
                    match rule.options_id {
                        None => break, // skip this child
                        Some(id) => {
                            return Ok(Some((
                                child,
                                ChildFetchOptions::new(self.pool.clone(), id),
                            )));
                        }
                    }
                }
            }
        }
    }
}

/// Filter a `CachedChildSource` through the `child_rules` stored in `pool` at `root_id`.
/// Compiles all matchers (pre-compiling regexes, sorting All/Any by cost) upfront, then
/// returns a lazy `CachedChildSource<ChildFetchOptions>` that evaluates rules on demand.
pub fn filter_children(
    source: Arc<CachedChildSource>,
    pool: Arc<EntryFetchOptionsPool>,
    root_id: OptionsId,
    provider: Arc<dyn FetchProvider>,
) -> Result<CachedChildSource<ChildFetchOptions>, Error> {
    let options = pool.get(root_id);
    let rules = options
        .child_rules
        .iter()
        .map(|r| {
            Ok(CompiledChildRule {
                matcher: compile_expr(&r.matcher)?,
                options_id: r.options_id,
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;

    Ok(CachedChildSource::new(Box::new(FilteringChildSource {
        cursor: source.owned_cursor(),
        rules,
        pool,
        provider,
        index: 0,
    })))
}

// --- Evaluation ---

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
                let pool = Arc::new(EntryFetchOptionsPool::default());
                let root_id = EntryFetchOptionsPool::DEFAULT_ID;
                provider.fetch_entry(identifier, pool, root_id).await
            })
            .await
    }
}

fn evaluate_expr<'a>(
    expr: &'a CompiledMatcherExpr,
    ctx: &'a MatchContext<'a>,
    provider: Arc<dyn FetchProvider>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, Error>> + Send + 'a>> {
    Box::pin(async move {
        match expr {
            CompiledMatcherExpr::Matcher(m) => evaluate_matcher(m, ctx, provider).await,
            CompiledMatcherExpr::Not(inner) => Ok(!evaluate_expr(inner, ctx, provider).await?),
            CompiledMatcherExpr::All(exprs) => {
                for e in exprs.iter() {
                    if !evaluate_expr(e, ctx, provider.clone()).await? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            CompiledMatcherExpr::Any(exprs) => {
                for e in exprs.iter() {
                    if evaluate_expr(e, ctx, provider.clone()).await? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        }
    })
}

async fn evaluate_matcher(
    matcher: &CompiledChildMatcher,
    ctx: &MatchContext<'_>,
    provider: Arc<dyn FetchProvider>,
) -> Result<bool, Error> {
    match matcher {
        CompiledChildMatcher::Always => Ok(true),
        CompiledChildMatcher::Relation(rel) => Ok(evaluate_relation(rel, ctx)),
        CompiledChildMatcher::EntryData(data) => evaluate_entry_data(data, ctx, provider).await,
        CompiledChildMatcher::ChildrenSatisfy { matcher, mode } => {
            evaluate_children_satisfy(matcher, mode, ctx, provider).await
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
    data: &CompiledEntryDataMatcher,
    ctx: &MatchContext<'_>,
    provider: Arc<dyn FetchProvider>,
) -> Result<bool, Error> {
    match data {
        CompiledEntryDataMatcher::EntryType(t) => Ok(ctx.child.entry_type == *t),
        CompiledEntryDataMatcher::NameRegex(regex) => Ok(ctx
            .child
            .name
            .as_deref()
            .map(|n| regex.is_match(n))
            .unwrap_or(false)),
        CompiledEntryDataMatcher::HasSource(source) => Ok(ctx.child.sources.get(source).is_some()),
        CompiledEntryDataMatcher::DurationRange { min, max } => {
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
        CompiledEntryDataMatcher::ExternalType(t) => {
            Ok(ctx.child.external_type.as_ref() == t.as_str())
        }
        CompiledEntryDataMatcher::YouTube(yt) => {
            let entity = ctx.get_entity(provider).await?;
            Ok(evaluate_youtube(yt, entity))
        }
        CompiledEntryDataMatcher::MusicBrainz(mb) => {
            let entity = ctx.get_entity(provider).await?;
            Ok(evaluate_musicbrainz(mb, entity))
        }
    }
}

async fn evaluate_children_satisfy(
    matcher: &CompiledMatcherExpr,
    mode: &QuantifierMode,
    ctx: &MatchContext<'_>,
    provider: Arc<dyn FetchProvider>,
) -> Result<bool, Error> {
    let entity = ctx.get_entity(provider.clone()).await?;

    let mut match_count: u32 = 0;
    let mut total: u32 = 0;

    'sources: for source in &entity.children {
        let mut cursor = source.cursor();
        let mut i: usize = 0;

        while let Some((child, _)) = cursor.next().await? {
            total += 1;
            let child_entity_cell = OnceCell::new();
            let child_ctx = MatchContext {
                child: &child,
                child_index: i,
                entity_cell: &child_entity_cell,
            };
            if evaluate_expr(matcher, &child_ctx, provider.clone()).await? {
                match_count += 1;
            }
            i += 1;

            // Early exit when outcome is already determined regardless of remaining children.
            match mode {
                QuantifierMode::Count { max: Some(max), .. } if match_count > *max => {
                    return Ok(false);
                }
                _ => {}
            }

            // Combined static check: type knowledge + count bounds.
            let (tribool, remaining) = cursor.static_check(matcher);
            match tribool {
                Tribool::False => {
                    // No remaining items in this source will match; move to the next source.
                    continue 'sources;
                }
                Tribool::True => {
                    // All remaining items in this source will match; drain and count them.
                    while cursor.next().await?.is_some() {
                        match_count += 1;
                        total += 1;
                    }
                    continue 'sources;
                }
                Tribool::Indeterminate => {}
            }
            if let Some(remaining) = remaining {
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

    fn always_rule(pool: &mut EntryFetchOptionsPool, options: EntryFetchOptions) -> ChildRule {
        ChildRule {
            matcher: ChildMatcherExpr::Matcher(ChildMatcher::Always),
            options_id: Some(pool.insert(options)),
        }
    }

    fn entry_type_rule(
        pool: &mut EntryFetchOptionsPool,
        t: EntryType,
        options: EntryFetchOptions,
    ) -> ChildRule {
        ChildRule {
            matcher: ChildMatcherExpr::Matcher(ChildMatcher::EntryData(
                EntryDataMatcher::EntryType(t),
            )),
            options_id: Some(pool.insert(options)),
        }
    }

    fn name_regex_rule(
        pool: &mut EntryFetchOptionsPool,
        pattern: &str,
        options: EntryFetchOptions,
    ) -> ChildRule {
        ChildRule {
            matcher: ChildMatcherExpr::Matcher(ChildMatcher::EntryData(
                EntryDataMatcher::NameRegex(pattern.to_string()),
            )),
            options_id: Some(pool.insert(options)),
        }
    }

    fn index_range_rule(
        pool: &mut EntryFetchOptionsPool,
        min: Option<u32>,
        max: Option<u32>,
        options: EntryFetchOptions,
    ) -> ChildRule {
        ChildRule {
            matcher: ChildMatcherExpr::Matcher(ChildMatcher::Relation(
                RelationMatcher::IndexRange { min, max },
            )),
            options_id: Some(pool.insert(options)),
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
            _pool: Arc<EntryFetchOptionsPool>,
            _root_id: OptionsId,
        ) -> Result<EntityResult, Error> {
            panic!("should not be called — test only uses ChildRef-level matchers")
        }
    }

    /// Drain a `CachedChildSource<ChildFetchOptions>` into a vec of `(ChildRef, ChildFetchOptions)`.
    async fn drain(
        source: CachedChildSource<ChildFetchOptions>,
    ) -> Vec<(ChildRef, ChildFetchOptions)> {
        let mut cursor = source.cursor();
        let mut out = Vec::new();
        while let Some(item) = cursor.next().await.unwrap() {
            out.push(item);
        }
        out
    }

    fn run_filter(
        children: Vec<ChildRef>,
        pool: EntryFetchOptionsPool,
        root_rules: Vec<ChildRule>,
    ) -> impl std::future::Future<Output = Vec<(ChildRef, ChildFetchOptions)>> {
        let source = Arc::new(CachedChildSource::from_children(children));
        let mut pool = pool;
        let root_id = pool.insert(EntryFetchOptions {
            child_rules: root_rules,
        });
        let pool = Arc::new(pool);
        let filtered = filter_children(source, pool, root_id, Arc::new(PanicProvider)).unwrap();
        drain(filtered)
    }

    #[tokio::test]
    async fn test_always_matches_all() {
        let children = vec![
            make_child(EntryType::Track, "Song A", "youtube"),
            make_child(EntryType::Artist, "Artist B", "spotify"),
        ];
        let mut pool = EntryFetchOptionsPool::default();
        let rule = always_rule(&mut pool, EntryFetchOptions::default());
        let result = run_filter(children, pool, vec![rule]).await;
        assert_eq!(result.len(), 2);
    }

    #[tokio::test]
    async fn test_entry_type_filter() {
        let children = vec![
            make_child(EntryType::Track, "Song", "youtube"),
            make_child(EntryType::Artist, "Artist", "youtube"),
            make_child(EntryType::Track, "Another Song", "youtube"),
        ];
        let mut pool = EntryFetchOptionsPool::default();
        let rule = entry_type_rule(&mut pool, EntryType::Track, EntryFetchOptions::default());
        let result = run_filter(children, pool, vec![rule]).await;
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
        let mut pool = EntryFetchOptionsPool::default();
        let rule = name_regex_rule(&mut pool, "【MV】", EntryFetchOptions::default());
        let result = run_filter(children, pool, vec![rule]).await;
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
        let mut pool = EntryFetchOptionsPool::default();
        let rule = index_range_rule(&mut pool, Some(1), Some(3), EntryFetchOptions::default());
        let result = run_filter(children, pool, vec![rule]).await;
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0.name.as_deref(), Some("B"));
        assert_eq!(result[1].0.name.as_deref(), Some("C"));
    }

    #[tokio::test]
    async fn test_first_match_wins() {
        let children = vec![make_child(EntryType::Track, "Original Song", "youtube")];
        let mut pool = EntryFetchOptionsPool::default();
        // opts_a has one child rule (always); opts_b is empty
        let inner_rule = always_rule(&mut pool, EntryFetchOptions::default());
        let opts_a = EntryFetchOptions {
            child_rules: vec![inner_rule],
        };
        let rule_a = name_regex_rule(&mut pool, "Original", opts_a);
        let rule_b = always_rule(&mut pool, EntryFetchOptions::default());
        let result = run_filter(children, pool, vec![rule_a, rule_b]).await;
        assert_eq!(result.len(), 1);
        // Should match the first rule (opts_a, which has child_rules) not the second (empty)
        assert_eq!(result[0].1.get().child_rules.len(), 1);
    }

    #[tokio::test]
    async fn test_no_match_excluded() {
        let children = vec![make_child(EntryType::Artist, "Artist", "youtube")];
        let mut pool = EntryFetchOptionsPool::default();
        let rule = entry_type_rule(&mut pool, EntryType::Track, EntryFetchOptions::default());
        let result = run_filter(children, pool, vec![rule]).await;
        assert_eq!(result.len(), 0);
    }

    #[tokio::test]
    async fn test_not_combinator() {
        let children = vec![
            make_child(EntryType::Track, "Song", "youtube"),
            make_child(EntryType::Artist, "Artist", "youtube"),
        ];
        let mut pool = EntryFetchOptionsPool::default();
        let options_id = pool.insert(EntryFetchOptions::default());
        let result = run_filter(
            children,
            pool,
            vec![ChildRule {
                matcher: ChildMatcherExpr::Not(Box::new(ChildMatcherExpr::Matcher(
                    ChildMatcher::EntryData(EntryDataMatcher::EntryType(EntryType::Artist)),
                ))),
                options_id: Some(options_id),
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
        let mut pool = EntryFetchOptionsPool::default();
        let options_id = pool.insert(EntryFetchOptions::default());
        let result = run_filter(
            children,
            pool,
            vec![ChildRule {
                matcher: ChildMatcherExpr::All(vec![
                    ChildMatcherExpr::Matcher(ChildMatcher::EntryData(
                        EntryDataMatcher::EntryType(EntryType::Track),
                    )),
                    ChildMatcherExpr::Matcher(ChildMatcher::EntryData(
                        EntryDataMatcher::NameRegex("Original".to_string()),
                    )),
                ]),
                options_id: Some(options_id),
            }],
        )
        .await;
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0.name.as_deref(), Some("Original Song"));
    }

    // --- IndexRange: early exit without over-consuming ---

    /// Lazy source that generates `total` items on demand, counting every `next()` call.
    struct GenerativeChildSource {
        current: u64,
        total: u64,
        call_count: Arc<AtomicUsize>,
    }

    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;

    fn generative_evaluate_expr(expr: &CompiledMatcherExpr) -> Tribool {
        use crate::providers::types::{default_eval_leaf, static_eval_expr};
        static_eval_expr(expr, &|matcher| match matcher {
            CompiledChildMatcher::EntryData(CompiledEntryDataMatcher::EntryType(t)) => {
                (*t == EntryType::Track).into()
            }
            _ => default_eval_leaf(matcher),
        })
    }

    #[async_trait::async_trait]
    impl ChildSource<()> for GenerativeChildSource {
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

        fn evaluate_expr(&self, expr: &CompiledMatcherExpr) -> Tribool {
            generative_evaluate_expr(expr)
        }
    }

    #[async_trait::async_trait]
    impl ChildSource<ChildFetchOptions> for GenerativeChildSource {
        async fn next(&mut self) -> Result<Option<(ChildRef, ChildFetchOptions)>, Error> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            if self.current >= self.total {
                return Ok(None);
            }
            let child = make_child(EntryType::Track, "track", "youtube");
            self.current += 1;
            let pool = Arc::new(EntryFetchOptionsPool::default());
            Ok(Some((
                child,
                ChildFetchOptions::new(pool, EntryFetchOptionsPool::DEFAULT_ID),
            )))
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            let remaining = (self.total - self.current) as usize;
            (remaining, Some(remaining))
        }

        fn evaluate_expr(&self, expr: &CompiledMatcherExpr) -> Tribool {
            generative_evaluate_expr(expr)
        }
    }

    #[tokio::test]
    async fn test_index_range_does_not_over_consume() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let source = GenerativeChildSource {
            current: 0,
            total: 200_000,
            call_count: Arc::clone(&call_count),
        };
        let cached = Arc::new(CachedChildSource::new(Box::new(source)));
        let mut pool = EntryFetchOptionsPool::default();
        let rule = index_range_rule(&mut pool, None, Some(100), EntryFetchOptions::default());
        let root_id = pool.insert(EntryFetchOptions {
            child_rules: vec![rule],
        });
        let pool = Arc::new(pool);
        let filtered = filter_children(cached, pool, root_id, Arc::new(PanicProvider)).unwrap();
        let result = drain(filtered).await;

        assert_eq!(result.len(), 100);
        // Exactly 100 source calls: the guard fires before fetching item at index 100.
        assert_eq!(call_count.load(Ordering::SeqCst), 100);
    }

    // --- ChildrenSatisfy with lazy source ---

    /// A lazy `ChildSource<ChildFetchOptions>` that counts every `next()` call.
    struct CountingSource {
        items: std::vec::IntoIter<ChildRef>,
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ChildSource<ChildFetchOptions> for CountingSource {
        async fn next(&mut self) -> Result<Option<(ChildRef, ChildFetchOptions)>, Error> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            let pool = Arc::new(EntryFetchOptionsPool::default());
            Ok(self.items.next().map(|c| {
                (
                    c,
                    ChildFetchOptions::new(pool, EntryFetchOptionsPool::DEFAULT_ID),
                )
            }))
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
            _pool: Arc<EntryFetchOptionsPool>,
            _root_id: OptionsId,
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
            children: vec![Arc::new(CachedChildSource::new(Box::new(source)))],
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
        pool: &mut EntryFetchOptionsPool,
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
            options_id: Some(pool.insert(options)),
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
            let mut pool = EntryFetchOptionsPool::default();
            let rule = children_satisfy_rule(&mut pool, "MV", 0.75, EntryFetchOptions::default());
            let root_id = pool.insert(EntryFetchOptions {
                child_rules: vec![rule],
            });
            let pool = Arc::new(pool);
            let filtered = filter_children(source, pool, root_id, provider).unwrap();
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
            let mut pool = EntryFetchOptionsPool::default();
            let rule = children_satisfy_rule(&mut pool, "MV", 0.75, EntryFetchOptions::default());
            let root_id = pool.insert(EntryFetchOptions {
                child_rules: vec![rule],
            });
            let pool = Arc::new(pool);
            let filtered = filter_children(source, pool, root_id, provider).unwrap();
            drain(filtered).await
        };

        // Playlist B matched 2/2 = 1.0 ≥ 0.75 — included
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0.name.as_deref(), Some("Playlist B"));

        // Both songs consumed lazily
        assert_eq!(call_count.load(Ordering::SeqCst), 3);
    }

    // --- evaluate_expr / static_check early exit ---

    fn make_entity_with_generative_source(n: u64, call_count: Arc<AtomicUsize>) -> EntityResult {
        let source = GenerativeChildSource {
            current: 0,
            total: n,
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
            children: vec![Arc::new(CachedChildSource::new(Box::new(source)))],
            aliases: vec![],
        }
    }

    fn children_satisfy_type_rule(
        pool: &mut EntryFetchOptionsPool,
        t: EntryType,
        min: u32,
        options: EntryFetchOptions,
    ) -> ChildRule {
        ChildRule {
            matcher: ChildMatcherExpr::Matcher(ChildMatcher::ChildrenSatisfy {
                matcher: Box::new(ChildMatcherExpr::Matcher(ChildMatcher::EntryData(
                    EntryDataMatcher::EntryType(t),
                ))),
                mode: QuantifierMode::Count {
                    min: Some(min),
                    max: None,
                },
            }),
            options_id: Some(pool.insert(options)),
        }
    }

    // Source yields only tracks. Filtering for Album (min=1):
    // evaluate_expr returns False after first item → only 1 next() call instead of N.
    #[tokio::test]
    async fn test_evaluate_expr_false_exits_after_first_item() {
        const URL: &str = "https://www.youtube.com/playlist?list=EVAL_FALSE";
        let call_count = Arc::new(AtomicUsize::new(0));

        let entity = make_entity_with_generative_source(5, Arc::clone(&call_count));
        let provider = Arc::new(SingleResultProvider::new(URL, entity));

        let source = Arc::new(CachedChildSource::from_children(vec![make_playlist_ref(
            URL,
        )]));
        let mut pool = EntryFetchOptionsPool::default();
        let rule = children_satisfy_type_rule(
            &mut pool,
            EntryType::Artist,
            1,
            EntryFetchOptions::default(),
        );
        let root_id = pool.insert(EntryFetchOptions {
            child_rules: vec![rule],
        });
        let pool = Arc::new(pool);
        let filtered = filter_children(source, pool, root_id, provider).unwrap();
        let result = drain(filtered).await;

        // Source only yields tracks → ChildrenSatisfy(Album, min=1) can never be satisfied.
        assert_eq!(result.len(), 0);
        // evaluate_expr returns False after the first item; remaining 4 are never fetched.
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    // Source yields only tracks. Filtering for Track (min=1):
    // evaluate_expr returns True after first item → remaining items drained without per-item
    // evaluation, but all next() calls still happen to count them.
    #[tokio::test]
    async fn test_evaluate_expr_true_drains_without_per_item_eval() {
        const URL: &str = "https://www.youtube.com/playlist?list=EVAL_TRUE";
        let call_count = Arc::new(AtomicUsize::new(0));

        let entity = make_entity_with_generative_source(5, Arc::clone(&call_count));
        let provider = Arc::new(SingleResultProvider::new(URL, entity));

        let source = Arc::new(CachedChildSource::from_children(vec![make_playlist_ref(
            URL,
        )]));
        let mut pool = EntryFetchOptionsPool::default();
        let rule = children_satisfy_type_rule(
            &mut pool,
            EntryType::Track,
            1,
            EntryFetchOptions::default(),
        );
        let root_id = pool.insert(EntryFetchOptions {
            child_rules: vec![rule],
        });
        let pool = Arc::new(pool);
        let filtered = filter_children(source, pool, root_id, provider).unwrap();
        let result = drain(filtered).await;

        // All 5 children are tracks → ChildrenSatisfy(Track, min=1) passes.
        assert_eq!(result.len(), 1);
        // 6 next() calls: 1 evaluated + 4 drained + 1 None sentinel. Items 2-5 skip
        // per-item evaluation thanks to evaluate_expr returning True.
        assert_eq!(call_count.load(Ordering::SeqCst), 6);
    }
}
