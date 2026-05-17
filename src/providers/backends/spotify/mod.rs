pub mod album;
pub mod artist;
pub mod canonicalize;
pub mod client;
pub mod playlist;
pub mod track;
pub mod types;

pub const SOURCE: &str = "spotify";

use std::{env, future::Future, sync::Arc};

use async_trait::async_trait;

use crate::{
    http::HttpClient,
    providers::{
        CanonicalizeProvider, FetchProvider, RawFetchProvider, TryDefault,
        backends::spotify::{
            album::{get_album, get_album_raw},
            artist::{get_artist, get_artist_albums_raw, get_artist_raw},
            canonicalize::{
                canonicalize, match_album_url, match_artist_url, match_playlist_url,
                match_track_url,
            },
            client::SpotifyClient,
            playlist::{get_playlist, get_playlist_raw},
            track::{get_track, get_track_raw},
        },
        matcher::filter_children,
        types::{
            CanonicalizeResult, EntityResult, EntryFetchOptions, EntryFetchOptionsPool, Error,
            OptionsId,
        },
    },
};

pub struct Provider {
    client: SpotifyClient,
}

impl Provider {
    pub fn new(client_id: &str, client_secret: &str) -> Self {
        Self {
            client: SpotifyClient::new(client_id, client_secret),
        }
    }

    pub fn new_with_http_client(
        http_client: Arc<dyn HttpClient>,
        client_id: &str,
        client_secret: &str,
    ) -> Self {
        Self {
            client: SpotifyClient::new_with_client(http_client, client_id, client_secret),
        }
    }
}

impl TryDefault for Provider {
    type Error = Error;

    fn try_default() -> Result<Self, Error> {
        let client_id = env::var("SPOTIFY_CLIENT_ID")
            .map_err(|_| Error::MissingCredentials("SPOTIFY_CLIENT_ID".into()))?;
        let client_secret = env::var("SPOTIFY_CLIENT_SECRET")
            .map_err(|_| Error::MissingCredentials("SPOTIFY_CLIENT_SECRET".into()))?;
        Ok(Self::new(&client_id, &client_secret))
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
    ) -> Result<EntityResult, Error> {
        let result = if match_track_url(identifier).is_some() {
            get_track(&self.client, identifier).await?
        } else if match_album_url(identifier).is_some() {
            get_album(&self.client, identifier).await?
        } else if match_playlist_url(identifier).is_some() {
            get_playlist(&self.client, identifier).await?
        } else if match_artist_url(identifier).is_some() {
            get_artist(&self.client, identifier).await?
        } else {
            return Err(Error::InvalidUrl(identifier.to_string()));
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
        "spotify"
    }

    async fn raw_fetch<F, E, FR, R>(
        &self,
        url: &str,
        _fetch_options: EntryFetchOptions,
        path_key: &str,
        callback: F,
    ) -> Result<R, E>
    where
        F: FnOnce(&serde_json::Value) -> FR + Send,
        E: From<Error> + Send + 'static,
        FR: Future<Output = Result<R, E>> + Send + 'static,
        R: Send,
    {
        if match_track_url(url).is_some() {
            return get_track_raw(&self.client, url, |v: &serde_json::Value, _id| callback(v))
                .await;
        }
        if match_album_url(url).is_some() {
            return get_album_raw(&self.client, url, |v: &serde_json::Value, _id| callback(v))
                .await;
        }
        if match_playlist_url(url).is_some() {
            return get_playlist_raw(&self.client, url, |v: &serde_json::Value, _id| callback(v))
                .await;
        }
        if match_artist_url(url).is_some() {
            if path_key == "albums" {
                return get_artist_albums_raw(&self.client, url, |v: &serde_json::Value| {
                    callback(v)
                })
                .await;
            }
            return get_artist_raw(&self.client, url, |v: &serde_json::Value, _id| callback(v))
                .await;
        }
        Err(Error::InvalidUrl(url.to_string()).into())
    }
}
