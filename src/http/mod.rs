use std::{marker::PhantomData, sync::Arc};

use async_trait::async_trait;
use http_body_util::BodyExt;
use serde::{Serialize, de::DeserializeOwned};

mod cache;
pub mod coalescer;
mod default;
pub mod quota;
pub mod scheduler;
mod types;

pub use coalescer::{CoalesceKey, CoalesceRule, Coalescer, SplitResponse};
pub use quota::QuotaCounter;
use tokio::sync::RwLock;
pub use types::{
    BodyExtractError, Error, HeaderName, HeaderValue, Method, RawResponse, Request, Response,
    ResponseBody, ResponseStatus,
};

use crate::http::types::{HasHeaders, ResponseBodyState};

#[async_trait]
pub trait BodyExtractor: Send + Sync {
    async fn extract(&self, res: RawResponse<'_>) -> Result<ResponseBody, BodyExtractError>;

    async fn extract_response(&self, res: RawResponse<'_>) -> Result<Response, BodyExtractError> {
        Ok(Response {
            status: res.status,
            headers: res
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            body: RwLock::new(ResponseBodyState::Extracted(self.extract(res).await?)),
        })
    }
}

pub enum BodyExtractorCow<'a> {
    Borrowed(&'a dyn BodyExtractor),
    Owned(Arc<dyn BodyExtractor>),
}

impl<'a> Clone for BodyExtractorCow<'a> {
    fn clone(&self) -> Self {
        match self {
            Self::Borrowed(b) => Self::Borrowed(*b),
            Self::Owned(o) => Self::Owned(Arc::clone(o)),
        }
    }
}

impl<'a> BodyExtractorCow<'a> {
    pub fn as_ref(&self) -> &dyn BodyExtractor {
        match self {
            BodyExtractorCow::Borrowed(extractor) => *extractor,
            BodyExtractorCow::Owned(extractor) => extractor.as_ref(),
        }
    }
}

impl<'a, T> From<&'a T> for BodyExtractorCow<'a>
where
    T: BodyExtractor + 'a,
{
    fn from(value: &'a T) -> Self {
        Self::Borrowed(value)
    }
}

impl From<Arc<dyn BodyExtractor>> for BodyExtractorCow<'static> {
    fn from(value: Arc<dyn BodyExtractor>) -> Self {
        Self::Owned(value)
    }
}

#[async_trait]
impl<'a> BodyExtractor for BodyExtractorCow<'a> {
    async fn extract(&self, res: RawResponse<'_>) -> Result<ResponseBody, BodyExtractError> {
        match self {
            BodyExtractorCow::Borrowed(extractor) => extractor.extract(res).await,
            BodyExtractorCow::Owned(extractor) => extractor.extract(res).await,
        }
    }

    async fn extract_response(&self, res: RawResponse<'_>) -> Result<Response, BodyExtractError> {
        match self {
            BodyExtractorCow::Borrowed(extractor) => extractor.extract_response(res).await,
            BodyExtractorCow::Owned(extractor) => extractor.extract_response(res).await,
        }
    }
}

pub fn bytes_body_extractor() -> &'static impl BodyExtractor {
    struct BytesExtractor;
    static BYTES_EXTRACTOR: BytesExtractor = BytesExtractor;
    #[async_trait]
    impl BodyExtractor for BytesExtractor {
        async fn extract(&self, res: RawResponse<'_>) -> Result<ResponseBody, BodyExtractError> {
            let bytes = BodyExt::collect(res.body).await?.to_bytes();
            Ok(ResponseBody::Bytes(bytes))
        }
    }
    &BYTES_EXTRACTOR
}

