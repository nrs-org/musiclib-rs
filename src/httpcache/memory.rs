// Memory-backed HTTP cache

use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;

use crate::http::{BodyExtractor, Response};

#[derive(Default)]
pub struct MemoryHttpCache {
    storage: DashMap<String, Arc<Response>>,
}

#[async_trait]
impl super::HttpCache for MemoryHttpCache {
    async fn set(
        &self,
        key: String,
        _method: &crate::http::Method,
        _policy: Option<&super::CachePolicy>,
        value: Arc<Response>,
    ) -> Result<(), super::Error> {
        self.storage.insert(key, value);
        Ok(())
    }

    async fn get(
        &self,
        key: &str,
        _extractor: &dyn BodyExtractor,
    ) -> Result<Option<Arc<Response>>, super::Error> {
        Ok(self.storage.get(key).map(|v| v.value().clone()))
    }
}
