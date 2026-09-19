use std::{collections::HashSet, sync::Arc};

use crate::providers::{
    FetchProvider,
    std_values::StandardProviderKeys,
    types::{
        EntityResult, EntryFetchOptionsPool, EntrySpecificData, EntryType, ExternalSources,
        OptionsId, child_next,
    },
};
use futures::future::join_all;
use tracing::{debug, warn};

use super::state::{ChildEdge, Pair, PairMetadata, State};

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
            state
                .is_rel
                .lock()
                .unwrap()
                .push((input.clone(), canonical.clone()));
        }

        // 3. Claim the canonical pair. If another task owns it, we're done —
        //    the is_rel above (and our caller's has_rel/is_rel) is preserved.
        if !state.claim(&canonical) {
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
            extra,
            specific_data,
            children,
            aliases,
        } = result;
        let entry_type = entry_type_of(&specific_data);

        // 5. Store per-pair metadata on the canonical pair.
        state.metadata.lock().unwrap().insert(
            canonical.clone(),
            PairMetadata {
                entry_type,
                release_date,
                extra,
                specific_data,
                aliases,
            },
        );
        if let Some(p) = state.progress() {
            p.fetched(&canonical);
        }

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
            state
                .is_rel
                .lock()
                .unwrap()
                .push((canonical.clone(), src.clone()));
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
                            state
                                .is_rel
                                .lock()
                                .unwrap()
                                .push((primary.clone(), sibling.clone()));
                        }
                        let pos = child_ref.position.as_ref();
                        state.has_rel.lock().unwrap().push(ChildEdge {
                            parent: canonical.clone(),
                            child: primary.clone(),
                            disc_no: pos.and_then(|p| p.disc_no),
                            track_no: pos.map(|p| p.track_no),
                            contributions: child_ref.contributions.clone(),
                        });
                        subs.push(import(
                            Arc::clone(&state),
                            Arc::clone(&providers),
                            Arc::clone(&pool),
                            primary,
                            child_fetch_opts.id,
                        ));
                    }
                    Ok(None) => break,
                    Err(e) => {
                        warn!("child cursor error: {}", error_chain(&e));
                        break;
                    }
                }
            }
        }

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

fn entry_type_of(data: &EntrySpecificData) -> EntryType {
    match data {
        EntrySpecificData::Track { .. } => EntryType::Track,
        EntrySpecificData::Release { .. } => EntryType::Release,
        EntrySpecificData::ReleaseGroup { .. } => EntryType::ReleaseGroup,
        EntrySpecificData::Artist => EntryType::Artist,
    }
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
