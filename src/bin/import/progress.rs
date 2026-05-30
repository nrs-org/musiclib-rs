use std::io::{self, Write};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use musiclib_rs::http::{BodyExtractorCow, Error, HttpClient, Request, Response};

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
