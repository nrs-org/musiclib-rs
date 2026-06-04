mod artist;
pub mod canonicalize;
mod playlist;
mod track;

pub use artist::{get_artist, get_artist_albums_raw, get_artist_raw, get_artist_sets_raw};
pub use canonicalize::{match_artist_url, match_playlist_url, match_track_url};
pub use playlist::{get_playlist, get_playlist_raw};
pub use track::{get_track, get_track_raw};

use std::{env, future::Future, sync::Arc};

use async_trait::async_trait;

use crate::{
    http::{HttpClient, default_http_client},
    providers::{
        CanonicalizeProvider, FetchProvider, RawFetchProvider, TryDefault,
        backends::ytdlp::YtdlpClient,
        matcher::filter_children,
        types::{
            CanonicalizeResult, EntityResult, EntryFetchOptions, EntryFetchOptionsPool, Error,
            OptionsId,
        },
    },
};

pub const SOURCE: &str = "soundcloud";
pub const EXTERNAL_TYPE_TRACK: &str = "soundcloud:track";
pub const EXTERNAL_TYPE_PLAYLIST: &str = "soundcloud:playlist";
pub const EXTERNAL_TYPE_ARTIST: &str = "soundcloud:artist";

pub struct Provider {
    client: Arc<YtdlpClient>,
}

impl Provider {
    pub fn new(http: Arc<dyn HttpClient>, server_url: String) -> Self {
        Self {
            client: Arc::new(YtdlpClient::new(http, server_url)),
        }
    }
}

impl TryDefault for Provider {
    type Error = Error;

    fn try_default() -> Result<Self, Error> {
        let server_url = env::var("YTDLP_SERVER_URL")
            .map_err(|_| Error::MissingCredentials("YTDLP_SERVER_URL".into()))?;
        Ok(Self::new(default_http_client(), server_url))
    }
}

#[async_trait]
impl CanonicalizeProvider for Provider {
    async fn canonicalize(&self, source_key: &str, identifier: &str) -> Option<CanonicalizeResult> {
        canonicalize::Canonicalizer
            .canonicalize(source_key, identifier)
            .await
    }
}

#[async_trait]
impl FetchProvider for Provider {
    async fn fetch_entry(
        self: Arc<Self>,
        source_key: &str,
        identifier: &str,
        pool: Arc<EntryFetchOptionsPool>,
        root_id: OptionsId,
    ) -> Result<EntityResult, Error> {
        let external_type = canonicalize::canonicalize(
            crate::providers::std_values::StandardProviderKeys::UNKNOWN_URL,
            identifier,
        )
        .map(|c| c.external_type)
        .ok_or_else(|| Error::UnsupportedSourceKey(source_key.to_string()))?;
        let external_type: &str = &external_type;
        let result = if external_type == EXTERNAL_TYPE_TRACK {
            get_track(&self.client, identifier).await?
        } else if external_type == EXTERNAL_TYPE_PLAYLIST {
            get_playlist(Arc::clone(&self.client), identifier).await?
        } else if external_type == EXTERNAL_TYPE_ARTIST {
            get_artist(Arc::clone(&self.client), identifier).await?
        } else {
            return Err(Error::UnsupportedSourceKey(external_type.to_string()));
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
        "soundcloud"
    }

    async fn raw_fetch<F, E, FR, R>(
        &self,
        source_key: &str,
        identifier: &str,
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
        let external_type = canonicalize::canonicalize(
            crate::providers::std_values::StandardProviderKeys::UNKNOWN_URL,
            identifier,
        )
        .map(|c| c.external_type)
        .ok_or_else(|| E::from(Error::UnsupportedSourceKey(source_key.to_string())))?;
        let external_type: &str = &external_type;
        if external_type == EXTERNAL_TYPE_TRACK {
            return get_track_raw::<serde_json::Value, _, _, _, _>(
                &self.client,
                identifier,
                callback,
            )
            .await;
        }
        if external_type == EXTERNAL_TYPE_PLAYLIST {
            return get_playlist_raw::<serde_json::Value, _, _, _, _>(
                &self.client,
                identifier,
                callback,
            )
            .await;
        }
        if external_type == EXTERNAL_TYPE_ARTIST {
            if path_key == "sets" {
                return get_artist_sets_raw::<serde_json::Value, _, _, _, _>(
                    &self.client,
                    identifier,
                    callback,
                )
                .await;
            }
            if path_key == "albums" {
                return get_artist_albums_raw::<serde_json::Value, _, _, _, _>(
                    &self.client,
                    identifier,
                    callback,
                )
                .await;
            }
            return get_artist_raw::<serde_json::Value, _, _, _, _>(
                &self.client,
                identifier,
                callback,
            )
            .await;
        }
        Err(Error::UnsupportedSourceKey(external_type.to_string()).into())
    }
}
