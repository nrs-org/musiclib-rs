//! Request-coalescing HTTP layer.
//!
//! Sits below caches and above the scheduler. Per-endpoint [`CoalesceRule`]s
//! merge single-entity requests into one batched upstream call and split the
//! response back. See `BATCHING_PROVIDERS.md` for the design.
//!
//! # When a batch fires
//!
//! - **Full** batches (`max_batch` distinct requests queued) fire immediately.
//! - **Partial** batches fire only when the process is idle per [`Activity`]:
//!   every piece of running work is parked in some coalescer queue, so waiting
//!   any longer can't add ids to any batch. At that point the coordinator fires
//!   *one* partial batch — the fullest — and waits for idleness again, since
//!   that batch's response may feed ids into the others.
//! - An optional `max_hold` backstop fires a batch whose oldest request has
//!   waited that long regardless of activity (liveness against unrelated work
//!   that keeps the process permanently busy).
//!
//! Identical requests (same URL) queued in one group share a single slot in the
//! merged call; every waiter gets its own copy of the split response.

use std::{
    borrow::Cow,
    collections::HashMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::oneshot;

use crate::http::{
    BodyExtractor, BodyExtractorCow, Error, HeaderName, HeaderValue, HttpClient, RawResponse,
    Request, Response, ResponseStatus, activity::Activity, bytes_body_extractor,
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

    /// Merge `reqs` (all sharing the same [`CoalesceKey`], pairwise-distinct
    /// URLs) into one upstream request. `1 <= reqs.len() <= self.max_batch()`.
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

/// One caller blocked on a queued request.
struct Waiter {
    extractor: BodyExtractorCow<'static>,
    reply: oneshot::Sender<Result<Arc<Response>, Error>>,
    /// Shared with the waiter's [`Parked`] token: whoever flips it first
    /// re-increments the activity counter the waiter gave up when it parked.
    restored: Arc<AtomicBool>,
}

impl Waiter {
    fn restore(&self, activity: &Activity) {
        if !self.restored.swap(true, Ordering::SeqCst) {
            activity.inc();
        }
    }
}

/// A distinct request queued in a group, with everyone waiting on it.
struct Entry {
    req: Request,
    waiters: Vec<Waiter>,
    since: Instant,
}

struct Group {
    rule: Arc<dyn CoalesceRule>,
    /// Distinct requests in arrival order.
    entries: Vec<Entry>,
    /// URL → index into `entries`, for sharing a slot between identical requests.
    index: HashMap<String, usize>,
}

impl Group {
    fn fill(&self) -> f64 {
        self.entries.len() as f64 / self.rule.max_batch().max(1) as f64
    }

    fn oldest(&self) -> Option<Instant> {
        self.entries.first().map(|e| e.since)
    }

    /// Remove and return up to `max_batch` entries, dropping waiters (and
    /// entries) whose callers have gone away.
    fn take_batch(&mut self) -> Vec<Entry> {
        let n = self.entries.len().min(self.rule.max_batch().max(1));
        let mut batch: Vec<Entry> = self.entries.drain(..n).collect();
        self.index = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.req.url.clone(), i))
            .collect();
        for entry in &mut batch {
            entry.waiters.retain(|w| !w.reply.is_closed());
        }
        batch.retain(|e| !e.waiters.is_empty());
        batch
    }
}

struct Shared {
    inner: Arc<dyn HttpClient>,
    rules: Vec<Arc<dyn CoalesceRule>>,
    activity: Activity,
    max_hold: Option<Duration>,
    groups: Mutex<HashMap<CoalesceKey, Group>>,
    coordinator_started: AtomicBool,
}

