use async_trait::async_trait;

use crate::httpcache::HttpCache;

use super::{HttpClient, Request, Response};
use std::sync::Arc;

pub struct CacheHttpClient {
    pub client: Arc<dyn HttpClient>,
    pub cache: Arc<dyn HttpCache>,
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
        }
    }
}

#[async_trait]
impl HttpClient for CacheHttpClient {
    async fn make_request(
        &self,
        req: Request,
        body_extractor: &dyn super::BodyExtractor,
    ) -> Result<Arc<Response>, super::Error> {
        if !req.force_refetch
            && let Some(cached) = self.cache.get_req(&req, body_extractor).await?
        {
            return Ok(cached);
        }
        let res = self
            .client
            .make_request(req.clone(), body_extractor)
            .await?;
        self.cache.set_req(&req, res.clone()).await?;
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
        assert_eq!(res1.body.as_text(), Some("Hello, World! (counter: 0)"));
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);

        let res2 = client.get_text(req.clone()).await?;
        assert_eq!(res2.body.as_text(), Some("Hello, World! (counter: 0)"));
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);

        let req_force = Request {
            force_refetch: true,
            ..req
        };

        let res3 = client.get_text(req_force).await?;
        assert_eq!(res3.body.as_text(), Some("Hello, World! (counter: 1)"));
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
        assert_eq!(res1.body.as_text(), Some("Hello, World!"));

        let res2 = client.get_text(req.clone()).await?;
        assert_eq!(res2.body.as_text(), Some("Hello, World!"));

        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);

        Ok(())
    }
}
