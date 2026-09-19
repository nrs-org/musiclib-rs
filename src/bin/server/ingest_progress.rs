//! Feeds a running `ingest` job's `JobProgress.detail` from the import
//! pipeline's live per-pair fetch events (`pipeline::state::ImportProgress`)
//! plus the process-wide HTTP activity snapshot (`pipeline::progress::
//! HttpActivityClient`) — together, close to what the `import` CLI shows via
//! its per-domain progress bars and `debug!` fetch logs, but polled instead
//! of drawn to a terminal. See `player.rs`/`store_protocol.rs`'s `ingest` for
//! how this is wired to a job.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use musiclib_rs::pipeline::{progress::HttpActivityClient, state::ImportProgress};
use serde_json::json;

use crate::jobs::JobHandle;

type Pair = (String, String);

/// Recent fetch-event lines kept for display — enough to show what's
/// currently happening without the payload growing over a long-running
/// import (matches `MAX_SHOWN` order-of-magnitude on the frontend side).
const RECENT_LINES: usize = 12;

/// Failures get their own, much larger buffer than `recent`: on a big
/// channel import, hundreds of successful fetches can push every failure out
/// of a 12-line rolling window long before anyone looks at it (this is
/// exactly what happened investigating a stuck `@ShirakamiFubuki` import —
/// the job's live view only ever showed the last dozen lines, mostly
/// successes, so the specific failures were unrecoverable once the job
/// finished). Capped rather than unbounded so a pathological run (a channel
/// that's mostly dead links) can't grow the polled JSON payload without
/// bound; `entries_failed` in the report stays an exact, uncapped count even
/// once this list is full.
const MAX_FAILURES: usize = 100;

pub struct JobProgressSink {
    handle: JobHandle,
    activity: Arc<HttpActivityClient>,
    /// Shared with every other job (and the CLI's own counter is per-run) —
    /// a process-wide running total, same scope as `activity`. See
    /// `AppState::youtube_quota`.
    youtube_quota: Arc<AtomicU64>,
    fetched: AtomicU64,
    failed: AtomicU64,
    recent: Mutex<VecDeque<String>>,
    failures: Mutex<Vec<String>>,
}

impl JobProgressSink {
    pub fn new(
        handle: JobHandle,
        activity: Arc<HttpActivityClient>,
        youtube_quota: Arc<AtomicU64>,
    ) -> Arc<Self> {
        Arc::new(Self {
            handle,
            activity,
            youtube_quota,
            fetched: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            recent: Mutex::new(VecDeque::with_capacity(RECENT_LINES)),
            failures: Mutex::new(Vec::new()),
        })
    }

    fn push_line(&self, line: String) {
        let mut recent = self.recent.lock().unwrap();
        if recent.len() == RECENT_LINES {
            recent.pop_front();
        }
        recent.push_back(line);
    }

    fn report(&self, message: String) {
        let recent: Vec<String> = self.recent.lock().unwrap().iter().cloned().collect();
        let failures = self.failures.lock().unwrap().clone();
        self.handle.progress_detail(
            "fetching",
            message,
            Some(json!({
                "entries_fetched": self.fetched.load(Ordering::Relaxed),
                "entries_failed": self.failed.load(Ordering::Relaxed),
                "recent": recent,
                "failures": failures,
                "http": self.activity.snapshot(),
                "youtube_quota_units": self.youtube_quota.load(Ordering::Relaxed),
            })),
        );
    }
}

impl ImportProgress for JobProgressSink {
    fn fetching(&self, pair: &Pair) {
        let line = format!("fetching {}:{}", pair.0, pair.1);
        self.push_line(line.clone());
        self.report(line);
    }

    fn fetched(&self, pair: &Pair) {
        let n = self.fetched.fetch_add(1, Ordering::Relaxed) + 1;
        self.push_line(format!("✓ {}:{}", pair.0, pair.1));
        self.report(format!(
            "{n} entr{} fetched",
            if n == 1 { "y" } else { "ies" }
        ));
    }

    fn fetch_failed(&self, pair: &Pair, error: &str) {
        self.failed.fetch_add(1, Ordering::Relaxed);
        let line = format!("{}:{}: {}", pair.0, pair.1, error);
        {
            let mut failures = self.failures.lock().unwrap();
            if failures.len() < MAX_FAILURES {
                failures.push(line.clone());
            }
        }
        self.push_line(format!("✕ {line}"));
        self.report(format!("✕ {line}"));
    }
}
