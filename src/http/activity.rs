//! Process-wide "is anything still running?" signal driving the [`Coalescer`].
//!
//! The coalescer's contract is: **never send a partial batch while any
//! still-running work could add ids to it without first waiting on that
//! batch.** Work that is parked in a coalescer queue can't add anything until
//! a batch fires; everything else can. So a partial batch may fire exactly when
//! every piece of work is parked — i.e. when this counter says the process is
//! idle.
//!
//! The counter is a signed sum of:
//!
//! - `+1` while a [`track`](Activity::track)ed root future is being polled.
//!   Traversal CPU work between awaits (parsing a response, spawning child
//!   fetches) happens inside such a poll, so it is covered with no gaps.
//! - `+1` while a request is anywhere inside the HTTP stack
//!   ([`layer`](Activity::layer), the outermost layer): cache lookups, scheduler
//!   queues, retry backoff, network I/O.
//! - `-1` while a request sits parked in a coalescer queue (cancelling that
//!   request's `+1` from the layer above), restored *by the dispatcher* before
//!   the reply is sent so there's no window between "reply sent" and "waiter
//!   polled again" where the process looks idle.
//! - `+1` for each in-flight merged upstream call — its response may itself
//!   contain ids for another partial batch.
//! - `+1` for any [`busy`](Activity::busy) guard (other async I/O that can lead
//!   to new requests).
//!
//! Idle ⇔ counter ≤ 0. Work that isn't tracked only ever makes the coalescer
//! fire *earlier* (a gap looks like idleness), never hang.
//!
//! [`Coalescer`]: super::Coalescer

use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicIsize, AtomicU64, Ordering},
    },
    task::{Context, Poll},
};

use async_trait::async_trait;
use tokio::sync::Notify;

use super::{BodyExtractorCow, Error, HttpClient, Request, Response};

#[derive(Default)]
struct Inner {
    count: AtomicIsize,
    /// Bumped every time the counter drops to ≤ 0. Waiters compare epochs
    /// rather than re-reading `count`, so a brief idle moment is never missed.
    idle_epoch: AtomicU64,
    notify: Notify,
}

/// Cheaply clonable handle to an activity counter.
#[derive(Clone, Default)]
pub struct Activity(Arc<Inner>);

impl Activity {
    pub fn new() -> Self {
        Self::default()
    }

    /// The process-wide counter. [`HttpClientConfig::build`] wires its layers
    /// and coalescer to this one, and the import pipeline tracks its
    /// traversal roots with it.
    ///
    /// [`HttpClientConfig::build`]: super::HttpClientConfig::build
    pub fn global() -> &'static Activity {
        static GLOBAL: OnceLock<Activity> = OnceLock::new();
        GLOBAL.get_or_init(Activity::new)
    }

    pub fn is_idle(&self) -> bool {
        self.0.count.load(Ordering::SeqCst) <= 0
    }

    /// Mark the process busy until the guard is dropped.
    pub fn busy(&self) -> BusyGuard {
        self.inc();
        BusyGuard(self.clone())
    }

    /// Count `fut` as running while (and only while) it is being polled.
    /// Wrap the root of any traversal that issues coalescable requests.
    pub fn track<F: Future>(&self, fut: F) -> Tracked<F> {
        Tracked {
            activity: self.clone(),
            fut: Box::pin(fut),
        }
    }

    /// Wrap `inner` so every request counts as running while inside it. Must
    /// be the outermost layer above any [`Coalescer`](super::Coalescer)
    /// sharing this counter.
    pub fn layer(&self, inner: Arc<dyn HttpClient>) -> Arc<dyn HttpClient> {
        Arc::new(ActivityLayer {
            inner,
            activity: self.clone(),
        })
    }

    pub(crate) fn inc(&self) {
        self.0.count.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn dec(&self) {
        if self.0.count.fetch_sub(1, Ordering::SeqCst) <= 1 {
            self.0.idle_epoch.fetch_add(1, Ordering::SeqCst);
            self.0.notify.notify_waiters();
        }
    }

    pub(crate) fn idle_epoch(&self) -> u64 {
        self.0.idle_epoch.load(Ordering::SeqCst)
    }

    /// Resolve once the process has gone idle (again) since `epoch` was read.
    pub(crate) async fn idle_since(&self, epoch: u64) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.idle_epoch() != epoch {
                return;
            }
            notified.await;
        }
    }
}

/// See [`Activity::busy`].
pub struct BusyGuard(Activity);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.dec();
    }
}

/// See [`Activity::track`].
pub struct Tracked<F> {
    activity: Activity,
    fut: Pin<Box<F>>,
}

impl<F: Future> Future for Tracked<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let _guard = self.activity.busy();
        self.fut.as_mut().poll(cx)
    }
}

struct ActivityLayer {
    inner: Arc<dyn HttpClient>,
    activity: Activity,
}

#[async_trait]
impl HttpClient for ActivityLayer {
    async fn make_request(
        &self,
        req: Request,
        body_extractor: BodyExtractorCow<'static>,
    ) -> Result<Arc<Response>, Error> {
        let _guard = self.activity.busy();
        self.inner.make_request(req, body_extractor).await
    }
}
