// Memory-backed HTTP cache

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use dashmap::DashMap;

use crate::http::{BodyExtractorCow, Response};

// A `moka::sync::Cache`-based bound was tried here and reverted: it caused a
// reproducible whole-process hang partway through an import (no panic, no
// DB-level lock — `BEGIN IMMEDIATE` against the DB file from a separate
// `sqlite3` process succeeded instantly while the import sat frozen), on two
// different talents right after switching to it. moka's sync API does
// blocking-capable bookkeeping (eviction, etc.) that isn't meant to be driven
// from inside an async fn the way this trait requires; that's the leading
// suspect, but it wasn't root-caused before reverting. Replaced with a
// hand-rolled approximate LRU below, built only from primitives already used
// safely elsewhere in this codebase (`DashMap`, an atomic counter) — no
// background threads, no cross-task coordination, nothing that can deadlock
// the way the moka swap did.

/// Cap on distinct cached responses (see the module doc above for why a
/// bound matters at all: a single big import can otherwise hold tens of
/// thousands of full response bodies alive for the whole process).
const MAX_CACHED_RESPONSES: usize = 20_000;
/// Batch eviction down to this many on overflow, instead of evicting one
/// entry per insert — `DashMap` has no ordered index, so finding LRU victims
/// means a full scan+sort; batching amortizes that cost across ~2000 inserts.
const EVICT_TO: usize = 18_000;

pub struct MemoryHttpCache {
    storage: DashMap<String, (Arc<Response>, u64)>,
    clock: AtomicU64,
}

impl Default for MemoryHttpCache {
    fn default() -> Self {
        Self {
            storage: DashMap::new(),
            clock: AtomicU64::new(0),
        }
    }
}

impl MemoryHttpCache {
    fn tick(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::Relaxed)
    }

    fn maybe_evict(&self) {
        if self.storage.len() <= MAX_CACHED_RESPONSES {
            return;
        }
        let mut entries: Vec<(String, u64)> = self
            .storage
            .iter()
            .map(|e| (e.key().clone(), e.value().1))
            .collect();
        entries.sort_unstable_by_key(|(_, tick)| *tick);
        let to_remove = entries.len().saturating_sub(EVICT_TO);
        for (key, _) in entries.into_iter().take(to_remove) {
            self.storage.remove(&key);
        }
    }
}

#[async_trait]
impl super::HttpCache for MemoryHttpCache {
    async fn set(
        &self,
        key: String,
        _method: &crate::http::Method,
        _policy: Option<&super::CachePolicy>,
        value: Arc<Response>,
    ) -> Result<(), super::Error> {
        let tick = self.tick();
        self.storage.insert(key, (value, tick));
        self.maybe_evict();
        Ok(())
    }

    async fn get(
        &self,
        key: &str,
        _extractor: BodyExtractorCow<'static>,
    ) -> Result<Option<Arc<Response>>, super::Error> {
        let tick = self.tick();
        Ok(self.storage.get_mut(key).map(|mut entry| {
            entry.1 = tick;
            entry.0.clone()
        }))
    }
}
