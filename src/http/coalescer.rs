//! Request-coalescing HTTP layer.
//!
//! Sits below caches and above the scheduler. Per-endpoint [`CoalesceRule`]s
//! merge concurrent single-entity requests into one batched upstream call and
//! split the response back. See `BATCHING_PROVIDERS.md` for the design.

use std::{
    borrow::Cow,
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};

use crate::http::{
    BodyExtractor, BodyExtractorCow, Error, HeaderName, HeaderValue, HttpClient, RawResponse,
    Request, Response, ResponseStatus, bytes_body_extractor,
};

/// Opaque grouping key. Two requests with equal [`CoalesceKey`] (under the same
/// rule) may be merged into a single upstream call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CoalesceKey(pub u64);

/// One sub-response produced by [`CoalesceRule::split`], the synthetic
/// equivalent of a single-entity call.
pub struct SplitResponse {
    pub status: ResponseStatus,
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub body: Bytes,
}

/// A single batchable endpoint. The [`Coalescer`] layer holds a `Vec` of these
/// and tries each one in order; the first to return `Some` from [`group`] wins.
pub trait CoalesceRule: Send + Sync + 'static {
    /// Short, stable identifier for this rule, used in diagnostic logs
    /// (e.g. `"youtube_videos.list"`).
    fn name(&self) -> &'static str;

    /// If this request is batchable under this rule, return a stable key
    /// identifying its group. Requests with equal keys may be merged.
    ///
    /// Returning `None` means "this rule doesn't own this request" — the
    /// `Coalescer` tries the next rule or falls through to the inner client.
    fn group(&self, req: &Request) -> Option<CoalesceKey>;

    /// Maximum number of input requests per merged upstream call.
    fn max_batch(&self) -> usize;

    /// Upper bound on how long the driver will wait for siblings to arrive
    /// before firing a partial batch. The actual wait per iteration is scaled
    /// linearly by how empty the batch still is: empty batch gets the full
    /// wait, nearly-full batch gets near-zero. Default: 10ms — covers the
    /// async-I/O fan-out window (e.g. sqlx cache lookups serialising sibling
    /// requests on their way down the stack) without adding meaningful
    /// latency compared to typical network round-trips.
    fn max_wait(&self) -> Duration {
        Duration::from_millis(1000)
    }

    /// Merge `reqs` (all sharing the same [`CoalesceKey`]) into one upstream
    /// request. `1 <= reqs.len() <= self.max_batch()`.
    fn merge(&self, reqs: &[Request]) -> Request;

    /// Split the merged response back into per-input synthetic responses. The
    /// output must have the same length and ordering as `reqs`. `None` for an
    /// input means "no data for this id" — the [`Coalescer`] returns a
    /// synthetic 404 to that waiter.
    ///
    /// Errors are returned as `Arc<Error>` so the [`Coalescer`] can hand the
    /// same structured error to every waiter via [`Error::Batch`] without
    /// requiring `Error: Clone`.
    fn split(
        &self,
        merged_status: ResponseStatus,
        merged_headers: &[(HeaderName, HeaderValue)],
        merged_body: &Bytes,
        reqs: &[Request],
    ) -> Result<Vec<Option<SplitResponse>>, Arc<Error>>;
}

struct Pending {
    req: Request,
    extractor: BodyExtractorCow<'static>,
    reply: oneshot::Sender<Result<Arc<Response>, Error>>,
}

/// Coalescing HTTP layer. Wraps an inner client; requests matched by any
/// registered [`CoalesceRule`] are batched, others pass through.
pub struct Coalescer {
    inner: Arc<dyn HttpClient>,
    rules: Vec<Arc<dyn CoalesceRule>>,
    groups: Mutex<HashMap<CoalesceKey, mpsc::UnboundedSender<Pending>>>,
}

impl Coalescer {
    pub fn new(inner: Arc<dyn HttpClient>, rules: Vec<Arc<dyn CoalesceRule>>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            rules,
            groups: Mutex::new(HashMap::new()),
        })
    }

    fn pick_rule(&self, req: &Request) -> Option<(Arc<dyn CoalesceRule>, CoalesceKey)> {
        self.rules
            .iter()
            .find_map(|rule| rule.group(req).map(|key| (Arc::clone(rule), key)))
    }

    fn sender_for_group(
        &self,
        key: CoalesceKey,
        rule: Arc<dyn CoalesceRule>,
    ) -> mpsc::UnboundedSender<Pending> {
        let mut groups = self.groups.lock().expect("Coalescer groups Mutex poisoned");
        if let Some(tx) = groups.get(&key) {
            return tx.clone();
        }
        let (tx, rx) = mpsc::unbounded_channel::<Pending>();
        let inner = Arc::clone(&self.inner);
        tokio::spawn(drive_group(rule, inner, rx));
        groups.insert(key, tx.clone());
        tx
    }
}

