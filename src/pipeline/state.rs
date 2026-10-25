use std::collections::{HashMap, HashSet, hash_map::Entry};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::musicdb::{
    AliasKey, LINK_VIA_FETCH, LINK_VIA_LISTING, LinkKey, ParentEdges, Replacements, alias_key,
    link_key,
};
use crate::providers::types::{
    Alias, Contribution, EntrySpecificData, EntryType, Image, OptionsId, Privacy,
};

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
    pub image: Option<Image>,
    pub privacy: Option<Privacy>,
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
    pub image: Option<Image>,
}

#[derive(Default)]
struct RunEdges {
    fetched: HashSet<Pair>,
    edges: HashMap<Pair, ParentEdges>,
    aliases: HashMap<Pair, HashSet<AliasKey>>,
    gone: HashSet<Pair>,
    links: HashMap<(Pair, &'static str), HashSet<LinkKey>>,
}

/// How an `IsRel` was stated (`pair_link.via`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LinkVia {
    /// In the asserting pair's own fetch result.
    Fetch,
    /// Between one child's sibling pairs in the asserting pair's listing.
    Listing,
}

impl LinkVia {
    /// The `pair_link.via` value back; anything unknown reads as `Listing`.
    pub fn from_stored(via: &str) -> Self {
        if via == LINK_VIA_FETCH {
            LinkVia::Fetch
        } else {
            LinkVia::Listing
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            LinkVia::Fetch => LINK_VIA_FETCH,
            LinkVia::Listing => LINK_VIA_LISTING,
        }
    }
}

