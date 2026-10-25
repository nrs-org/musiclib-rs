//! Growing listings (a channel's uploads, an artist's discography, a
//! playlist) cached as one snapshot each, so a library can be updated without
//! refetching every page of every listing. See `docs/plan-update.md`.
//!
//! A snapshot is the ordered list of a listing's `ChildRef`s, stored in the
//! HTTP cache DB under the listing's key (`youtube:uploads:UU…`). The page
//! requests of a snapshot listing go through the HTTP cache only when a
//! snapshot is built from it; a refresh bypasses it, since the snapshot is
//! then the one copy.
//!
//! How a listing is served depends on the run's `ListingMode`:
//!
//! - `Cached`: serve the snapshot. With none, walk the pages through the HTTP
//!   cache (as before snapshots existed) and store the result with an
//!   unknown age (`created_at = 0`).
//! - `Auto`: refresh, once per run. A `NewestFirst` listing with a snapshot
//!   is refreshed incrementally: pages are fetched until one holds only known
//!   items, and the fetched items go in front of the snapshot. Anything else
//!   is fetched in full.
//! - `Full`: fetch every listing in full.
//!
//! A snapshot is written in one statement once its walk succeeds, so a
//! refresh that fails partway keeps the old one, and a refresh that can't run
//! (failure, quota budget spent) falls back to `Cached` behaviour. The
//! importer always gets a complete listing.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::{Read, Write},
    sync::Arc,
};

use dashmap::DashMap;
use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use futures::{StreamExt, stream};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, Statement};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::{
    http::{bypass_cache, quota::QuotaLedger},
    providers::types::{ChildRef, CycleGuard, Error, PageFetcher, PaginatedChildSource},
};

/// How a listing's items are ordered, which decides how it can be refreshed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingOrder {
    /// New items appear at the start (YouTube uploads), so a refresh can stop
    /// at the first page of known items.
    NewestFirst,
    /// No usable order: a refresh fetches every page.
    Unordered,
}

/// How the listings of a run are served. See the module doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListingMode {
    #[default]
    Cached,
    Auto,
    Full,
}

/// Identifies one listing: `kind` names what is listed (`"youtube:uploads"`)
/// and `id` whose (the uploads playlist ID).
#[derive(Debug, Clone)]
pub struct ListingKey {
    pub kind: &'static str,
    pub id: String,
}

impl ListingKey {
    pub fn new(kind: &'static str, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
        }
    }

    fn db_key(&self) -> String {
        format!("{}:{}", self.kind, self.id)
    }

    /// Only YouTube has a daily quota worth budgeting.
    fn budgeted(&self) -> bool {
        self.kind.starts_with("youtube:")
    }
}

/// Builds a listing's page fetcher from its ID alone, so the full-refresh
/// pass can refresh a listing no walk reached in this run.
pub type FetcherFactory = Arc<dyn Fn(&str) -> Box<dyn PageFetcher> + Send + Sync>;

/// The YouTube units refreshes may use today, across all runs.
pub struct QuotaBudget {
    pub units: u64,
    pub ledger: Arc<QuotaLedger>,
}

/// A snapshot the full-refresh pass skips when no walk has read it for this
/// long: no root reaches that listing any more.
const PASS_MAX_IDLE_SECS: i64 = 14 * 24 * 60 * 60;
/// The summary warns when the oldest snapshot's last full fetch is older than
/// this: the daily budget no longer covers a weekly cycle.
const STALE_WARN_SECS: i64 = 7 * 24 * 60 * 60;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct KindStats {
    /// Served from the snapshot without a request.
    pub served: usize,
    /// Built from pages read through the HTTP cache (no snapshot yet).
    pub built: usize,
    pub incremental: usize,
    pub full: usize,
    /// Refreshes not run because the quota budget was spent.
    pub skipped_budget: usize,
    /// Refreshes that failed; the old snapshot or the HTTP cache was used.
    pub failed: usize,
    /// Items a refresh found that its snapshot didn't have.
    pub new_items: usize,
    /// Refreshes of a listing without a snapshot: nothing to compare with,
    /// so none of their items count in `new_items`.
    pub no_baseline: usize,
}

struct Snapshot {
    items: Vec<ChildRef>,
    created_at: i64,
}

enum RefreshError {
    /// The quota budget ran out before or during the refresh.
    Budget,
    Fetch(Error),
}

impl From<Error> for RefreshError {
    fn from(e: Error) -> Self {
        Self::Fetch(e)
    }
}

pub struct ListingStore {
    db: DatabaseConnection,
    mode: ListingMode,
    /// Per-kind overrides of `mode` (`providers.yaml`'s `listings:`).
    kind_modes: HashMap<String, ListingMode>,
    budget: Option<QuotaBudget>,
    factories: std::sync::RwLock<HashMap<&'static str, (ListingOrder, FetcherFactory)>>,
    /// One lock per listing key: concurrent readers of a listing (a pair
    /// claimed under several option sets) wait for one refresh, then read
    /// its snapshot.
    locks: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
    /// Keys refreshed (or attempted) in this run, never refreshed again.
    refreshed: std::sync::Mutex<HashSet<String>>,
    /// Keys whose `last_used_at` was bumped in this run.
    seen: std::sync::Mutex<HashSet<String>>,
    stats: std::sync::Mutex<BTreeMap<&'static str, KindStats>>,
}

