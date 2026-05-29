pub mod artist;
pub mod canonicalize;
pub mod client;
pub mod master;
pub mod release;
pub mod track;
pub mod types;

pub const SOURCE: &str = "discogs";

use std::{env, future::Future, sync::Arc};

use async_trait::async_trait;

use crate::{
    http::HttpClient,
    providers::{
        CanonicalizeProvider, FetchProvider, RawFetchProvider, TryDefault,
        backends::discogs::{
            artist::{get_artist, get_artist_raw, get_artist_releases_raw},
            canonicalize::{canonicalize, match_track_url},
            client::DiscogsClient,
            master::{get_master, get_master_raw},
            release::{get_release, get_release_raw},
            track::get_track,
        },
        matcher::filter_children,
        types::{
            CanonicalizeResult, EntityResult, EntryFetchOptions, EntryFetchOptionsPool, Error,
            OptionsId,
        },
    },
};

pub struct Provider {
    client: DiscogsClient,
}

impl Provider {
    pub fn new(token: Option<String>) -> Result<Self, Error> {
        Ok(Self {
            client: DiscogsClient::new(token)?,
        })
    }

    pub fn new_with_client(
        http_client: Arc<dyn HttpClient>,
        token: Option<String>,
    ) -> Result<Self, Error> {
        Ok(Self {
            client: DiscogsClient::new_with_client(http_client, token)?,
        })
    }
}

impl TryDefault for Provider {
    type Error = Error;

    fn try_default() -> Result<Self, Error> {
        let token = env::var("DISCOGS_USER_TOKEN").ok();
        Self::new(token)
    }
}

#[async_trait]
impl CanonicalizeProvider for Provider {
    async fn canonicalize(&self, source_key: &str, identifier: &str) -> Option<CanonicalizeResult> {
        canonicalize(source_key, identifier)
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
        use crate::providers::backends::discogs::types::{
            EXTERNAL_TYPE_ARTIST, EXTERNAL_TYPE_MASTER, EXTERNAL_TYPE_RELEASE, EXTERNAL_TYPE_TRACK,
        };
        let external_type = canonicalize(
            crate::providers::std_values::StandardProviderKeys::UNKNOWN_URL,
            identifier,
        )
        .map(|c| c.external_type)
        .ok_or_else(|| Error::UnsupportedSourceKey(source_key.to_string()))?;
        let external_type: &str = &external_type;
        let result = if external_type == EXTERNAL_TYPE_TRACK {
            get_track(&self.client, identifier).await?
        } else if external_type == EXTERNAL_TYPE_RELEASE {
            get_release(&self.client, identifier).await?
        } else if external_type == EXTERNAL_TYPE_MASTER {
            get_master(&self.client, identifier).await?
        } else if external_type == EXTERNAL_TYPE_ARTIST {
            get_artist(&self.client, identifier).await?
        } else {
            return Err(Error::UnsupportedSourceKey(source_key.to_string()));
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
        "discogs"
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
        use crate::providers::backends::discogs::types::{
            EXTERNAL_TYPE_ARTIST, EXTERNAL_TYPE_MASTER, EXTERNAL_TYPE_RELEASE, EXTERNAL_TYPE_TRACK,
        };
        let external_type = canonicalize(
            crate::providers::std_values::StandardProviderKeys::UNKNOWN_URL,
            identifier,
        )
        .map(|c| c.external_type)
        .ok_or_else(|| E::from(Error::UnsupportedSourceKey(source_key.to_string())))?;
        let external_type: &str = &external_type;
        if external_type == EXTERNAL_TYPE_TRACK {
            // A track pseudo-URL is backed by the parent release endpoint.
            let (release_id, _) = match_track_url(identifier)
                .ok_or_else(|| Error::InvalidUrl(identifier.to_string()))?;
            let release_url = format!("https://www.discogs.com/release/{release_id}");
            return get_release_raw(&self.client, &release_url, |v: &serde_json::Value, _id| {
                callback(v)
            })
            .await;
        }
        if external_type == EXTERNAL_TYPE_RELEASE {
            return get_release_raw(&self.client, identifier, |v: &serde_json::Value, _id| {
                callback(v)
            })
            .await;
        }
        if external_type == EXTERNAL_TYPE_MASTER {
            return get_master_raw(&self.client, identifier, |v: &serde_json::Value, _id| {
                callback(v)
            })
            .await;
        }
        if external_type == EXTERNAL_TYPE_ARTIST {
            if path_key == "releases" {
                return get_artist_releases_raw(
                    &self.client,
                    identifier,
                    |v: &serde_json::Value| callback(v),
                )
                .await;
            }
            return get_artist_raw(&self.client, identifier, |v: &serde_json::Value, _id| {
                callback(v)
            })
            .await;
        }
        Err(Error::UnsupportedSourceKey(source_key.to_string()).into())
    }
}