/// An "a and b are one entity" link, with the fetched pair that stated it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IsRel {
    pub a: Pair,
    pub b: Pair,
    pub asserted_by: Pair,
    pub via: LinkVia,
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
    /// Claimed pairs → the option sets each has been processed under.
    claimed: Mutex<HashMap<Pair, HashSet<OptionsId>>>,
    pub metadata: Mutex<HashMap<Pair, PairMetadata>>,
    pub is_rel: Mutex<Vec<IsRel>>,
    pub has_rel: Mutex<Vec<ChildEdge>>,
    pub stubs: Mutex<HashMap<Pair, StubInfo>>,
    /// Every non-artist pair fetched completely in this run, and every edge
    /// seen under any parent, by canonical pair (recorded by `flush`). Kept
    /// for the whole run: unlike the buffers above, flush never drains it,
    /// and it isn't counted in `approx_bytes` (which only measures what a
    /// flush can drain). At commit, it's what replaces older edges under
    /// re-fetched parents (see `MusicDb::commit_import_run`).
    run_edges: Mutex<RunEdges>,
    /// Pairs whose class keeps their entry's id when a flush splits it
    /// (`prefer_keeper`).
    preferred_keepers: Mutex<HashSet<Pair>>,
    /// Safety cap on fetches per run (see `charge_fetch`); `usize::MAX` = none.
    fetch_budget: AtomicUsize,
    fetches: AtomicUsize,
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
            claimed: Mutex::new(HashMap::new()),
            metadata: Mutex::new(HashMap::new()),
            is_rel: Mutex::new(Vec::new()),
            has_rel: Mutex::new(Vec::new()),
            stubs: Mutex::new(HashMap::new()),
            run_edges: Mutex::new(RunEdges::default()),
            preferred_keepers: Mutex::new(HashSet::new()),
            fetch_budget: AtomicUsize::new(usize::MAX),
            fetches: AtomicUsize::new(0),
            approx_bytes: AtomicUsize::new(0),
            progress,
        }
    }

    /// Returns `true` if the caller should process `pair` under `options`:
    /// it hasn't been processed under them yet, and they can add something.
    /// `leaf` options (no child rules) add nothing once the pair has been
    /// processed under any options, since every pass records the pair's own
    /// data and structural edges; only child rules differ between passes.
    ///
    /// Claims accumulate instead of first-claimer-wins, so the result doesn't
    /// depend on arrival order: a collaborator artist reached as a leaf from a
    /// collab track still gets its discography when its own root import
    /// reaches it, and a pair reached under two different option sets is
    /// walked under both. Option sets are never merged into one rule list
    /// (rules are first-match-wins, so concatenation order would matter);
    /// each is evaluated on its own pass.
    pub fn claim(&self, pair: &Pair, options: OptionsId, leaf: bool) -> bool {
        let mut claimed = self.claimed.lock().unwrap();
        match claimed.entry(pair.clone()) {
            Entry::Vacant(v) => {
                v.insert(HashSet::from([options]));
                true
            }
            Entry::Occupied(_) if leaf => false,
            Entry::Occupied(mut o) => o.get_mut().insert(options),
        }
    }

    /// Record a pair's fetched metadata. Prefer this over locking `metadata`
    /// directly so the size counter stays accurate.
    pub fn insert_metadata(&self, pair: Pair, meta: PairMetadata) {
        let key_size = pair_bytes(&pair);
        let size = key_size + metadata_bytes(&meta);
        // A pair upgraded from a leaf claim (see `claim`) is inserted twice.
        let old = self.metadata.lock().unwrap().insert(pair, meta);
        self.approx_bytes.fetch_add(size, Ordering::Relaxed);
        if let Some(old) = old {
            self.approx_bytes
                .fetch_sub(key_size + metadata_bytes(&old), Ordering::Relaxed);
        }
    }

    /// Record an is_rel edge. Prefer this over locking `is_rel` directly so
    /// the size counter stays accurate.
    pub fn push_is_rel(&self, a: Pair, b: Pair, asserted_by: Pair, via: LinkVia) {
        let size = is_rel_bytes(&a, &b, &asserted_by);
        self.is_rel.lock().unwrap().push(IsRel {
            a,
            b,
            asserted_by,
            via,
        });
        self.approx_bytes.fetch_add(size, Ordering::Relaxed);
    }

    /// Record a has_rel edge. Prefer this over locking `has_rel` directly so
    /// the size counter stays accurate.
    pub fn push_has_rel(&self, edge: ChildEdge) {
        let size = edge_bytes(&edge);
        self.has_rel.lock().unwrap().push(edge);
        self.approx_bytes.fetch_add(size, Ordering::Relaxed);
    }

    /// Cap the number of fetches this run may make. Once reached, further
    /// fetches are refused (`charge_fetch` returns false) so the traversal
    /// winds down quickly, and `ingest_entries` fails the run (rolling it
    /// back) instead of committing a partial import.
    pub fn set_fetch_budget(&self, max: Option<usize>) {
        self.fetch_budget
            .store(max.unwrap_or(usize::MAX), Ordering::Relaxed);
    }

    /// Count one fetch against the budget. `false` once it's exhausted.
    pub(super) fn charge_fetch(&self) -> bool {
        self.fetches.fetch_add(1, Ordering::Relaxed) < self.fetch_budget.load(Ordering::Relaxed)
    }

    /// Fetches attempted so far, and whether the budget was exceeded.
    pub fn fetch_count(&self) -> (usize, bool) {
        let n = self.fetches.load(Ordering::Relaxed);
        (n, n > self.fetch_budget.load(Ordering::Relaxed))
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
                if existing.image.is_none()
                    && let Some(image) = info.image
                {
                    self.approx_bytes
                        .fetch_add(image.url.len(), Ordering::Relaxed);
                    existing.image = Some(image);
                }
            }
        }
    }

    /// When a flush of this state splits the entry `pair` is in, the part
    /// holding `pair` keeps the entry's id (and with it plays, display names
    /// and soft relations), whatever its size or age.
    pub fn prefer_keeper(&self, pair: Pair) {
        self.preferred_keepers.lock().unwrap().insert(pair);
    }

    pub(super) fn preferred_keepers(&self) -> HashSet<Pair> {
        self.preferred_keepers.lock().unwrap().clone()
    }

    /// Record that `pair` (canonical) was fetched completely as a non-artist
    /// entity in this run, so its edges as of this run replace older ones.
    pub(super) fn note_fetched_parent(&self, pair: Pair) {
        self.run_edges.lock().unwrap().fetched.insert(pair);
    }

    /// Record the aliases a fetch of `pair` (canonical, any entry type)
    /// reported, so they replace older ones at commit.
    pub(super) fn note_aliases(&self, pair: &Pair, aliases: &[Alias]) {
        let mut run_edges = self.run_edges.lock().unwrap();
        let current = run_edges.aliases.entry(pair.clone()).or_default();
        for a in aliases {
            current.insert(alias_key(
                &a.name,
                a.locale.as_deref(),
                &a.extra.to_string(),
                a.primary,
            ));
        }
    }

    /// Record a link (canonical pairs) as stated by its asserter this run.
    pub(super) fn note_link(&self, rel: &IsRel) {
        self.run_edges
            .lock()
            .unwrap()
            .links
            .entry((rel.asserted_by.clone(), rel.via.as_str()))
            .or_default()
            .insert(link_key(&rel.a, &rel.b));
    }

    /// Record that the provider reported `pair` (canonical) deleted.
    pub(super) fn note_gone(&self, pair: Pair) {
        self.run_edges.lock().unwrap().gone.insert(pair);
    }

    /// Record an edge (canonical pairs) as current for its parent.
    pub(super) fn note_edge(&self, edge: &ChildEdge) {
        let mut run_edges = self.run_edges.lock().unwrap();
        let current = run_edges.edges.entry(edge.parent.clone()).or_default();
        current.children.insert(edge.child.clone());
        for c in &edge.contributions {
            current.contributions.insert((
                edge.child.clone(),
                c.role.clone(),
                c.main_artist,
                c.extra.to_string(),
            ));
        }
    }

    /// The current edges of every non-artist parent fetched in this run (an
    /// empty set for one that listed nothing) and the current aliases of
    /// every fetched pair, and the pairs found deleted upstream, for
    /// `MusicDb::commit_import_run`.
    pub fn take_replacements(&self) -> Replacements {
        let RunEdges {
            fetched,
            mut edges,
            aliases,
            gone,
            links,
        } = std::mem::take(&mut *self.run_edges.lock().unwrap());
        let parents = fetched
            .into_iter()
            .map(|pair| {
                let current = edges.remove(&pair).unwrap_or_default();
                (pair, current)
            })
            .collect();
        Replacements {
            parents,
            aliases,
            gone,
            links,
        }
    }

    /// What `take_replacements` would return now, leaving it in place: the
    /// final flush reads it to find the links this run retracts.
    pub fn peek_replacements(&self) -> Replacements {
        let run_edges = self.run_edges.lock().unwrap();
        let parents = run_edges
            .fetched
            .iter()
            .map(|pair| {
                let current = run_edges.edges.get(pair).cloned().unwrap_or_default();
                (pair.clone(), current)
            })
            .collect();
        Replacements {
            parents,
            aliases: run_edges.aliases.clone(),
            gone: run_edges.gone.clone(),
            links: run_edges.links.clone(),
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

pub(super) fn is_rel_bytes(a: &Pair, b: &Pair, asserted_by: &Pair) -> usize {
    pair_bytes(a) + pair_bytes(b) + pair_bytes(asserted_by) + 16
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
            songs,
        } => {
            duration_ms.len() * 8
                + positions.keys().map(|k| k.len() + 32).sum::<usize>()
                + songs.iter().flatten().map(|w| w.len() + 24).sum::<usize>()
        }
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
    32 + s.name.as_ref().map_or(0, |n| n.len()) + s.image.as_ref().map_or(0, |i| i.url.len())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_accumulate_per_options() {
        let state = State::new();
        let pair: Pair = ("x".into(), "album".into());
        assert!(state.claim(&pair, 1, false), "first claim");
        assert!(!state.claim(&pair, 1, false), "same options again");
        assert!(state.claim(&pair, 2, false), "different options");
        assert!(
            !state.claim(&pair, 0, true),
            "a leaf adds nothing once claimed"
        );

        let other: Pair = ("x".into(), "artist".into());
        assert!(state.claim(&other, 0, true), "a leaf can claim first");
        assert!(
            state.claim(&other, 1, false),
            "then expanding options still run"
        );
    }
}
