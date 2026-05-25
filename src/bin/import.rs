use std::{future::Future, path::Path, pin::Pin, sync::Arc};

use musiclib_rs::{
    musicdb::MusicDb,
    providers::{
        FetchProvider,
        fetch_options_yaml::load_from_file,
        registry::{RegistryConfig, build_providers},
        types::{
            ChildFetchOptions, ChildMatcher, ChildMatcherExpr, ChildRef, ChildRule, ChildSource,
            Contribution, EntityResult, EntryFetchOptions, EntryFetchOptionsPool,
            EntrySpecificData, EntryType, ExternalSources, OptionsId,
        },
    },
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt::init();

    let mut url = None;
    let mut fetch_options_path: Option<String> = None;
    let mut db_path = "musiclib.db".to_string();

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
            other => {
                if url.is_none() {
                    url = Some(other.to_string());
                } else {
                    anyhow::bail!("unexpected argument: {other}");
                }
            }
        }
    }

    let url = url.expect("usage: import <url> [--fetch-options <path>] [--db <path>]");

    let providers = build_providers(&RegistryConfig::default())?;
    if providers.is_empty() {
        anyhow::bail!("No providers available — check your credential env vars");
    }
    println!("Loaded {} provider(s)", providers.len());

    let db = MusicDb::new(&format!("sqlite://{}?mode=rwc", db_path)).await?;

    let (pool, root_id) = match fetch_options_path {
        Some(ref path) => {
            let (pool, root_id, _hash) = load_from_file(Path::new(path)).await?;
            println!("Fetch options: {path}");
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
            println!("Fetch options: default (1 level deep)");
            (Arc::new(pool), root_id)
        }
    };

    let importer = Arc::new(Importer { providers, db });
    let entry_id = import_url(
        Arc::clone(&importer),
        url.clone(),
        Arc::clone(&pool),
        root_id,
    )
    .await?;
    println!("Done. Root entry id = {entry_id}");
    Ok(())
}

struct Importer {
    providers: Vec<Arc<dyn FetchProvider>>,
    db: MusicDb,
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
fn import_identifier(
    importer: Arc<Importer>,
    identifier: String,
    provider: Arc<dyn FetchProvider>,
    pool: Arc<EntryFetchOptionsPool>,
    options_id: OptionsId,
) -> BoxFuture<'static, anyhow::Result<i64>> {
    Box::pin(async move {
        if let Some(id) = importer
            .db
            .find_entry_by_source("canonical", &identifier)
            .await?
        {
            println!("  cached  {identifier} → id={id}");
            return Ok(id);
        }

        println!("  fetching {identifier}");
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

        println!("  stored  {identifier} → id={entry_id}");
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

        let mut child_edges: Vec<(i64, i64, Option<i32>, Option<i32>)> = Vec::new();
        let mut contribs: Vec<(i64, i64, Contribution)> = Vec::new();

        for child_source in &result.children {
            let mut cursor = child_source.owned_cursor();
            loop {
                let item: Option<(ChildRef, ChildFetchOptions)> = match cursor.next().await {
                    Ok(item) => item,
                    Err(e) => {
                        eprintln!("  warn: child cursor error: {e}");
                        break;
                    }
                };
                let Some((child_ref, child_fetch_opts)) = item else {
                    break;
                };

                match import_child(
                    Arc::clone(&importer),
                    child_ref.clone(),
                    child_fetch_opts,
                    Arc::clone(&pool),
                )
                .await
                {
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
                    Err(e) => eprintln!("  warn: could not import child: {e}"),
                }
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

        // Cross-import: try every stored source URL against all providers and merge
        // any new results into the same entry row.
        cross_import_sources(
            Arc::clone(&importer),
            entry_id,
            result.sources,
            pool,
            options_id,
        )
        .await;

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
        for (_, identifiers) in &sources.0 {
            for url in identifiers {
                for provider in &importer.providers {
                    let Some(canon) = provider.canonicalize(url).await else {
                        continue;
                    };
                    let canonical_id = canon.canonical_identifier;

                    // Already claimed by any entry — skip.
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

                    println!("  cross-importing {canonical_id} → id={entry_id}");

                    match Arc::clone(provider)
                        .fetch_entry(&canonical_id, Arc::clone(&pool), options_id)
                        .await
                    {
                        Ok(result) => {
                            if let Err(e) = merge_provider_result(
                                Arc::clone(&importer),
                                entry_id,
                                canonical_id.clone(),
                                result,
                                Arc::clone(&pool),
                                options_id,
                            )
                            .await
                            {
                                eprintln!(
                                    "  warn: cross-import merge failed for {canonical_id}: {e}"
                                );
                            }
                        }
                        Err(e) => {
                            eprintln!("  warn: cross-import fetch failed for {canonical_id}: {e}");
                        }
                    }
                }
            }
        }
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
                        eprintln!(
                            "  warn: fetch failed for child {:?} ({source}:{identifier}): {e}",
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