impl ListingStore {
    /// Open the store on the HTTP cache DB (see `httpcache::db::connect`),
    /// creating its table if needed.
    pub async fn new(db: DatabaseConnection, mode: ListingMode) -> Result<Self, DbErr> {
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS listing_snapshot (
                key TEXT PRIMARY KEY NOT NULL,
                kind TEXT NOT NULL,
                items BLOB NOT NULL,
                created_at INTEGER NOT NULL,
                refreshed_at INTEGER NOT NULL,
                last_used_at INTEGER NOT NULL
            )",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS listing_snapshot_kind_created
             ON listing_snapshot (kind, created_at)",
        )
        .await?;
        Ok(Self {
            db,
            mode,
            kind_modes: HashMap::new(),
            budget: None,
            factories: Default::default(),
            locks: DashMap::new(),
            refreshed: Default::default(),
            seen: Default::default(),
            stats: Default::default(),
        })
    }

    pub fn with_kind_modes(mut self, kind_modes: HashMap<String, ListingMode>) -> Self {
        self.kind_modes = kind_modes;
        self
    }

    pub fn with_budget(mut self, budget: QuotaBudget) -> Self {
        self.budget = Some(budget);
        self
    }

    /// Let the full-refresh pass rebuild `kind`'s fetchers. Only
    /// `NewestFirst` kinds are ever picked by the pass.
    pub fn register_factory(&self, kind: &'static str, order: ListingOrder, f: FetcherFactory) {
        self.factories.write().unwrap().insert(kind, (order, f));
    }

    /// The listing `key`, served according to the run's mode. Lazy: nothing
    /// happens until the source is first polled.
    pub fn listing(
        self: &Arc<Self>,
        key: ListingKey,
        order: ListingOrder,
        fetcher: Box<dyn PageFetcher>,
    ) -> PaginatedChildSource {
        let store = Arc::clone(self);
        let stream = stream::once(async move { store.resolve(&key, order, fetcher).await })
            .flat_map(|(items, error)| {
                stream::iter(
                    items
                        .into_iter()
                        .map(|child| Ok((child, ())))
                        .chain(error.map(Err)),
                )
            })
            .boxed();
        PaginatedChildSource::from_stream(stream)
    }

    pub fn stats(&self) -> BTreeMap<&'static str, KindStats> {
        self.stats.lock().unwrap().clone()
    }

    fn mode_for(&self, kind: &str) -> ListingMode {
        self.kind_modes.get(kind).copied().unwrap_or(self.mode)
    }

    fn bump(&self, kind: &'static str, f: impl FnOnce(&mut KindStats)) {
        f(self.stats.lock().unwrap().entry(kind).or_default());
    }

    /// The listing's items, plus the error that cut the walk short when no
    /// complete copy could be had (the items are then the pages read so far,
    /// as a plain `PaginatedChildSource` would have yielded).
    async fn resolve(
        &self,
        key: &ListingKey,
        order: ListingOrder,
        fetcher: Box<dyn PageFetcher>,
    ) -> (Vec<ChildRef>, Option<Error>) {
        let mut fetcher = CycleGuard::wrap(fetcher);
        let db_key = key.db_key();
        let lock = Arc::clone(&self.locks.entry(db_key.clone()).or_default());
        let _guard = lock.lock().await;

        let snapshot = match self.load(&db_key).await {
            Ok(s) => s,
            Err(e) => {
                warn!("listing {db_key}: reading snapshot failed ({e}); fetching instead");
                None
            }
        };
        let mode = self.mode_for(key.kind);
        let fresh = !self.refreshed.lock().unwrap().contains(&db_key);
        if mode == ListingMode::Cached || !fresh {
            return self.serve_cached(key, &db_key, snapshot, fetcher).await;
        }
        self.refreshed.lock().unwrap().insert(db_key.clone());

        // A `NewestFirst` listing without a snapshot is first built from the
        // HTTP cache and then refreshed incrementally: on the first run after
        // snapshots ship, that costs a page or two instead of a full fetch.
        // (For a listing never fetched before, the cached walk is a plain
        // fetch and the refresh costs one extra page.)
        let mut snapshot = snapshot;
        if snapshot.is_none() && mode == ListingMode::Auto && order == ListingOrder::NewestFirst {
            let (items, error) = fetch_pages(fetcher.as_mut()).await;
            if error.is_some() {
                return (items, error);
            }
            if let Err(e) = self.store(&db_key, key.kind, &items, 0, now()).await {
                warn!("listing {db_key}: writing snapshot failed: {e}");
            }
            self.bump(key.kind, |s| s.built += 1);
            snapshot = Some(Snapshot {
                items,
                created_at: 0,
            });
        }

        let incremental =
            mode == ListingMode::Auto && order == ListingOrder::NewestFirst && snapshot.is_some();
        let result = if incremental {
            let snap = snapshot.as_ref().expect("checked above");
            self.refresh_incremental(key, fetcher.as_mut(), snap).await
        } else {
            self.fetch_fresh(key, fetcher.as_mut())
                .await
                .map(|items| (items, true))
        };
        match result {
            Ok((items, complete)) => {
                let new_items = match &snapshot {
                    Some(s) => {
                        let known: HashSet<String> = s.items.iter().filter_map(item_key).collect();
                        items
                            .iter()
                            .filter_map(item_key)
                            .filter(|k| !known.contains(k))
                            .count()
                    }
                    None => 0,
                };
                let now = now();
                let created_at = match (&snapshot, complete) {
                    (Some(s), false) => s.created_at,
                    _ => now,
                };
                if let Err(e) = self.store(&db_key, key.kind, &items, created_at, now).await {
                    warn!("listing {db_key}: writing snapshot failed: {e}");
                }
                self.seen.lock().unwrap().insert(db_key);
                self.bump(key.kind, |s| {
                    if complete {
                        s.full += 1
                    } else {
                        s.incremental += 1
                    }
                    s.new_items += new_items;
                    s.no_baseline += usize::from(snapshot.is_none());
                });
                (items, None)
            }
            Err(RefreshError::Budget) => {
                self.bump(key.kind, |s| s.skipped_budget += 1);
                self.serve_cached(key, &db_key, snapshot, fetcher).await
            }
            Err(RefreshError::Fetch(e)) => {
                warn!("listing {db_key}: refresh failed ({e}); using the cached copy");
                self.bump(key.kind, |s| s.failed += 1);
                self.serve_cached(key, &db_key, snapshot, fetcher).await
            }
        }
    }

    /// Serve the snapshot, or build one from pages read through the HTTP
    /// cache.
    async fn serve_cached(
        &self,
        key: &ListingKey,
        db_key: &str,
        snapshot: Option<Snapshot>,
        mut fetcher: Box<dyn PageFetcher>,
    ) -> (Vec<ChildRef>, Option<Error>) {
        if let Some(snap) = snapshot {
            if self.seen.lock().unwrap().insert(db_key.to_owned())
                && let Err(e) = self.touch(db_key).await
            {
                warn!("listing {db_key}: updating last use failed: {e}");
            }
            self.bump(key.kind, |s| s.served += 1);
            return (snap.items, None);
        }
        let (items, error) = fetch_pages(fetcher.as_mut()).await;
        if error.is_none() {
            // Pages from the HTTP cache may be of any age.
            if let Err(e) = self.store(db_key, key.kind, &items, 0, now()).await {
                warn!("listing {db_key}: writing snapshot failed: {e}");
            }
            self.seen.lock().unwrap().insert(db_key.to_owned());
            self.bump(key.kind, |s| s.built += 1);
        }
        (items, error)
    }

    /// Check the quota budget before a refresh page.
    async fn check_budget(&self, key: &ListingKey) -> Result<(), RefreshError> {
        let Some(budget) = self.budget.as_ref().filter(|_| key.budgeted()) else {
            return Ok(());
        };
        match budget.ledger.used_today().await {
            Ok(used) if used >= budget.units => Err(RefreshError::Budget),
            Ok(_) => Ok(()),
            Err(e) => {
                // Without the ledger there's no telling how much is left;
                // refreshing blind could eat the manual-refresh reserve.
                warn!("quota ledger unreadable ({e}); treating the budget as spent");
                Err(RefreshError::Budget)
            }
        }
    }

    async fn fetch_page_fresh(
        &self,
        key: &ListingKey,
        fetcher: &mut dyn PageFetcher,
        token: Option<&str>,
    ) -> Result<crate::providers::types::ChildPage, RefreshError> {
        self.check_budget(key).await?;
        Ok(bypass_cache(fetcher.fetch_page(token)).await?)
    }

    /// Every page, bypassing the HTTP cache.
    async fn fetch_fresh(
        &self,
        key: &ListingKey,
        fetcher: &mut dyn PageFetcher,
    ) -> Result<Vec<ChildRef>, RefreshError> {
        let mut items = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let page = self
                .fetch_page_fresh(key, fetcher, token.as_deref())
                .await?;
            items.extend(page.children);
            match page.next_page_token {
                Some(t) => token = Some(t),
                None => return Ok(items),
            }
        }
    }

    /// Pages until one holds only items `snap` has, merged in front of
    /// `snap`. `true` when the listing ran out first, i.e. the result is a
    /// full fetch.
    async fn refresh_incremental(
        &self,
        key: &ListingKey,
        fetcher: &mut dyn PageFetcher,
        snap: &Snapshot,
    ) -> Result<(Vec<ChildRef>, bool), RefreshError> {
        let known: HashSet<String> = snap.items.iter().filter_map(item_key).collect();
        let mut fetched = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let page = self
                .fetch_page_fresh(key, fetcher, token.as_deref())
                .await?;
            let all_known = !page.children.is_empty()
                && page
                    .children
                    .iter()
                    .all(|c| item_key(c).is_some_and(|k| known.contains(&k)));
            fetched.extend(page.children);
            if all_known {
                return Ok((merge(fetched, &snap.items), false));
            }
            match page.next_page_token {
                Some(t) => token = Some(t),
                None => return Ok((fetched, true)),
            }
        }
    }

    /// After the walk: fully refresh `NewestFirst` snapshots, oldest full
    /// fetch first, until the quota budget is spent. Does nothing without a
    /// budget. Items it finds are imported by the next run's walk, which
    /// serves them from the refreshed snapshot.
    pub async fn full_refresh_pass(&self) -> Result<(), DbErr> {
        if self.budget.is_none() {
            return Ok(());
        }
        let factories: HashMap<&'static str, FetcherFactory> = self
            .factories
            .read()
            .unwrap()
            .iter()
            .filter(|(kind, (order, _))| {
                *order == ListingOrder::NewestFirst
                    && kind.starts_with("youtube:")
                    && self.mode_for(kind) != ListingMode::Cached
            })
            .map(|(kind, (_, f))| (*kind, Arc::clone(f)))
            .collect();
        if factories.is_empty() {
            return Ok(());
        }
        let placeholders = vec!["?"; factories.len()].join(", ");
        let mut values: Vec<sea_orm::Value> = factories.keys().map(|k| (*k).into()).collect();
        values.push((now() - PASS_MAX_IDLE_SECS).into());
        let rows = self
            .db
            .query_all_raw(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                format!(
                    "SELECT key, kind FROM listing_snapshot
                     WHERE kind IN ({placeholders}) AND last_used_at >= ?
                     ORDER BY created_at ASC, key ASC"
                ),
                values,
            ))
            .await?;

        let mut refreshed = 0usize;
        for row in rows {
            let db_key: String = row.try_get("", "key")?;
            let kind: String = row.try_get("", "kind")?;
            let Some((&kind, factory)) = factories.get_key_value(kind.as_str()) else {
                continue;
            };
            if !self.refreshed.lock().unwrap().insert(db_key.clone()) {
                continue;
            }
            let Some(id) = db_key.strip_prefix(kind).and_then(|r| r.strip_prefix(':')) else {
                continue;
            };
            let key = ListingKey::new(kind, id);
            let lock = Arc::clone(&self.locks.entry(db_key.clone()).or_default());
            let _guard = lock.lock().await;
            // Only used to count new items; an unreadable one is replaced.
            let old = self.load(&db_key).await.ok().flatten();
            let mut fetcher = CycleGuard::wrap(factory(id));
            match self.fetch_fresh(&key, fetcher.as_mut()).await {
                Ok(items) => {
                    let no_baseline = usize::from(old.is_none());
                    let new_items = old.map_or(0, |s| {
                        let known: HashSet<String> = s.items.iter().filter_map(item_key).collect();
                        items
                            .iter()
                            .filter_map(item_key)
                            .filter(|k| !known.contains(k))
                            .count()
                    });
                    let now = now();
                    self.store(&db_key, kind, &items, now, now).await?;
                    self.bump(kind, |s| {
                        s.full += 1;
                        s.new_items += new_items;
                        s.no_baseline += no_baseline;
                    });
                    refreshed += 1;
                }
                Err(RefreshError::Budget) => break,
                Err(RefreshError::Fetch(e)) => {
                    warn!("listing {db_key}: full refresh failed: {e}");
                    self.bump(kind, |s| s.failed += 1);
                }
            }
        }
        info!("full-refresh pass: {refreshed} listing(s) refreshed");
        Ok(())
    }

    /// Log per-kind counts, and warn when the oldest budgeted snapshot's last
    /// full fetch is over a week old.
    pub async fn log_summary(&self) {
        for (kind, s) in self.stats() {
            info!(
                "listings {kind}: {} served, {} built, {} incremental, {} full, \
                 {} skipped (budget), {} failed, {} new item(s) \
                 ({} listing(s) had no earlier copy to compare with)",
                s.served,
                s.built,
                s.incremental,
                s.full,
                s.skipped_budget,
                s.failed,
                s.new_items,
                s.no_baseline
            );
        }
        // `created_at = 0` is unknown age (built from the HTTP cache), not 1970.
        let ages = self
            .db
            .query_one_raw(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "SELECT MIN(NULLIF(created_at, 0)) AS oldest,
                        SUM(created_at = 0) AS unknown
                 FROM listing_snapshot
                 WHERE kind LIKE 'youtube:%' AND last_used_at >= ?",
                [(now() - PASS_MAX_IDLE_SECS).into()],
            ))
            .await;
        let Ok(Some(row)) = ages else { return };
        if let Ok(Some(unknown)) = row.try_get::<Option<i64>>("", "unknown")
            && unknown > 0
        {
            info!(
                "{unknown} YouTube listing snapshot(s) built from the HTTP cache \
                 have not been fully refreshed yet"
            );
        }
        if let Ok(Some(oldest)) = row.try_get::<Option<i64>>("", "oldest")
            && now() - oldest > STALE_WARN_SECS
        {
            let days = (now() - oldest) / (24 * 60 * 60);
            warn!(
                "oldest YouTube listing snapshot was last fully fetched {} day(s) ago; \
                 the daily budget no longer covers a weekly full refresh",
                days
            );
        }
    }

    async fn load(&self, db_key: &str) -> Result<Option<Snapshot>, Error> {
        let row = self
            .db
            .query_one_raw(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "SELECT items, created_at FROM listing_snapshot WHERE key = ?",
                [db_key.into()],
            ))
            .await
            .map_err(db_error)?;
        let Some(row) = row else { return Ok(None) };
        let blob: Vec<u8> = row.try_get("", "items").map_err(db_error)?;
        let created_at: i64 = row.try_get("", "created_at").map_err(db_error)?;
        let mut json = Vec::new();
        ZlibDecoder::new(blob.as_slice())
            .read_to_end(&mut json)
            .map_err(|e| Error::Listing(format!("decompressing snapshot: {e}")))?;
        let items = serde_json::from_slice(&json)
            .map_err(|e| Error::Listing(format!("decoding snapshot: {e}")))?;
        Ok(Some(Snapshot { items, created_at }))
    }

    async fn store(
        &self,
        db_key: &str,
        kind: &str,
        items: &[ChildRef],
        created_at: i64,
        now: i64,
    ) -> Result<(), DbErr> {
        let json = serde_json::to_vec(items).map_err(|e| DbErr::Custom(e.to_string()))?;
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&json)
            .map_err(|e| DbErr::Custom(e.to_string()))?;
        let blob = enc.finish().map_err(|e| DbErr::Custom(e.to_string()))?;
        self.db
            .execute_raw(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "INSERT INTO listing_snapshot
                    (key, kind, items, created_at, refreshed_at, last_used_at)
                 VALUES (?, ?, ?, ?, ?, ?)
                 ON CONFLICT (key) DO UPDATE SET
                    kind = excluded.kind, items = excluded.items,
                    created_at = excluded.created_at,
                    refreshed_at = excluded.refreshed_at,
                    last_used_at = excluded.last_used_at",
                [
                    db_key.into(),
                    kind.into(),
                    blob.into(),
                    created_at.into(),
                    now.into(),
                    now.into(),
                ],
            ))
            .await?;
        Ok(())
    }

    async fn touch(&self, db_key: &str) -> Result<(), DbErr> {
        self.db
            .execute_raw(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "UPDATE listing_snapshot SET last_used_at = ? WHERE key = ?",
                [now().into(), db_key.into()],
            ))
            .await?;
        Ok(())
    }
}

