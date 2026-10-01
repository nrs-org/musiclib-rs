use std::{collections::HashSet, sync::Arc};

use crate::providers::{
    FetchProvider,
    std_values::StandardProviderKeys,
    types::{
        EntityResult, EntryFetchOptionsPool, EntryType, ExternalSources, OptionsId, child_next,
    },
};
use futures::future::join_all;
use tracing::{debug, warn};

use super::state::{ChildEdge, Pair, PairMetadata, State, StubInfo};

type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'static>>;

fn error_chain(e: &dyn std::error::Error) -> String {
    let top = e.to_string();
    let mut src = e.source();
    let mut root = None;
    while let Some(cause) = src {
        root = Some(cause.to_string());
        src = cause.source();
    }
    match root {
        Some(r) if !top.contains(&r) => format!("{top}: {r}"),
        _ => top,
    }
}

/// Fetch the entity referred to by `input`, record metadata + relations, recurse
/// into sources and children. Structured concurrency: every spawned subtask is
/// awaited before this future resolves.
///
/// `input` is the pair the caller knows about. It will be linked via `is_rel`
/// to the canonical pair returned by the URL provider, so equivalence classes
/// span both the source-emitted form (e.g. `(youtube, long_url)`) and the
/// provider's canonical form (e.g. `(youtube, short_url)`). The source key is
/// the provider's plain key (`youtube`); `youtube:video` is the `external_type`
/// label, used only for child-type filtering, not as a source key.
pub fn import(
    state: Arc<State>,
    providers: Arc<Vec<Arc<dyn FetchProvider>>>,
    pool: Arc<EntryFetchOptionsPool>,
    input: Pair,
    options_id: OptionsId,
) -> BoxFuture<()> {
    Box::pin(async move {
        // 1. Canonicalize the input URL via the first provider that recognises it.
        let Some((canonical, provider, provider_idx)) =
            canonicalize_first(&providers, &input.1).await
        else {
            debug!("no provider recognised {}:{}", input.0, input.1);
            return;
        };

        // 2. Link the input pair to the canonical pair if they differ. Done
        //    before the claim so a losing claimer still records the link.
        if input != canonical {
            state.push_is_rel(input.clone(), canonical.clone());
        }

        // 3. Claim (canonical pair, options). If this pair was already
        //    processed under these options (or these options add nothing),
        //    we're done — the is_rel above (and our caller's has_rel/is_rel)
        //    is preserved. Otherwise the pair is processed again under these
        //    options too: each pair ends up walked under the union of every
        //    option set that reached it, whatever order they arrived in (see
        //    `State::claim`). The refetch is served from the HTTP cache.
        let leaf = pool.get(options_id).child_rules.is_empty();
        if !state.claim(&canonical, options_id, leaf) {
            return;
        }

        // 4. Fetch. On failure we log and bail.
        debug!("fetching {}:{}", canonical.0, canonical.1);
        if let Some(p) = state.progress() {
            p.fetching(&canonical);
        }
        let result = match provider
            .fetch_entry(&canonical.0, &canonical.1, pool.clone(), options_id)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let msg = error_chain(&e);
                warn!("fetch failed for {}:{}: {}", canonical.0, canonical.1, msg);
                if let Some(p) = state.progress() {
                    p.fetch_failed(&canonical, &msg);
                }
                return;
            }
        };

        let EntityResult {
            release_date,
            mut sources,
            // Not retained: fetch-options matchers that need raw provider data
            // (youtube/musicbrainz-specific fields) do their own independent,
            // HTTP-cache-backed fetch via `MatchContext::get_entity` rather
            // than reading this — keeping the whole raw response alive in
            // `PairMetadata` for the rest of the import serves nothing.
            extra: _,
            specific_data,
            children,
            aliases,
        } = result;
        let entry_type = specific_data.entry_type();

        // 5. Per-pair metadata for the canonical pair. Stored only after
        //    step 7 has listed every child (see there), so a truncated
        //    listing never leaves a pair that looks fully fetched.
        let metadata = PairMetadata {
            entry_type,
            release_date,
            specific_data,
            aliases,
        };

        // 5b. Cross-link: ask every other provider to enrich `sources` with
        //     IDs in its own namespace. Fixed-point: each pass calls every
        //     provider that (a) hasn't been called yet and (b) doesn't already
        //     own a namespace present in `sources`. Loop until no IDs are
        //     added. HTTP-layer cache makes repeats cheap.
        let mut called: HashSet<usize> = HashSet::new();
        called.insert(provider_idx);
        loop {
            let mut changed = false;
            for (i, p) in providers.iter().enumerate() {
                if called.contains(&i) {
                    continue;
                }
                if provider_owns_any(p.as_ref(), &sources).await {
                    called.insert(i);
                    continue;
                }
                match p.resolve_external_source(entry_type, &sources).await {
                    Ok(new) => {
                        called.insert(i);
                        if let Some(new) = new {
                            for (k, ids) in new.0 {
                                let entry = sources.0.entry(k).or_default();
                                let before = entry.len();
                                entry.extend(ids);
                                if entry.len() > before {
                                    changed = true;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!(
                            "resolve_external_source failed for {}:{}: {}",
                            canonical.0,
                            canonical.1,
                            error_chain(&e)
                        );
                        called.insert(i);
                    }
                }
            }
            if !changed {
                break;
            }
        }

        let mut subs: Vec<BoxFuture<()>> = vec![];

        // 6. is_rel + recursive import for every other source returned by this
        //    fetch. Passing the full pair (not just the URL) lets the recursive
        //    call link its own canonicalisation back to what we observed.
        //    Cross-type sources (e.g. a Release linking to a Track URL) are
        //    dropped: they would fuse entries of different types in union-find.
        for src in flatten_pairs(&sources) {
            if src == canonical {
                continue;
            }
            if let Some(t) = pair_entry_type(&providers, &src).await
                && t != entry_type
            {
                warn!(
                    "skip cross-type source {}:{} ({:?} != {:?})",
                    src.0, src.1, t, entry_type
                );
                continue;
            }
            state.push_is_rel(canonical.clone(), src.clone());
            subs.push(import(
                Arc::clone(&state),
                Arc::clone(&providers),
                Arc::clone(&pool),
                src,
                options_id,
            ));
        }

        // 7. has_rel for every child. Pick one pair from the child's sources
        //    as the edge's child end (others are linked via is_rel) and recurse.
        //    Edges are buffered: if a listing fails partway (e.g. a 429 on a
        //    later page), a non-artist parent is treated as a failed fetch and
        //    neither its edges nor its metadata are stored, so an album never
        //    ends up with a tracklist that silently stops at page 2. Artists
        //    keep what was listed: a discography is policy-filtered anyway, so
        //    a partial one claims nothing. Children already listed are still
        //    imported either way, and their sibling is_rel links are kept:
        //    those are facts about the child, not about this listing.
        let mut edges: Vec<ChildEdge> = Vec::new();
        let mut listing_error: Option<String> = None;
        for child_source in &children {
            let mut cursor = child_source.owned_cursor();
            loop {
                match child_next(&mut cursor).await {
                    Ok(Some((child_ref, child_fetch_opts))) => {
                        let child_pairs = flatten_pairs(&child_ref.sources);
                        let Some(primary) = child_pairs.first().cloned() else {
                            warn!("child {:?} has no sources; skipping", child_ref.name);
                            continue;
                        };
                        for sibling in child_pairs.iter().skip(1) {
                            state.push_is_rel(primary.clone(), sibling.clone());
                        }
                        // Used only if the child ends up never fetched.
                        state.insert_stub(
                            primary.clone(),
                            StubInfo {
                                entry_type: child_ref.entry_type,
                                name: child_ref.name.clone(),
                                duration_ms: child_ref.duration_ms,
                            },
                        );
                        let pos = child_ref.position.as_ref();
                        edges.push(ChildEdge {
                            parent: canonical.clone(),
                            child: primary.clone(),
                            disc_no: pos.and_then(|p| p.disc_no),
                            track_no: pos.map(|p| p.track_no),
                            contributions: child_ref.contributions.clone(),
                            original_relation_kind: child_ref
                                .original_relation_kind
                                .clone()
                                .map(std::borrow::Cow::into_owned),
                        });
                        // `None`: listed but not to be fetched; the edge and
                        // stub info above are all it gets.
                        if let Some(child_options) = child_fetch_opts.id {
                            subs.push(import(
                                Arc::clone(&state),
                                Arc::clone(&providers),
                                Arc::clone(&pool),
                                primary,
                                child_options,
                            ));
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        let msg = error_chain(&e);
                        warn!(
                            "child listing failed for {}:{}: {}",
                            canonical.0, canonical.1, msg
                        );
                        listing_error.get_or_insert(msg);
                        break;
                    }
                }
            }
        }

        match listing_error {
            Some(msg) if entry_type != EntryType::Artist => {
                if let Some(p) = state.progress() {
                    p.fetch_failed(&canonical, &format!("child listing incomplete: {msg}"));
                }
            }
            _ => {
                state.insert_metadata(canonical.clone(), metadata);
                for edge in edges {
                    state.push_has_rel(edge);
                }
                if let Some(p) = state.progress() {
                    p.fetched(&canonical);
                }
            }
        }

        // Drop this entity's child listings before waiting on the children.
        // A listing we stopped reading early (it failed partway) can still
        // hold requests its stream had already started ahead of us
        // (`buffer_unordered` prefetch). Kept alive but never polled, each
        // keeps `http::Activity` busy forever, so the coalescer never sees
        // the process idle and every parked request waits on it: the import
        // hangs. Dropping the streams cancels those requests.
        drop(children);

        // 8. Await all sub-tasks. Each handles its own errors.
        join_all(subs).await;
    })
}

/// Resolve the entry type for a pair by asking each provider to canonicalize it
/// using its own source key. Returns `None` for unrecognised domains (which can
/// never carry typed metadata, so keeping them is safe).
async fn pair_entry_type(providers: &[Arc<dyn FetchProvider>], pair: &Pair) -> Option<EntryType> {
    for p in providers {
        if let Some(c) = p.canonicalize(&pair.0, &pair.1).await {
            return Some(c.entry_type);
        }
    }
    None
}

async fn canonicalize_first(
    providers: &[Arc<dyn FetchProvider>],
    url: &str,
) -> Option<(Pair, Arc<dyn FetchProvider>, usize)> {
    for (i, provider) in providers.iter().enumerate() {
        if let Some(canon) = provider
            .canonicalize(StandardProviderKeys::UNKNOWN_URL, url)
            .await
        {
            return Some((
                (
                    canon.canonical_source_key.into_owned(),
                    canon.canonical_identifier,
                ),
                Arc::clone(provider),
                i,
            ));
        }
    }
    None
}

/// True if `provider` claims any (key, id) pair currently in `sources` —
/// i.e. it owns one of those namespaces. canonicalize is a sync pattern
/// match, so this is cheap.
async fn provider_owns_any(provider: &dyn FetchProvider, sources: &ExternalSources) -> bool {
    for (key, ids) in &sources.0 {
        for id in ids {
            if provider.canonicalize(key.as_ref(), id).await.is_some() {
                return true;
            }
        }
    }
    false
}

fn flatten_pairs(sources: &ExternalSources) -> Vec<Pair> {
    let mut out = Vec::new();
    for (key, ids) in &sources.0 {
        for id in ids {
            out.push((key.to_string(), id.clone()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::{
        borrow::Cow,
        collections::HashSet,
        pin::Pin,
        task::{Context, Poll},
    };

    use futures::Stream;

    use super::*;
    use crate::providers::{
        CanonicalizeProvider,
        types::{
            CachedChildSource, CanonicalizeResult, ChildFetchOptions, ChildRef, ChildSource,
            EntrySpecificData, Error,
        },
    };

    const SOURCE: &str = "fake";

    /// Yields its children, then one listing error (like a 429 on page 2).
    struct TruncatedListing {
        items: std::vec::IntoIter<ChildRef>,
        failed: bool,
    }

    impl Stream for TruncatedListing {
        type Item = Result<(ChildRef, ChildFetchOptions), Error>;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let pool = Arc::new(EntryFetchOptionsPool::default());
            if let Some(c) = self.items.next() {
                return Poll::Ready(Some(Ok((
                    c,
                    ChildFetchOptions::new(pool, EntryFetchOptionsPool::DEFAULT_ID),
                ))));
            }
            if !self.failed {
                self.failed = true;
                return Poll::Ready(Some(Err(Error::InvalidUrl("page 2: 429".into()))));
            }
            Poll::Ready(None)
        }
    }

    impl ChildSource<ChildFetchOptions> for TruncatedListing {}

    /// `"album"` (a release) and `"artist"` each list `"track1"` and then fail;
    /// `"track1"` is a leaf track.
    struct FakeProvider;

    fn entry_type_for(identifier: &str) -> Option<EntryType> {
        match identifier {
            "album" => Some(EntryType::Release),
            "artist" => Some(EntryType::Artist),
            "track1" => Some(EntryType::Track),
            _ => None,
        }
    }

    #[async_trait::async_trait]
    impl CanonicalizeProvider for FakeProvider {
        async fn canonicalize(
            &self,
            _source_key: &str,
            identifier: &str,
        ) -> Option<CanonicalizeResult> {
            Some(CanonicalizeResult {
                canonical_source_key: Cow::Borrowed(SOURCE),
                canonical_identifier: identifier.to_string(),
                entry_type: entry_type_for(identifier)?,
                external_type: Cow::Borrowed(""),
            })
        }
    }

    #[async_trait::async_trait]
    impl FetchProvider for FakeProvider {
        async fn fetch_entry(
            self: Arc<Self>,
            _source_key: &str,
            identifier: &str,
            _pool: Arc<EntryFetchOptionsPool>,
            _root_id: OptionsId,
        ) -> Result<EntityResult, Error> {
            let (specific_data, children) = match identifier {
                "track1" => (
                    EntrySpecificData::Track {
                        duration_ms: vec![],
                        positions: Default::default(),
                    },
                    vec![],
                ),
                parent => {
                    let track = ChildRef {
                        entry_type: EntryType::Track,
                        name: Some("Track 1".to_string()),
                        sources: [(Cow::Borrowed(SOURCE), HashSet::from(["track1".to_string()]))]
                            .into(),
                        ..Default::default()
                    };
                    let listing = TruncatedListing {
                        items: vec![track].into_iter(),
                        failed: false,
                    };
                    let specific_data = if parent == "artist" {
                        EntrySpecificData::Artist
                    } else {
                        EntrySpecificData::Release {
                            release_type: None,
                            num_discs: None,
                            num_tracks: None,
                        }
                    };
                    (
                        specific_data,
                        vec![Arc::new(CachedChildSource::new(Box::new(listing)))],
                    )
                }
            };
            Ok(EntityResult {
                release_date: None,
                sources: Default::default(),
                extra: Default::default(),
                specific_data,
                children,
                aliases: vec![],
            })
        }
    }

    async fn import_root(identifier: &str) -> Arc<State> {
        let state = Arc::new(State::new());
        let providers: Arc<Vec<Arc<dyn FetchProvider>>> = Arc::new(vec![Arc::new(FakeProvider)]);
        import(
            Arc::clone(&state),
            providers,
            Arc::new(EntryFetchOptionsPool::default()),
            (
                StandardProviderKeys::UNKNOWN_URL.to_string(),
                identifier.to_string(),
            ),
            EntryFetchOptionsPool::DEFAULT_ID,
        )
        .await;
        state
    }

    fn pair(identifier: &str) -> Pair {
        (SOURCE.to_string(), identifier.to_string())
    }

    /// An album whose tracklist fails partway is stored as neither fetched nor
    /// partially listed; the track it did list is still imported on its own.
    #[tokio::test]
    async fn truncated_listing_fails_non_artist_parent() {
        let state = import_root("album").await;
        let metadata = state.metadata.lock().unwrap();
        assert!(!metadata.contains_key(&pair("album")));
        assert!(metadata.contains_key(&pair("track1")));
        assert!(state.has_rel.lock().unwrap().is_empty());
    }

    /// An artist keeps its metadata and the partial discography it listed.
    #[tokio::test]
    async fn truncated_listing_keeps_artist_and_partial_discography() {
        let state = import_root("artist").await;
        assert!(state.metadata.lock().unwrap().contains_key(&pair("artist")));
        let edges = state.has_rel.lock().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].parent, pair("artist"));
        assert_eq!(edges[0].child, pair("track1"));
    }

    /// A small catalogue that filters its children through the fetch options
    /// like a real backend: `artist` lists track `t1`; `t1` lists its album;
    /// `album` lists `t1` and `t2`.
    struct CatalogueProvider;

    fn child(entry_type: EntryType, id: &str) -> ChildRef {
        ChildRef {
            entry_type,
            name: Some(id.to_string()),
            sources: [(Cow::Borrowed(SOURCE), HashSet::from([id.to_string()]))].into(),
            ..Default::default()
        }
    }

    #[async_trait::async_trait]
    impl CanonicalizeProvider for CatalogueProvider {
        async fn canonicalize(
            &self,
            _source_key: &str,
            identifier: &str,
        ) -> Option<CanonicalizeResult> {
            let entry_type = match identifier {
                "artist" => EntryType::Artist,
                "album" => EntryType::Release,
                "t1" | "t2" => EntryType::Track,
                _ => return None,
            };
            Some(CanonicalizeResult {
                canonical_source_key: Cow::Borrowed(SOURCE),
                canonical_identifier: identifier.to_string(),
                entry_type,
                external_type: Cow::Borrowed(""),
            })
        }
    }

    #[async_trait::async_trait]
    impl FetchProvider for CatalogueProvider {
        async fn fetch_entry(
            self: Arc<Self>,
            _source_key: &str,
            identifier: &str,
            pool: Arc<EntryFetchOptionsPool>,
            root_id: OptionsId,
        ) -> Result<EntityResult, Error> {
            let track = EntrySpecificData::Track {
                duration_ms: vec![],
                positions: Default::default(),
            };
            let (specific_data, children) = match identifier {
                "artist" => (
                    EntrySpecificData::Artist,
                    vec![child(EntryType::Track, "t1")],
                ),
                "t1" => (track, vec![child(EntryType::Release, "album")]),
                "t2" => (track, vec![]),
                _ => (
                    EntrySpecificData::Release {
                        release_type: None,
                        num_discs: None,
                        num_tracks: None,
                    },
                    vec![child(EntryType::Track, "t1"), child(EntryType::Track, "t2")],
                ),
            };
            let parent_type = specific_data.entry_type();
            let children = Arc::new(CachedChildSource::from_children(children));
            let filtered = crate::providers::matcher::filter_children(
                children,
                pool,
                root_id,
                self,
                parent_type,
            )?;
            Ok(EntityResult {
                release_date: None,
                sources: Default::default(),
                extra: Default::default(),
                specific_data,
                children: vec![Arc::new(filtered)],
                aliases: vec![],
            })
        }
    }

    fn type_rule(
        entry_type: EntryType,
        options_id: OptionsId,
    ) -> crate::providers::types::ChildRule {
        use crate::providers::types::{ChildMatcher, ChildMatcherExpr, EntryDataMatcher};
        crate::providers::types::ChildRule {
            matcher: ChildMatcherExpr::Matcher(ChildMatcher::EntryData(
                EntryDataMatcher::EntryType(entry_type),
            )),
            options_id: Some(options_id),
        }
    }

    /// `main` (artist discography: tracks via `track`), `track` (album as a
    /// leaf), `full` (an album's tracks as leaves).
    fn catalogue_pool() -> (Arc<EntryFetchOptionsPool>, OptionsId, OptionsId) {
        use crate::providers::types::EntryFetchOptions;
        let leaf = EntryFetchOptionsPool::DEFAULT_ID;
        let mut pool = EntryFetchOptionsPool::default();
        let track = pool.insert(EntryFetchOptions {
            child_rules: vec![type_rule(EntryType::Release, leaf)],
        });
        let main = pool.insert(EntryFetchOptions {
            child_rules: vec![type_rule(EntryType::Track, track)],
        });
        let full = pool.insert(EntryFetchOptions {
            child_rules: vec![type_rule(EntryType::Track, leaf)],
        });
        (Arc::new(pool), main, full)
    }

    async fn run_roots(
        roots: &[(&str, OptionsId)],
        pool: Arc<EntryFetchOptionsPool>,
    ) -> Arc<State> {
        let state = Arc::new(State::new());
        let providers: Arc<Vec<Arc<dyn FetchProvider>>> =
            Arc::new(vec![Arc::new(CatalogueProvider)]);
        for (id, options) in roots {
            import(
                Arc::clone(&state),
                Arc::clone(&providers),
                Arc::clone(&pool),
                (
                    StandardProviderKeys::UNKNOWN_URL.to_string(),
                    id.to_string(),
                ),
                *options,
            )
            .await;
        }
        state
    }

    fn snapshot(state: &State) -> (Vec<Pair>, Vec<(Pair, Pair)>) {
        let mut fetched: Vec<Pair> = state.metadata.lock().unwrap().keys().cloned().collect();
        fetched.sort();
        let mut edges: Vec<(Pair, Pair)> = state
            .has_rel
            .lock()
            .unwrap()
            .iter()
            .map(|e| (e.parent.clone(), e.child.clone()))
            .collect();
        edges.sort();
        edges.dedup();
        (fetched, edges)
    }

    /// An album reached as a leaf (from a track) still records its whole
    /// tracklist, as stubs for the tracks it doesn't fetch.
    #[tokio::test]
    async fn leaf_album_records_tracklist_as_stubs() {
        let (pool, main, _) = catalogue_pool();
        let state = run_roots(&[("artist", main)], pool).await;
        let (fetched, edges) = snapshot(&state);
        assert_eq!(fetched, vec![pair("album"), pair("artist"), pair("t1")]);
        assert!(edges.contains(&(pair("album"), pair("t2"))));
        let stubs = state.stubs.lock().unwrap();
        assert_eq!(stubs[&pair("t2")].name.as_deref(), Some("t2"));
    }

    /// The album is reached as a leaf through the artist's track, and as
    /// `full` from its own root. Both orders fetch its other track: under
    /// first-claimer-wins, the artist-first order never would.
    #[tokio::test]
    async fn overlapping_roots_give_same_result_in_either_order() {
        let (pool, main, full) = catalogue_pool();
        let artist_first = run_roots(&[("artist", main), ("album", full)], Arc::clone(&pool)).await;
        let album_first = run_roots(&[("album", full), ("artist", main)], pool).await;

        let a = snapshot(&artist_first);
        assert_eq!(a, snapshot(&album_first));
        assert!(
            a.0.contains(&pair("t2")),
            "the album's other track is fetched"
        );
    }
}
