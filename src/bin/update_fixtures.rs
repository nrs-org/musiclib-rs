use std::{collections::HashMap, env, fs, path::PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{info, warn};

use musiclib_rs::providers::std_values::StandardProviderKeys;
use musiclib_rs::providers::{
    RawFetchProvider, TryDefault,
    backends::{discogs, musicbrainz, nicovideo, soundcloud, spotify, youtube_api},
    types::{EntryFetchOptions, Error},
};

#[derive(Debug, Deserialize)]
struct FixturesManifest {
    fixtures: Vec<FixtureBatch>,
}

#[derive(Debug, Deserialize)]
struct FixtureBatch {
    backend: BatchBackends,
    entries: Vec<FixtureEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum BatchBackends {
    Single(String),
    Multi(Vec<String>),
}

impl BatchBackends {
    fn as_slice(&self) -> &[String] {
        match self {
            BatchBackends::Single(value) => std::slice::from_ref(value),
            BatchBackends::Multi(values) => values.as_slice(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum FixturePath {
    Single(PathBuf),
    Multi(HashMap<String, PathBuf>),
}

#[derive(Debug, Deserialize, Clone)]
struct FixtureEntry {
    path: FixturePath,
    url: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();

    let mut args = env::args().skip(1);
    let manifest_path = args.next().unwrap_or_else(|| "fixtures.yaml".to_string());
    let filter = args.next();

    let manifest_contents =
        fs::read_to_string(&manifest_path).with_context(|| "read fixtures.yaml")?;
    let manifest: FixturesManifest =
        serde_yaml_ng::from_str(&manifest_contents).with_context(|| "parse fixtures.yaml")?;

    let mut by_backend: HashMap<String, Vec<FixtureEntry>> = HashMap::new();

    for batch in manifest.fixtures {
        let backends: Vec<String> = batch.backend.as_slice().to_vec();
        for fixture in &batch.entries {
            for backend in &backends {
                if let Some(filter) = &filter {
                    let path_match = match &fixture.path {
                        FixturePath::Single(path) => path.to_string_lossy().contains(filter),
                        FixturePath::Multi(paths) => paths
                            .values()
                            .any(|path| path.to_string_lossy().contains(filter)),
                    };
                    if !path_match && !fixture.url.contains(filter) && !backend.contains(filter) {
                        continue;
                    }
                }
                by_backend
                    .entry(backend.clone())
                    .or_default()
                    .push(fixture.clone());
            }
        }
    }

    for (backend, fixtures) in by_backend {
        match backend.as_str() {
            "youtube_api" => {
                update_fixtures::<youtube_api::Provider>(&fixtures, EntryFetchOptions::default())
                    .await?;
            }
            "musicbrainz" => {
                update_fixtures::<musicbrainz::types::Provider>(
                    &fixtures,
                    EntryFetchOptions::default(),
                )
                .await?;
            }
            "spotify" => {
                update_fixtures::<spotify::Provider>(&fixtures, EntryFetchOptions::default())
                    .await?;
            }
            "discogs" => {
                update_fixtures::<discogs::Provider>(&fixtures, EntryFetchOptions::default())
                    .await?;
            }
            "soundcloud" => {
                update_fixtures::<soundcloud::Provider>(&fixtures, EntryFetchOptions::default())
                    .await?;
            }
            "nicovideo" => {
                update_fixtures::<nicovideo::Provider>(&fixtures, EntryFetchOptions::default())
                    .await?;
            }
            other => {
                warn!("Unknown backend: {other}");
            }
        }
    }

    Ok(())
}

async fn promisify<T>(value: T) -> T {
    value
}

async fn update_fixtures<P>(fixtures: &[FixtureEntry], options: EntryFetchOptions) -> Result<()>
where
    P: RawFetchProvider + TryDefault<Error = Error> + Send + Sync,
{
    match P::try_default() {
        Ok(provider) => {
            for fixture in fixtures {
                let paths = match fixture.path {
                    FixturePath::Single(ref path) => {
                        HashMap::from([("metadata".into(), path.clone())])
                    }
                    FixturePath::Multi(ref paths) => paths.clone(),
                };

                let canonical = provider
                    .canonicalize(StandardProviderKeys::UNKNOWN_URL, &fixture.url)
                    .await
                    .ok_or_else(|| {
                        anyhow::anyhow!("{}: unrecognised URL: {}", P::name(), fixture.url)
                    })?;

                for (key, path) in &paths {
                    info!(
                        "{}: {} -> {} ({})",
                        P::name(),
                        fixture.url,
                        path.display(),
                        key
                    );
                    provider
                        .raw_fetch(
                            &canonical.canonical_source_key,
                            &canonical.canonical_identifier,
                            options.clone(),
                            key,
                            |value| promisify(write_json(path, value)),
                        )
                        .await?;
                }
            }

            Ok(())
        }
        Err(Error::MissingCredentials(msg)) => {
            warn!(
                "Skipping {} fixture due to missing credentials: {}",
                P::name(),
                msg
            );
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("initialize {} provider", P::name())),
    }
}

fn write_json(path: &PathBuf, value: &serde_json::Value) -> Result<()> {
    let pretty = serde_json::to_string_pretty(value)?;
    fs::write(path, pretty).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}
