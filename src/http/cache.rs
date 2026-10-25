use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;

use crate::httpcache::{CachePolicy, HttpCache};

use super::{HttpClient, Request, Response};

#[derive(Default, serde::Deserialize)]
pub struct CacheClientConfig {
    /// Policy applied to requests whose host is not listed in `domains`.
    /// `None` means don't cache by default.
    pub default_policy: Option<CachePolicy>,
    /// Per-host policy overrides. `None` value means don't cache for that host.
    pub domains: HashMap<String, Option<CachePolicy>>,
}

impl CacheClientConfig {
    fn resolve_policy(&self, url: &str) -> Option<&CachePolicy> {
        let host = reqwest::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned));

        if let Some(host) = host
            && let Some(domain_policy) = self.domains.get(&host)
        {
            return domain_policy.as_ref();
        }

        self.default_policy.as_ref()
    }
}

pub struct CacheHttpClient {
    pub client: Arc<dyn HttpClient>,
    pub cache: Arc<dyn HttpCache>,
    pub config: CacheClientConfig,
}

pub trait IntoCachedHttpClient: Sized {
    fn into_cached<C>(self) -> CacheHttpClient
    where
        C: HttpCache + Default + 'static,
    {
        Self::into_cached_with_cache(self, Arc::new(C::default()))
    }

    fn into_cached_with_cache(self, cache: Arc<dyn HttpCache>) -> CacheHttpClient;
}

impl IntoCachedHttpClient for Arc<dyn HttpClient> {
    fn into_cached_with_cache(self, cache: Arc<dyn HttpCache>) -> CacheHttpClient {
        CacheHttpClient {
            client: self,
            cache,
            config: CacheClientConfig::default(),
        }
    }
}

#[async_trait]
impl HttpClient for CacheHttpClient {
    async fn make_request(
        &self,
        req: Request,
        body_extractor: super::BodyExtractorCow<'static>,
    ) -> Result<Arc<Response>, super::Error> {
        if !req.no_cache
            && !req.force_refetch
            && let Some(cached) = self
                .cache
                .get_req(&req, body_extractor.clone())
                .await
                .map_err(|source| super::Error::Cache {
                    url: req.url.clone(),
                    source,
                })?
        {
            return Ok(cached);
        }
        let res = self
            .client
            .make_request(req.clone(), body_extractor)
            .await?;
        // A failed write only costs a later refetch; failing the request would
        // throw away a response already paid for (quota, rate limit).
        if !req.no_cache {
            let policy = self.config.resolve_policy(&req.url);
            if let Err(e) = self.cache.set_req(&req, policy, res.clone()).await {
                tracing::warn!("caching {} failed: {e}", req.url);
            }
        }
        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        http::{HttpClient, Request, cache::IntoCachedHttpClient, default_http_client},
        httpcache::{DbHttpCache, MemoryHttpCache},
        test_utils::{MockServer, init_test_logger},
    };
    use http_body_util::Full;
    use hyper::{Response as HyperResponse, body::Bytes};
    use std::sync::{Arc, atomic::AtomicUsize};

    #[tokio::test]
    async fn test_memory_cache() -> anyhow::Result<()> {
        init_test_logger();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();
        let server = MockServer::new(move |_| {
            let counter_clone = counter_clone.clone();
            async move {
                let counter_value = counter_clone.load(std::sync::atomic::Ordering::SeqCst);
                counter_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let response = format!("Hello, World! (counter: {})", counter_value);
                Ok(HyperResponse::new(Full::new(Bytes::from(response))))
            }
        })
        .await?;

        let client = default_http_client().into_cached::<MemoryHttpCache>();
        let req = Request {
            url: server.route("/"),
            ..Default::default()
        };

        let res1 = client.get_text(req.clone()).await?;
        assert_eq!(&*res1.text().await?, "Hello, World! (counter: 0)");
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);

        let res2 = client.get_text(req.clone()).await?;
        assert_eq!(&*res2.text().await?, "Hello, World! (counter: 0)");
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);

        let req_force = Request {
            force_refetch: true,
            ..req
        };

        let res3 = client.get_text(req_force).await?;
        assert_eq!(&*res3.text().await?, "Hello, World! (counter: 1)");
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_sqlite_cache() -> anyhow::Result<()> {
        init_test_logger();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();
        let server = MockServer::new(move |_| {
            let counter_clone = counter_clone.clone();
            async move {
                counter_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(HyperResponse::new(Full::new(Bytes::from("Hello, World!"))))
            }
        })
        .await?;

        let cache = DbHttpCache::new_in_memory().await?;
        let client = default_http_client().into_cached_with_cache(Arc::new(cache));
        let req = Request {
            url: server.route("/"),
            ..Default::default()
        };

        let res1 = client.get_text(req.clone()).await?;
        assert_eq!(&*res1.text().await?, "Hello, World!");

        let res2 = client.get_text(req.clone()).await?;
        assert_eq!(&*res2.text().await?, "Hello, World!");

        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);

        Ok(())
    }

    /// A cache that misses on every read and fails every write.
    struct FailingWrites;

    #[async_trait::async_trait]
    impl crate::httpcache::HttpCache for FailingWrites {
        async fn set(
            &self,
            _key: String,
            _method: &crate::http::Method,
            _policy: Option<&crate::httpcache::CachePolicy>,
            _value: Arc<crate::http::Response>,
        ) -> Result<(), crate::httpcache::Error> {
            Err(
                crate::httpcache::db::Error::Database(sea_orm::DbErr::Custom(
                    "pool timed out".into(),
                ))
                .into(),
            )
        }

        async fn get(
            &self,
            _key: &str,
            _extractor: crate::http::BodyExtractorCow<'static>,
        ) -> Result<Option<Arc<crate::http::Response>>, crate::httpcache::Error> {
            Ok(None)
        }
    }

    /// A failed cache write still returns the fetched response.
    #[tokio::test]
    async fn failed_cache_write_returns_response() -> anyhow::Result<()> {
        init_test_logger();
        let server = MockServer::new(|_| async {
            Ok(HyperResponse::new(Full::new(Bytes::from("Hello, World!"))))
        })
        .await?;
        let client = default_http_client().into_cached_with_cache(Arc::new(FailingWrites));
        let req = Request {
            url: server.route("/"),
            ..Default::default()
        };
        let res = client.get_text(req).await?;
        assert_eq!(&*res.text().await?, "Hello, World!");
        Ok(())
    }
}