impl Shared {
    fn fire(&self, rule: Arc<dyn CoalesceRule>, batch: Vec<Entry>) {
        if batch.is_empty() {
            return;
        }
        tracing::debug!(
            rule = rule.name(),
            size = batch.len(),
            "coalescer: firing batch"
        );
        // Taken synchronously so the process never looks idle between
        // "batch left the queue" and "batch is in flight".
        let guard = self.activity.busy();
        let inner = Arc::clone(&self.inner);
        let activity = self.activity.clone();
        tokio::spawn(async move {
            dispatch_batch(rule, inner, activity, batch).await;
            drop(guard);
        });
    }

    /// One coordinator step: fire backstop-expired batches, then (if idle) the
    /// single fullest partial batch. Returns the next backstop deadline.
    fn step(&self) -> Option<Instant> {
        let mut groups = self.groups.lock().expect("Coalescer groups Mutex poisoned");

        if let Some(hold) = self.max_hold {
            let now = Instant::now();
            for group in groups.values_mut() {
                while group.oldest().is_some_and(|t| t + hold <= now) {
                    let batch = group.take_batch();
                    self.fire(Arc::clone(&group.rule), batch);
                }
            }
        }

        if self.activity.is_idle()
            && let Some(group) = groups
                .values_mut()
                .filter(|g| !g.entries.is_empty())
                .max_by(|a, b| a.fill().total_cmp(&b.fill()))
        {
            let batch = group.take_batch();
            self.fire(Arc::clone(&group.rule), batch);
        }

        groups.retain(|_, g| !g.entries.is_empty());
        let hold = self.max_hold?;
        groups
            .values()
            .filter_map(Group::oldest)
            .min()
            .map(|t| t + hold)
    }
}

/// Coalescing HTTP layer. Wraps an inner client; requests matched by any
/// registered [`CoalesceRule`] are batched, others pass through.
///
/// Must sit below an [`Activity::layer`] sharing the same `activity`: parking a
/// request cancels the `+1` that layer holds for it.
pub struct Coalescer {
    shared: Arc<Shared>,
}

impl Coalescer {
    pub fn new(
        inner: Arc<dyn HttpClient>,
        rules: Vec<Arc<dyn CoalesceRule>>,
        activity: Activity,
        max_hold: Option<Duration>,
    ) -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(Shared {
                inner,
                rules,
                activity,
                max_hold,
                groups: Mutex::new(HashMap::new()),
                coordinator_started: AtomicBool::new(false),
            }),
        })
    }

    fn pick_rule(&self, req: &Request) -> Option<(Arc<dyn CoalesceRule>, CoalesceKey)> {
        self.shared
            .rules
            .iter()
            .find_map(|rule| rule.group(req).map(|key| (Arc::clone(rule), key)))
    }

    fn ensure_coordinator(&self) {
        if !self.shared.coordinator_started.swap(true, Ordering::SeqCst) {
            tokio::spawn(coordinate(
                Arc::downgrade(&self.shared),
                self.shared.activity.clone(),
            ));
        }
    }
}

/// A waiter's claim that it is parked: gives up one unit of activity on
/// creation; restores it on drop unless the dispatcher already did.
struct Parked {
    activity: Activity,
    restored: Arc<AtomicBool>,
}

