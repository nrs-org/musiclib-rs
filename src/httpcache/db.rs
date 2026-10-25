use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    io::Read,
    io::Write,
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use dashmap::DashMap;
use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use reqwest::StatusCode;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectOptions, ConnectionTrait, Database,
    DatabaseConnection, DbErr, EntityTrait, FromJsonQueryResult, QueryFilter, QuerySelect,
    TransactionTrait, sea_query,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::{
    http::{BodyExtractorCow, HeaderName, HeaderValue, RawResponse, Response},
    httpcache::{CachePolicy, HttpCache, ResponseCachePolicy},
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Invalid status code in cache entry: {0}")]
    InvalidStatusCode(i32),
    #[error("Database error: {0}")]
    Database(#[from] DbErr),
    #[error("Body read error: {0}")]
    BodyRead(#[from] Box<crate::http::Error>),
    #[error("Body compression error: {0}")]
    BodyCompression(#[source] std::io::Error),
    #[error("Body decompression error: {0}")]
    BodyDecompression(#[source] std::io::Error),
    #[error("Unsupported HTTP cache body encoding: {0}")]
    UnsupportedBodyEncoding(String),
    #[error("HTTP cache writer has stopped")]
    WriterStopped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, FromJsonQueryResult)]
struct HeaderMap(Vec<(String, String)>);

mod cache_entry {
    use sea_orm::entity::prelude::*;
    use time::OffsetDateTime;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "cache_entry")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub key: String,
        pub method: String,
        pub url: String,
        pub status: i32,
        #[sea_orm(column_type = "JsonBinary")]
        pub headers: super::HeaderMap,
        pub body: Vec<u8>,
        /// Internal storage encoding for `body`; `NULL` means identity.
        pub body_encoding: Option<String>,
        pub created_at: OffsetDateTime,
        pub expires_at: OffsetDateTime,
        pub stale_at: OffsetDateTime,
        /// How many times this key has consecutively returned the same status code.
        pub consecutive_count: i32,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

/// SQLite-backed HTTP cache. SQLite takes one writer at a time, so writes
/// don't go through a pool: `set` queues the row and one writer task commits
/// queued rows in batches, one transaction each. Reads use their own pool and,
/// under WAL, never wait on the writer. A row is served from `pending` until
/// its batch commits.
pub struct DbHttpCache {
    /// On a file, a read-only pool. In memory, the writer's connection: an
    /// in-memory database is private to the connection that opened it.
    readers: DatabaseConnection,
    /// The one writing connection, shared by the writer task and maintenance.
    writer: DatabaseConnection,
    queue: mpsc::Sender<Op>,
    pending: Arc<DashMap<String, Pending>>,
    seq: AtomicU64,
}

/// Rows wait at most this long in the queue before their batch commits.
const BATCH_WINDOW: Duration = Duration::from_millis(50);
const BATCH_MAX: usize = 500;
/// A full queue makes `set` wait: backpressure, not an error.
const QUEUE_CAPACITY: usize = 1024;
const READERS: u32 = 16;
/// Waiting for a pooled connection is queueing, not a failure, so the wait is
/// long enough never to cut a busy import short.
const ACQUIRE_WAIT: Duration = Duration::from_secs(600);

/// Every open cache's queue, for `flush_all`.
static QUEUES: Mutex<Vec<mpsc::WeakSender<Op>>> = Mutex::new(Vec::new());

/// Commit every open DB cache's queued rows. Binaries that write the cache
/// call this before exiting: rows still queued are lost with the runtime.
pub async fn flush_all() {
    let queues: Vec<mpsc::Sender<Op>> = {
        let mut queues = QUEUES.lock().unwrap();
        queues.retain(|q| q.upgrade().is_some());
        queues
            .iter()
            .filter_map(mpsc::WeakSender::upgrade)
            .collect()
    };
    for queue in queues {
        flush_queue(&queue).await;
    }
}

async fn flush_queue(queue: &mpsc::Sender<Op>) {
    let (tx, rx) = oneshot::channel();
    if queue.send(Op::Flush(tx)).await.is_ok() {
        let _ = rx.await;
    }
}

struct Pending {
    seq: u64,
    response: Arc<Response>,
}

enum Op {
    Put(Box<Row>),
    /// Answered once every row queued before it has been written.
    Flush(oneshot::Sender<()>),
}

struct Row {
    seq: u64,
    key: String,
    method: String,
    status: i32,
    headers: HeaderMap,
    body: Vec<u8>,
    body_encoding: Option<String>,
    created_at: time::OffsetDateTime,
    /// `None`: no cache policy configured at all (see `set`).
    policy: Option<ResponseCachePolicy>,
}

async fn run_writer(
    db: DatabaseConnection,
    mut queue: mpsc::Receiver<Op>,
    pending: Arc<DashMap<String, Pending>>,
) {
    while let Some(first) = queue.recv().await {
        let mut rows = Vec::new();
        let mut flushes = Vec::new();
        let mut next = Some(first);
        let deadline = tokio::time::Instant::now() + BATCH_WINDOW;
        while let Some(op) = next.take() {
            match op {
                Op::Put(row) => rows.push(*row),
                Op::Flush(done) => flushes.push(done),
            }
            // A flush is waiting on this batch: write it now.
            if rows.len() >= BATCH_MAX || !flushes.is_empty() {
                break;
            }
            next = tokio::time::timeout_at(deadline, queue.recv())
                .await
                .ok()
                .flatten();
        }
        if !rows.is_empty() {
            // Only cache: a lost batch costs a refetch, never a failed request.
            if let Err(e) = write_batch(&db, &rows).await {
                tracing::warn!("HTTP cache: dropping {} response(s): {e}", rows.len());
            }
            for row in &rows {
                pending.remove_if(&row.key, |_, p| p.seq == row.seq);
            }
        }
        for done in flushes {
            let _ = done.send(());
        }
    }
}

/// `try_write_batch`, retried once when another process holds the lock past
/// `busy_timeout` (the server and an import can share the file).
async fn write_batch(db: &DatabaseConnection, rows: &[Row]) -> Result<(), DbErr> {
    match try_write_batch(db, rows).await {
        Err(e) if e.to_string().contains("database is locked") => {
            tokio::time::sleep(Duration::from_secs(1)).await;
            try_write_batch(db, rows).await
        }
        res => res,
    }
}

async fn try_write_batch(db: &DatabaseConnection, rows: &[Row]) -> Result<(), DbErr> {
    let txn = db.begin().await?;

    // The previous status and streak of every key, for `consecutive_count`.
    // Read in the batch's own transaction, so with one writer it can't race.
    let keys: HashSet<&str> = rows.iter().map(|r| r.key.as_str()).collect();
    let mut streaks: HashMap<String, (i32, i32)> = cache_entry::Entity::find()
        .select_only()
        .column(cache_entry::Column::Key)
        .column(cache_entry::Column::Status)
        .column(cache_entry::Column::ConsecutiveCount)
        .filter(cache_entry::Column::Key.is_in(keys))
        .into_tuple::<(String, i32, i32)>()
        .all(&txn)
        .await?
        .into_iter()
        .map(|(key, status, count)| (key, (status, count)))
        .collect();

    // In queue order, so a key set twice in one batch keeps the later row
    // and counts both.
    let mut latest: HashMap<&str, cache_entry::ActiveModel> = HashMap::new();
    for row in rows {
        let consecutive_count = match streaks.get(&row.key) {
            Some(&(status, count)) if status == row.status => count.max(0) + 1,
            _ => 0,
        };
        streaks.insert(row.key.clone(), (row.status, consecutive_count));
        let (expires_at, stale_at) = expiry(row.policy.as_ref(), consecutive_count, row.created_at);
        latest.insert(
            &row.key,
            cache_entry::ActiveModel {
                key: Set(row.key.clone()),
                method: Set(row.method.clone()),
                url: Set(String::new()),
                status: Set(row.status),
                headers: Set(row.headers.clone()),
                body: Set(row.body.clone()),
                body_encoding: Set(row.body_encoding.clone()),
                created_at: Set(row.created_at),
                expires_at: Set(expires_at),
                stale_at: Set(stale_at),
                consecutive_count: Set(consecutive_count),
            },
        );
    }

    cache_entry::Entity::insert_many(latest.into_values())
        .on_conflict(
            sea_query::OnConflict::column(cache_entry::Column::Key)
                .update_columns([
                    cache_entry::Column::Body,
                    cache_entry::Column::BodyEncoding,
                    cache_entry::Column::Method,
                    cache_entry::Column::Url,
                    cache_entry::Column::Headers,
                    cache_entry::Column::Status,
                    cache_entry::Column::CreatedAt,
                    cache_entry::Column::ExpiresAt,
                    cache_entry::Column::StaleAt,
                    cache_entry::Column::ConsecutiveCount,
                ])
                .to_owned(),
        )
        .exec(&txn)
        .await?;
    txn.commit().await
}

/// `(expires_at, stale_at)` for a response stored at `now`.
fn expiry(
    policy: Option<&ResponseCachePolicy>,
    consecutive_count: i32,
    now: time::OffsetDateTime,
) -> (time::OffsetDateTime, time::OffsetDateTime) {
    match policy {
        None => (now + time::Duration::days(1), now),
        Some(p) => {
            let ttl = p.ttl.compute(consecutive_count.max(0) as u32);
            let ttl = time::Duration::try_from(ttl).unwrap_or(time::Duration::days(1));
            let swr = time::Duration::try_from(p.swr).unwrap_or(time::Duration::ZERO);
            (now + ttl, now + ttl + swr)
        }
    }
}

const ZLIB_ENCODING: &str = "zlib";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RecompressionStats {
    pub scanned: usize,
    pub compressed: usize,
    pub unchanged: usize,
    pub saved_bytes: u64,
}

fn encode_body(body: &[u8]) -> Result<(Vec<u8>, Option<String>), Error> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(body).map_err(Error::BodyCompression)?;
    let compressed = encoder.finish().map_err(Error::BodyCompression)?;

    // Small or already-compressed responses can grow under zlib. Keep those
    // in their original form and mark them as identity by leaving the marker
    // NULL.
    if compressed.len() < body.len() {
        Ok((compressed, Some(ZLIB_ENCODING.to_owned())))
    } else {
        Ok((body.to_vec(), None))
    }
}

fn decode_body(body: Vec<u8>, encoding: Option<&str>) -> Result<Vec<u8>, Error> {
    match encoding {
        None | Some("identity") => Ok(body),
        Some(ZLIB_ENCODING) => {
            let mut decoder = ZlibDecoder::new(body.as_slice());
            let mut decoded = Vec::new();
            decoder
                .read_to_end(&mut decoded)
                .map_err(Error::BodyDecompression)?;
            Ok(decoded)
        }
        Some(other) => Err(Error::UnsupportedBodyEncoding(other.to_owned())),
    }
}

impl DbHttpCache {
    pub async fn new(db_url: String) -> Result<Self, Error> {
        Self::open(db_url).await
    }

    pub async fn new_in_memory() -> Result<Self, Error> {
        Self::open("sqlite::memory:".to_string()).await
    }

    async fn open(db_url: String) -> Result<Self, Error> {
        let is_memory = db_url.contains(":memory:") || db_url.contains("mode=memory");
        let db_url = if db_url.starts_with("sqlite://") && !db_url.contains('?') {
            format!("{db_url}?mode=rwc")
        } else {
            db_url
        };

        let writer = Database::connect(connect_options(&db_url, 1, false, is_memory)).await?;
        writer
            .get_schema_registry("musiclib_rs::httpcache::db::*")
            .sync(&writer)
            .await?;
        let readers = if is_memory {
            writer.clone()
        } else {
            Database::connect(connect_options(&db_url, READERS, true, false)).await?
        };

        let (queue, rx) = mpsc::channel(QUEUE_CAPACITY);
        QUEUES.lock().unwrap().push(queue.downgrade());
        let pending = Arc::new(DashMap::new());
        tokio::spawn(run_writer(writer.clone(), rx, Arc::clone(&pending)));

        Ok(Self {
            readers,
            writer,
            queue,
            pending,
            seq: AtomicU64::new(0),
        })
    }

    /// Commit every row queued so far.
    pub async fn flush(&self) {
        flush_queue(&self.queue).await;
    }
}

fn connect_options(
    db_url: &str,
    connections: u32,
    read_only: bool,
    is_memory: bool,
) -> ConnectOptions {
    let mut opts = ConnectOptions::new(db_url.to_owned());
    opts.sqlx_logging(false)
        .max_connections(connections)
        .acquire_timeout(ACQUIRE_WAIT);
    if !is_memory {
        // Per-connection settings, so they go on the template every pooled
        // connection is opened from, not in a one-off `PRAGMA`. The busy
        // timeout covers other processes writing the same file.
        opts.map_sqlx_sqlite_opts(move |o| {
            use sea_orm::sqlx::sqlite::{SqliteJournalMode, SqliteSynchronous};
            o.journal_mode(SqliteJournalMode::Wal)
                .synchronous(SqliteSynchronous::Normal)
                .busy_timeout(Duration::from_secs(30))
                .read_only(read_only)
        });
    }
    opts
}

/// Open the HTTP cache DB at `db_url` for the other stores kept in the same
/// file (listing snapshots, the quota ledger), without touching its schema.
/// Their traffic is about one read and one write per listing, so a few
/// connections suffice; the busy timeout covers the cache's writer.
pub async fn connect(db_url: String) -> Result<DatabaseConnection, DbErr> {
    let is_memory = db_url.contains(":memory:") || db_url.contains("mode=memory");
    let db_url = if db_url.starts_with("sqlite://") && !db_url.contains('?') {
        format!("{db_url}?mode=rwc")
    } else {
        db_url
    };
    let connections = if is_memory { 1 } else { 4 };
    Database::connect(connect_options(&db_url, connections, false, is_memory)).await
}

impl DbHttpCache {
    /// Recompress all existing identity-encoded entries that benefit from
    /// zlib. This is intentionally separate from opening the cache so normal
    /// startup never rewrites the whole database.
    pub async fn recompress_existing(&self) -> Result<RecompressionStats, Error> {
        let entries = cache_entry::Entity::find().all(&self.writer).await?;
        let mut stats = RecompressionStats {
            scanned: entries.len(),
            ..Default::default()
        };

        for model in entries {
            if model.body_encoding.as_deref() == Some(ZLIB_ENCODING) {
                stats.unchanged += 1;
                continue;
            }

            let old_len = model.body.len();
            let (body, encoding) = encode_body(&model.body)?;
            let Some(_) = encoding else {
                stats.unchanged += 1;
                continue;
            };

            let new_len = body.len();
            let mut active: cache_entry::ActiveModel = model.into();
            active.body = Set(body);
            active.body_encoding = Set(encoding);
            active.update(&self.writer).await?;

            stats.compressed += 1;
            stats.saved_bytes += (old_len - new_len) as u64;
        }

        Ok(stats)
    }

    pub async fn vacuum(&self) -> Result<(), Error> {
        self.writer.execute_unprepared("VACUUM").await?;
        Ok(())
    }

    fn map_response(model: cache_entry::Model) -> Result<RawResponse<'static>, Error> {
        let status = u16::try_from(model.status)
            .map_err(|_| Error::InvalidStatusCode(model.status))
            .and_then(|code| {
                StatusCode::from_u16(code).map_err(|_| Error::InvalidStatusCode(code as i32))
            })?;

        let body = reqwest::Body::from(decode_body(model.body, model.body_encoding.as_deref())?);
        let mut headers = Vec::new();

        for (k, v) in model.headers.0 {
            if let Ok(k) = HeaderName::from_str(&k)
                && let Ok(v) = HeaderValue::from_str(&v)
            {
                headers.push((k, v));
            }
        }

        let headers = Cow::Owned(headers);

        Ok(RawResponse {
            status,
            headers,
            body,
        })
    }
}

#[async_trait]
impl HttpCache for DbHttpCache {
    async fn set(
        &self,
        key: String,
        method: &crate::http::Method,
        policy: Option<&CachePolicy>,
        value: Arc<Response>,
    ) -> Result<(), super::Error> {
        let status = value.status.as_u16();

        // Two distinct "no policy" cases, handled differently:
        //   - No `CachePolicy` configured at all: cache with a short default,
        //     no SWR — a reasonable default for a minimal setup.
        //   - A `CachePolicy` *is* configured but has nothing to say about
        //     this exact status (no matching rule, or a matching rule with an
        //     explicit `policy: None`): don't cache at all. Falling back to
        //     the 1-day default here previously meant any status the policy
        //     author didn't anticipate — e.g. 429 — got cached as if it were
        //     a stable result, silently replaying our own cached rate-limit
        //     response for a day even after the real rate limit had cleared.
        //     An unconfigured status must default to "don't cache", not "cache
        //     for a day" — caching is an opt-in optimization, and the safe
        //     default on the "I didn't think about this" path is to just make
        //     the live request again next time.
        // Expiry itself is computed by the writer, which knows the previous
        // entry's streak (`consecutive_count`).
        let policy = match policy {
            None => None,
            Some(p) => match p.resolve(status) {
                None => return Ok(()),
                Some(response_policy) => Some(response_policy.clone()),
            },
        };

        let body = value
            .body_to_bytes()
            .await
            .map_err(|err| Error::BodyRead(Box::new(err)))?
            .to_vec();
        let (body, body_encoding) = encode_body(&body)?;
        let headers = HeaderMap(
            value
                .headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                .collect(),
        );

        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        self.pending.insert(
            key.clone(),
            Pending {
                seq,
                response: Arc::clone(&value),
            },
        );
        let row = Row {
            seq,
            key: key.clone(),
            method: method.to_string(),
            status: status as i32,
            headers,
            body,
            body_encoding,
            created_at: time::OffsetDateTime::now_utc(),
            policy,
        };
        if self.queue.send(Op::Put(Box::new(row))).await.is_err() {
            self.pending.remove_if(&key, |_, p| p.seq == seq);
            return Err(Error::WriterStopped.into());
        }
        Ok(())
    }

    async fn get(
        &self,
        key: &str,
        extractor: BodyExtractorCow<'static>,
    ) -> Result<Option<Arc<Response>>, super::Error> {
        if let Some(pending) = self.pending.get(key) {
            return Ok(Some(Arc::clone(&pending.response)));
        }
        match cache_entry::Entity::find_by_id(key)
            .one(&self.readers)
            .await
            .map_err(Error::Database)?
        {
            None => Ok(None),
            Some(model) => {
                let raw = Self::map_response(model)?;
                Ok(Some(Arc::new(Response::from_raw(raw, extractor))))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{borrow::Cow, time::Duration};

    use super::{DbHttpCache, decode_body, encode_body};
    use crate::{
        http::{BodyExtractor, Method, RawResponse, bytes_body_extractor},
        httpcache::{
            CachePolicy, HttpCache, ResponseCachePolicy, StatusCacheRule, StatusMatcher, TtlPolicy,
        },
    };

    #[test]
    fn zlib_round_trip_preserves_body() {
        let original =
            br#"{"items":[{"name":"WATAME","description":"repeated response data"}]}"#.repeat(32);
        let (encoded, encoding) = encode_body(&original).expect("compression succeeds");
        assert_eq!(encoding.as_deref(), Some("zlib"));
        assert!(encoded.len() < original.len());
        assert_eq!(decode_body(encoded, encoding.as_deref()).unwrap(), original);
    }

    #[test]
    fn incompressible_small_body_stays_identity() {
        let original = [0_u8, 1, 2, 3, 4, 5, 6, 7];
        let (encoded, encoding) = encode_body(&original).expect("compression succeeds");
        assert_eq!(encoding, None);
        assert_eq!(decode_body(encoded, None).unwrap(), original);
    }

    async fn response_with_status(status: reqwest::StatusCode) -> super::Response {
        bytes_body_extractor()
            .extract_response(RawResponse {
                status,
                headers: Cow::Borrowed(&[]),
                body: reqwest::Body::from(&[] as &[u8]),
            })
            .await
            .expect("constructing a bare response never fails")
    }

    /// A `CachePolicy` that only says anything about 200 — the shape of a
    /// real deployment's config (see `http.yaml`'s `default_policy`), which
    /// has no rule for 429 at all.
    fn policy_covering_only_200() -> CachePolicy {
        CachePolicy {
            status_rules: vec![StatusCacheRule {
                status: StatusMatcher::Exact(200),
                policy: Some(ResponseCachePolicy {
                    ttl: TtlPolicy::Fixed(Duration::from_secs(3600)),
                    swr: Duration::ZERO,
                }),
            }],
        }
    }

    #[tokio::test]
    async fn status_with_no_matching_rule_is_not_cached() {
        // Regression test: a status the policy author never anticipated (429,
        // in the incident this guards against — Spotify rate-limiting us for
        // ~8.3h) must not fall back to a default cache duration. Caching an
        // unconfigured status silently replays that response — a rate limit
        // in this case — long after the real condition may have cleared.
        let cache = DbHttpCache::new_in_memory().await.unwrap();
        let policy = policy_covering_only_200();
        let res = response_with_status(reqwest::StatusCode::TOO_MANY_REQUESTS).await;

        cache
            .set(
                "key:429".to_string(),
                &Method::GET,
                Some(&policy),
                std::sync::Arc::new(res),
            )
            .await
            .unwrap();

        let hit = cache
            .get("key:429", bytes_body_extractor().into())
            .await
            .unwrap();
        assert!(
            hit.is_none(),
            "429 with no matching rule must not be cached"
        );
    }

    #[tokio::test]
    async fn explicit_none_policy_is_not_cached() {
        // A rule that matches but explicitly opts out (`policy: None`) must
        // behave the same as "no rule at all" — not fall back to a default.
        let cache = DbHttpCache::new_in_memory().await.unwrap();
        let policy = CachePolicy {
            status_rules: vec![StatusCacheRule {
                status: StatusMatcher::Exact(429),
                policy: None,
            }],
        };
        let res = response_with_status(reqwest::StatusCode::TOO_MANY_REQUESTS).await;

        cache
            .set(
                "key:429-explicit".to_string(),
                &Method::GET,
                Some(&policy),
                std::sync::Arc::new(res),
            )
            .await
            .unwrap();

        let hit = cache
            .get("key:429-explicit", bytes_body_extractor().into())
            .await
            .unwrap();
        assert!(hit.is_none(), "explicit `policy: None` must not be cached");
    }

    #[tokio::test]
    async fn status_with_a_configured_rule_is_still_cached() {
        // Contrast case: this behavior change must not stop statuses that
        // *are* configured from caching normally.
        let cache = DbHttpCache::new_in_memory().await.unwrap();
        let policy = policy_covering_only_200();
        let res = response_with_status(reqwest::StatusCode::OK).await;

        cache
            .set(
                "key:200".to_string(),
                &Method::GET,
                Some(&policy),
                std::sync::Arc::new(res),
            )
            .await
            .unwrap();

        let hit = cache
            .get("key:200", bytes_body_extractor().into())
            .await
            .unwrap();
        assert!(
            hit.is_some(),
            "a status with a configured rule must still cache"
        );
    }

    /// The stored `(status, consecutive_count)` of `key`, read past the queue.
    async fn streak(cache: &DbHttpCache, key: &str) -> Option<(i32, i32)> {
        use sea_orm::EntityTrait;
        super::cache_entry::Entity::find_by_id(key)
            .one(&cache.writer)
            .await
            .unwrap()
            .map(|m| (m.status, m.consecutive_count))
    }

    async fn put(cache: &DbHttpCache, key: &str, status: reqwest::StatusCode) {
        let res = std::sync::Arc::new(response_with_status(status).await);
        cache
            .set(key.to_string(), &Method::GET, None, res)
            .await
            .unwrap();
    }

    /// A queued row is served before its batch commits, and from the DB after.
    #[tokio::test]
    async fn queued_row_is_readable_before_and_after_commit() {
        let cache = DbHttpCache::new_in_memory().await.unwrap();
        put(&cache, "k", reqwest::StatusCode::OK).await;
        let hit = cache.get("k", bytes_body_extractor().into()).await.unwrap();
        assert!(hit.is_some());

        cache.flush().await;
        assert!(cache.pending.is_empty());
        assert_eq!(streak(&cache, "k").await, Some((200, 0)));
        let hit = cache.get("k", bytes_body_extractor().into()).await.unwrap();
        assert_eq!(hit.unwrap().status, reqwest::StatusCode::OK);
    }

    /// `consecutive_count` counts repeats of one status across batches and
    /// within one, and resets when the status changes.
    #[tokio::test]
    async fn consecutive_count_tracks_status_streaks() {
        let cache = DbHttpCache::new_in_memory().await.unwrap();
        put(&cache, "k", reqwest::StatusCode::NOT_FOUND).await;
        cache.flush().await;
        put(&cache, "k", reqwest::StatusCode::NOT_FOUND).await;
        put(&cache, "k", reqwest::StatusCode::NOT_FOUND).await;
        cache.flush().await;
        assert_eq!(streak(&cache, "k").await, Some((404, 2)));

        put(&cache, "k", reqwest::StatusCode::OK).await;
        cache.flush().await;
        assert_eq!(streak(&cache, "k").await, Some((200, 0)));
    }

    /// More rows than one batch holds all land, and `flush_all` commits them.
    #[tokio::test]
    async fn flush_all_commits_every_queued_row() {
        let cache = DbHttpCache::new_in_memory().await.unwrap();
        let n = super::BATCH_MAX * 2 + 7;
        for i in 0..n {
            put(&cache, &format!("k{i}"), reqwest::StatusCode::OK).await;
        }
        super::flush_all().await;
        assert!(cache.pending.is_empty());
        for i in [0, super::BATCH_MAX, n - 1] {
            assert!(streak(&cache, &format!("k{i}")).await.is_some());
        }
    }

    /// On a file, reads go through the read-only pool and see committed rows,
    /// including from a cache opened later on the same file.
    #[tokio::test]
    async fn file_cache_reads_committed_rows() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("cache.db").display());
        let cache = DbHttpCache::new(url.clone()).await.unwrap();
        put(&cache, "k", reqwest::StatusCode::OK).await;
        cache.flush().await;
        let hit = cache.get("k", bytes_body_extractor().into()).await.unwrap();
        assert!(hit.is_some());
        drop(cache);

        let reopened = DbHttpCache::new(url).await.unwrap();
        let hit = reopened
            .get("k", bytes_body_extractor().into())
            .await
            .unwrap();
        assert_eq!(hit.unwrap().status, reqwest::StatusCode::OK);
    }
}
