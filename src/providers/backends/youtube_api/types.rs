use std::{env, sync::Arc};

use async_trait::async_trait;
use http::HeaderValue;
use serde::{Deserialize, Serialize};

use crate::{
    http::HttpClient,
    providers::{
        CanonicalizeProvider, FetchProvider, RawFetchProvider, TryDefault,
        backends::youtube_api::{
            canonicalize::{canonicalize, match_channel_url, match_playlist_url, match_video_url},
            channel::{get_channel, get_channel_raw},
            client::YoutubeClient,
            playlist::{get_playlist, get_playlist_items_raw, get_playlist_raw},
            video::{get_video, get_video_raw},
        },
        types::{CanonicalizeResult, EntityResult, Error},
    },
};

#[derive(Clone, Copy, Serialize, Deserialize)]
pub enum StringFilterMode {
    Include,
    Exclude,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct StringFilter {
    pattern: String,
    priority: i32,
    mode: StringFilterMode,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct StringFilterSet {
    filters: Vec<StringFilter>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct DurationRange {
    min: Option<u64>,
    max: Option<u64>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct VideoDiscographyFetchOptions {
    // true -> only fetch videos with category 10 (Music)
    // (this is not really reliable)
    filter_music_category: bool,

    title_filters: StringFilterSet,
    description_filters: StringFilterSet,
    duration_range: Option<DurationRange>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct PlaylistDiscographyFetchOptions {
    title_filters: StringFilterSet,
    description_filters: StringFilterSet,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct YtMusicDiscographyFetchOptions {}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct DiscographyFetchOptions {
    videos: Option<VideoDiscographyFetchOptions>,
    playlists: Option<PlaylistDiscographyFetchOptions>,
    ytmusic: Option<YtMusicDiscographyFetchOptions>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct EntryFetchOptions {
    discography: Option<DiscographyFetchOptions>,
}

pub struct Provider {
    client: YoutubeClient,
}

impl Provider {
    pub fn from_client_and_key(
        client: Arc<dyn HttpClient>,
        api_key: HeaderValue,
    ) -> Result<Self, Error> {
        Ok(Self {
            client: YoutubeClient::new_with_client_and_key(client, api_key)?,
        })
    }

    pub fn from_key(api_key: HeaderValue) -> Result<Self, Error> {
        Ok(Self {
            client: YoutubeClient::new_with_client_and_key(
                crate::http::default_http_client(),
                api_key,
            )?,
        })
    }

    pub fn new() -> Result<Self, Error> {
        let api_key = env::var("YOUTUBE_API_KEY")
            .map_err(|e| Error::MissingCredentials(format!("Missing YOUTUBE_API_KEY: {e}")))?;
        Ok(Self {
            client: YoutubeClient::new(api_key)?,
        })
    }
}

impl TryDefault for Provider {
    type Error = Error;

    fn try_default() -> Result<Self, Error>
    where
        Self: Sized,
    {
        Self::new()
    }
}

#[async_trait]
impl CanonicalizeProvider for Provider {
    async fn canonicalize(&self, url: &str) -> Option<CanonicalizeResult> {
        canonicalize(url)
    }
}

#[async_trait]
impl FetchProvider for Provider {
    async fn fetch_entry(
        &self,
        identifier: &str,
        _fetch_options: crate::providers::types::EntryFetchOptions,
    ) -> Result<EntityResult, crate::providers::types::Error> {
        if match_video_url(identifier).is_some() {
            return get_video(&self.client, identifier).await;
        }
        if match_playlist_url(identifier).is_some() {
            return get_playlist(&self.client, identifier).await;
        }
        if match_channel_url(identifier).is_some() {
            return get_channel(&self.client, identifier).await;
        }

        Err(Error::MissingCredentials("Invalid YouTube URL".to_string()))
    }
}

impl RawFetchProvider for Provider {
    fn name() -> &'static str {
        "youtube_api"
    }

    async fn raw_fetch<F, E, FR, R>(
        &self,
        url: &str,
        _fetch_options: crate::providers::types::EntryFetchOptions,
        path_key: &str,
        callback: F,
    ) -> Result<R, E>
    where
        F: FnOnce(&serde_json::Value) -> FR + Send,
        E: From<Error> + Send + 'static,
        FR: Future<Output = Result<R, E>> + Send + 'static,
        R: Send,
    {
        if match_video_url(url).is_some() {
            return get_video_raw(&self.client, url, |value: &serde_json::Value, _id| {
                callback(value)
            })
            .await;
        }

        if match_playlist_url(url).is_some() {
            if path_key == "items" {
                return get_playlist_items_raw(
                    &self.client,
                    url,
                    |value: &serde_json::Value, _id| callback(value),
                )
                .await;
            }
            if path_key == "path" || path_key == "metadata" {
                return get_playlist_raw(&self.client, url, |value: &serde_json::Value, _id| {
                    callback(value)
                })
                .await;
            }
        }

        if match_channel_url(url).is_some() {
            return get_channel_raw(
                &self.client,
                url,
                |value: &serde_json::Value, _kind, _id| callback(value),
            )
            .await;
        }

        Err(Error::InvalidUrl(url.to_string()).into())
    }
}

pub const EXTERNAL_TYPE_VIDEO: &str = "youtube:video";
pub const EXTERNAL_TYPE_PLAYLIST: &str = "youtube:playlist";
pub const EXTERNAL_TYPE_CHANNEL_ID: &str = "youtube:channel_id";
pub const EXTERNAL_TYPE_CUSTOM_CHANNEL: &str = "youtube:custom_channel";
pub const EXTERNAL_TYPE_USER_CHANNEL: &str = "youtube:user_channel";
pub const EXTERNAL_TYPE_HANDLE: &str = "youtube:handle";
