use std::{env, future::Future, sync::Arc};

use async_trait::async_trait;

use crate::{
    http::HttpClient,
    providers::{
        CanonicalizeProvider, FetchProvider, RawFetchProvider, TryDefault,
        backends::musicbrainz::{
            SOURCE,
            artist::{
                get_artist, get_artist_raw, get_artist_recordings_raw,
                get_artist_release_groups_raw, get_artist_releases_raw,
            },
            canonicalize::{
                canonicalize, match_artist_url, match_recording_url, match_release_group_url,
                match_release_url,
            },
            client::MusicBrainzClient,
            recording::{get_recording, get_recording_raw},
            release::{get_release, get_release_raw},
            release_group::{get_release_group, get_release_group_raw},
            url::lookup_url,
        },
        matcher::filter_children,
        types::{
            CanonicalizeResult, EntityResult, EntryFetchOptionsPool, EntryType, Error,
            ExternalSources, OptionsId,
        },
    },
};

pub const EXTERNAL_TYPE_ARTIST: &str = "musicbrainz:artist";
pub const EXTERNAL_TYPE_RELEASE_GROUP: &str = "musicbrainz:release_group";
pub const EXTERNAL_TYPE_RELEASE: &str = "musicbrainz:release";
pub const EXTERNAL_TYPE_RECORDING: &str = "musicbrainz:recording";

pub struct Provider {
    client: MusicBrainzClient,
}

impl Provider {
    pub fn new(token: Option<String>) -> Result<Self, Error> {
        Ok(Self {
            client: MusicBrainzClient::new(token)?,
        })
    }

    pub fn new_with_client(
        client: Arc<dyn HttpClient>,
        token: Option<String>,
    ) -> Result<Self, Error> {
        Ok(Self {
            client: MusicBrainzClient::new_with_client(client, token)?,
        })
    }
}

impl TryDefault for Provider {
    type Error = Error;

    fn try_default() -> Result<Self, Error> {
        let token = env::var("MUSICBRAINZ_TOKEN").ok();
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
        let result = if match_recording_url(identifier).is_some() {
            get_recording(&self.client, identifier).await?
        } else if match_release_url(identifier).is_some() {
            get_release(&self.client, identifier).await?
        } else if match_release_group_url(identifier).is_some() {
            get_release_group(&self.client, identifier).await?
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

    async fn resolve_external_source(
        &self,
        _entry_type: EntryType,
        sources: &ExternalSources,
    ) -> Result<Option<ExternalSources>, Error> {
        let mut result = ExternalSources::default();

        for (source_key, urls) in sources.0.iter() {
            // Skip MB sources — no need to look ourselves up
            if source_key.as_ref() == SOURCE {
                continue;
            }
            for url in urls {
                if let Some(found) = lookup_url(&self.client, url).await? {
                    for (k, v) in found.0 {
                        result.0.entry(k).or_default().extend(v);
                    }
                }
            }
        }

        Ok(if result.0.is_empty() {
            None
        } else {
            Some(result)
        })
    }
}

impl RawFetchProvider for Provider {
    fn name() -> &'static str {
        "musicbrainz"
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
        if match_recording_url(url).is_some() {
            return get_recording_raw(&self.client, url, |value: &serde_json::Value, _id| {
                callback(value)
            })
            .await;
        }

        if match_release_url(url).is_some() {
            return get_release_raw(&self.client, url, |value: &serde_json::Value, _id| {
                callback(value)
            })
            .await;
        }

        if match_release_group_url(url).is_some() {
            return get_release_group_raw(&self.client, url, |value: &serde_json::Value, _id| {
                callback(value)
            })
            .await;
        }

        if match_artist_url(url).is_some() {
            if path_key == "release_groups" {
                return get_artist_release_groups_raw(
                    &self.client,
                    url,
                    |value: &serde_json::Value| callback(value),
                )
                .await;
            }
            if path_key == "releases" {
                return get_artist_releases_raw(&self.client, url, |value: &serde_json::Value| {
                    callback(value)
                })
                .await;
            }
            if path_key == "recordings" {
                return get_artist_recordings_raw(
                    &self.client,
                    url,
                    |value: &serde_json::Value| callback(value),
                )
                .await;
            }
            return get_artist_raw(&self.client, url, |value: &serde_json::Value, _id| {
                callback(value)
            })
            .await;
        }

        Err(Error::InvalidUrl(url.to_string()).into())
    }
}
