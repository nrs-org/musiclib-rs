use std::io::{self, Write};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use crate::http::{BodyExtractorCow, Error, HttpClient, Request, Response};
use async_trait::async_trait;
use dashmap::DashMap;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use serde::Serialize;

struct DomainStats {
    bar: ProgressBar,
    in_flight: AtomicUsize,
    done: AtomicUsize,
}

impl DomainStats {
    fn new(multi: &MultiProgress, host: &str) -> Self {
        let bar = multi.add(ProgressBar::new_spinner());
        bar.set_style(
            ProgressStyle::default_spinner()
                .template("  {spinner} {prefix:<35} {msg}")
                .unwrap(),
        );
        bar.set_prefix(host.to_string());
        bar.enable_steady_tick(Duration::from_millis(80));
        Self {
            bar,
            in_flight: AtomicUsize::new(0),
            done: AtomicUsize::new(0),
        }
    }

    fn update(&self) {
        let in_flight = self.in_flight.load(Ordering::Relaxed);
        let done = self.done.load(Ordering::Relaxed);
        self.bar
            .set_message(format!("{in_flight:>3} in-flight  {done} done"));
    }
}

pub struct ProgressHttpClient {
    inner: Arc<dyn HttpClient>,
    domains: Arc<DashMap<String, DomainStats>>,
    pub multi: Arc<MultiProgress>,
}

impl ProgressHttpClient {
    pub fn new(inner: Arc<dyn HttpClient>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            domains: Arc::new(DashMap::new()),
            multi: Arc::new(MultiProgress::new()),
        })
    }
}

fn extract_host(url: &str) -> &str {
    url.split_once("://")
        .and_then(|(_, rest)| rest.split('/').next())
        .unwrap_or("unknown")
}

#[async_trait]
impl HttpClient for ProgressHttpClient {
    async fn make_request(
        &self,
        req: Request,
        body_extractor: BodyExtractorCow<'static>,
    ) -> Result<Arc<Response>, Error> {
        let host = extract_host(&req.url).to_string();

        // Drop the Ref before the .await.
        {
            self.domains
                .entry(host.clone())
                .or_insert_with(|| DomainStats::new(&self.multi, &host));
            let stats = self.domains.get(&host).unwrap();
            stats.in_flight.fetch_add(1, Ordering::Relaxed);
            stats.update();
        }

        let result = self.inner.make_request(req, body_extractor).await;

        {
            let stats = self.domains.get(&host).unwrap();
            stats.in_flight.fetch_sub(1, Ordering::Relaxed);
            stats.done.fetch_add(1, Ordering::Relaxed);
            stats.update();
        }

        result
    }
}

// ── headless activity tracking (server binary's job progress) ──────────────

/// One domain's live request counts, as returned by `HttpActivityClient::snapshot`.
#[derive(Clone, Serialize)]
pub struct DomainActivity {
    pub host: String,
    pub in_flight: usize,
    pub done: usize,
}

struct DomainCounts {
    in_flight: AtomicUsize,
    done: AtomicUsize,
}

/// Same per-domain in-flight/done counting as `ProgressHttpClient`, without
/// the `indicatif` terminal bars — for a process with no terminal to draw
/// them on (the `server` binary), which instead exposes `snapshot()` to embed
/// in a polled job's progress payload. One instance wraps the whole process's
/// shared `HttpClient`, so a snapshot reflects every request in flight across
/// every concurrently-running job, not just the one polling it — acceptable
/// for what's documented elsewhere as a local, single-user server.
pub struct HttpActivityClient {
    inner: Arc<dyn HttpClient>,
    domains: DashMap<String, DomainCounts>,
}

impl HttpActivityClient {
    pub fn new(inner: Arc<dyn HttpClient>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            domains: DashMap::new(),
        })
    }

    /// Domains with an in-flight request first, then by host name, so a
    /// truncated display shows what's actually happening right now.
    pub fn snapshot(&self) -> Vec<DomainActivity> {
        let mut out: Vec<DomainActivity> = self
            .domains
            .iter()
            .map(|e| DomainActivity {
                host: e.key().clone(),
                in_flight: e.in_flight.load(Ordering::Relaxed),
                done: e.done.load(Ordering::Relaxed),
            })
            .collect();
        out.sort_by(|a, b| {
            b.in_flight
                .cmp(&a.in_flight)
                .then_with(|| a.host.cmp(&b.host))
        });
        out
    }
}

#[async_trait]
impl HttpClient for HttpActivityClient {
    async fn make_request(
        &self,
        req: Request,
        body_extractor: BodyExtractorCow<'static>,
    ) -> Result<Arc<Response>, Error> {
        let host = extract_host(&req.url).to_string();

        self.domains
            .entry(host.clone())
            .or_insert_with(|| DomainCounts {
                in_flight: AtomicUsize::new(0),
                done: AtomicUsize::new(0),
            });
        self.domains
            .get(&host)
            .unwrap()
            .in_flight
            .fetch_add(1, Ordering::Relaxed);

        let result = self.inner.make_request(req, body_extractor).await;

        let counts = self.domains.get(&host).unwrap();
        counts.in_flight.fetch_sub(1, Ordering::Relaxed);
        counts.done.fetch_add(1, Ordering::Relaxed);

        result
    }
}

// ── tracing integration ──────────────────────────────────────────────────────

/// Buffers a single tracing event and flushes it via `MultiProgress::println`
/// on drop, which suspends the bars, prints the line, then redraws them.
pub struct MultiProgressWriter {
    multi: Arc<MultiProgress>,
    buf: Vec<u8>,
}

impl Write for MultiProgressWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for MultiProgressWriter {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let s = String::from_utf8_lossy(&self.buf);
        // println adds its own newline, strip the trailing one from the formatter
        let line = s.trim_end_matches('\n');
        if !line.is_empty() {
            let _ = self.multi.println(line);
        }
    }
}

/// `MakeWriter` impl for `tracing_subscriber::fmt`. Pass to
/// `.with_writer(MultiProgressMakeWriter::new(arc_multi))`.
pub struct MultiProgressMakeWriter {
    multi: Arc<MultiProgress>,
}

impl MultiProgressMakeWriter {
    pub fn new(multi: Arc<MultiProgress>) -> Self {
        Self { multi }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for MultiProgressMakeWriter {
    type Writer = MultiProgressWriter;

    fn make_writer(&'a self) -> Self::Writer {
        MultiProgressWriter {
            multi: Arc::clone(&self.multi),
            buf: Vec::new(),
        }
    }
}
