use std::collections::{HashMap, HashSet};
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
    pub extra: serde_json::Value,
    pub specific_data: EntrySpecificData,
    pub aliases: Vec<Alias>,
}

/// A "parent has child" edge between two pairs, with the structural position
/// and any role contributions attached to that edge.
pub struct ChildEdge {
    pub parent: Pair,
    pub child: Pair,
    pub disc_no: Option<i32>,
    pub track_no: Option<i32>,
    pub contributions: Vec<Contribution>,
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
/// perspective; `flush` consumes them.
pub struct State {
    claimed: Mutex<HashSet<Pair>>,
    pub metadata: Mutex<HashMap<Pair, PairMetadata>>,
    pub is_rel: Mutex<Vec<(Pair, Pair)>>,
    pub has_rel: Mutex<Vec<ChildEdge>>,
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
            progress,
        }
    }

    /// Returns `true` if this call is the first to claim `pair` (caller should
    /// fetch). Returns `false` if another task already owns it.
    pub fn claim(&self, pair: &Pair) -> bool {
        self.claimed.lock().unwrap().insert(pair.clone())
    }

    pub(super) fn progress(&self) -> Option<&Arc<dyn ImportProgress>> {
        self.progress.as_ref()
    }
}
