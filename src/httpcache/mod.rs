use crate::http::BodyExtractError;
use std::{borrow::Cow, sync::Arc};

use async_trait::async_trait;

use crate::http::{BodyExtractorCow, Method, Request, Response};

pub use db::DbHttpCache;
pub use memory::MemoryHttpCache;
pub use policy::{CachePolicy, ResponseCachePolicy, StatusCacheRule, StatusMatcher, TtlPolicy};

pub mod db;
mod memory;
pub mod policy;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Database error: {0}")]
    Database(#[from] db::Error),
    #[error("Error extracting response body: {0}")]
    BodyExtract(Arc<BodyExtractError>),
    #[error("Error reading bytes from response body: {0}")]
    BodyRead(#[from] Box<crate::http::Error>),
    #[error("Reserialization error: {0}")]
    Reserialization(#[from] erased_serde::Error),
}

impl From<BodyExtractError> for Error {
    fn from(e: BodyExtractError) -> Self {
        Error::BodyExtract(Arc::new(e))
    }
}

#[async_trait]
pub trait HttpCache: Send + Sync {
    async fn set(
        &self,
        key: String,
        method: &Method,
        policy: Option<&CachePolicy>,
        value: Arc<Response>,
    ) -> Result<(), Error>;
    async fn get(
        &self,
        key: &str,
        extractor: BodyExtractorCow<'static>,
    ) -> Result<Option<Arc<Response>>, Error>;

    fn default_cache_key(&self, method: &Method, url: &str) -> String {
        format!("{}:{}", method, url)
    }

    async fn get_req(
        &self,
        req: &Request,
        extractor: BodyExtractorCow<'static>,
    ) -> Result<Option<Arc<Response>>, Error> {
        let key = req
            .cache_key
            .as_ref()
            .map(|k| Cow::Borrowed(k.as_str()))
            .unwrap_or_else(|| Cow::Owned(self.default_cache_key(&req.method, &req.url)));

        self.get(key.as_ref(), extractor).await
    }

    async fn set_req(
        &self,
        req: &Request,
        policy: Option<&CachePolicy>,
        res: Arc<Response>,
    ) -> Result<(), Error> {
        let key = req
            .cache_key
            .as_ref()
            .map(|k| Cow::Borrowed(k.as_str()))
            .unwrap_or_else(|| Cow::Owned(self.default_cache_key(&req.method, &req.url)));

        self.set(key.into_owned(), &req.method, policy, res).await
    }
}
