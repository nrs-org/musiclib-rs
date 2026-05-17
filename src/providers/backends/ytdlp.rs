use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    http::{HttpClient, Request, json_body_extractor},
    providers::types::Error,
};

/// Thin client for the ytdlp server.
/// Site-specific providers (soundcloud, nicovideo, …) hold one of these and
/// call `fetch` with their canonical identifier to get the raw yt-dlp JSON.
pub struct YtdlpClient {
    http: Arc<dyn HttpClient>,
    server_url: String,
}

impl YtdlpClient {
    pub fn new(http: Arc<dyn HttpClient>, server_url: String) -> Self {
        Self { http, server_url }
    }

    pub async fn fetch(&self, url: &str) -> Result<Value, Error> {
        let encoded = urlencoding::encode(url);
        let request_url = format!(
            "{}/entities/{}",
            self.server_url.trim_end_matches('/'),
            encoded
        );

        let response = self
            .http
            .make_request(
                Request {
                    url: request_url,
                    ..Default::default()
                },
                &json_body_extractor::<Value>(),
            )
            .await?;

        if !response.status.is_success() {
            return Err(Error::InvalidUrl(format!(
                "ytdlp server returned {} for {url}",
                response.status
            )));
        }

        response
            .body
            .as_json::<Value>()
            .cloned()
            .ok_or_else(|| Error::InvalidUrl("ytdlp server returned non-JSON body".into()))
    }

    pub async fn fetch_as<T: DeserializeOwned>(&self, url: &str) -> Result<T, Error> {
        let value = self.fetch(url).await?;
        serde_json::from_value(value)
            .map_err(|e| Error::InvalidUrl(format!("failed to deserialize ytdlp response: {e}")))
    }
}