/// A plain listing for backends built without a store, a snapshot listing
/// otherwise.
pub fn listing_source(
    store: Option<&Arc<ListingStore>>,
    key: ListingKey,
    order: ListingOrder,
    fetcher: Box<dyn PageFetcher>,
) -> PaginatedChildSource {
    match store {
        Some(store) => store.listing(key, order, fetcher),
        None => PaginatedChildSource::new(fetcher),
    }
}

/// Every page through whatever cache layers apply, stopping at the first
/// error.
async fn fetch_pages(fetcher: &mut dyn PageFetcher) -> (Vec<ChildRef>, Option<Error>) {
    let mut items = Vec::new();
    let mut token: Option<String> = None;
    loop {
        match fetcher.fetch_page(token.as_deref()).await {
            Ok(page) => {
                items.extend(page.children);
                match page.next_page_token {
                    Some(t) => token = Some(t),
                    None => return (items, None),
                }
            }
            Err(e) => return (items, Some(e)),
        }
    }
}

/// `fetched`, then every item of `old` that `fetched` doesn't have.
fn merge(fetched: Vec<ChildRef>, old: &[ChildRef]) -> Vec<ChildRef> {
    let have: HashSet<String> = fetched.iter().filter_map(item_key).collect();
    let mut out = fetched;
    out.extend(
        old.iter()
            .filter(|c| item_key(c).is_none_or(|k| !have.contains(&k)))
            .cloned(),
    );
    out
}