pub fn text_body_extractor() -> &'static impl BodyExtractor {
    struct TextExtractor;
    static TEXT_EXTRACTOR: TextExtractor = TextExtractor;
    #[async_trait]
    impl BodyExtractor for TextExtractor {
        async fn extract(&self, res: RawResponse<'_>) -> Result<ResponseBody, BodyExtractError> {
            // for our purposes, we just parse the text as utf-8
            // it's mainstream enough in 2026 anw
            match bytes_body_extractor().extract(res).await? {
                ResponseBody::Bytes(bytes) => {
                    let text = String::from_utf8_lossy(&bytes);
                    Ok(ResponseBody::Text(text.into()))
                }
                other => unreachable!(
                    "Bytes extractor should always return bytes, got: {:?}",
                    other
                ),
            }
        }
    }
    &TEXT_EXTRACTOR
}

pub fn json_body_extractor<T: DeserializeOwned + Serialize + Send + Sync + 'static>()
-> Arc<dyn BodyExtractor> {
    struct JsonExtractor<T>(PhantomData<T>);
    #[async_trait]
    impl<T: DeserializeOwned + Serialize + Send + Sync + 'static> BodyExtractor for JsonExtractor<T> {
        async fn extract(&self, res: RawResponse<'_>) -> Result<ResponseBody, BodyExtractError> {
            match bytes_body_extractor().extract(res).await? {
                ResponseBody::Bytes(bytes) => {
                    let json = serde_json::from_slice::<T>(&bytes)?;
                    Ok(ResponseBody::from_json(json))
                }
                other => unreachable!(
                    "Bytes extractor should always return bytes, got: {:?}",
                    other
                ),
            }
        }
    }
    Arc::new(JsonExtractor::<T>(PhantomData))
}

pub fn auto_body_extractor() -> &'static impl BodyExtractor {
    struct AutoExtractor;
    static AUTO_EXTRACTOR: AutoExtractor = AutoExtractor;
    #[async_trait]
    impl BodyExtractor for AutoExtractor {
        async fn extract(&self, res: RawResponse<'_>) -> Result<ResponseBody, BodyExtractError> {
            let content_type = res
                .get_header(&reqwest::header::CONTENT_TYPE)
                .map(|v| v.to_str().unwrap_or_default())
                .unwrap_or_default();

            if content_type.contains("application/json") {
                json_body_extractor::<serde_json::Value>()
                    .as_ref()
                    .extract(res)
                    .await
            } else if content_type.contains("text/") {
                text_body_extractor().extract(res).await
            } else {
                bytes_body_extractor().extract(res).await
            }
        }
    }
    &AUTO_EXTRACTOR
}

#[async_trait]
pub trait HttpClient: Send + Sync {
    async fn make_request(
        &self,
        req: Request,
        body_extractor: BodyExtractorCow<'static>,
    ) -> Result<Arc<Response>, Error>;

    #[allow(non_snake_case)]
    fn GET(&self, req: Request) -> Request {
        Request {
            method: Method::GET,
            ..req
        }
    }

    async fn get(&self, req: Request) -> Result<Arc<Response>, Error> {
        self.make_request(self.GET(req), auto_body_extractor().into())
            .await
    }

    async fn get_bytes(&self, req: Request) -> Result<Arc<Response>, Error> {
        self.make_request(self.GET(req), bytes_body_extractor().into())
            .await
    }

    async fn get_text(&self, req: Request) -> Result<Arc<Response>, Error> {
        self.make_request(self.GET(req), text_body_extractor().into())
            .await
    }
}

