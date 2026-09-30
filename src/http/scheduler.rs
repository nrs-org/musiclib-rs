use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use dashmap::DashMap;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use super::{BodyExtractorCow, Error, HttpClient, Request, Response};

type ReplyTx = oneshot::Sender<Result<Arc<Response>, Error>>;

struct RequestEnvelope {
    req: Request,
    extractor: BodyExtractorCow<'static>,
    reply: ReplyTx,
}

mod duration_defaults {
    use std::time::Duration;
    pub fn default_initial_backoff() -> Duration {
        Duration::from_secs(1)
    }
    pub fn default_max_backoff() -> Duration {
        Duration::from_secs(60)
    }
}

/// Two-phase retry configuration for 429 responses.
///
/// Phase 1: honour the server's `Retry-After` header for up to `retry_after_attempts` retries.
///   If a 429 has no `Retry-After` header, skip immediately to phase 2.
/// Phase 2: exponential backoff for up to `backoff_attempts` additional retries.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RetryConfig {
    /// Max retries that use the `Retry-After` header (phase 1).
    #[serde(default = "RetryConfig::default_retry_after_attempts")]
    pub retry_after_attempts: u32,
    /// Max retries using exponential backoff after phase 1 is exhausted (phase 2).
    #[serde(default = "RetryConfig::default_backoff_attempts")]
    pub backoff_attempts: u32,
    /// Initial backoff duration for phase 2 (e.g. `"1s"`, `"500ms"`).
    #[serde(
        deserialize_with = "crate::duration::deserialize",
        default = "duration_defaults::default_initial_backoff"
    )]
    pub initial_backoff: Duration,
    /// Backoff multiplier per phase-2 attempt.
    #[serde(default = "RetryConfig::default_backoff_multiplier")]
    pub backoff_multiplier: f64,
    /// Upper bound on phase-2 backoff (e.g. `"1m"`, `"1h"`).
    #[serde(
        deserialize_with = "crate::duration::deserialize",
        default = "duration_defaults::default_max_backoff"
    )]
    pub max_backoff: Duration,
    /// Extra status codes to treat as rate-limit responses (in addition to 429).
    /// e.g. MusicBrainz uses 503 instead of 429.
    #[serde(default)]
    pub rate_limit_statuses: Vec<u16>,
}

impl RetryConfig {
    fn default_retry_after_attempts() -> u32 {
        5
    }
    fn default_backoff_attempts() -> u32 {
        3
    }
    fn default_backoff_multiplier() -> f64 {
        2.0
    }

    pub fn default_backoff() -> Self {
        Self {
            retry_after_attempts: Self::default_retry_after_attempts(),
            backoff_attempts: Self::default_backoff_attempts(),
            initial_backoff: duration_defaults::default_initial_backoff(),
            backoff_multiplier: Self::default_backoff_multiplier(),
            max_backoff: duration_defaults::default_max_backoff(),
            rate_limit_statuses: Vec::new(),
        }
    }

    fn is_rate_limited(&self, status: reqwest::StatusCode) -> bool {
        status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || self.rate_limit_statuses.contains(&status.as_u16())
    }
}

fn default_channel_capacity() -> usize {
    64
}

/// Controls how the scheduler dispatches requests.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
pub struct SchedulerConfig {
    /// Maximum number of requests in flight simultaneously. `None` = unlimited.
    pub max_concurrent: Option<usize>,

    /// Retry-on-429 behaviour. `None` disables retries (caller sees the 429).
    pub retry: Option<RetryConfig>,

    /// Capacity of the incoming request channel.
    #[serde(default = "default_channel_capacity")]
    pub channel_capacity: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self::unlimited()
    }
}

impl SchedulerConfig {
    /// Unlimited concurrency, retry on 429. Good for YouTube-style APIs.
    pub fn unlimited() -> Self {
        Self {
            max_concurrent: None,
            retry: Some(RetryConfig::default_backoff()),
            channel_capacity: default_channel_capacity(),
        }
    }

    /// Capped concurrency, retry on 429. Good for MusicBrainz-style APIs.
    pub fn capped(max_concurrent: usize) -> Self {
        Self {
            max_concurrent: Some(max_concurrent),
            retry: Some(RetryConfig::default_backoff()),
            channel_capacity: default_channel_capacity(),
        }
    }
}

/// Parse the `Retry-After` header value. Supports the delay-seconds form only
/// (an integer number of seconds). The HTTP-date form is ignored and returns
/// `None`, falling through to exponential backoff.
fn parse_retry_after(res: &Response) -> Option<Duration> {
    use reqwest::header::HeaderName;
    use std::str::FromStr;

    let name = HeaderName::from_static("retry-after");
    let value = res
        .headers
        .iter()
        .find(|(k, _)| *k == name)
        .and_then(|(_, v)| v.to_str().ok())?;

    // Try delay-seconds first (a plain integer).
    if let Ok(secs) = u64::from_str(value.trim()) {
        return Some(Duration::from_secs(secs));
    }

    None
}

