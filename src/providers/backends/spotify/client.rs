use std::{future::Future, sync::Arc};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::sync::Mutex;

use crate::{
    http::{
        self, HeaderName, HeaderValue, HttpClient, Method, Request, default_http_client,
        json_body_extractor,
    },
    providers::{backends::build_url, types::Error},
};

pub struct SpotifyClient {
    client: Arc<dyn HttpClient>,
    credentials: Credentials,
    /// Cached access token; fetched lazily on first request.
    cached_token: Mutex<Option<HeaderValue>>,
}

enum Credentials {
    ClientCredentials {
        encoded: String,
    },
    #[cfg(test)]
    Static(HeaderValue),
}

impl Clone for SpotifyClient {
    fn clone(&self) -> Self {
        #[cfg(test)]
        if let Credentials::Static(v) = &self.credentials {
            return Self {
                client: self.client.clone(),
                credentials: Credentials::Static(v.clone()),
                cached_token: Mutex::new(Some(v.clone())),
            };
        }
        let credentials = match &self.credentials {
            Credentials::ClientCredentials { encoded } => Credentials::ClientCredentials {
                encoded: encoded.clone(),
            },
            #[cfg(test)]
            Credentials::Static(_) => unreachable!(),
        };
        Self {
            client: self.client.clone(),
            credentials,
            cached_token: Mutex::new(None),
        }
    }
}

const BASE_URL: &str = "https://api.spotify.com/v1";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";

#[derive(Debug, Serialize, Deserialize)]
struct TokenResponse {
    access_token: String,
}

/// Wraps an API response so that error bodies (e.g. 401) still deserialize
/// successfully, letting us inspect the status code before deciding what to do.
#[derive(Clone, Deserialize, Serialize)]
#[serde(untagged)]
enum ApiResponse<T> {
    Ok(T),
    Err(serde_json::Value),
}

impl<T> ApiResponse<T> {
    fn into_inner(self) -> Option<T> {
        match self {
            ApiResponse::Ok(v) => Some(v),
            ApiResponse::Err(_) => None,
        }
    }
}

impl SpotifyClient {
    pub fn new(client_id: &str, client_secret: &str) -> Self {
        Self::new_with_client(default_http_client(), client_id, client_secret)
    }

    pub fn new_with_client(
        client: Arc<dyn HttpClient>,
        client_id: &str,
        client_secret: &str,
    ) -> Self {
        let encoded = BASE64.encode(format!("{client_id}:{client_secret}"));
        Self {
            client,
            credentials: Credentials::ClientCredentials { encoded },
            cached_token: Mutex::new(None),
        }
    }

    /// Construct a client with a pre-existing token — intended for tests only.
    #[cfg(test)]
    pub fn new_with_token(client: Arc<dyn HttpClient>, token: &str) -> Self {
        let access_token =
            HeaderValue::from_str(&format!("Bearer {token}")).expect("valid token header");
        Self {
            client,
            credentials: Credentials::Static(access_token.clone()),
            cached_token: Mutex::new(Some(access_token)),
        }
    }

    async fn get_or_fetch_token(&self) -> Result<HeaderValue, Error> {
        let mut guard = self.cached_token.lock().await;
        if let Some(token) = guard.as_ref() {
            return Ok(token.clone());
        }

        let encoded = match &self.credentials {
            Credentials::ClientCredentials { encoded } => encoded,
            #[cfg(test)]
            Credentials::Static(_) => unreachable!("static token always pre-populated"),
        };

        let auth_header = HeaderValue::from_str(&format!("Basic {encoded}"))
            .map_err(|_| Error::InvalidCredentials("Invalid Spotify credentials".into()))?;

        let response = self
            .client
            .make_request(
                Request {
                    method: Method::POST,
                    url: TOKEN_URL.to_string(),
                    headers: vec![
                        (HeaderName::from_static("authorization"), auth_header),
                        (
                            HeaderName::from_static("content-type"),
                            HeaderValue::from_static("application/x-www-form-urlencoded"),
                        ),
                    ],
                    body: Some("grant_type=client_credentials".into()),
                    no_cache: true,
                    ..Default::default()
                },
                json_body_extractor::<TokenResponse>().into(),
            )
            .await
            .map_err(Error::from)?;

        let token_resp = response.json::<TokenResponse>().await.map_err(|_| {
            Error::AuthenticationFailed("Failed to parse Spotify token response".into())
        })?;

        let token = HeaderValue::from_str(&format!("Bearer {}", token_resp.access_token))
            .map_err(|_| Error::AuthenticationFailed("Invalid Spotify access token".into()))?;

        *guard = Some(token.clone());
        Ok(token)
    }

    pub fn build_url(endpoint: &str, params: &[(&str, &str)]) -> String {
        let url = format!("{BASE_URL}/{endpoint}");
        build_url(url, params)
    }

    async fn invalidate_token(&self) {
        let mut guard = self.cached_token.lock().await;
        *guard = None;
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

        for attempt in 0..2 {
            let token = self.get_or_fetch_token().await.map_err(E::from)?;

            let response = self
                .client
                .get_json::<ApiResponse<T>>(Request {
                    url: url.clone(),
                    headers: vec![(HeaderName::from_static("authorization"), token)],
                    // bypass cache on retry so a cached 401 doesn't loop forever
                    force_refetch: attempt > 0,
                    ..Default::default()
                })
                .await
                .map_err(Error::from)
                .map_err(E::from)?;

            if response.status == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                self.invalidate_token().await;
                continue;
            }

            match response.status.as_u16() {
                200..=299 => {
                    let inner = response
                        .json::<ApiResponse<T>>()
                        .await
                        .map_err(Error::from)
                        .map_err(E::from)?
                        .clone()
                        .into_inner()
                        .ok_or_else(|| {
                            E::from(Error::AuthenticationFailed(format!(
                                "Spotify API returned an error for {url}"
                            )))
                        })?;
                    return callback(&inner).await;
                }
                404 => return Err(E::from(Error::NotFound(url.clone()))),
                _ => {
                    return Err(E::from(Error::Http(http::Error::HttpStatus(
                        response.status,
                    ))));
                }
            }
        }

        Err(E::from(Error::AuthenticationFailed(
            "Spotify API: still unauthorized after token refresh".into(),
        )))
    }
}
