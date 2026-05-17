use std::{env, future::Future, sync::Arc};

use async_trait::async_trait;

use crate::{
    http::{HttpClient, Request, default_http_client, json_body_extractor},
    providers::{
        CanonicalizeProvider, RawFetchProvider, TryDefault,
        backends::youtube_api::match_channel_url,
        types::{CanonicalizeResult, EntryFetchOptions, EntryType, Error},
    },
};

pub struct Provider {
    http: Arc<dyn HttpClient>,
    server_url: String,
}

impl TryDefault for Provider {
    type Error = Error;

    fn try_default() -> Result<Self, Error> {
        let server_url = env::var("YTMUSICAPI_SERVER_URL")
            .map_err(|_| Error::MissingCredentials("YTMUSICAPI_SERVER_URL".into()))?;
        Ok(Self {
            http: default_http_client(),
            server_url,
        })
    }
}

#[async_trait]
impl CanonicalizeProvider for Provider {
    async fn canonicalize(&self, url: &str) -> Option<CanonicalizeResult> {
        // Accept the same channel URLs as the youtube_api provider.
        match_channel_url(url)?;
        Some(CanonicalizeResult {
            canonical_identifier: url.to_string(),
            entry_type: EntryType::Artist,
            external_type: "youtube:channel".into(),
        })
    }
}

impl Provider {
    async fn fetch_json(&self, url: &str) -> Result<serde_json::Value, Error> {
        let response = self
            .http
            .make_request(
                Request {
                    url: url.to_string(),
                    ..Default::default()
                },
                &json_body_extractor::<serde_json::Value>(),
            )
            .await?;
        if !response.status.is_success() {
            return Err(Error::InvalidUrl(format!(
                "ytmusicapi server returned {} for {url}",
                response.status
            )));
        }
        response
            .body
            .as_json::<serde_json::Value>()
            .cloned()
            .ok_or_else(|| Error::InvalidUrl("ytmusicapi server returned non-JSON body".into()))
    }
}

impl RawFetchProvider for Provider {
    fn name() -> &'static str {
        "ytmusicapi"
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
        let channel_match =
            match_channel_url(url).ok_or_else(|| Error::InvalidUrl(url.to_string()))?;
        let channel_id = channel_match.id;
        let base = self.server_url.trim_end_matches('/');

        let request_url = match path_key {
            "discography" => format!(
                "{base}/artists/{}/discography",
                urlencoding::encode(channel_id)
            ),
            _ => format!("{base}/artists/{}", urlencoding::encode(channel_id)),
        };

        let value = self.fetch_json(&request_url).await.map_err(Into::into)?;
        callback(&value).await
    }
}
