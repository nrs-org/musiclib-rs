use std::collections::{HashMap, HashSet, hash_map::Entry};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::providers::types::{Alias, Contribution, EntrySpecificData, EntryType};

/// A `(source_key, identifier)` pair. The only unit of identity during import.
pub type Pair = (String, String);

/// Per-pair observations collected from a single fetch. Two pairs in the same
/// equivalence class each get their own `PairMetadata`; nothing is reconciled
/// at write time.
pub struct PairMetadata {
    pub entry_type: EntryType,
    pub release_date: Option<String>,
    pub specific_data: EntrySpecificData,
    pub aliases: Vec<Alias>,
}

/// What a parent's listing said about a child (from its `ChildRef`). Kept so a
/// child that never gets fetched is still stored as a stub with a type, name
/// and duration instead of an anonymous pair. Ignored at flush time for pairs
/// that were fetched.
#[derive(Clone, Debug, PartialEq)]
pub struct StubInfo {
    pub entry_type: EntryType,
    pub name: Option<String>,
    pub duration_ms: Option<i64>,
}

/// A "parent has child" edge between two pairs, with the structural position
/// and any role contributions attached to that edge.
pub struct ChildEdge {
    pub parent: Pair,
    pub child: Pair,
    pub disc_no: Option<i32>,
    pub track_no: Option<i32>,
    pub contributions: Vec<Contribution>,
    /// Copied from `ChildRef::original_relation_kind`; when set, `child` is
    /// the *original* and `parent` is the `kind`-transformation of it.
    pub original_relation_kind: Option<String>,
}

/// Live observer for a single `import()` traversal — optional, purely
/// diagnostic (never consulted for correctness). Lets a caller that cares
/// about progress (the `server` binary's job UI) watch per-pair fetch events
/// as they happen instead of only seeing the final `State` once `import()`
/// resolves. Default bodies are no-ops so an implementor only overrides the
/// events it displays.
pub trait ImportProgress: Send + Sync {
    fn fetching(&self, _pair: &Pair) {}
    fn fetched(&self, _pair: &Pair) {}
    fn fetch_failed(&self, _pair: &Pair, _error: &str) {}
}

/// Shared import state. All fields are append-only from the importer's
/// perspective; `flush` consumes them (and, for a periodic/non-final flush,
/// can hand some of it back — see `pipeline::flush`'s deferral of classes
/// that touch pre-existing entries).
pub struct State {
    claimed: Mutex<HashSet<Pair>>,
    pub metadata: Mutex<HashMap<Pair, PairMetadata>>,
    pub is_rel: Mutex<Vec<(Pair, Pair)>>,
    pub has_rel: Mutex<Vec<ChildEdge>>,
    pub stubs: Mutex<HashMap<Pair, StubInfo>>,
    /// Running estimate, in bytes, of everything currently buffered in
    /// `metadata`/`is_rel`/`has_rel`. This is what `pipeline::ingest`'s
    /// periodic-flush watchdog watches to decide when to drain the buffer.
    ///
    /// It's tracked in-process rather than sampled from OS-level RSS on
    /// purpose: RSS is a whole-process figure confounded by things that have
    /// nothing to do with this buffer (HTTP response buffering, allocator
    /// arenas, tokio runtime overhead), and glibc's allocator typically
    /// doesn't return freed pages to the OS, so RSS can stay high right after
    /// a flush and make a watchdog re-trigger in a tight loop instead of
    /// settling back down. This counter instead measures exactly what a
    /// flush would drain, so it's deterministic, portable, and directly tied
    /// to the thing actually being bounded.
    approx_bytes: AtomicUsize,
    progress: Option<Arc<dyn ImportProgress>>,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    pub fn new() -> Self {
        Self::with_progress(None)
    }

    pub fn with_progress(progress: Option<Arc<dyn ImportProgress>>) -> Self {
        Self {
            claimed: Mutex::new(HashSet::new()),
            metadata: Mutex::new(HashMap::new()),
            is_rel: Mutex::new(Vec::new()),
            has_rel: Mutex::new(Vec::new()),
            stubs: Mutex::new(HashMap::new()),
            approx_bytes: AtomicUsize::new(0),
            progress,
        }
    }

    /// Returns `true` if this call is the first to claim `pair` (caller should
    /// fetch). Returns `false` if another task already owns it.
    pub fn claim(&self, pair: &Pair) -> bool {
        self.claimed.lock().unwrap().insert(pair.clone())
    }

    /// Record a pair's fetched metadata. Prefer this over locking `metadata`
    /// directly so the size counter stays accurate.
    pub fn insert_metadata(&self, pair: Pair, meta: PairMetadata) {
        let size = pair_bytes(&pair) + metadata_bytes(&meta);
        self.metadata.lock().unwrap().insert(pair, meta);
        self.approx_bytes.fetch_add(size, Ordering::Relaxed);
    }