/// An item's identity within its listing: its smallest `(source, identifier)`
/// pair. `None` for an item without sources, which the importer skips anyway.
fn item_key(child: &ChildRef) -> Option<String> {
    child
        .sources
        .0
        .iter()
        .flat_map(|(source, ids)| ids.iter().map(move |id| format!("{source}\0{id}")))
        .min()
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

fn db_error(e: DbErr) -> Error {
    Error::Listing(format!("snapshot DB: {e}"))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, AtomicUsize, Ordering},
        },
    };

    use super::*;
    use crate::providers::types::{ChildPage, child_next};

    const KIND: &str = "youtube:uploads";

    /// A listing of video IDs served two per page, newest first. Every call
    /// counts, and adds a unit to `quota` as a real YouTube call would.
    #[derive(Clone, Default)]
    struct Fake {
        items: Arc<Mutex<Vec<&'static str>>>,
        calls: Arc<AtomicUsize>,
        quota: Arc<AtomicU64>,
        /// Fail the call with this index (0-based, across clones).
        fail_at: Arc<Mutex<Option<usize>>>,
    }

    impl Fake {
        fn new(items: &[&'static str]) -> Self {
            let fake = Self::default();
            fake.set(items);
            fake
        }
        fn set(&self, items: &[&'static str]) {
            *self.items.lock().unwrap() = items.to_vec();
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
        fn fetcher(&self) -> Box<dyn PageFetcher> {
            Box::new(self.clone())
        }
    }

    #[async_trait::async_trait]
    impl PageFetcher for Fake {
        async fn fetch_page(&mut self, token: Option<&str>) -> Result<ChildPage, Error> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            self.quota.fetch_add(1, Ordering::SeqCst);
            if *self.fail_at.lock().unwrap() == Some(n) {
                return Err(Error::NotFound("injected failure".into()));
            }
            let items = self.items.lock().unwrap().clone();
            let start: usize = token.map_or(0, |t| t.parse().unwrap());
            let end = (start + 2).min(items.len());
            Ok(ChildPage {
                children: items[start..end].iter().map(|id| child(id)).collect(),
                next_page_token: (end < items.len()).then(|| end.to_string()),
            })
        }
    }

    fn child(id: &str) -> ChildRef {
        ChildRef {
            sources: [("youtube".into(), HashSet::from([id.to_string()]))].into(),
            name: Some(id.to_uppercase()),
            ..Default::default()
        }
    }

    async fn db() -> DatabaseConnection {
        crate::httpcache::db::connect("sqlite::memory:".into())
            .await
            .unwrap()
    }

    async fn store(db: &DatabaseConnection, mode: ListingMode) -> Arc<ListingStore> {
        Arc::new(ListingStore::new(db.clone(), mode).await.unwrap())
    }

    async fn read_id(store: &Arc<ListingStore>, id: &str, fake: &Fake) -> Vec<String> {
        let key = ListingKey::new(KIND, id);
        let mut source = store.listing(key, ListingOrder::NewestFirst, fake.fetcher());
        let mut out = Vec::new();
        while let Some((c, ())) = child_next(&mut source).await.unwrap() {
            out.push(c.sources.first_identifier().unwrap().to_owned());
        }
        out
    }

    async fn read(store: &Arc<ListingStore>, fake: &Fake) -> Vec<String> {
        read_id(store, "UU1", fake).await
    }

    async fn created_at(db: &DatabaseConnection) -> i64 {
        let row = db
            .query_one_raw(Statement::from_string(
                db.get_database_backend(),
                "SELECT created_at FROM listing_snapshot",
            ))
            .await
            .unwrap()
            .unwrap();
        row.try_get("", "created_at").unwrap()
    }

    /// Cached mode builds a snapshot from the pages once (with unknown age),
    /// then a later run serves it without a request.
    #[tokio::test]
    async fn cached_builds_then_serves() {
        let db = db().await;
        let fake = Fake::new(&["e", "d", "c"]);
        let s = store(&db, ListingMode::Cached).await;
        assert_eq!(read(&s, &fake).await, ["e", "d", "c"]);
        assert_eq!(fake.calls(), 2);
        assert_eq!(created_at(&db).await, 0);

        fake.set(&["f", "e", "d", "c"]);
        let s = store(&db, ListingMode::Cached).await;
        assert_eq!(read(&s, &fake).await, ["e", "d", "c"]);
        assert_eq!(fake.calls(), 2);
        assert_eq!(s.stats()[KIND].served, 1);
    }

    /// An incremental refresh stops at the first page of known items and
    /// puts the new ones in front; with nothing new it costs one call.
    #[tokio::test]
    async fn incremental_stops_at_known_page() {
        let db = db().await;
        let fake = Fake::new(&["e", "d", "c", "b", "a"]);
        read(&store(&db, ListingMode::Cached).await, &fake).await;
        let built = fake.calls();

        fake.set(&["g", "f", "e", "d", "c", "b", "a"]);
        let s = store(&db, ListingMode::Auto).await;
        assert_eq!(read(&s, &fake).await, ["g", "f", "e", "d", "c", "b", "a"]);
        assert_eq!(fake.calls() - built, 2);
        let stats = &s.stats()[KIND];
        assert_eq!((stats.incremental, stats.new_items), (1, 2));
        // An incremental refresh keeps the age of the last full fetch.
        assert_eq!(created_at(&db).await, 0);

        let before = fake.calls();
        read(&store(&db, ListingMode::Auto).await, &fake).await;
        assert_eq!(fake.calls() - before, 1);
    }

    /// A refresh that runs out of pages before a known one is a full fetch:
    /// the snapshot is replaced and its age reset.
    #[tokio::test]
    async fn exhausted_incremental_replaces_snapshot() {
        let db = db().await;
        let fake = Fake::new(&["x"]);
        read(&store(&db, ListingMode::Cached).await, &fake).await;

        fake.set(&["a", "b", "c"]);
        let s = store(&db, ListingMode::Auto).await;
        assert_eq!(read(&s, &fake).await, ["a", "b", "c"]);
        assert_eq!(s.stats()[KIND].full, 1);
        assert!(created_at(&db).await > 0);
    }

    /// A refresh that fails partway serves, and keeps, the old snapshot.
    #[tokio::test]
    async fn failed_refresh_keeps_snapshot() {
        let db = db().await;
        let fake = Fake::new(&["c", "b", "a"]);
        read(&store(&db, ListingMode::Cached).await, &fake).await;

        fake.set(&["f", "e", "d", "c", "b", "a"]);
        *fake.fail_at.lock().unwrap() = Some(fake.calls() + 1);
        let s = store(&db, ListingMode::Full).await;
        assert_eq!(read(&s, &fake).await, ["c", "b", "a"]);
        assert_eq!(s.stats()[KIND].failed, 1);

        let s = store(&db, ListingMode::Cached).await;
        assert_eq!(read(&s, &fake).await, ["c", "b", "a"]);
    }

    /// Without a snapshot, a failed walk yields the pages read so far and
    /// then the error, as a plain paginated listing does.
    #[tokio::test]
    async fn failed_build_yields_partial_and_error() {
        let db = db().await;
        let fake = Fake::new(&["c", "b", "a"]);
        *fake.fail_at.lock().unwrap() = Some(1);
        let s = store(&db, ListingMode::Cached).await;
        let key = ListingKey::new(KIND, "UU1");
        let mut source = s.listing(key, ListingOrder::NewestFirst, fake.fetcher());
        assert!(child_next(&mut source).await.unwrap().is_some());
        assert!(child_next(&mut source).await.unwrap().is_some());
        assert!(child_next(&mut source).await.is_err());
    }

    /// In Auto mode a listing without a snapshot is built through the cache
    /// first, then refreshed incrementally.
    #[tokio::test]
    async fn auto_miss_builds_then_refreshes() {
        let db = db().await;
        let fake = Fake::new(&["c", "b", "a"]);
        let s = store(&db, ListingMode::Auto).await;
        assert_eq!(read(&s, &fake).await, ["c", "b", "a"]);
        assert_eq!(fake.calls(), 3);
        let stats = &s.stats()[KIND];
        assert_eq!((stats.built, stats.incremental), (1, 1));
    }

    /// An unordered listing without a snapshot is fetched in full with
    /// nothing to compare against: no new items, one without a baseline.
    /// Its next refresh has one.
    #[tokio::test]
    async fn first_unordered_refresh_has_no_baseline() {
        let db = db().await;
        let fake = Fake::new(&["c", "b", "a"]);
        let refresh = |items: &'static [&'static str]| {
            let (db, fake) = (db.clone(), fake.clone());
            async move {
                fake.set(items);
                let s = store(&db, ListingMode::Auto).await;
                let key = ListingKey::new(KIND, "PL1");
                let mut source = s.listing(key, ListingOrder::Unordered, fake.fetcher());
                while child_next(&mut source).await.unwrap().is_some() {}
                s.stats()[KIND].clone()
            }
        };

        let stats = refresh(&["c", "b", "a"]).await;
        assert_eq!((stats.full, stats.new_items, stats.no_baseline), (1, 0, 1));
        let stats = refresh(&["d", "c", "b", "a"]).await;
        assert_eq!((stats.full, stats.new_items, stats.no_baseline), (1, 1, 0));
    }

    /// A page whose next token is its own (YouTube's empty last page of some
    /// playlists) ends the refresh instead of looping on it.
    #[tokio::test]
    async fn self_referencing_page_token_ends_the_refresh() {
        struct Looping(Arc<AtomicUsize>);
        #[async_trait::async_trait]
        impl PageFetcher for Looping {
            async fn fetch_page(&mut self, token: Option<&str>) -> Result<ChildPage, Error> {
                let n = self.0.fetch_add(1, Ordering::SeqCst);
                assert!(n < 10, "still paging after {n} calls");
                Ok(match token {
                    None => ChildPage {
                        children: vec![child("a")],
                        next_page_token: Some("x".into()),
                    },
                    Some(_) => ChildPage {
                        children: vec![],
                        next_page_token: Some("x".into()),
                    },
                })
            }
        }
        let db = db().await;
        for mode in [ListingMode::Cached, ListingMode::Full] {
            let calls = Arc::new(AtomicUsize::new(0));
            let s = store(&db, mode).await;
            let key = ListingKey::new(KIND, "PL1");
            let fetcher = Box::new(Looping(Arc::clone(&calls)));
            let mut source = s.listing(key, ListingOrder::Unordered, fetcher);
            let mut out = Vec::new();
            while let Some((c, ())) = child_next(&mut source).await.unwrap() {
                out.push(c.sources.first_identifier().unwrap().to_owned());
            }
            assert_eq!(out, ["a"], "{mode:?}");
            assert_eq!(calls.load(Ordering::SeqCst), 2, "{mode:?}");
        }
    }

    /// Concurrent readers of one listing share a single refresh, and a
    /// listing is refreshed at most once per run.
    #[tokio::test]
    async fn one_refresh_per_run() {
        let db = db().await;
        let fake = Fake::new(&["c", "b", "a"]);
        read(&store(&db, ListingMode::Cached).await, &fake).await;
        let before = fake.calls();

        let s = store(&db, ListingMode::Full).await;
        let (a, b) = tokio::join!(read(&s, &fake), read(&s, &fake));
        assert_eq!(a, b);
        read(&s, &fake).await;
        assert_eq!(fake.calls() - before, 2);
        let stats = &s.stats()[KIND];
        assert_eq!((stats.full, stats.served), (1, 2));
    }

    /// A spent budget skips the refresh and serves the snapshot; a kind
    /// forced to `cached` is never refreshed.
    #[tokio::test]
    async fn budget_and_kind_override_skip_refresh() {
        let db = db().await;
        let fake = Fake::new(&["c", "b", "a"]);
        let ledger = QuotaLedger::new(db.clone(), Arc::clone(&fake.quota));
        let ledger = Arc::new(ledger.await.unwrap());
        // Building the snapshot spends 2 units, over the budget of 1.
        read(&store(&db, ListingMode::Cached).await, &fake).await;
        let before = fake.calls();

        let s = ListingStore::new(db.clone(), ListingMode::Full)
            .await
            .unwrap();
        let s = Arc::new(s.with_budget(QuotaBudget { units: 1, ledger }));
        assert_eq!(read(&s, &fake).await, ["c", "b", "a"]);
        assert_eq!(fake.calls(), before);
        assert_eq!(s.stats()[KIND].skipped_budget, 1);

        let s = ListingStore::new(db.clone(), ListingMode::Full)
            .await
            .unwrap();
        let s = Arc::new(s.with_kind_modes([(KIND.to_owned(), ListingMode::Cached)].into()));
        read(&s, &fake).await;
        assert_eq!(fake.calls(), before);
    }

    /// The pass refreshes the oldest snapshots first and stops when the
    /// budget is spent; a listing the walk refreshed is skipped.
    #[tokio::test]
    async fn pass_refreshes_oldest_first_within_budget() {
        let db = db().await;
        let quota = Arc::new(AtomicU64::new(0));
        let fakes: HashMap<&str, Fake> = ["old", "mid", "new", "walked"]
            .into_iter()
            .map(|id| {
                let fake = Fake {
                    quota: Arc::clone(&quota),
                    ..Fake::default()
                };
                fake.set(&["b", "a"]);
                (id, fake)
            })
            .collect();
        let seed = store(&db, ListingMode::Cached).await;
        for (id, age) in [("old", 100), ("mid", 200), ("new", 300), ("walked", 50)] {
            let db_key = format!("{KIND}:{id}");
            seed.store(&db_key, KIND, &[child("a")], age, now())
                .await
                .unwrap();
        }

        let ledger = Arc::new(
            QuotaLedger::new(db.clone(), Arc::clone(&quota))
                .await
                .unwrap(),
        );
        let s = ListingStore::new(db.clone(), ListingMode::Auto)
            .await
            .unwrap();
        let s = Arc::new(s.with_budget(QuotaBudget { units: 3, ledger }));
        let factory_fakes = fakes.clone();
        s.register_factory(
            KIND,
            ListingOrder::NewestFirst,
            Arc::new(move |id| factory_fakes[id].fetcher()),
        );
        // The walk refreshes "walked": its one page has the new item "b",
        // so the listing runs out before a known page (1 unit, a full fetch).
        read_id(&s, "walked", &fakes["walked"]).await;
        assert_eq!(quota.load(Ordering::SeqCst), 1);

        s.full_refresh_pass().await.unwrap();
        // Budget 3: "old" and "mid" fit, "new" doesn't; "walked" is skipped.
        assert_eq!(fakes["walked"].calls(), 1);
        assert_eq!(fakes["old"].calls(), 1);
        assert_eq!(fakes["mid"].calls(), 1);
        assert_eq!(fakes["new"].calls(), 0);
    }
}