async fn dispatch_with_retry(
    inner: &dyn HttpClient,
    req: Request,
    extractor: BodyExtractorCow<'static>,
    retry: Option<&RetryConfig>,
) -> Result<Arc<Response>, Error> {
    let extractor = Arc::new(extractor);

    let retry = match retry {
        Some(r) => r,
        None => {
            return inner
                .make_request(req, Arc::clone(&extractor).as_ref().clone_static())
                .await;
        }
    };

    // Phase 1: honour Retry-After header.
    let mut retry_after_remaining = retry.retry_after_attempts;
    loop {
        let res = inner
            .make_request(req.clone(), Arc::clone(&extractor).as_ref().clone_static())
            .await?;

        if !retry.is_rate_limited(res.status) {
            return Ok(res);
        }

        let Some(delay) = parse_retry_after(&res) else {
            // No Retry-After header — skip straight to phase 2.
            break;
        };
        // A server can send an arbitrarily large Retry-After (observed: Spotify
        // returning ~8.3 hours under heavy sustained traffic) — honoring that
        // verbatim looks indistinguishable from a hung process for any
        // practical session length. Cap it at `max_backoff`, the same ceiling
        // phase 2 already uses for "how long we're willing to wait between
        // retries" — a real multi-hour block still gets *some* retry instead
        // of silently parking the whole request tree until the process is
        // manually killed.
        let delay = delay.min(retry.max_backoff);

        if retry_after_remaining == 0 {
            break;
        }
        retry_after_remaining -= 1;

        tracing::debug!(
            remaining = retry_after_remaining,
            delay_ms = delay.as_millis(),
            "429 with Retry-After, waiting before retry (phase 1)",
        );
        tokio::time::sleep(delay).await;
    }

    // Phase 2: exponential backoff.
    let mut backoff = retry.initial_backoff;
    for remaining in (0..retry.backoff_attempts).rev() {
        let res = inner
            .make_request(req.clone(), Arc::clone(&extractor).as_ref().clone_static())
            .await?;

        if !retry.is_rate_limited(res.status) {
            return Ok(res);
        }

        if remaining == 0 {
            return Ok(res);
        }

        tracing::debug!(
            remaining,
            backoff_ms = backoff.as_millis(),
            "429 received, exponential backoff (phase 2)",
        );
        tokio::time::sleep(backoff).await;
        backoff = Duration::from_secs_f64(backoff.as_secs_f64() * retry.backoff_multiplier)
            .min(retry.max_backoff);
    }

    // backoff_attempts was 0 — make one final attempt and return whatever we get.
    inner
        .make_request(req, Arc::clone(&extractor).as_ref().clone_static())
        .await
}

/// Routes each request to a per-domain [`Scheduler`] worker, creating one
/// lazily on first use. This allows independent concurrency limits and retry
/// state per host while sharing a single [`HttpClient`] impl underneath.
pub struct DomainScheduler {
    inner: Arc<dyn HttpClient>,
    /// Per-host overrides. Keyed by exact host string (e.g. `"api.spotify.com"`).
    domain_configs: HashMap<String, SchedulerConfig>,
    /// Fallback config for hosts that have no explicit entry.
    default_config: SchedulerConfig,
    workers: DashMap<String, mpsc::Sender<RequestEnvelope>>,
}

impl DomainScheduler {
    pub fn new(inner: Arc<dyn HttpClient>, default_config: SchedulerConfig) -> Arc<Self> {
        Arc::new(Self {
            inner,
            domain_configs: HashMap::new(),
            default_config,
            workers: DashMap::new(),
        })
    }

