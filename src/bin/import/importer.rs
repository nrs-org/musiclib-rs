use std::sync::Arc;

use futures::future::join_all;
use musiclib_rs::providers::{
    FetchProvider,
    std_values::StandardProviderKeys,
    types::{
        EntityResult, EntryFetchOptionsPool, EntrySpecificData, EntryType, ExternalSources,
        OptionsId, child_next,
    },
};
use tracing::{info, warn};

use super::state::{ChildEdge, Pair, PairMetadata, State};

type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'static>>;

/// Fetch the entity referred to by `input`, record metadata + relations, recurse
/// into sources and children. Structured concurrency: every spawned subtask is
/// awaited before this future resolves.
///
/// `input` is the pair the caller knows about. It will be linked via `is_rel`
/// to the canonical pair returned by the URL provider, so equivalence classes
/// span both the source-emitted form (e.g. `(youtube, long_url)`) and the
/// provider's canonical form (e.g. `(youtube:video, short_url)`).
pub fn import(
    state: Arc<State>,
    providers: Arc<Vec<Arc<dyn FetchProvider>>>,
    pool: Arc<EntryFetchOptionsPool>,
    input: Pair,
    options_id: OptionsId,
) -> BoxFuture<()> {
    Box::pin(async move {
        // 1. Canonicalize the input URL via the first provider that recognises it.
        let Some((canonical, provider)) = canonicalize_first(&providers, &input.1).await else {
            warn!("no provider recognised {}:{}", input.0, input.1);
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
        info!("fetching {}:{}", canonical.0, canonical.1);
        let result = match provider
            .fetch_entry(&canonical.0, &canonical.1, pool.clone(), options_id)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!("fetch failed for {}:{}: {e}", canonical.0, canonical.1);
                return;
            }
        };

        let EntityResult {
            release_date,
            sources,
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

        let mut subs: Vec<BoxFuture<()>> = vec![];

        // 6. is_rel + recursive import for every other source returned by this
        //    fetch. Passing the full pair (not just the URL) lets the recursive
        //    call link its own canonicalisation back to what we observed.
        for src in flatten_pairs(&sources) {
            if src != canonical {
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
                        warn!("child cursor error: {e}");
                        break;
                    }
                }
            }
        }

        // 8. Await all sub-tasks. Each handles its own errors.
        join_all(subs).await;
    })
}

async fn canonicalize_first(
    providers: &[Arc<dyn FetchProvider>],
    url: &str,
) -> Option<(Pair, Arc<dyn FetchProvider>)> {
    for provider in providers {
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
            ));
        }
    }
    None
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
