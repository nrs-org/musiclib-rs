use std::{future::Future, sync::Arc};

use serde::{Serialize, de::DeserializeOwned};

use crate::{
    http::{HeaderName, HeaderValue, HttpClient, Request, default_http_client},
    providers::{backends::build_url, types::Error},
};

pub struct DiscogsClient {
    client: Arc<dyn HttpClient>,
    user_agent: HeaderValue,
    token: Option<HeaderValue>,
}

impl Clone for DiscogsClient {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            user_agent: self.user_agent.clone(),
            token: self.token.clone(),
        }
    }
}

const DEFAULT_USER_AGENT: &str = "musiclib-rs/0.1.0 +https://github.com/nrs-org/musiclib-rs";
const BASE_URL: &str = "https://api.discogs.com";

impl DiscogsClient {
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
                HeaderValue::from_str(&format!("Discogs token={t}"))
                    .map_err(|_| Error::InvalidCredentials("Invalid Discogs token".into()))
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
        build_url(url, params)
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

        let response = self
            .client
            .get_json::<T>(Request {
                url,
                headers,
                ..Default::default()
            })
            .await
            .map_err(Error::from)?;

        let result = response.body.as_json::<T>().expect("should be T");
        callback(result).await
    }
}
