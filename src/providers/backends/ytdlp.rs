use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    http::{HttpClient, Request, json_body_extractor},
    providers::types::{ChildPage, ChildRef, Error, PageFetcher, PaginatedChildSource},
};

/// Thin client for the ytdlp server.
/// Site-specific providers (soundcloud, nicovideo, …) hold one of these and
/// call `fetch` with their canonical identifier to get the raw yt-dlp JSON.
#[derive(Clone)]
pub struct YtdlpClient {
    http: Arc<dyn HttpClient>,
    server_url: String,
}

impl YtdlpClient {
    pub fn new(http: Arc<dyn HttpClient>, server_url: String) -> Self {
        Self { http, server_url }
    }

    pub async fn fetch(&self, url: &str) -> Result<Value, Error> {
        self.fetch_inner(url, "").await
    }

    /// Fetch playlist/collection metadata only, bypassing entry enumeration.
    /// The server adds `--playlist-items 0` so yt-dlp skips downloading entry info.
    pub async fn fetch_no_children(&self, url: &str) -> Result<Value, Error> {
        self.fetch_inner(url, "?no_entries").await
    }

    async fn fetch_inner(&self, url: &str, query: &str) -> Result<Value, Error> {
        let encoded = urlencoding::encode(url);
        let request_url = format!(
            "{}/entities/{}{}",
            self.server_url.trim_end_matches('/'),
            encoded,
            query,
        );

        let response = self
            .http
            .make_request(
                Request {
                    url: request_url,
                    ..Default::default()
                },
                json_body_extractor::<Value>().into(),
            )
            .await?;

        if !response.status.is_success() {
            let status = response.status;
            if status.as_u16() == 404 {
                return Err(Error::NotFound(format!("ytdlp: {url}")));
            }
            return Err(Error::InvalidUrl(format!(
                "ytdlp server returned {status} for {url}"
            )));
        }

        response
            .json::<Value>()
            .await
            .map(|json| json.clone())
            .map_err(|_| Error::InvalidUrl("ytdlp server returned non-JSON body".into()))
    }

    pub async fn fetch_as<T: DeserializeOwned>(&self, url: &str) -> Result<T, Error> {
        let value = self.fetch(url).await?;
        serde_json::from_value(value)
            .map_err(|e| Error::InvalidUrl(format!("failed to deserialize ytdlp response: {e}")))
    }

    /// Returns a `PaginatedChildSource` that, when first polled, performs a full
    /// fetch of `url` and passes the raw JSON to `convert` to produce `ChildRef`s.
    /// No network request is made until the source is actually iterated.
    pub fn lazy_children<F>(
        self: Arc<Self>,
        url: impl Into<String>,
        convert: F,
    ) -> PaginatedChildSource
    where
        F: FnOnce(Value) -> Result<Vec<ChildRef>, Error> + Send + 'static,
    {
        PaginatedChildSource::new(Box::new(YtdlpChildPageFetcher {
            client: self,
            url: url.into(),
            convert: Some(convert),
        }))
    }
}

struct YtdlpChildPageFetcher<F> {
    client: Arc<YtdlpClient>,
    url: String,
    convert: Option<F>,
}

#[async_trait::async_trait]
impl<F: FnOnce(Value) -> Result<Vec<ChildRef>, Error> + Send> PageFetcher
    for YtdlpChildPageFetcher<F>
{
    async fn fetch_page(&mut self, _page_token: Option<&str>) -> Result<ChildPage, Error> {
        let Some(convert) = self.convert.take() else {
            return Ok(ChildPage {
                children: vec![],
                next_page_token: None,
            });
        };
        let value = self.client.fetch(&self.url).await?;
        let children = convert(value)?;
        Ok(ChildPage {
            children,
            next_page_token: None,
        })
    }
}