    pub fn with_domain_configs(
        inner: Arc<dyn HttpClient>,
        default_config: SchedulerConfig,
        domain_configs: HashMap<String, SchedulerConfig>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner,
            domain_configs,
            default_config,
            workers: DashMap::new(),
        })
    }

    fn config_for(&self, host: &str) -> &SchedulerConfig {
        self.domain_configs
            .get(host)
            .unwrap_or(&self.default_config)
    }

    fn get_or_create_worker(&self, host: &str) -> mpsc::Sender<RequestEnvelope> {
        if let Some(entry) = self.workers.get(host) {
            return entry.value().clone();
        }

        // Use entry API to avoid a race where two threads both see a miss.
        self.workers
            .entry(host.to_string())
            .or_insert_with(|| {
                let config = self.config_for(host);
                let (tx, mut rx) = mpsc::channel::<RequestEnvelope>(config.channel_capacity);
                let inner = Arc::clone(&self.inner);
                let semaphore = config.max_concurrent.map(|n| Arc::new(Semaphore::new(n)));
                let retry = config.retry.clone();
                tokio::spawn(async move {
                    while let Some(envelope) = rx.recv().await {
                        let permit = match &semaphore {
                            Some(sem) => Some(
                                Arc::clone(sem)
                                    .acquire_owned()
                                    .await
                                    .expect("Semaphore closed"),
                            ),
                            None => None,
                        };
                        let inner = Arc::clone(&inner);
                        let retry = retry.clone();
                        tokio::spawn(async move {
                            let _permit: Option<OwnedSemaphorePermit> = permit;
                            let result = dispatch_with_retry(
                                inner.as_ref(),
                                envelope.req,
                                envelope.extractor,
                                retry.as_ref(),
                            )
                            .await;
                            let _ = envelope.reply.send(result);
                        });
                    }
                });
                tx
            })
            .clone()
    }
}

#[async_trait]
impl HttpClient for DomainScheduler {
    async fn make_request(
        &self,
        req: Request,
        body_extractor: BodyExtractorCow<'static>,
    ) -> Result<Arc<Response>, Error> {
        // Extract the host portion cheaply without pulling in a URL parser.
        // Expected form: scheme://host/path — we grab the segment between "://" and the next "/".
        let host = req
            .url
            .split_once("://")
            .and_then(|(_, rest)| rest.split('/').next())
            .unwrap_or("__unknown__")
            .to_string();

        let tx = self.get_or_create_worker(&host);

        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(RequestEnvelope {
            req,
            extractor: body_extractor,
            reply: reply_tx,
        })
        .await
        .expect("Per-domain scheduler task has stopped");

        reply_rx
            .await
            .expect("Per-domain scheduler dropped reply sender")
    }
}

/// Helper to cheaply re-wrap an `Arc<BodyExtractorCow<'static>>` for reuse
/// across retry attempts.
trait CloneStatic {
    fn clone_static(&self) -> BodyExtractorCow<'static>;
}

