use std::{any::Any, sync::Arc};

use http::{HeaderName, HeaderValue};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    http::{self, HttpClient, Request, default_http_client},
    providers::{backends::build_url, types::Error},
};

#[derive(Clone)]
pub struct YoutubeClient {
    pub(crate) client: Arc<dyn HttpClient>,
    api_key: HeaderValue,
    /// Base URL of the optional ytmusicapi server (e.g. `http://localhost:9001`).
    /// When present, `get_channel` will add a third child source with YTMusic discography.
    pub ytmusicapi_url: Option<String>,
}

impl YoutubeClient {
    pub const BASE_URL: &str = "https://www.googleapis.com/youtube/v3";

    pub fn new(api_key: String) -> Result<Self, Error> {
        Self::new_with_client(default_http_client(), api_key)
    }

    pub fn new_with_client(client: Arc<dyn HttpClient>, api_key: String) -> Result<Self, Error> {
        let api_key = HeaderValue::from_str(&api_key)
            .map_err(|e| Error::InvalidCredentials(format!("Invalid API key: {e}")))?;
        Self::new_with_client_and_key(client, api_key)
    }

    pub fn new_with_client_and_key(
        client: Arc<dyn HttpClient>,
        api_key: HeaderValue,
    ) -> Result<Self, Error> {
        let mut api_key = api_key;
        api_key.set_sensitive(true);
        Ok(Self {
            client,
            api_key,
            ytmusicapi_url: None,
        })
    }

    pub fn with_ytmusicapi_url(mut self, url: String) -> Self {
        self.ytmusicapi_url = Some(url);
        self
    }

    pub fn build_url(endpoint: &str, params: &[(&str, &str)]) -> String {
        let url = format!("{}/{}", Self::BASE_URL, endpoint);
        build_url(url, params)
    }

    pub async fn get<T, F, R, E, FR>(
        &self,
        endpoint: &str,
        params: &[(&str, &str)],
        callback: F,
    ) -> Result<R, E>
    where
        T: Any + Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
        F: FnOnce(&T) -> FR,
        E: From<Error> + Send + 'static,
        FR: Future<Output = Result<R, E>> + Send + 'static,
    {
        let url = Self::build_url(endpoint, params);

        let response = self
            .client
            .get_json::<T>(Request {
                url,
                headers: vec![(
                    HeaderName::from_static("x-goog-api-key"),
                    self.api_key.clone(),
                )],
                ..Default::default()
            })
            .await
            .map_err(Error::from)?;

        match response.status.as_u16() {
            200..=299 => {
                let result = response.json::<T>().await.map_err(Error::from)?;
                callback(&result).await
            }
            404 => Err(Error::NotFound("YouTube video/playlist/channel not found".into()).into()),
            _ => Err(Error::Http(http::Error::HttpStatus(response.status)).into()),
        }
    }
}
