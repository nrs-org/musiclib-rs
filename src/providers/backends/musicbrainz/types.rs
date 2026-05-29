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
            canonicalize::canonicalize,
            client::MusicBrainzClient,
            isrc::lookup_isrc,
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
        let external_type = canonicalize(
            crate::providers::std_values::StandardProviderKeys::UNKNOWN_URL,
            identifier,
        )
        .map(|c| c.external_type)
        .ok_or_else(|| Error::UnsupportedSourceKey(source_key.to_string()))?;
        let external_type: &str = &external_type;
        let result = if external_type == EXTERNAL_TYPE_RECORDING {
            get_recording(&self.client, identifier).await?
        } else if external_type == EXTERNAL_TYPE_RELEASE {
            get_release(&self.client, identifier).await?
        } else if external_type == EXTERNAL_TYPE_RELEASE_GROUP {
            get_release_group(&self.client, identifier).await?
        } else if external_type == EXTERNAL_TYPE_ARTIST {
            get_artist(&self.client, identifier).await?
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

    async fn resolve_external_source(
        &self,
        _entry_type: EntryType,
        sources: &ExternalSources,
    ) -> Result<Option<ExternalSources>, Error> {
        let mut result = ExternalSources::default();

        for (source_key, ids) in sources.0.iter() {
            // Skip MB sources — no need to look ourselves up
            if source_key.as_ref() == SOURCE {
                continue;
            }
            for id in ids {
                let found = if source_key.as_ref() == "isrc" {
                    lookup_isrc(&self.client, id).await?
                } else {
                    lookup_url(&self.client, id).await?
                };
                if let Some(found) = found {
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
        source_key: &str,
        identifier: &str,
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
        let external_type = canonicalize(
            crate::providers::std_values::StandardProviderKeys::UNKNOWN_URL,
            identifier,
        )
        .map(|c| c.external_type)
        .ok_or_else(|| E::from(Error::UnsupportedSourceKey(source_key.to_string())))?;
        let external_type: &str = &external_type;
        if external_type == EXTERNAL_TYPE_RECORDING {
            return get_recording_raw(
                &self.client,
                identifier,
                |value: &serde_json::Value, _id| callback(value),
            )
            .await;
        }

        if external_type == EXTERNAL_TYPE_RELEASE {
            return get_release_raw(
                &self.client,
                identifier,
                |value: &serde_json::Value, _id| callback(value),
            )
            .await;
        }

        if external_type == EXTERNAL_TYPE_RELEASE_GROUP {
            return get_release_group_raw(
                &self.client,
                identifier,
                |value: &serde_json::Value, _id| callback(value),
            )
            .await;
        }

        if external_type == EXTERNAL_TYPE_ARTIST {
            if path_key == "release_groups" {
                return get_artist_release_groups_raw(
                    &self.client,
                    identifier,
                    |value: &serde_json::Value| callback(value),
                )
                .await;
            }
            if path_key == "releases" {
                return get_artist_releases_raw(
                    &self.client,
                    identifier,
                    |value: &serde_json::Value| callback(value),
                )
                .await;
            }
            if path_key == "recordings" {
                return get_artist_recordings_raw(
                    &self.client,
                    identifier,
                    |value: &serde_json::Value| callback(value),
                )
                .await;
            }
            return get_artist_raw(
                &self.client,
                identifier,
                |value: &serde_json::Value, _id| callback(value),
            )
            .await;
        }

        Err(Error::UnsupportedSourceKey(external_type.to_string()).into())
    }
}