impl CloneStatic for BodyExtractorCow<'static> {
    fn clone_static(&self) -> BodyExtractorCow<'static> {
        match self {
            BodyExtractorCow::Borrowed(r) => BodyExtractorCow::Borrowed(*r),
            BodyExtractorCow::Owned(arc) => BodyExtractorCow::Owned(Arc::clone(arc)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use http_body_util::Full;
    use hyper::{Response as HyperResponse, body::Bytes};

    use crate::{
        http::{
            HttpClient, Request, ResponseStatus, default_http_client,
            scheduler::{DomainScheduler, RetryConfig, SchedulerConfig},
        },
        test_utils::{MockServer, init_test_logger},
    };

    #[tokio::test]
    async fn test_scheduler_basic() -> anyhow::Result<()> {
        init_test_logger();
        let server =
            MockServer::new(async |_| Ok(HyperResponse::new(Full::new(Bytes::from("hello")))))
                .await?;

        let client = DomainScheduler::new(default_http_client(), SchedulerConfig::unlimited());
        let req = Request {
            url: server.route("/"),
            ..Default::default()
        };

        let res = client.get(req).await?;
        assert_eq!(res.status, ResponseStatus::OK);
        assert_eq!(res.body_to_bytes().await?.as_ref(), b"hello");
        Ok(())
    }

    #[tokio::test]
    async fn test_concurrent_dispatches() -> anyhow::Result<()> {
        init_test_logger();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();
        let server = MockServer::new(move |_| {
            let c = counter_clone.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(HyperResponse::new(Full::new(Bytes::from("ok"))))
            }
        })
        .await?;

        let client = DomainScheduler::new(default_http_client(), SchedulerConfig::unlimited());
        let req = Request {
            url: server.route("/"),
            ..Default::default()
        };

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let client = client.clone();
                let req = req.clone();
                tokio::spawn(async move { client.get(req).await })
            })
            .collect();

        for handle in handles {
            handle.await??;
        }

        assert_eq!(counter.load(Ordering::SeqCst), 8);
        Ok(())
    }

    #[tokio::test]
    async fn test_max_concurrent() -> anyhow::Result<()> {
        init_test_logger();
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let in_flight_clone = in_flight.clone();
        let peak_clone = peak.clone();

        let server = MockServer::new(move |_| {
            let in_flight = in_flight_clone.clone();
            let peak = peak_clone.clone();
            async move {
                let current = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(current, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                Ok(HyperResponse::new(Full::new(Bytes::from("ok"))))
            }
        })
        .await?;

        let max_concurrent = 3;
        let client = DomainScheduler::new(
            default_http_client(),
            SchedulerConfig::capped(max_concurrent),
        );
        let req = Request {
            url: server.route("/"),
            ..Default::default()
        };

        let handles: Vec<_> = (0..9)
            .map(|_| {
                let client = client.clone();
                let req = req.clone();
                tokio::spawn(async move { client.get(req).await })
            })
            .collect();

        for handle in handles {
            handle.await??;
        }

        assert!(
            peak.load(Ordering::SeqCst) <= max_concurrent,
            "peak concurrency {} exceeded limit {}",
            peak.load(Ordering::SeqCst),
            max_concurrent
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_retry_after_header() -> anyhow::Result<()> {
        init_test_logger();
        // First request returns 429 with Retry-After: 0 (instant retry),
        // second returns 200.
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();
        let server = MockServer::new(move |_| {
            let count = call_count_clone.clone();
            async move {
                let n = count.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Ok(HyperResponse::builder()
                        .status(429)
                        .header("retry-after", "0")
                        .body(Full::new(Bytes::from("slow down")))
                        .unwrap())
                } else {
                    Ok(HyperResponse::new(Full::new(Bytes::from("ok"))))
                }
            }
        })
        .await?;

        let client = DomainScheduler::new(default_http_client(), SchedulerConfig::unlimited());
        let req = Request {
            url: server.route("/"),
            ..Default::default()
        };

        let res = client.get(req).await?;
        assert_eq!(res.status, ResponseStatus::OK);
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_retry_after_capped_at_max_backoff() -> anyhow::Result<()> {
        init_test_logger();
        // A server can send an absurd Retry-After (observed for real: Spotify
        // returning ~8.3 hours under sustained load) — honoring it verbatim
        // would sleep the request, and everything queued behind it on a
        // max_concurrent-limited domain, for that whole span. Verify it's
        // capped at `max_backoff` instead.
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();
        let server = MockServer::new(move |_| {
            let count = call_count_clone.clone();
            async move {
                let n = count.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Ok(HyperResponse::builder()
                        .status(429)
                        .header("retry-after", "36000") // 10 hours
                        .body(Full::new(Bytes::from("slow down")))
                        .unwrap())
                } else {
                    Ok(HyperResponse::new(Full::new(Bytes::from("ok"))))
                }
            }
        })
        .await?;

        let client = DomainScheduler::new(
            default_http_client(),
            SchedulerConfig {
                max_concurrent: None,
                retry: Some(RetryConfig {
                    retry_after_attempts: 1,
                    backoff_attempts: 0,
                    initial_backoff: Duration::from_millis(1),
                    backoff_multiplier: 2.0,
                    max_backoff: Duration::from_millis(20), // the cap under test
                    rate_limit_statuses: Vec::new(),
                }),
                channel_capacity: 64,
            },
        );
        let req = Request {
            url: server.route("/"),
            ..Default::default()
        };

        let started = std::time::Instant::now();
        let res = tokio::time::timeout(Duration::from_secs(5), client.get(req)).await??;
        assert_eq!(res.status, ResponseStatus::OK);
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
        // If the 10h Retry-After weren't capped, this wouldn't complete inside
        // the 5s timeout at all; also check it took roughly `max_backoff`, not
        // something merely-smaller-but-still-huge, to catch a cap that's
        // computed but not actually applied.
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "took {:?} — Retry-After cap doesn't seem to be applied",
            started.elapsed()
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_exponential_backoff_without_retry_after() -> anyhow::Result<()> {
        init_test_logger();
        // Two 429s without Retry-After, then a 200. Verify all three attempts
        // are made and the final response is OK.
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();
        let server = MockServer::new(move |_| {
            let count = call_count_clone.clone();
            async move {
                let n = count.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Ok(HyperResponse::builder()
                        .status(429)
                        .body(Full::new(Bytes::from("slow down")))
                        .unwrap())
                } else {
                    Ok(HyperResponse::new(Full::new(Bytes::from("ok"))))
                }
            }
        })
        .await?;

        let client = DomainScheduler::new(
            default_http_client(),
            SchedulerConfig {
                max_concurrent: None,
                retry: Some(RetryConfig {
                    retry_after_attempts: 0,
                    backoff_attempts: 5,
                    initial_backoff: Duration::from_millis(1), // fast for tests
                    backoff_multiplier: 2.0,
                    max_backoff: Duration::from_millis(10),
                    rate_limit_statuses: Vec::new(),
                }),
                channel_capacity: 64,
            },
        );
        let req = Request {
            url: server.route("/"),
            ..Default::default()
        };

        let res = client.get(req).await?;
        assert_eq!(res.status, ResponseStatus::OK);
        assert_eq!(call_count.load(Ordering::SeqCst), 3);
        Ok(())
    }
}