impl Parked {
    fn new(activity: Activity) -> Self {
        activity.dec();
        Self {
            activity,
            restored: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Drop for Parked {
    fn drop(&mut self) {
        if !self.restored.swap(true, Ordering::SeqCst) {
            self.activity.inc();
        }
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
            return self.shared.inner.make_request(req, body_extractor).await;
        };
        self.ensure_coordinator();

        let (reply_tx, reply_rx) = oneshot::channel();
        let parked = {
            let mut groups = self
                .shared
                .groups
                .lock()
                .expect("Coalescer groups Mutex poisoned");
            let group = groups.entry(key).or_insert_with(|| Group {
                rule: Arc::clone(&rule),
                entries: Vec::new(),
                index: HashMap::new(),
            });

            // Park inside the lock, after enqueueing: the coordinator can only
            // observe idleness once this request is visible in its group.
            let parked = Parked::new(self.shared.activity.clone());
            let waiter = Waiter {
                extractor: body_extractor,
                reply: reply_tx,
                restored: Arc::clone(&parked.restored),
            };
            match group.index.get(&req.url) {
                Some(&i) => group.entries[i].waiters.push(waiter),
                None => {
                    group.index.insert(req.url.clone(), group.entries.len());
                    group.entries.push(Entry {
                        req,
                        waiters: vec![waiter],
                        since: Instant::now(),
                    });
                }
            }

            if group.entries.len() >= rule.max_batch() {
                let batch = group.take_batch();
                self.shared.fire(rule, batch);
            }
            parked
        };

        let result = reply_rx
            .await
            .unwrap_or_else(|_| Err(Error::CoalescerInternal("dropped reply sender")));
        drop(parked);
        result
    }
}

/// Coordinator task (one per [`Coalescer`]): wakes every time the process goes
/// idle (or a backstop deadline passes) and fires what [`Shared::step`] allows.
async fn coordinate(shared: Weak<Shared>, activity: Activity) {
    loop {
        // Read the epoch *before* stepping so an idle transition that races
        // with the step still wakes us.
        let epoch = activity.idle_epoch();
        let deadline = {
            let Some(shared) = shared.upgrade() else {
                return;
            };
            shared.step()
        };
        match deadline {
            Some(at) => {
                tokio::select! {
                    _ = activity.idle_since(epoch) => {}
                    _ = tokio::time::sleep_until(at.into()) => {}
                }
            }
            None => activity.idle_since(epoch).await,
        }
    }
}

async fn dispatch_batch(
    rule: Arc<dyn CoalesceRule>,
    inner: Arc<dyn HttpClient>,
    activity: Activity,
    batch: Vec<Entry>,
) {
    let reqs: Vec<Request> = batch.iter().map(|e| e.req.clone()).collect();
    let merged_req = rule.merge(&reqs);

    let merged_res = match inner
        .make_request(
            merged_req,
            BodyExtractorCow::Borrowed(bytes_body_extractor()),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return fail_all(&activity, batch, Arc::new(e)),
    };

    let merged_status = merged_res.status;
    let merged_headers = merged_res.headers.clone();
    let merged_bytes = match merged_res.body_to_bytes().await {
        Ok(b) => b,
        Err(e) => return fail_all(&activity, batch, Arc::new(e)),
    };

    let splits = match rule.split(merged_status, &merged_headers, &merged_bytes, &reqs) {
        Ok(s) => s,
        Err(e) => return fail_all(&activity, batch, e),
    };

    if splits.len() != reqs.len() {
        tracing::error!(
            "CoalesceRule::split returned {} responses for {} inputs",
            splits.len(),
            reqs.len(),
        );
        return fail_all(
            &activity,
            batch,
            Arc::new(Error::CoalescerInternal("split arity mismatch")),
        );
    }

    for (entry, split) in batch.into_iter().zip(splits) {
        for waiter in entry.waiters {
            let (status, headers, body) = match &split {
                Some(s) => (s.status, s.headers.clone(), s.body.clone()),
                None => (ResponseStatus::NOT_FOUND, Vec::new(), Bytes::new()),
            };
            let raw = RawResponse {
                status,
                headers: Cow::Owned(headers),
                body: reqwest::Body::from(body),
            };
            let result = if split.is_some() {
                waiter.extractor.extract_response(raw).await
            } else {
                // Synthetic 404 with empty body: skip the typed extractor so
                // it doesn't fail trying to deserialise zero bytes as JSON.
                // The caller's status check handles the 404.
                bytes_body_extractor().extract_response(raw).await
            }
            .map(Arc::new)
            .map_err(Error::from);
            waiter.restore(&activity);
            let _ = waiter.reply.send(result);
        }
    }
}

fn fail_all(activity: &Activity, batch: Vec<Entry>, err: Arc<Error>) {
    for waiter in batch.into_iter().flat_map(|e| e.waiters) {
        waiter.restore(activity);
        let _ = waiter.reply.send(Err(Error::Batch(Arc::clone(&err))));
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
        Activity, BodyExtractor, BodyExtractorCow, Error, HeaderName, HeaderValue, HttpClient,
        Method, Request, Response, ResponseStatus,
    };

    /// A coalescer behind its activity layer, on a private counter (as
    /// `HttpClientConfig::build` does with the global one).
    fn tracked(inner: Arc<dyn HttpClient>, rule: Arc<dyn CoalesceRule>) -> Arc<dyn HttpClient> {
        tracked_with(inner, vec![rule], Activity::new(), None)
    }

    fn tracked_with(
        inner: Arc<dyn HttpClient>,
        rules: Vec<Arc<dyn CoalesceRule>>,
        activity: Activity,
        max_hold: Option<Duration>,
    ) -> Arc<dyn HttpClient> {
        activity.layer(Coalescer::new(inner, rules, activity.clone(), max_hold))
    }

    fn ids_of(url: &str) -> Vec<String> {
        let mut ids: Vec<String> = reqwest::Url::parse(url)
            .unwrap()
            .query_pairs()
            .find(|(k, _)| k == "id")
            .map(|(_, v)| v.split(',').map(str::to_owned).collect())
            .unwrap_or_default();
        ids.sort();
        ids
    }

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
        in_flight: AtomicUsize,
        peak_in_flight: AtomicUsize,
    }

    impl CountingClient {
        fn new() -> Arc<Self> {
            Self::with_delay_opt(None)
        }

        fn with_delay(delay: Duration) -> Arc<Self> {
            Self::with_delay_opt(Some(delay))
        }

        fn with_delay_opt(delay: Option<Duration>) -> Arc<Self> {
            Arc::new(Self {
                calls: std::sync::Mutex::new(Vec::new()),
                delay,
                in_flight: AtomicUsize::new(0),
                peak_in_flight: AtomicUsize::new(0),
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
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak_in_flight.fetch_max(now, Ordering::SeqCst);
            if let Some(d) = self.delay {
                tokio::time::sleep(d).await;
            }
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
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
        let coalescer = tracked(inner.clone(), rule.clone());

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
        let coalescer = tracked(inner.clone(), rule.clone());

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
        let coalescer = tracked(inner.clone(), rule.clone());

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
        let coalescer = tracked(inner.clone(), rule.clone());

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
        let coalescer = tracked(inner.clone(), rule.clone());

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
        let coalescer = tracked(inner.clone(), rule.clone());

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

    #[tokio::test]
    async fn partial_batch_waits_for_running_work() {
        // Root: request A, and concurrently a slow non-batchable call whose
        // result leads to request B. A must not fire alone while the slow call
        // is still running — it could (and does) add B to A's batch.
        let inner = CountingClient::with_delay(Duration::from_millis(50));
        let rule = TestRule::new("/api", 50);
        rule.set("A", "a");
        rule.set("B", "b");
        let activity = Activity::new();
        let c = tracked_with(inner.clone(), vec![rule], activity.clone(), None);

        let slow = Request {
            method: Method::GET,
            url: "http://example.test/slow".into(),
            ..Default::default()
        };
        let (a, b) = activity
            .track(futures::future::join(c.get_bytes(req_for("A")), async {
                c.get_bytes(slow).await.unwrap();
                c.get_bytes(req_for("B")).await
            }))
            .await;
        assert_eq!(&*a.unwrap().body_to_bytes().await.unwrap(), b"A:a");
        assert_eq!(&*b.unwrap().body_to_bytes().await.unwrap(), b"B:b");

        let calls = inner.calls();
        assert_eq!(calls.len(), 2, "slow call + one merged batch: {calls:?}");
        assert_eq!(ids_of(&calls[1].1), vec!["A", "B"]);
    }

    #[tokio::test]
    async fn identical_requests_share_a_slot() {
        let inner = CountingClient::new();
        let rule = TestRule::new("/api", 50);
        rule.set("A", "a");
        rule.set("B", "b");
        let c = tracked(inner.clone(), rule);

        let (a1, a2, b) = futures::future::join3(
            c.get_bytes(req_for("A")),
            c.get_bytes(req_for("A")),
            c.get_bytes(req_for("B")),
        )
        .await;
        assert_eq!(&*a1.unwrap().body_to_bytes().await.unwrap(), b"A:a");
        assert_eq!(&*a2.unwrap().body_to_bytes().await.unwrap(), b"A:a");
        assert_eq!(&*b.unwrap().body_to_bytes().await.unwrap(), b"B:b");

        let calls = inner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(ids_of(&calls[0].1), vec!["A", "B"]);
    }

    #[tokio::test]
    async fn duplicates_do_not_count_towards_max_batch() {
        // max_batch 2: A, A, B is two distinct ids → exactly one full batch.
        let inner = CountingClient::new();
        let rule = TestRule::new("/api", 2);
        rule.set("A", "a");
        rule.set("B", "b");
        let c = tracked(inner.clone(), rule);
        let _ = futures::future::join3(
            c.get_bytes(req_for("A")),
            c.get_bytes(req_for("A")),
            c.get_bytes(req_for("B")),
        )
        .await;
        assert_eq!(inner.call_count(), 1);
    }

    #[tokio::test]
    async fn idle_fires_one_partial_group_at_a_time() {
        // Two groups each holding a partial batch: the first one's response
        // could feed the second, so they must not be in flight together.
        let inner = CountingClient::with_delay(Duration::from_millis(20));
        let r1 = TestRule::new("/api", 50);
        let r2 = TestRule::new("/other", 50);
        r1.set("A", "a");
        r2.set("B", "b");
        let c = tracked_with(inner.clone(), vec![r1, r2], Activity::new(), None);
        let other = Request {
            method: Method::GET,
            url: "http://example.test/other?id=B".into(),
            ..Default::default()
        };
        let (a, b) = futures::future::join(c.get_bytes(req_for("A")), c.get_bytes(other)).await;
        a.unwrap();
        b.unwrap();
        assert_eq!(inner.call_count(), 2);
        assert_eq!(inner.peak_in_flight.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn max_hold_backstop_fires_while_busy() {
        let inner = CountingClient::new();
        let rule = TestRule::new("/api", 50);
        rule.set("A", "a");
        let activity = Activity::new();
        let c = tracked_with(
            inner.clone(),
            vec![rule],
            activity.clone(),
            Some(Duration::from_millis(20)),
        );
        // Unrelated work that never finishes: without the backstop A would
        // wait forever.
        let _busy = activity.busy();
        let res = tokio::time::timeout(Duration::from_secs(5), c.get_bytes(req_for("A")))
            .await
            .expect("backstop should have fired");
        assert_eq!(&*res.unwrap().body_to_bytes().await.unwrap(), b"A:a");
    }

    #[tokio::test]
    async fn cancelled_waiter_restores_activity() {
        let inner = CountingClient::new();
        let rule = TestRule::new("/api", 50);
        let activity = Activity::new();
        let c = tracked_with(inner.clone(), vec![rule], activity.clone(), None);

        // Hold the process busy so the request stays parked.
        let busy = activity.busy();
        let handle = tokio::spawn({
            let c = Arc::clone(&c);
            async move { c.get_bytes(req_for("A")).await }
        });
        tokio::task::yield_now().await;
        handle.abort();
        let _ = handle.await;
        // Layer's +1 and park's -1 must both be undone: still exactly `busy`.
        assert!(!activity.is_idle());
        drop(busy);
        assert!(activity.is_idle());
    }
}
