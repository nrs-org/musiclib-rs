use std::{borrow::Cow, io::Read, io::Write, str::FromStr, sync::Arc};

use async_trait::async_trait;
use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use reqwest::StatusCode;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ConnectOptions, ConnectionTrait, Database,
    DatabaseConnection, DbErr, EntityTrait, FromJsonQueryResult, sea_query,
};
use serde::{Deserialize, Serialize};

use crate::{
    http::{BodyExtractorCow, HeaderName, HeaderValue, RawResponse, Response},
    httpcache::{CachePolicy, HttpCache},
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

pub struct DbHttpCache {
    db: DatabaseConnection,
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
        let db = Self::init_db(db_url).await?;
        Ok(Self { db })
    }

    pub async fn new_in_memory() -> Result<Self, Error> {
        let db = Self::init_db("sqlite::memory:".to_string()).await?;
        Ok(Self { db })
    }

    async fn init_db(db_url: String) -> Result<DatabaseConnection, Error> {
        // A `:memory:` database is private per connection, so it must use a
        // single pooled connection or every cursor sees an empty DB.
        let is_memory = db_url.contains(":memory:") || db_url.contains("mode=memory");

        let db_url = if db_url.starts_with("sqlite://") && !db_url.contains('?') {
            format!("{db_url}?mode=rwc")
        } else {
            db_url
        };

        let mut opts = ConnectOptions::new(db_url);
        opts.sqlx_logging(false);
        if is_memory {
            opts.max_connections(1);
        } else {
            // A real connection pool (not a single serialized connection) is
            // required: the importer fans out cache reads/writes via
            // `buffer_unordered`, and funnelling them all through one connection
            // — or, worse, a global mutex held across body extraction —
            // deadlocks. sqlx applies a 5s `busy_timeout` per connection by
            // default, so concurrent writers wait rather than erroring.
            opts.max_connections(32)
                .acquire_timeout(std::time::Duration::from_secs(30));

            // journal_mode/synchronous/busy_timeout are per-*connection*
            // SQLite settings, so they must go through `map_sqlx_sqlite_opts`
            // (which customizes the template `SqliteConnectOptions` the pool
            // uses to establish every connection) rather than a `PRAGMA ...`
            // statement run once against the pooled `DatabaseConnection`
            // after connecting -- that only ever lands on whichever single
            // connection happens to service it, leaving the other 31 at
            // sqlx-sqlite's bare default (5s busy_timeout) instead of the 30s
            // intended here to cover worst-case write queues when
            // `buffer_unordered` fans out many concurrent cache writes.
            opts.map_sqlx_sqlite_opts(|o| {
                use sea_orm::sqlx::sqlite::{SqliteJournalMode, SqliteSynchronous};
                o.journal_mode(SqliteJournalMode::Wal)
                    .synchronous(SqliteSynchronous::Normal)
                    .busy_timeout(std::time::Duration::from_secs(30))
            });
        }

        let db = Database::connect(opts).await?;

        db.get_schema_registry("musiclib_rs::httpcache::db::*")
            .sync(&db)
            .await?;
        Ok(db)
    }
}

impl DbHttpCache {
    /// Recompress all existing identity-encoded entries that benefit from
    /// zlib. This is intentionally separate from opening the cache so normal
    /// startup never rewrites the whole database.
    pub async fn recompress_existing(&self) -> Result<RecompressionStats, Error> {
        let entries = cache_entry::Entity::find().all(&self.db).await?;
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
            active.update(&self.db).await?;

            stats.compressed += 1;
            stats.saved_bytes += (old_len - new_len) as u64;
        }

        Ok(stats)
    }

    pub async fn vacuum(&self) -> Result<(), Error> {
        self.db.execute_unprepared("VACUUM").await?;
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

        // Fetch the previous entry for this key to determine consecutive_count.
        let prev = cache_entry::Entity::find_by_id(&key)
            .one(&self.db)
            .await
            .map_err(Error::Database)?;

        let consecutive_count = match &prev {
            Some(m) if m.status == status as i32 => m.consecutive_count.max(0) + 1,
            _ => 0,
        };

        // Resolve cache policy for this status code, then compute expiry times.
        let (expires_at, stale_at) = match policy.and_then(|p| p.resolve(status)) {
            None => {
                // No policy or no matching rule: cache with a short default, no SWR.
                let now = time::OffsetDateTime::now_utc();
                (now + time::Duration::days(1), now)
            }
            Some(response_policy) => {
                let ttl = response_policy.ttl.compute(consecutive_count as u32);
                let ttl = time::Duration::try_from(ttl).unwrap_or(time::Duration::days(1));
                let swr =
                    time::Duration::try_from(response_policy.swr).unwrap_or(time::Duration::ZERO);

                let now = time::OffsetDateTime::now_utc();
                (now + ttl, now + ttl + swr)
            }
        };

        let body = value
            .body_to_bytes()
            .await
            .map_err(|err| Error::BodyRead(Box::new(err)))?
            .to_vec();
        let (body, body_encoding) = encode_body(&body)?;

        let entry = cache_entry::ActiveModel {
            key: Set(key),
            method: Set(method.to_string()),
            url: Set("".to_string()),
            status: Set(status as i32),
            headers: Set(HeaderMap(
                value
                    .headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                    .collect(),
            )),
            body: Set(body),
            body_encoding: Set(body_encoding),
            created_at: Set(time::OffsetDateTime::now_utc()),
            expires_at: Set(expires_at),
            stale_at: Set(stale_at),
            consecutive_count: Set(consecutive_count),
        };

        cache_entry::Entity::insert(entry)
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
            .exec(&self.db)
            .await
            .map_err(Error::Database)?;

        Ok(())
    }

    async fn get(
        &self,
        key: &str,
        extractor: BodyExtractorCow<'static>,
    ) -> Result<Option<Arc<Response>>, super::Error> {
        match cache_entry::Entity::find_by_id(key)
            .one(&self.db)
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
    use super::{decode_body, encode_body};

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
}
