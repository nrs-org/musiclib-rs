pub mod canonicalize;
mod mylist;
mod series;
mod user;
mod video;

pub use canonicalize::{match_mylist_url, match_series_url, match_user_url, match_video_url};
pub use mylist::{get_mylist, get_mylist_raw};
pub use series::{get_series, get_series_raw};
pub use user::{get_user, get_user_raw};
pub use video::{get_video, get_video_raw};

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

pub const SOURCE: &str = "nicovideo";
pub const EXTERNAL_TYPE_VIDEO: &str = "nicovideo:video";
pub const EXTERNAL_TYPE_MYLIST: &str = "nicovideo:mylist";
pub const EXTERNAL_TYPE_SERIES: &str = "nicovideo:series";
pub const EXTERNAL_TYPE_ARTIST: &str = "nicovideo:artist";

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
        let result = if external_type == EXTERNAL_TYPE_VIDEO {
            get_video(&self.client, identifier).await?
        } else if external_type == EXTERNAL_TYPE_SERIES {
            get_series(Arc::clone(&self.client), identifier).await?
        } else if external_type == EXTERNAL_TYPE_MYLIST {
            get_mylist(Arc::clone(&self.client), identifier).await?
        } else if external_type == EXTERNAL_TYPE_ARTIST {
            get_user(Arc::clone(&self.client), identifier).await?
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
                        result.specific_data.entry_type(),
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
        "nicovideo"
    }

    async fn raw_fetch<F, E, FR, R>(
        &self,
        source_key: &str,
        identifier: &str,
        _fetch_options: EntryFetchOptions,
        _path_key: &str,
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
        if external_type == EXTERNAL_TYPE_VIDEO {
            return get_video_raw::<serde_json::Value, _, _, _, _>(
                &self.client,
                identifier,
                callback,
            )
            .await;
        }
        if external_type == EXTERNAL_TYPE_SERIES {
            return get_series_raw::<serde_json::Value, _, _, _, _>(
                &self.client,
                identifier,
                callback,
            )
            .await;
        }
        if external_type == EXTERNAL_TYPE_MYLIST {
            return get_mylist_raw::<serde_json::Value, _, _, _, _>(
                &self.client,
                identifier,
                callback,
            )
            .await;
        }
        if external_type == EXTERNAL_TYPE_ARTIST {
            return get_user_raw::<serde_json::Value, _, _, _, _>(
                &self.client,
                identifier,
                callback,
            )
            .await;
        }
        Err(Error::UnsupportedSourceKey(source_key.to_string()).into())
    }
}
