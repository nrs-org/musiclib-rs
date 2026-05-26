use std::{future::Future, path::Path, pin::Pin, sync::Arc};

use dashmap::DashMap;
use futures::future::join_all;
use musiclib_rs::{
    http::HttpClientConfig,
    musicdb::MusicDb,
    providers::{
        FetchProvider,
        fetch_options_yaml::load_from_file,
        registry::{RegistryConfig, build_providers},
        types::{
            ChildFetchOptions, ChildMatcher, ChildMatcherExpr, ChildRef, ChildRule, Contribution,
            EntityResult, EntryFetchOptions, EntryFetchOptionsPool, EntrySpecificData, EntryType,
            ExternalSources, OptionsId, child_next,
        },
    },
};
use tokio::sync::watch;
use tracing::{info, warn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt::init();

    let mut url = None;
    let mut fetch_options_path: Option<String> = None;
    let mut db_path = "musiclib.db".to_string();
    let mut registry_config_path: Option<String> = None;
    let mut http_config_path: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fetch-options" => {
                fetch_options_path = Some(
                    args.next()
                        .expect("--fetch-options requires a path argument"),
                );
            }
            "--db" => {
                db_path = args.next().expect("--db requires a path argument");
            }
            "--registry-config" => {
                registry_config_path = Some(
                    args.next()
                        .expect("--registry-config requires a path argument"),
                );
            }
            "--http-config" => {
                http_config_path =
                    Some(args.next().expect("--http-config requires a path argument"));
            }
            other => {
                if url.is_none() {
                    url = Some(other.to_string());
                } else {
                    anyhow::bail!("unexpected argument: {other}");
                }
            }
        }
    }

    let url = url.expect(
        "usage: import <url> [--fetch-options <path>] [--db <path>] [--registry-config <path>] [--http-config <path>]",
    );

    let registry_config = match registry_config_path {
        Some(ref path) => {
            let text = tokio::fs::read_to_string(path).await?;
            let config: RegistryConfig = serde_yaml_ng::from_str(&text)?;
            info!("Registry config: {path}");
            config
        }
        None => RegistryConfig::default(),
    };
    let http_config = match http_config_path {
        Some(ref path) => {
            let text = tokio::fs::read_to_string(path).await?;
            let config: HttpClientConfig = serde_yaml_ng::from_str(&text)?;
            info!("HTTP client config: {path}");
            config
        }
        None => HttpClientConfig::default(),
    };
    let http = http_config.build().await?;
    let providers = build_providers(&registry_config, http)?;
    if providers.is_empty() {
        anyhow::bail!("No providers available — check your credential env vars");
    }
    info!("Loaded {} provider(s)", providers.len());

    let db = MusicDb::new(&format!("sqlite://{}?mode=rwc", db_path)).await?;

    let (pool, root_id) = match fetch_options_path {
        Some(ref path) => {
            let (pool, root_id, _hash) = load_from_file(Path::new(path)).await?;
            info!("Fetch options: {path}");
            (pool, root_id)
        }
        None => {
            let mut pool = EntryFetchOptionsPool::default();
            let root_id = pool.insert(EntryFetchOptions {
                child_rules: vec![ChildRule {
                    matcher: ChildMatcherExpr::Matcher(ChildMatcher::Always),
                    options_id: Some(EntryFetchOptionsPool::DEFAULT_ID),
                }],
            });
            info!("Fetch options: default (1 level deep)");
            (Arc::new(pool), root_id)
        }
    };

    let (background_tx, mut background_rx) = tokio::sync::mpsc::channel::<()>(1);
    let importer = Arc::new(Importer {
        providers,
        db,
        in_flight: DashMap::new(),
        background_tx,
    });
    let entry_id = import_url(
        Arc::clone(&importer),
        url.clone(),
        Arc::clone(&pool),
        root_id,
    )
    .await?;
    // Drop our strong reference so the channel closes once all background tasks finish.
    drop(importer);
    background_rx.recv().await;
    info!("Done. Root entry id = {entry_id}");
    Ok(())
}

struct Importer {
    providers: Vec<Arc<dyn FetchProvider>>,
    db: MusicDb,
    /// Deduplicates concurrent fetches for the same canonical identifier.
    /// Value is a watch channel that resolves to the DB entry id once the fetch completes.
    in_flight: DashMap<String, watch::Receiver<Option<i64>>>,
    /// Kept alive for the duration of the import; dropping it signals `drain_rx` that all
    /// background tasks have finished.
    background_tx: tokio::sync::mpsc::Sender<()>,
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Canonicalize `url` with the first matching provider, then import.
fn import_url(
    importer: Arc<Importer>,
    url: String,
    pool: Arc<EntryFetchOptionsPool>,
    options_id: OptionsId,
) -> BoxFuture<'static, anyhow::Result<i64>> {
    Box::pin(async move {
        for provider in &importer.providers {
            if let Some(canon) = provider.canonicalize(&url).await {
                return import_identifier(
                    Arc::clone(&importer),
                    canon.canonical_identifier,
                    Arc::clone(provider),
                    pool,
                    options_id,
                )
                .await;
            }
        }
        anyhow::bail!("No provider recognised URL: {url}")
    })
}