impl dyn HttpClient {
    pub async fn get_json<T: DeserializeOwned + Serialize + Send + Sync + 'static>(
        &self,
        req: Request,
    ) -> Result<Arc<Response>, Error> {
        self.make_request(req, BodyExtractorCow::Owned(json_body_extractor::<T>()))
            .await
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

/// Configuration for building a layered [`HttpClient`] stack.
///
/// Configuration for building a layered [`HttpClient`] stack.
///
/// The stack from innermost to outermost is:
/// `DefaultHttpClient` → `DomainScheduler` → DB cache (optional) → memory cache (optional)
///
/// Cache hits at an outer layer bypass all inner layers including scheduling.
///
/// Example YAML:
/// ```yaml
/// memory_cache: true
/// db_cache: sqlite://http_cache.db
/// schedulers:
///   "*":                     # default for all unlisted domains
///     max_concurrent: ~      # unlimited
///     retry:
///       max_retries: 5
///       initial_backoff: 1000
///       backoff_multiplier: 2.0
///       max_backoff: 60000
///   musicbrainz.org:
///     max_concurrent: 1
///   api.spotify.com:
///     max_concurrent: ~
/// ```
#[derive(serde::Deserialize)]
pub struct DbCacheConfig {
    #[serde(default = "DbCacheConfig::default_path")]
    pub path: String,
    #[serde(default)]
    pub cache_policy: cache::CacheClientConfig,
}

impl DbCacheConfig {
    fn default_path() -> String {
        format!(
            "sqlite://{}",
            crate::app_dirs::cache_dir().join("http_cache.db").display()
        )
    }
}

#[derive(serde::Deserialize)]
#[serde(default)]
pub struct HttpClientConfig {
    /// Scheduling configs keyed by host. The special key `"*"` is the default
    /// applied to any domain not explicitly listed.
    pub schedulers: std::collections::HashMap<String, scheduler::SchedulerConfig>,
    /// Persistent DB cache. `None` disables it.
    pub db_cache: Option<DbCacheConfig>,
    /// Wrap the outermost layer in an in-process memory cache.
    pub memory_cache: bool,

    /// Coalesce rules applied between the scheduler and the cache layers.
    /// Not deserialised from YAML — populated programmatically by the caller
    /// (e.g. via `providers::registry::coalesce_rules()`).
    #[serde(skip)]
    pub coalescer_rules: Vec<Arc<dyn CoalesceRule>>,

    /// When `Some`, every outgoing YouTube Data API request increments this
    /// counter. Cache hits don't count (the counter sits below the cache
    /// layers); scheduler retries each count as a separate request, matching
    /// YouTube's quota billing. Read the value after the run to get the total.
    #[serde(skip)]
    pub youtube_quota_counter: Option<Arc<std::sync::atomic::AtomicU64>>,
}

impl Default for HttpClientConfig {
    fn default() -> Self {
        Self {
            schedulers: std::collections::HashMap::new(),
            db_cache: None,
            memory_cache: false,
            coalescer_rules: Vec::new(),
            youtube_quota_counter: None,
        }
    }
}

impl HttpClientConfig {
    pub async fn build(self) -> Result<Arc<dyn HttpClient>, crate::httpcache::db::Error> {
        use crate::httpcache::{DbHttpCache, HttpCache, MemoryHttpCache};
        use cache::IntoCachedHttpClient;

        // Base → (optional) quota counter → scheduler.
        // Counter sits below the scheduler so retries are billed individually,
        // matching YouTube's quota accounting.
        let base = default_http_client();
        let counted_base: Arc<dyn HttpClient> = match self.youtube_quota_counter {
            Some(c) => QuotaCounter::new(base, c),
            None => base,
        };
        let mut schedulers = self.schedulers;
        let default_config = schedulers
            .remove("*")
            .unwrap_or_else(scheduler::SchedulerConfig::unlimited);
        let scheduled: Arc<dyn HttpClient> = scheduler::DomainScheduler::with_domain_configs(
            counted_base,
            default_config,
            schedulers,
        );

        // Optional coalescer layer (above scheduler, below caches).
        let with_coalescer: Arc<dyn HttpClient> = if self.coalescer_rules.is_empty() {
            scheduled
        } else {
            Coalescer::new(scheduled, self.coalescer_rules)
        };

        // Optional DB cache layer
        let with_db: Arc<dyn HttpClient> = match self.db_cache {
            Some(DbCacheConfig { path, cache_policy }) => {
                let db = Arc::new(DbHttpCache::new(path).await?) as Arc<dyn HttpCache>;
                Arc::new(cache::CacheHttpClient {
                    client: with_coalescer,
                    cache: db,
                    config: cache_policy,
                })
            }
            None => with_coalescer,
        };

        // Optional memory cache layer (outermost)
        Ok(if self.memory_cache {
            Arc::new(with_db.into_cached::<MemoryHttpCache>())
        } else {
            with_db
        })
    }
}
