use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

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

/// Shared import state. All fields are append-only from the importer's
/// perspective; `flush` consumes them.
pub struct State {
    claimed: Mutex<HashSet<Pair>>,
    pub metadata: Mutex<HashMap<Pair, PairMetadata>>,
    pub is_rel: Mutex<Vec<(Pair, Pair)>>,
    pub has_rel: Mutex<Vec<ChildEdge>>,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    pub fn new() -> Self {
        Self {
            claimed: Mutex::new(HashSet::new()),
            metadata: Mutex::new(HashMap::new()),
            is_rel: Mutex::new(Vec::new()),
            has_rel: Mutex::new(Vec::new()),
        }
    }

    /// Returns `true` if this call is the first to claim `pair` (caller should
    /// fetch). Returns `false` if another task already owns it.
    pub fn claim(&self, pair: &Pair) -> bool {
        self.claimed.lock().unwrap().insert(pair.clone())
    }
}