/// Fetch an entry by its canonical identifier and insert it if not already present.
/// Concurrent calls for the same identifier will block on the first caller's result
/// rather than issuing duplicate API requests.
fn import_identifier(
    importer: Arc<Importer>,
    identifier: String,
    provider: Arc<dyn FetchProvider>,
    pool: Arc<EntryFetchOptionsPool>,
    options_id: OptionsId,
) -> BoxFuture<'static, anyhow::Result<i64>> {
    Box::pin(async move {
        // Check DB first (already-completed imports from a prior run).
        if let Some(id) = importer
            .db
            .find_entry_by_source("canonical", &identifier)
            .await?
        {
            info!("cached {identifier} → id={id}");
            return Ok(id);
        }

        // Try to claim this identifier. Use `entry()` for atomic check-and-insert.
        let (tx, _rx) = watch::channel(None);
        let existing_rx = match importer.in_flight.entry(identifier.clone()) {
            dashmap::Entry::Vacant(e) => {
                e.insert(tx.subscribe());
                None
            }
            dashmap::Entry::Occupied(e) => Some(e.get().clone()),
        };

        if let Some(mut rx) = existing_rx {
            // Another task is already fetching this identifier — wait for it.
            info!("waiting {identifier}");
            rx.wait_for(|v| v.is_some()).await?;
            return Ok(rx.borrow().unwrap());
        }

        // We won the race — do the actual fetch.
        info!("fetching {identifier}");
        let result = Arc::clone(&provider)
            .fetch_entry(&identifier, Arc::clone(&pool), options_id)
            .await
            .map_err(|e| anyhow::anyhow!("fetch_entry failed for {identifier}: {e}"))?;

        let entry_id = importer
            .db
            .insert_entry(
                entry_type_of(&result.specific_data),
                result.release_date.as_deref(),
                Some(result.extra.to_string()),
                &result.specific_data,
            )
            .await?;

        merge_provider_result(
            Arc::clone(&importer),
            entry_id,
            identifier.clone(),
            result,
            pool,
            options_id,
        )
        .await?;

        info!("stored {identifier} → id={entry_id}");

        // Notify waiters and clean up.
        let _ = tx.send(Some(entry_id));
        importer.in_flight.remove(&identifier);

        Ok(entry_id)
    })
}

/// Insert one provider's result into an existing entry row, then cross-import any
/// other providers that can be reached via the newly stored source URLs.
fn merge_provider_result(
    importer: Arc<Importer>,
    entry_id: i64,
    canonical_id: String,
    result: EntityResult,
    pool: Arc<EntryFetchOptionsPool>,
    options_id: OptionsId,
) -> BoxFuture<'static, anyhow::Result<()>> {
    Box::pin(async move {
        // Pre-insert the canonical id so recursive calls see this entry as already claimed.
        // Collect all source pairs (canonical + external) and flush in one batch.
        let mut source_pairs: Vec<(&str, &str)> = vec![("canonical", &canonical_id)];
        for (src, ids) in &result.sources.0 {
            for id in ids {
                source_pairs.push((src, id));
            }
        }
        importer
            .db
            .insert_sources_batch(entry_id, source_pairs)
            .await?;

        importer
            .db
            .insert_aliases_batch(entry_id, &result.aliases)
            .await?;

        // Collect all child refs across all child sources first, then import in parallel.
        let mut child_items: Vec<(ChildRef, ChildFetchOptions)> = Vec::new();
        for child_source in &result.children {
            let mut cursor = child_source.owned_cursor();
            loop {
                let item: Option<(ChildRef, ChildFetchOptions)> =
                    match child_next(&mut cursor).await {
                        Ok(item) => item,
                        Err(e) => {
                            warn!("child cursor error: {e}");
                            break;
                        }
                    };
                let Some(item) = item else { break };
                child_items.push(item);
            }
        }

        let child_futures = child_items.iter().map(|(child_ref, child_fetch_opts)| {
            import_child(
                Arc::clone(&importer),
                child_ref.clone(),
                child_fetch_opts.clone(),
                Arc::clone(&pool),
            )
        });
        let child_results = join_all(child_futures).await;

        let mut child_edges: Vec<(i64, i64, Option<i32>, Option<i32>)> = Vec::new();
        let mut contribs: Vec<(i64, i64, Contribution)> = Vec::new();

        for ((child_ref, _), result) in child_items.iter().zip(child_results) {
            match result {
                Ok(child_id) => {
                    let pos = child_ref.position.as_ref();
                    child_edges.push((
                        entry_id,
                        child_id,
                        pos.and_then(|p| p.disc_no),
                        pos.map(|p| p.track_no),
                    ));
                    for contrib in child_ref.contributions.iter().cloned() {
                        contribs.push((entry_id, child_id, contrib));
                    }
                }
                Err(e) => warn!("could not import child: {e}"),
            }
        }

        importer.db.insert_child_edges_batch(child_edges).await?;
        importer
            .db
            .insert_contributions_batch(
                entry_id,
                contribs.into_iter().map(|(_, artist_id, c)| (artist_id, c)),
            )
            .await?;

        // Cross-import: fire-and-forget — the entry is already stored so waiters
        // don't need to block on this.
        tokio::spawn(cross_import_sources(
            Arc::clone(&importer),
            entry_id,
            result.sources,
            pool,
            options_id,
        ));

        Ok(())
    })
}

