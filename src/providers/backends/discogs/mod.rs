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
            canonicalize::{
                canonicalize, match_artist_url, match_master_url, match_release_url,
                match_track_url,
            },
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
        } else if match_release_url(identifier).is_some() {
            get_release(&self.client, identifier).await?
        } else if match_master_url(identifier).is_some() {
            get_master(&self.client, identifier).await?
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
        "discogs"
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
            // A track pseudo-URL is backed by the parent release endpoint.
            let release_url = {
                let (release_id, _) = match_track_url(url).unwrap();
                format!("https://www.discogs.com/release/{release_id}")
            };
            return get_release_raw(&self.client, &release_url, |v: &serde_json::Value, _id| {
                callback(v)
            })
            .await;
        }
        if match_release_url(url).is_some() {
            return get_release_raw(&self.client, url, |v: &serde_json::Value, _id| callback(v))
                .await;
        }
        if match_master_url(url).is_some() {
            return get_master_raw(&self.client, url, |v: &serde_json::Value, _id| callback(v))
                .await;
        }
        if match_artist_url(url).is_some() {
            if path_key == "releases" {
                return get_artist_releases_raw(&self.client, url, |v: &serde_json::Value| {
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
