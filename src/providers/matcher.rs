use std::sync::Arc;

use regex::Regex;
use tokio::sync::OnceCell;

use crate::providers::{
    FetchProvider,
    types::{
        ChildMatcher, ChildMatcherExpr, ChildRef, ChildRule, EntityResult, EntryDataMatcher,
        EntryFetchOptions, EntrySpecificData, Error, QuantifierMode, RelationMatcher,
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

/// Async source of children — abstracts over Vec, paginated API responses, etc.
#[async_trait::async_trait]
pub trait ChildSource: Send {
    /// Return the next child, or `None` when exhausted.
    async fn next(&mut self) -> Result<Option<ChildRef>, Error>;

    /// Optional size hint: (lower_bound, upper_bound).
    /// Used by early termination to estimate remaining children.
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}

/// Wraps a `Vec<ChildRef>` as a `ChildSource`.
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
    async fn next(&mut self) -> Result<Option<ChildRef>, Error> {
        let item = self.children.next();
        if item.is_some() {
            self.remaining = self.remaining.saturating_sub(1);
        }
        Ok(item)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

/// A resolved child: the original `ChildRef` paired with the options to use when fetching it.
pub struct ResolvedChild {
    pub child: ChildRef,
    pub options: Arc<EntryFetchOptions>,
}

/// Evaluate `child_rules` against children from a source (first-match-wins per child).
/// Children that don't match any rule are excluded.
pub async fn evaluate_child_rules(
    source: &mut dyn ChildSource,
    rules: &[ChildRule],
    provider: &dyn FetchProvider,
    backend_evaluator: &dyn BackendMatcherEvaluator,
) -> Result<Vec<ResolvedChild>, Error> {
    let mut resolved = Vec::new();
    let mut index: usize = 0;

    while let Some(child) = source.next().await? {
        let entity_cell = OnceCell::new();
        let ctx = MatchContext {
            child: &child,
            child_index: index,
            entity_cell: &entity_cell,
        };

        for rule in rules {
            if evaluate_expr(&rule.matcher, &ctx, provider, backend_evaluator).await? {
                resolved.push(ResolvedChild {
                    child,
                    options: rule.options.clone(),
                });
                break;
            }
        }

        index += 1;
    }

    Ok(resolved)
}

struct MatchContext<'a> {
    child: &'a ChildRef,
    child_index: usize,
    entity_cell: &'a OnceCell<EntityResult>,
}

impl MatchContext<'_> {
    async fn get_entity(&self, provider: &dyn FetchProvider) -> Result<&EntityResult, Error> {
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
    provider: &'a dyn FetchProvider,
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
                    if !evaluate_expr(e, ctx, provider, backend_evaluator).await? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            ChildMatcherExpr::Any(exprs) => {
                for e in exprs.iter() {
                    if evaluate_expr(e, ctx, provider, backend_evaluator).await? {
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
    provider: &dyn FetchProvider,
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
    provider: &dyn FetchProvider,
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
    provider: &dyn FetchProvider,
    backend_evaluator: &dyn BackendMatcherEvaluator,
) -> Result<bool, Error> {
    let entity = ctx.get_entity(provider).await?;
    let children = &entity.children;

    if children.is_empty() {
        return match mode {
            QuantifierMode::Count { min, .. } => Ok(min.unwrap_or(0) == 0),
            QuantifierMode::Ratio { .. } => Ok(false),
        };
    }

    let mut match_count: u32 = 0;
    let total = children.len() as u32;

    for (i, child) in children.iter().enumerate() {
        let child_entity_cell = OnceCell::new();
        let child_ctx = MatchContext {
            child,
            child_index: i,
            entity_cell: &child_entity_cell,
        };
        if evaluate_expr(matcher, &child_ctx, provider, backend_evaluator).await? {
            match_count += 1;
        }
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
            &self,
            _identifier: &str,
            _fetch_options: EntryFetchOptions,
        ) -> Result<EntityResult, Error> {
            panic!("should not be called — test only uses ChildRef-level matchers")
        }
    }

    #[tokio::test]
    async fn test_always_matches_all() {
        let children = vec![
            make_child(EntryType::Track, "Song A", "youtube"),
            make_child(EntryType::Artist, "Artist B", "spotify"),
        ];
        let rules = vec![always_rule(EntryFetchOptions::default())];

        let result = evaluate_child_rules(
            &mut VecChildSource::new(children),
            &rules,
            &PanicProvider,
            &NoOpEvaluator,
        )
        .await
        .unwrap();

        assert_eq!(result.len(), 2);
    }

    #[tokio::test]
    async fn test_entry_type_filter() {
        let children = vec![
            make_child(EntryType::Track, "Song", "youtube"),
            make_child(EntryType::Artist, "Artist", "youtube"),
            make_child(EntryType::Track, "Another Song", "youtube"),
        ];
        let rules = vec![entry_type_rule(
            EntryType::Track,
            EntryFetchOptions::default(),
        )];

        let result = evaluate_child_rules(
            &mut VecChildSource::new(children),
            &rules,
            &PanicProvider,
            &NoOpEvaluator,
        )
        .await
        .unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].child.name.as_deref(), Some("Song"));
        assert_eq!(result[1].child.name.as_deref(), Some("Another Song"));
    }

    #[tokio::test]
    async fn test_name_regex_filter() {
        let children = vec![
            make_child(EntryType::Track, "Original Song【MV】", "youtube"),
            make_child(EntryType::Track, "Cover Song", "youtube"),
            make_child(EntryType::Track, "Another Original【MV】", "youtube"),
        ];
        let rules = vec![name_regex_rule("【MV】", EntryFetchOptions::default())];

        let result = evaluate_child_rules(
            &mut VecChildSource::new(children),
            &rules,
            &PanicProvider,
            &NoOpEvaluator,
        )
        .await
        .unwrap();

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
        let rules = vec![index_range_rule(
            Some(1),
            Some(3),
            EntryFetchOptions::default(),
        )];

        let result = evaluate_child_rules(
            &mut VecChildSource::new(children),
            &rules,
            &PanicProvider,
            &NoOpEvaluator,
        )
        .await
        .unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].child.name.as_deref(), Some("B"));
        assert_eq!(result[1].child.name.as_deref(), Some("C"));
    }

    #[tokio::test]
    async fn test_first_match_wins() {
        let children = vec![make_child(EntryType::Track, "Original Song", "youtube")];

        let opts_a = EntryFetchOptions {
            child_rules: vec![always_rule(EntryFetchOptions::default())],
        };
        let opts_b = EntryFetchOptions::default();

        let rules = vec![name_regex_rule("Original", opts_a), always_rule(opts_b)];

        let result = evaluate_child_rules(
            &mut VecChildSource::new(children),
            &rules,
            &PanicProvider,
            &NoOpEvaluator,
        )
        .await
        .unwrap();

        assert_eq!(result.len(), 1);
        // Should match the first rule (has child_rules) not the second (empty)
        assert_eq!(result[0].options.child_rules.len(), 1);
    }

    #[tokio::test]
    async fn test_no_match_excluded() {
        let children = vec![make_child(EntryType::Artist, "Artist", "youtube")];
        let rules = vec![entry_type_rule(
            EntryType::Track,
            EntryFetchOptions::default(),
        )];

        let result = evaluate_child_rules(
            &mut VecChildSource::new(children),
            &rules,
            &PanicProvider,
            &NoOpEvaluator,
        )
        .await
        .unwrap();

        assert_eq!(result.len(), 0);
    }

    #[tokio::test]
    async fn test_not_combinator() {
        let children = vec![
            make_child(EntryType::Track, "Song", "youtube"),
            make_child(EntryType::Artist, "Artist", "youtube"),
        ];
        let rules = vec![ChildRule {
            matcher: ChildMatcherExpr::Not(Box::new(ChildMatcherExpr::Matcher(
                ChildMatcher::EntryData(EntryDataMatcher::EntryType(EntryType::Artist)),
            ))),
            options: Arc::new(EntryFetchOptions::default()),
        }];

        let result = evaluate_child_rules(
            &mut VecChildSource::new(children),
            &rules,
            &PanicProvider,
            &NoOpEvaluator,
        )
        .await
        .unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].child.entry_type, EntryType::Track);
    }

    #[tokio::test]
    async fn test_all_combinator() {
        let children = vec![
            make_child(EntryType::Track, "Original Song", "youtube"),
            make_child(EntryType::Track, "Cover Song", "youtube"),
            make_child(EntryType::Artist, "Original Artist", "youtube"),
        ];
        // Track AND name contains "Original"
        let rules = vec![ChildRule {
            matcher: ChildMatcherExpr::All(vec![
                ChildMatcherExpr::Matcher(ChildMatcher::EntryData(EntryDataMatcher::EntryType(
                    EntryType::Track,
                ))),
                ChildMatcherExpr::Matcher(ChildMatcher::EntryData(EntryDataMatcher::NameRegex(
                    "Original".to_string(),
                ))),
            ]),
            options: Arc::new(EntryFetchOptions::default()),
        }];

        let result = evaluate_child_rules(
            &mut VecChildSource::new(children),
            &rules,
            &PanicProvider,
            &NoOpEvaluator,
        )
        .await
        .unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].child.name.as_deref(), Some("Original Song"));
    }
}
