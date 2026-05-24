use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use super::{BodyExtractorCow, Error, HttpClient, Request, Response};

type ReplyTx = oneshot::Sender<Result<Arc<Response>, Error>>;

struct RequestEnvelope {
    req: Request,
    extractor: BodyExtractorCow<'static>,
    reply: ReplyTx,
}

/// A handle to a running scheduler. Implements [`HttpClient`] by enqueuing
/// requests onto the scheduler's channel and awaiting the oneshot reply.
pub struct ScheduledHttpClient {
    tx: mpsc::Sender<RequestEnvelope>,
}

#[async_trait]
impl HttpClient for ScheduledHttpClient {
    async fn make_request(
        &self,
        req: Request,
        body_extractor: BodyExtractorCow<'static>,
    ) -> Result<Arc<Response>, Error> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(RequestEnvelope {
                req,
                extractor: body_extractor,
                reply: reply_tx,
            })
            .await
            .expect("Scheduler task has stopped");
        reply_rx.await.expect("Scheduler dropped reply sender")
    }
}

/// Exponential backoff configuration for 429 retries.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts after a 429.
    pub max_retries: u32,
    /// Initial backoff duration (used when no `Retry-After` header is present).
    pub initial_backoff: Duration,
    /// Backoff is multiplied by this factor on each attempt.
    pub backoff_multiplier: f64,
    /// Upper bound on backoff duration.
    pub max_backoff: Duration,
}

impl RetryConfig {
    pub fn default_backoff() -> Self {
        Self {
            max_retries: 5,
            initial_backoff: Duration::from_secs(1),
            backoff_multiplier: 2.0,
            max_backoff: Duration::from_secs(60),
        }
    }
}

/// Controls how the scheduler dispatches requests.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Maximum number of requests in flight simultaneously. `None` = unlimited.
    pub max_concurrent: Option<usize>,

    /// Retry-on-429 behaviour. `None` disables retries (caller sees the 429).
    pub retry: Option<RetryConfig>,

    /// Capacity of the incoming request channel.
    pub channel_capacity: usize,
}

impl SchedulerConfig {
    /// Unlimited concurrency, retry on 429. Good for YouTube-style APIs.
    pub fn unlimited() -> Self {
        Self {
            max_concurrent: None,
            retry: Some(RetryConfig::default_backoff()),
            channel_capacity: 64,
        }
    }

    /// Capped concurrency, retry on 429. Good for MusicBrainz-style APIs.
    pub fn capped(max_concurrent: usize) -> Self {
        Self {
            max_concurrent: Some(max_concurrent),
            retry: Some(RetryConfig::default_backoff()),
            channel_capacity: 64,
        }
    }
}

pub struct Scheduler {
    inner: Arc<dyn HttpClient>,
    rx: mpsc::Receiver<RequestEnvelope>,
    semaphore: Option<Arc<Semaphore>>,
    retry: Option<RetryConfig>,
}

impl Scheduler {
    pub fn spawn(inner: Arc<dyn HttpClient>, config: SchedulerConfig) -> ScheduledHttpClient {
        let (tx, rx) = mpsc::channel(config.channel_capacity);
        let scheduler = Self {
            inner,
            rx,
            semaphore: config.max_concurrent.map(|n| Arc::new(Semaphore::new(n))),
            retry: config.retry,
        };
        tokio::spawn(scheduler.run());
        ScheduledHttpClient { tx }
    }

    async fn run(mut self) {
        while let Some(envelope) = self.rx.recv().await {
            let permit = match &self.semaphore {
                Some(sem) => Some(
                    Arc::clone(sem)
                        .acquire_owned()
                        .await
                        .expect("Semaphore closed"),
                ),
                None => None,
            };

            let inner = Arc::clone(&self.inner);
            let retry = self.retry.clone();
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
        .find(|(k, _)| k == &name)
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
    use reqwest::StatusCode;

    // Clone the extractor for potential retries. We always need an owned
    // BodyExtractorCow<'static> for each attempt.
    let extractor = Arc::new(extractor);

    let retry = match retry {
        Some(r) => r,
        None => {
            return inner
                .make_request(req, Arc::clone(&extractor).as_ref().clone_static())
                .await;
        }
    };

    let mut backoff = retry.initial_backoff;

    for attempt in 0..=retry.max_retries {
        let res = inner
            .make_request(req.clone(), Arc::clone(&extractor).as_ref().clone_static())
            .await?;

        if res.status != StatusCode::TOO_MANY_REQUESTS {
            return Ok(res);
        }

        if attempt == retry.max_retries {
            // Return the 429 response to the caller after exhausting retries.
            return Ok(res);
        }

        // Prefer Retry-After, fall back to exponential backoff.
        let delay = parse_retry_after(&res).unwrap_or(backoff);

        tracing::warn!(
            attempt = attempt + 1,
            max_retries = retry.max_retries,
            delay_ms = delay.as_millis(),
            "429 received, waiting before retry",
        );

        tokio::time::sleep(delay).await;

        backoff = (Duration::from_secs_f64(backoff.as_secs_f64() * retry.backoff_multiplier))
            .min(retry.max_backoff);
    }

    unreachable!()
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
            scheduler::{Scheduler, SchedulerConfig},
        },
        test_utils::{MockServer, init_test_logger},
    };

    #[tokio::test]
    async fn test_scheduler_basic() -> anyhow::Result<()> {
        init_test_logger();
        let server =
            MockServer::new(async |_| Ok(HyperResponse::new(Full::new(Bytes::from("hello")))))
                .await?;

        let client = Scheduler::spawn(default_http_client(), SchedulerConfig::unlimited());
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

        let client = Arc::new(Scheduler::spawn(
            default_http_client(),
            SchedulerConfig::unlimited(),
        ));
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
        let client = Arc::new(Scheduler::spawn(
            default_http_client(),
            SchedulerConfig::capped(max_concurrent),
        ));
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

        let client = Scheduler::spawn(default_http_client(), SchedulerConfig::unlimited());
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

        use crate::http::scheduler::RetryConfig;
        let client = Scheduler::spawn(
            default_http_client(),
            SchedulerConfig {
                max_concurrent: None,
                retry: Some(RetryConfig {
                    max_retries: 5,
                    initial_backoff: Duration::from_millis(1), // fast for tests
                    backoff_multiplier: 2.0,
                    max_backoff: Duration::from_millis(10),
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
