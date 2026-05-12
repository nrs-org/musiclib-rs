use std::{any::Any, sync::Arc};

use http::{HeaderName, HeaderValue};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    http::{HttpClient, Request, Response, default_http_client},
    providers::{backends::build_url, types::Error},
};

pub struct YoutubeClient {
    client: Arc<dyn HttpClient>,
    api_key: HeaderValue,
}

impl YoutubeClient {
    pub const BASE_URL: &str = "https://www.googleapis.com/youtube/v3";

    pub fn new(api_key: String) -> Result<Self, Error> {
        Self::new_with_client(default_http_client(), api_key)
    }

    pub fn new_with_client(client: Arc<dyn HttpClient>, api_key: String) -> Result<Self, Error> {
        let mut api_key = HeaderValue::from_str(&api_key)
            .map_err(|e| Error::InvalidCredentials(format!("Invalid API key: {e}")))?;
        api_key.set_sensitive(true);
        Ok(Self { client, api_key })
    }

    pub fn build_url(endpoint: &str, params: &[(&str, &str)]) -> String {
        let url = format!("{}/{}", Self::BASE_URL, endpoint);
        build_url(url, params)
    }

    pub async fn get<T, F, R, FR>(
        &self,
        endpoint: &str,
        params: &[(&str, &str)],
        callback: F,
    ) -> Result<R, Error>
    where
        T: Any + Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
        F: FnOnce(&T) -> FR,
        FR: Future<Output = Result<R, Error>> + Send + 'static,
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
            .await?;

        let result = response.body.as_json::<T>().expect("should be T");
        callback(result).await
    }
}
