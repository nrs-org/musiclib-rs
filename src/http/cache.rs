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

tokio::task_local! {
    static BYPASS: bool;
}

/// Run `fut` with every cache layer bypassed: requests it makes are neither
/// read from nor written to any `CacheHttpClient`, as if each had `no_cache`
/// set. Used to refresh a listing without the request builders below it
/// having to know (see `providers::listing`).
///
/// Scoped with a task-local, so it only reaches requests made from the task
/// polling `fut`. That covers the whole stack down to the caches: the
/// activity, memory-cache and DB-cache layers run in the caller's task, and
/// only layers below the caches (coalescer, scheduler) spawn.
pub async fn bypass<F: Future>(fut: F) -> F::Output {
    BYPASS.scope(true, fut).await
}

/// Undo `bypass` for `fut`: for lookups made while refreshing a listing whose
/// answers don't change (e.g. resolving an album's playlist ID), so they stay
/// cached.
pub async fn allow<F: Future>(fut: F) -> F::Output {
    BYPASS.scope(false, fut).await
}

fn bypassed() -> bool {
    BYPASS.try_with(|b| *b).unwrap_or(false)
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
        let no_cache = req.no_cache || bypassed();
        if !no_cache
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
        if !no_cache {
            let policy = self.config.resolve_policy(&req.url);
            self.cache
                .set_req(&req, policy, res.clone())
                .await
                .map_err(|source| super::Error::Cache {
                    url: req.url.clone(),
                    source,
                })?;
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

    /// Inside `bypass`, a request skips the cache both ways: it goes to the
    /// server despite a cached entry and doesn't replace that entry.
    #[tokio::test]
    async fn bypass_skips_cache_read_and_write() -> anyhow::Result<()> {
        init_test_logger();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();
        let server = MockServer::new(move |_| {
            let n = counter_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move { Ok(HyperResponse::new(Full::new(Bytes::from(format!("{n}"))))) }
        })
        .await?;

        let client = default_http_client().into_cached::<MemoryHttpCache>();
        let req = Request {
            url: server.route("/"),
            ..Default::default()
        };

        assert_eq!(&*client.get_text(req.clone()).await?.text().await?, "0");
        let bypassed = super::bypass(client.get_text(req.clone())).await?;
        assert_eq!(&*bypassed.text().await?, "1");
        assert_eq!(&*client.get_text(req.clone()).await?.text().await?, "0");
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);

        // `allow` inside `bypass` uses the cache again.
        let allowed = super::bypass(super::allow(client.get_text(req))).await?;
        assert_eq!(&*allowed.text().await?, "0");
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
}
