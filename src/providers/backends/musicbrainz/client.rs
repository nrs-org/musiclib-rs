use std::{future::Future, sync::Arc, time::Duration};

use serde::{Serialize, de::DeserializeOwned};

use crate::{
    http::{HeaderName, HeaderValue, HttpClient, Request, default_http_client},
    providers::{backends::build_url, types::Error},
};

pub struct MusicBrainzClient {
    client: Arc<dyn HttpClient>,
    user_agent: HeaderValue,
    token: Option<HeaderValue>,
}

impl Clone for MusicBrainzClient {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            user_agent: self.user_agent.clone(),
            token: self.token.clone(),
        }
    }
}

// Default User-Agent as required by the MusicBrainz API ToS.
const DEFAULT_USER_AGENT: &str = "musiclib-rs/0.1.0 ( https://github.com/nrs-org/musiclib-rs )";

const BASE_URL: &str = "https://musicbrainz.org/ws/2";

/// Wraps an API response so that unexpected JSON shapes still deserialize,
/// letting us inspect the status code before deciding what to do.
#[derive(Clone, serde::Deserialize, Serialize)]
#[serde(untagged)]
enum ApiResponse<T> {
    Ok(T),
    Err(serde_json::Value),
}

impl<T> ApiResponse<T> {
    fn into_ok(self) -> Option<T> {
        match self {
            ApiResponse::Ok(v) => Some(v),
            ApiResponse::Err(_) => None,
        }
    }
}

const MAX_RETRIES: u32 = 6;
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

impl MusicBrainzClient {
    pub fn new(token: Option<String>) -> Result<Self, Error> {
        Self::new_with_client(default_http_client(), token)
    }

    pub fn new_with_client(
        client: Arc<dyn HttpClient>,
        token: Option<String>,
    ) -> Result<Self, Error> {
        let user_agent = HeaderValue::from_str(DEFAULT_USER_AGENT)
            .map_err(|_| Error::InvalidCredentials("Invalid User-Agent header value".into()))?;
        let token = token
            .map(|t| {
                HeaderValue::from_str(&format!("Bearer {t}"))
                    .map_err(|_| Error::InvalidCredentials("Invalid token header value".into()))
            })
            .transpose()?;
        Ok(Self {
            client,
            user_agent,
            token,
        })
    }

    pub fn build_url(endpoint: &str, params: &[(&str, &str)]) -> String {
        let url = format!("{BASE_URL}/{endpoint}");
        // Always include fmt=json.
        let mut all_params: Vec<(&str, &str)> = vec![("fmt", "json")];
        all_params.extend_from_slice(params);
        build_url(url, &all_params)
    }

    pub async fn get<T, F, R, E, FR>(
        &self,
        endpoint: &str,
        params: &[(&str, &str)],
        callback: F,
    ) -> Result<R, E>
    where
        T: std::any::Any + Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
        F: FnOnce(&T) -> FR,
        E: From<Error> + Send + 'static,
        FR: Future<Output = Result<R, E>> + Send + 'static,
    {
        let url = Self::build_url(endpoint, params);

        let mut headers = vec![(
            HeaderName::from_static("user-agent"),
            self.user_agent.clone(),
        )];
        if let Some(token) = &self.token {
            headers.push((HeaderName::from_static("authorization"), token.clone()));
        }

        let mut backoff = INITIAL_BACKOFF;
        for attempt in 0..MAX_RETRIES {
            let response = self
                .client
                .get_json::<ApiResponse<T>>(Request {
                    url: url.clone(),
                    headers: headers.clone(),
                    ..Default::default()
                })
                .await
                .map_err(Error::from)
                .map_err(E::from)?;

            match response.status.as_u16() {
                429 | 503 => {
                    // Rate limited — back off and retry.
                    let retry_after = response
                        .headers
                        .iter()
                        .find(|(k, _)| k.as_str().eq_ignore_ascii_case("retry-after"))
                        .and_then(|(_, v)| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .map(Duration::from_secs)
                        .unwrap_or(backoff);

                    eprintln!(
                        "  [musicbrainz] rate limited (attempt {}/{MAX_RETRIES}), \
                         waiting {}s",
                        attempt + 1,
                        retry_after.as_secs()
                    );
                    tokio::time::sleep(retry_after).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                    continue;
                }
                200..=299 => {
                    let data = response
                        .json::<ApiResponse<T>>()
                        .await
                        .map_err(Error::from)?
                        .clone()
                        .into_ok()
                        .ok_or_else(|| {
                            Error::InvalidUrl(format!(
                                "unexpected response shape from MusicBrainz for {url}"
                            ))
                        })
                        .map_err(E::from)?;
                    return callback(&data).await;
                }
                status => {
                    return Err(E::from(Error::InvalidUrl(format!(
                        "MusicBrainz returned HTTP {status} for {url}"
                    ))));
                }
            }
        }

        Err(E::from(Error::InvalidUrl(format!(
            "MusicBrainz rate limit not resolved after {MAX_RETRIES} retries for {url}"
        ))))
    }
}
