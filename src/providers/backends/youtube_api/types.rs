use std::{env, sync::Arc};

use async_trait::async_trait;
use http::HeaderValue;

use crate::{
    http::HttpClient,
    providers::{
        CanonicalizeProvider, FetchProvider, RawFetchProvider, TryDefault,
        backends::youtube_api::{
            canonicalize::{canonicalize, match_channel_url, match_playlist_url, match_video_url},
            channel::{
                get_channel, get_channel_playlists_raw, get_channel_raw, get_channel_uploads_raw,
            },
            client::YoutubeClient,
            playlist::{get_playlist, get_playlist_items_raw, get_playlist_raw},
            video::{get_video, get_video_raw},
        },
        matcher::filter_children,
        types::{CanonicalizeResult, EntityResult, EntryFetchOptionsPool, Error, OptionsId},
    },
};

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
        let mut client = YoutubeClient::new(api_key)?;
        if let Ok(url) = env::var("YTMUSICAPI_SERVER_URL") {
            client = client.with_ytmusicapi_url(url);
        }
        Ok(Self { client })
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
        self: Arc<Self>,
        identifier: &str,
        pool: Arc<EntryFetchOptionsPool>,
        root_id: OptionsId,
    ) -> Result<EntityResult, crate::providers::types::Error> {
        let result = if match_video_url(identifier).is_some() {
            get_video(&self.client, identifier).await?
        } else if match_playlist_url(identifier).is_some() {
            get_playlist(&self.client, identifier).await?
        } else if match_channel_url(identifier).is_some() {
            get_channel(&self.client, identifier).await?
        } else {
            return Err(Error::MissingCredentials("Invalid YouTube URL".to_string()));
        };

        Ok(EntityResult {
            children: result
                .children
                .iter()
                .map(|s| {
                    Ok(Arc::new(filter_children(
                        s.clone(),
                        pool.clone(),
                        root_id,
                        self.clone(),
                    )?))
                })
                .collect::<Result<Vec<_>, Error>>()?,
            release_date: result.release_date,
            sources: result.sources,
            extra: result.extra,
            specific_data: result.specific_data,
            aliases: result.aliases,
        })
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
            if path_key == "playlists" {
                return get_channel_playlists_raw(
                    &self.client,
                    url,
                    |value: &serde_json::Value| callback(value),
                )
                .await;
            }
            if path_key == "uploads" {
                return get_channel_uploads_raw(&self.client, url, |value: &serde_json::Value| {
                    callback(value)
                })
                .await;
            }
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
