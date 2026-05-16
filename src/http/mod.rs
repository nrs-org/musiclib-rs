use std::{marker::PhantomData, sync::Arc};

use async_trait::async_trait;
use serde::{Serialize, de::DeserializeOwned};

mod cache;
mod default;
mod types;

pub use types::{
    BodyExtractError, Error, HeaderName, HeaderValue, Method, Request, Response, ResponseBody,
    ResponseStatus,
};

#[async_trait]
pub trait BodyExtractor: Send + Sync {
    async fn extract(&self, res: reqwest::Response) -> Result<ResponseBody, BodyExtractError>;

    async fn extract_response(&self, res: reqwest::Response) -> Result<Response, BodyExtractError> {
        Ok(Response {
            status: res.status(),
            headers: res
                .headers()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            body: self.extract(res).await?,
        })
    }
}

pub fn bytes_body_extractor() -> impl BodyExtractor {
    struct BytesExtractor;
    #[async_trait]
    impl BodyExtractor for BytesExtractor {
        async fn extract(&self, res: reqwest::Response) -> Result<ResponseBody, BodyExtractError> {
            Ok(ResponseBody::from(res.bytes().await?.to_vec()))
        }
    }
    BytesExtractor
}

pub fn text_body_extractor() -> impl BodyExtractor {
    struct TextExtractor;
    #[async_trait]
    impl BodyExtractor for TextExtractor {
        async fn extract(&self, res: reqwest::Response) -> Result<ResponseBody, BodyExtractError> {
            Ok(ResponseBody::from(res.text().await?))
        }
    }
    TextExtractor
}

pub fn json_body_extractor<T: DeserializeOwned + Serialize + Send + Sync + 'static>()
-> impl BodyExtractor {
    struct JsonExtractor<T>(PhantomData<T>);
    #[async_trait]
    impl<T: DeserializeOwned + Serialize + Send + Sync + 'static> BodyExtractor for JsonExtractor<T> {
        async fn extract(&self, res: reqwest::Response) -> Result<ResponseBody, BodyExtractError> {
            Ok(ResponseBody::from_json(res.json::<T>().await?))
        }
    }
    JsonExtractor(PhantomData::<T>)
}

pub fn auto_body_extractor() -> impl BodyExtractor {
    struct AutoExtractor;
    #[async_trait]
    impl BodyExtractor for AutoExtractor {
        async fn extract(&self, res: reqwest::Response) -> Result<ResponseBody, BodyExtractError> {
            let content_type = res
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .map(|v| v.to_str().unwrap_or(""))
                .unwrap_or("");

            if content_type.contains("application/json") {
                json_body_extractor::<serde_json::Value>()
                    .extract(res)
                    .await
            } else if content_type.contains("text/") {
                text_body_extractor().extract(res).await
            } else {
                bytes_body_extractor().extract(res).await
            }
        }
    }
    AutoExtractor
}

#[async_trait]
pub trait HttpClient: Send + Sync {
    async fn make_request(
        &self,
        req: Request,
        body_extractor: &dyn BodyExtractor,
    ) -> Result<Arc<Response>, Error>;

    #[allow(non_snake_case)]
    fn GET(&self, req: Request) -> Request {
        Request {
            method: Method::GET,
            ..req
        }
    }

    async fn get(&self, req: Request) -> Result<Arc<Response>, Error> {
        self.make_request(self.GET(req), &auto_body_extractor())
            .await
    }

    async fn get_bytes(&self, req: Request) -> Result<Arc<Response>, Error> {
        self.make_request(self.GET(req), &bytes_body_extractor())
            .await
    }

    async fn get_text(&self, req: Request) -> Result<Arc<Response>, Error> {
        self.make_request(self.GET(req), &text_body_extractor())
            .await
    }
}

impl dyn HttpClient {
    pub async fn get_json<T: DeserializeOwned + Serialize + Send + Sync + 'static>(
        &self,
        req: Request,
    ) -> Result<Arc<Response>, Error> {
        self.make_request(req, &json_body_extractor::<T>()).await
    }
}

pub fn default_http_client() -> Arc<dyn HttpClient> {
    Arc::new(default::DefaultHttpClient::default())
}

pub fn cache_http_client() -> Arc<dyn HttpClient> {
    use crate::httpcache::MemoryHttpCache;
    use cache::IntoCachedHttpClient;
    Arc::new(default_http_client().into_cached::<MemoryHttpCache>())
}