#[async_trait]
impl HttpClient for Coalescer {
    async fn make_request(
        &self,
        req: Request,
        body_extractor: BodyExtractorCow<'static>,
    ) -> Result<Arc<Response>, Error> {
        let Some((rule, key)) = self.pick_rule(&req) else {
            return self.inner.make_request(req, body_extractor).await;
        };
        let tx = self.sender_for_group(key, rule);
        let (reply_tx, reply_rx) = oneshot::channel();
        if tx
            .send(Pending {
                req,
                extractor: body_extractor,
                reply: reply_tx,
            })
            .is_err()
        {
            return Err(Error::CoalescerInternal("group driver has stopped"));
        }
        reply_rx
            .await
            .unwrap_or_else(|_| Err(Error::CoalescerInternal("dropped reply sender")))
    }
}

/// Driver task per group. Adaptive-wait drain: each loop iteration sleeps for
/// `max_wait * (remaining_capacity / max_batch)` — long when the batch is
/// nearly empty, near-zero when it's nearly full — then drains anything that
/// arrived. Fires when the queue stops growing or `max_batch` is reached.
/// Sleeping (rather than `yield_now`-ing) is required because the real cause
/// of staggering is async I/O upstream (e.g. sqlx cache lookups), which a
/// scheduler-fairness yield can't paper over.
async fn drive_group(
    rule: Arc<dyn CoalesceRule>,
    inner: Arc<dyn HttpClient>,
    mut rx: mpsc::UnboundedReceiver<Pending>,
) {
    let max_batch = rule.max_batch();
    let max_wait = rule.max_wait();
    loop {
        let Some(first) = rx.recv().await else {
            return;
        };
        let mut batch = vec![first];

        loop {
            let remaining = max_batch.saturating_sub(batch.len());
            let factor = remaining as f64 / max_batch.max(1) as f64;
            tokio::time::sleep(max_wait.mul_f64(factor)).await;

            let before = batch.len();
            while batch.len() < max_batch {
                match rx.try_recv() {
                    Ok(p) => batch.push(p),
                    Err(_) => break,
                }
            }
            if batch.len() >= max_batch {
                break; // API ceiling hit
            }
            if batch.len() == before {
                break; // drained dry
            }
        }

        tracing::debug!(
            rule = rule.name(),
            size = batch.len(),
            "coalescer: firing batch"
        );

        let rule = Arc::clone(&rule);
        let inner = Arc::clone(&inner);
        tokio::spawn(async move {
            dispatch_batch(rule, inner, batch).await;
        });
    }
}

async fn dispatch_batch(
    rule: Arc<dyn CoalesceRule>,
    inner: Arc<dyn HttpClient>,
    batch: Vec<Pending>,
) {
    let reqs: Vec<Request> = batch.iter().map(|p| p.req.clone()).collect();
    let merged_req = rule.merge(&reqs);

    let merged_res = match inner
        .make_request(
            merged_req,
            BodyExtractorCow::Borrowed(bytes_body_extractor()),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return fail_all(batch, Arc::new(e)),
    };

    let merged_status = merged_res.status;
    let merged_headers = merged_res.headers.clone();
    let merged_bytes = match merged_res.body_to_bytes().await {
        Ok(b) => b,
        Err(e) => return fail_all(batch, Arc::new(e)),
    };

    let splits = match rule.split(merged_status, &merged_headers, &merged_bytes, &reqs) {
        Ok(s) => s,
        Err(e) => return fail_all(batch, e),
    };

    if splits.len() != reqs.len() {
        tracing::error!(
            "CoalesceRule::split returned {} responses for {} inputs",
            splits.len(),
            reqs.len(),
        );
        return fail_all(
            batch,
            Arc::new(Error::CoalescerInternal("split arity mismatch")),
        );
    }

    for (p, split) in batch.into_iter().zip(splits) {
        let Pending {
            extractor, reply, ..
        } = p;
        let result = match split {
            Some(s) => {
                let raw = RawResponse {
                    status: s.status,
                    headers: Cow::Owned(s.headers),
                    body: reqwest::Body::from(s.body),
                };
                match extractor.extract_response(raw).await {
                    Ok(resp) => Ok(Arc::new(resp)),
                    Err(e) => Err(Error::from(e)),
                }
            }
            None => {
                // Synthetic 404 with empty body: skip the typed extractor so
                // it doesn't fail trying to deserialise zero bytes as JSON.
                // The caller's status check handles the 404.
                let raw = RawResponse {
                    status: ResponseStatus::NOT_FOUND,
                    headers: Cow::Owned(Vec::new()),
                    body: reqwest::Body::from(Bytes::new()),
                };
                match bytes_body_extractor().extract_response(raw).await {
                    Ok(resp) => Ok(Arc::new(resp)),
                    Err(e) => Err(Error::from(e)),
                }
            }
        };
        let _ = reply.send(result);
    }
}