    /// Record an is_rel edge. Prefer this over locking `is_rel` directly so
    /// the size counter stays accurate.
    pub fn push_is_rel(&self, a: Pair, b: Pair) {
        let size = pair_bytes(&a) + pair_bytes(&b) + 16;
        self.is_rel.lock().unwrap().push((a, b));
        self.approx_bytes.fetch_add(size, Ordering::Relaxed);
    }

    /// Record a has_rel edge. Prefer this over locking `has_rel` directly so
    /// the size counter stays accurate.
    pub fn push_has_rel(&self, edge: ChildEdge) {
        let size = edge_bytes(&edge);
        self.has_rel.lock().unwrap().push(edge);
        self.approx_bytes.fetch_add(size, Ordering::Relaxed);
    }

    /// Record what a listing said about `pair`. A pair listed several times
    /// (an artist credited on many tracks) keeps the first non-empty name and
    /// duration seen.
    pub fn insert_stub(&self, pair: Pair, info: StubInfo) {
        let mut stubs = self.stubs.lock().unwrap();
        match stubs.entry(pair) {
            Entry::Vacant(v) => {
                let size = pair_bytes(v.key()) + stub_bytes(&info);
                v.insert(info);
                self.approx_bytes.fetch_add(size, Ordering::Relaxed);
            }
            Entry::Occupied(mut o) => {
                let existing = o.get_mut();
                if existing.name.is_none()
                    && let Some(name) = info.name
                {
                    self.approx_bytes.fetch_add(name.len(), Ordering::Relaxed);
                    existing.name = Some(name);
                }
                if existing.duration_ms.is_none() {
                    existing.duration_ms = info.duration_ms;
                }
            }
        }
    }
    /// Current estimated size of buffered state, in bytes.
    pub fn approx_bytes(&self) -> usize {
        self.approx_bytes.load(Ordering::Relaxed)
    }

    /// Called by `flush` right after it drains `metadata`/`is_rel`/`has_rel`
    /// via `mem::take`, to zero out the portion of the counter that
    /// corresponds to what was actually removed. Anything `flush` re-buffers
    /// afterwards (deferred classes — see `pipeline::flush`) goes back in
    /// through `insert_metadata`/`push_is_rel`/`push_has_rel` above, which
    /// re-adds it, so the net effect is that the counter only ever reflects
    /// what's genuinely still sitting in `State`.
    pub(super) fn sub_bytes(&self, n: usize) {
        self.approx_bytes.fetch_sub(n, Ordering::Relaxed);
    }

    pub(super) fn progress(&self) -> Option<&Arc<dyn ImportProgress>> {
        self.progress.as_ref()
    }
}

pub(super) fn pair_bytes(p: &Pair) -> usize {
    p.0.len() + p.1.len() + 48 // two String headers + allocator/HashMap overhead
}

pub(super) fn metadata_bytes(m: &PairMetadata) -> usize {
    64 // enum discriminant + struct overhead, approximate
        + m.release_date.as_ref().map_or(0, |s| s.len())
        + specific_data_bytes(&m.specific_data)
        + m.aliases.iter().map(alias_bytes).sum::<usize>()
}

fn specific_data_bytes(d: &EntrySpecificData) -> usize {
    match d {
        EntrySpecificData::Track {
            duration_ms,
            positions,
        } => duration_ms.len() * 8 + positions.keys().map(|k| k.len() + 32).sum::<usize>(),
        EntrySpecificData::Release { release_type, .. } => {
            release_type.as_ref().map_or(0, |s| s.len()) + 24
        }
        EntrySpecificData::ReleaseGroup { primary_type } => {
            primary_type.as_ref().map_or(0, |s| s.len()) + 8
        }
        EntrySpecificData::Artist => 0,
    }
}

pub(super) fn stub_bytes(s: &StubInfo) -> usize {
    32 + s.name.as_ref().map_or(0, |n| n.len())
}

pub(super) fn alias_bytes(a: &Alias) -> usize {
    a.name.len()
        + a.source.len()
        + a.locale.as_ref().map_or(0, |s| s.len())
        + json_bytes(&a.extra)
        + 32
}

fn contribution_bytes(c: &Contribution) -> usize {
    c.role.len() + c.source.len() + json_bytes(&c.extra) + 32
}

pub(super) fn edge_bytes(e: &ChildEdge) -> usize {
    pair_bytes(&e.parent)
        + pair_bytes(&e.child)
        + e.contributions
            .iter()
            .map(contribution_bytes)
            .sum::<usize>()
        + e.original_relation_kind.as_ref().map_or(0, |s| s.len())
        + 48
}

fn json_bytes(v: &serde_json::Value) -> usize {
    match v {
        serde_json::Value::Null | serde_json::Value::Bool(_) => 4,
        serde_json::Value::Number(_) => 8,
        serde_json::Value::String(s) => s.len(),
        serde_json::Value::Array(a) => a.iter().map(json_bytes).sum(),
        serde_json::Value::Object(m) => m.iter().map(|(k, v)| k.len() + json_bytes(v)).sum(),
    }
}