/// For each source URL, try every provider. If one can canonicalize it and the
/// canonical id is not yet stored, fetch and merge the result into `entry_id`.
fn cross_import_sources(
    importer: Arc<Importer>,
    entry_id: i64,
    sources: ExternalSources,
    pool: Arc<EntryFetchOptionsPool>,
    options_id: OptionsId,
) -> BoxFuture<'static, ()> {
    Box::pin(async move {
        let mut tasks = Vec::new();
        for identifiers in sources.0.values() {
            for url in identifiers {
                for provider in &importer.providers {
                    let Some(canon) = provider.canonicalize(url).await else {
                        continue;
                    };
                    let canonical_id = canon.canonical_identifier;

                    // Already in DB — skip.
                    if importer
                        .db
                        .find_entry_by_source("canonical", &canonical_id)
                        .await
                        .ok()
                        .flatten()
                        .is_some()
                    {
                        continue;
                    }

                    // Atomically claim this identifier; skip if another task already did.
                    let (tx, _rx) = watch::channel(None);
                    let claimed = match importer.in_flight.entry(canonical_id.clone()) {
                        dashmap::Entry::Vacant(e) => {
                            e.insert(tx.subscribe());
                            true
                        }
                        dashmap::Entry::Occupied(_) => false,
                    };
                    if !claimed {
                        continue;
                    }

                    info!("cross-importing {canonical_id} → id={entry_id}");

                    let importer = Arc::clone(&importer);
                    let pool = Arc::clone(&pool);
                    let provider = Arc::clone(provider);
                    let _bg = importer.background_tx.clone();
                    tasks.push(tokio::spawn(async move {
                        let _bg = _bg;
                        match provider
                            .fetch_entry(&canonical_id, Arc::clone(&pool), options_id)
                            .await
                        {
                            Ok(result) => {
                                if let Err(e) = merge_provider_result(
                                    Arc::clone(&importer),
                                    entry_id,
                                    canonical_id.clone(),
                                    result,
                                    pool,
                                    options_id,
                                )
                                .await
                                {
                                    warn!("cross-import merge failed for {canonical_id}: {e}");
                                }
                            }
                            Err(e) => {
                                warn!("cross-import fetch failed for {canonical_id}: {e}");
                            }
                        }
                        importer.in_flight.remove(&canonical_id);
                    }));
                }
            }
        }
        join_all(tasks).await;
    })
}

/// Resolve a `ChildRef` to a DB entry id, trying each of its source identifiers.
fn import_child(
    importer: Arc<Importer>,
    child_ref: ChildRef,
    child_fetch_opts: ChildFetchOptions,
    pool: Arc<EntryFetchOptionsPool>,
) -> BoxFuture<'static, anyhow::Result<i64>> {
    Box::pin(async move {
        for (source, identifiers) in &child_ref.sources.0 {
            for id in identifiers {
                if let Some(entry_id) = importer.db.find_entry_by_source(source, id).await? {
                    return Ok(entry_id);
                }
            }
        }

        for (source, identifiers) in &child_ref.sources.0 {
            for identifier in identifiers {
                match import_url(
                    Arc::clone(&importer),
                    identifier.clone(),
                    Arc::clone(&pool),
                    child_fetch_opts.id,
                )
                .await
                {
                    Ok(entry_id) => {
                        let pairs: Vec<(&str, &str)> = child_ref
                            .sources
                            .0
                            .iter()
                            .flat_map(|(src, ids)| {
                                ids.iter().map(move |id| (src.as_ref(), id.as_ref()))
                            })
                            .collect();
                        importer.db.insert_sources_batch(entry_id, pairs).await?;
                        return Ok(entry_id);
                    }
                    Err(e) => {
                        warn!(
                            "fetch failed for child {:?} ({source}:{identifier}): {e}",
                            child_ref.name
                        );
                    }
                }
            }
        }

        anyhow::bail!(
            "could not resolve child (name={:?}, sources={})",
            child_ref.name,
            child_ref.sources.len()
        )
    })
}

fn entry_type_of(specific_data: &EntrySpecificData) -> EntryType {
    match specific_data {
        EntrySpecificData::Track { .. } => EntryType::Track,
        EntrySpecificData::Release { .. } => EntryType::Release,
        EntrySpecificData::ReleaseGroup { .. } => EntryType::ReleaseGroup,
        EntrySpecificData::Artist => EntryType::Artist,
    }
}
