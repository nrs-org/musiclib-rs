use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use musiclib_rs::providers::{
    backends::youtube_api::{
        YoutubeClient, get_channel_raw, get_playlist_items_raw, get_playlist_raw, get_video_raw,
        match_channel_url, match_playlist_url, match_video_url,
    },
    types::EntryFetchOptions,
};

#[derive(Debug, Deserialize)]
struct FixturesManifest {
    fixtures: Vec<FixtureEntry>,
}

#[derive(Debug, Deserialize)]
struct FixtureEntry {
    backend: String,
    path: PathBuf,
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

    for fixture in manifest.fixtures {
        if let Some(filter) = &filter
            && !fixture.path.to_string_lossy().contains(filter)
            && !fixture.url.contains(filter)
            && !fixture.backend.contains(filter)
        {
            continue;
        }

        match fixture.backend.as_str() {
            "youtube_api" => {
                update_youtube_fixture(&fixture, EntryFetchOptions::default()).await?;
            }
            other => {
                eprintln!("Unknown backend: {other}");
            }
        }
    }

    Ok(())
}

async fn promisify<T>(value: T) -> T {
    value
}

async fn update_youtube_fixture(fixture: &FixtureEntry, _options: EntryFetchOptions) -> Result<()> {
    let api_key = env::var("YOUTUBE_API_KEY").with_context(|| "missing YOUTUBE_API_KEY in env")?;
    let client = YoutubeClient::new(api_key)?;

    println!("youtube_api: {} -> {}", fixture.url, fixture.path.display());

    if match_video_url(&fixture.url).is_some() {
        get_video_raw(&client, &fixture.url, |value: &serde_json::Value, _id| {
            promisify(write_json(&fixture.path, value))
        })
        .await?;
        return Ok(());
    }

    if match_playlist_url(&fixture.url).is_some() {
        if fixture
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.contains("_items_"))
            .unwrap_or(false)
        {
            get_playlist_items_raw(&client, &fixture.url, |value: &serde_json::Value, _id| {
                promisify(write_json(&fixture.path, value))
            })
            .await?;
            return Ok(());
        }

        get_playlist_raw(&client, &fixture.url, |value: &serde_json::Value, _id| {
            let result = write_json(&fixture.path, value);
            async { result }
        })
        .await?;
        return Ok(());
    }

    if match_channel_url(&fixture.url).is_some() {
        get_channel_raw(
            &client,
            &fixture.url,
            |value: &serde_json::Value, _kind, _id| promisify(write_json(&fixture.path, value)),
        )
        .await?;
        return Ok(());
    }

    anyhow::bail!("Unrecognized YouTube URL: {}", fixture.url);
}

fn write_json(path: &PathBuf, value: &serde_json::Value) -> Result<()> {
    let pretty = serde_json::to_string_pretty(value)?;
    fs::write(path, pretty).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}