fn fail_all(batch: Vec<Pending>, err: Arc<Error>) {
    for p in batch {
        let _ = p.reply.send(Err(Error::Batch(Arc::clone(&err))));
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

    use async_trait::async_trait;
    use bytes::Bytes;

    use super::{CoalesceKey, CoalesceRule, Coalescer, SplitResponse};
    use crate::http::{
        BodyExtractor, BodyExtractorCow, Error, HeaderName, HeaderValue, HttpClient, Method,
        Request, Response, ResponseStatus,
    };

    /// Test rule: groups by URL path, merges by collecting `id` query params,
    /// splits by emitting one body of `"<id>:<value>"` per request, where
    /// `value` comes from a `HashMap<id, value>` configured per test.
    struct TestRule {
        path: String,
        max_batch: usize,
        values: std::sync::Mutex<std::collections::HashMap<String, String>>,
    }

    impl TestRule {
        fn new(path: &str, max_batch: usize) -> Arc<Self> {
            Arc::new(Self {
                path: path.to_string(),
                max_batch,
                values: std::sync::Mutex::new(std::collections::HashMap::new()),
            })
        }

        fn set(&self, id: &str, value: &str) {
            self.values
                .lock()
                .unwrap()
                .insert(id.to_string(), value.to_string());
        }

        fn extract_id(req: &Request) -> Option<String> {
            reqwest::Url::parse(&req.url)
                .ok()?
                .query_pairs()
                .find(|(k, _)| k == "id")
                .map(|(_, v)| v.into_owned())
        }
    }

    impl CoalesceRule for TestRule {
        fn name(&self) -> &'static str {
            "test_rule"
        }

        fn group(&self, req: &Request) -> Option<CoalesceKey> {
            let url = reqwest::Url::parse(&req.url).ok()?;
            if url.path() != self.path {
                return None;
            }
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            url.host_str().hash(&mut h);
            url.path().hash(&mut h);
            Some(CoalesceKey(h.finish()))
        }

        fn max_batch(&self) -> usize {
            self.max_batch
        }

        fn merge(&self, reqs: &[Request]) -> Request {
            let template = &reqs[0];
            let mut url = reqwest::Url::parse(&template.url).expect("valid template URL");
            let ids: Vec<String> = reqs.iter().filter_map(Self::extract_id).collect();
            let preserved: Vec<(String, String)> = url
                .query_pairs()
                .filter(|(k, _)| k != "id")
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            url.query_pairs_mut().clear();
            for (k, v) in &preserved {
                url.query_pairs_mut().append_pair(k, v);
            }
            url.query_pairs_mut().append_pair("id", &ids.join(","));
            Request {
                method: template.method.clone(),
                url: url.into(),
                headers: template.headers.clone(),
                body: template.body.clone(),
                ..Default::default()
            }
        }

        fn split(
            &self,
            status: ResponseStatus,
            headers: &[(HeaderName, HeaderValue)],
            _merged_body: &Bytes,
            reqs: &[Request],
        ) -> Result<Vec<Option<SplitResponse>>, Arc<Error>> {
            let values = self.values.lock().unwrap();
            Ok(reqs
                .iter()
                .map(|req| {
                    let id = Self::extract_id(req)?;
                    values.get(&id).map(|v| SplitResponse {
                        status,
                        headers: headers.to_vec(),
                        body: Bytes::from(format!("{id}:{v}")),
                    })
                })
                .collect())
        }
    }

    /// Counting inner HTTP client: records every (Method, URL) it sees,
    /// returns an empty 200 body.
    struct CountingClient {
        calls: std::sync::Mutex<Vec<(Method, String)>>,
        delay: Option<Duration>,
    }

    impl CountingClient {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: std::sync::Mutex::new(Vec::new()),
                delay: None,
            })
        }

        fn with_delay(delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                calls: std::sync::Mutex::new(Vec::new()),
                delay: Some(delay),
            })
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }

        fn calls(&self) -> Vec<(Method, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl HttpClient for CountingClient {
        async fn make_request(
            &self,
            req: Request,
            extractor: BodyExtractorCow<'static>,
        ) -> Result<Arc<Response>, Error> {
            self.calls
                .lock()
                .unwrap()
                .push((req.method.clone(), req.url.clone()));
            if let Some(d) = self.delay {
                tokio::time::sleep(d).await;
            }
            let raw = crate::http::RawResponse {
                status: ResponseStatus::OK,
                headers: std::borrow::Cow::Owned(Vec::new()),
                body: reqwest::Body::from(Bytes::from_static(b"")),
            };
            let resp = extractor.extract_response(raw).await?;
            Ok(Arc::new(resp))
        }
    }

    fn req_for(id: &str) -> Request {
        Request {
            method: Method::GET,
            url: format!("http://example.test/api?id={id}"),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn concurrent_requests_coalesce_into_one_call() {
        let inner = CountingClient::new();
        let rule = TestRule::new("/api", 50);
        for id in ["A", "B", "C", "D"] {
            rule.set(id, &format!("val-{id}"));
        }
        let coalescer = Coalescer::new(inner.clone(), vec![rule.clone()]);

        // Fan out 4 concurrent requests via tokio::spawn.
        let mut handles = Vec::new();
        for id in ["A", "B", "C", "D"] {
            let c = Arc::clone(&coalescer) as Arc<dyn HttpClient>;
            handles.push(tokio::spawn(async move { c.get_bytes(req_for(id)).await }));
        }
        for h in handles {
            let resp = h.await.unwrap().unwrap();
            let body = resp.body_to_bytes().await.unwrap();
            let text = std::str::from_utf8(&body).unwrap();
            assert!(text.starts_with(&format!("{}:", &text[..1])));
        }

        // Should have exactly one upstream call.
        assert_eq!(inner.call_count(), 1, "expected single batched call");
        let calls = inner.calls();
        let url = &calls[0].1;
        // The id param should contain all four ids in some order, comma-joined.
        let url_parsed = reqwest::Url::parse(url).unwrap();
        let id = url_parsed
            .query_pairs()
            .find(|(k, _)| k == "id")
            .map(|(_, v)| v.into_owned())
            .unwrap();
        let mut ids: Vec<&str> = id.split(',').collect();
        ids.sort();
        assert_eq!(ids, vec!["A", "B", "C", "D"]);
    }

    #[tokio::test]
    async fn unmatched_requests_pass_through() {
        let inner = CountingClient::new();
        let rule = TestRule::new("/api", 50);
        let coalescer = Coalescer::new(inner.clone(), vec![rule.clone()]);

        let unmatched = Request {
            method: Method::GET,
            url: "http://example.test/other?id=X".into(),
            ..Default::default()
        };
        let _ = (coalescer as Arc<dyn HttpClient>)
            .get_bytes(unmatched)
            .await
            .unwrap();
        assert_eq!(inner.call_count(), 1);
        assert_eq!(inner.calls()[0].1, "http://example.test/other?id=X");
    }

    #[tokio::test]
    async fn missing_id_in_split_yields_404() {
        let inner = CountingClient::new();
        let rule = TestRule::new("/api", 50);
        // Only A and B are populated; C will be missing.
        rule.set("A", "a");
        rule.set("B", "b");
        let coalescer = Coalescer::new(inner.clone(), vec![rule.clone()]);

        let mut handles = Vec::new();
        for id in ["A", "B", "C"] {
            let c = Arc::clone(&coalescer) as Arc<dyn HttpClient>;
            handles.push(tokio::spawn(async move {
                let resp = c.get_bytes(req_for(id)).await.unwrap();
                (id, resp.status, resp.body_to_bytes().await.unwrap())
            }));
        }

        let mut results: Vec<_> = futures::future::join_all(handles)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        results.sort_by_key(|(id, _, _)| *id);

        assert_eq!(results[0].0, "A");
        assert_eq!(results[0].1, ResponseStatus::OK);
        assert_eq!(&*results[0].2, b"A:a");

        assert_eq!(results[1].0, "B");
        assert_eq!(results[1].1, ResponseStatus::OK);
        assert_eq!(&*results[1].2, b"B:b");

        assert_eq!(results[2].0, "C");
        assert_eq!(results[2].1, ResponseStatus::NOT_FOUND);
        assert!(results[2].2.is_empty());

        assert_eq!(inner.call_count(), 1);
    }

    #[tokio::test]
    async fn over_max_batch_chunks_into_multiple_calls() {
        // 7 ids, max_batch = 3 → expect ceil(7/3) = 3 batches.
        let inner = CountingClient::new();
        let rule = TestRule::new("/api", 3);
        let ids: Vec<String> = (0..7).map(|i| format!("id{i}")).collect();
        for id in &ids {
            rule.set(id, "v");
        }
        let coalescer = Coalescer::new(inner.clone(), vec![rule.clone()]);

        let mut handles = Vec::new();
        for id in &ids {
            let c = Arc::clone(&coalescer) as Arc<dyn HttpClient>;
            let id = id.clone();
            handles.push(tokio::spawn(async move { c.get_bytes(req_for(&id)).await }));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }

        let calls = inner.calls();
        assert_eq!(
            calls.len(),
            3,
            "expected 3 batched calls for 7 ids @ 3/call"
        );

        // Sum of id-counts across calls equals 7.
        let mut total_ids = 0;
        for (_, url) in &calls {
            let u = reqwest::Url::parse(url).unwrap();
            let id = u
                .query_pairs()
                .find(|(k, _)| k == "id")
                .map(|(_, v)| v.into_owned())
                .unwrap();
            total_ids += id.split(',').count();
        }
        assert_eq!(total_ids, 7);
    }

    #[tokio::test]
    async fn sequential_requests_are_not_coalesced() {
        // No fan-out — issue requests one after the other, each fully completing
        // before the next. The driver should fire each as a single-id batch.
        let inner = CountingClient::with_delay(Duration::from_millis(5));
        let rule = TestRule::new("/api", 50);
        for id in ["A", "B"] {
            rule.set(id, "v");
        }
        let coalescer: Arc<dyn HttpClient> = Coalescer::new(inner.clone(), vec![rule.clone()]);

        coalescer.get_bytes(req_for("A")).await.unwrap();
        coalescer.get_bytes(req_for("B")).await.unwrap();

        assert_eq!(
            inner.call_count(),
            2,
            "sequential requests should produce two separate calls"
        );
    }

    #[tokio::test]
    async fn failed_batch_propagates_to_all_waiters() {
        // Inner client that always errors.
        struct FailingClient {
            calls: AtomicUsize,
        }
        #[async_trait]
        impl HttpClient for FailingClient {
            async fn make_request(
                &self,
                _req: Request,
                _extractor: BodyExtractorCow<'static>,
            ) -> Result<Arc<Response>, Error> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Err(Error::HttpStatus(ResponseStatus::INTERNAL_SERVER_ERROR))
            }
        }
        let inner = Arc::new(FailingClient {
            calls: AtomicUsize::new(0),
        });
        let rule = TestRule::new("/api", 50);
        let coalescer = Coalescer::new(inner.clone(), vec![rule.clone()]);

        let mut handles = Vec::new();
        for id in ["A", "B", "C"] {
            let c = Arc::clone(&coalescer) as Arc<dyn HttpClient>;
            handles.push(tokio::spawn(async move { c.get_bytes(req_for(id)).await }));
        }
        let results = futures::future::join_all(handles).await;
        for r in results {
            let r = r.unwrap();
            match r {
                Err(Error::Batch(inner)) => match &*inner {
                    Error::HttpStatus(s) if *s == ResponseStatus::INTERNAL_SERVER_ERROR => {}
                    other => panic!("expected inner HttpStatus(500), got {other:?}"),
                },
                other => panic!("expected Error::Batch, got {other:?}"),
            }
        }
        // Single batched call, single failure, three waiters all see Batch.
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
    }
}
