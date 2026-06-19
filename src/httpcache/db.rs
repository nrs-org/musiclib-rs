use std::{borrow::Cow, str::FromStr, sync::Arc};

use async_trait::async_trait;
use reqwest::StatusCode;
use sea_orm::{
    ActiveValue::Set, ConnectOptions, Database, DatabaseConnection, DbErr, EntityTrait,
    FromJsonQueryResult, sea_query,
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
        }

        let db = Database::connect(opts).await?;

        if !is_memory {
            // WAL is a persistent, DB-level setting; enabling it once lets many
            // readers and one writer proceed concurrently (cache hits are reads).
            // busy_timeout tells SQLite how long to spin-wait for the write lock
            // before returning SQLITE_BUSY; 30 s covers worst-case write queues
            // when buffer_unordered fans out many concurrent cache writes.
            sea_orm::ConnectionTrait::execute_unprepared(
                &db,
                "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=30000;",
            )
            .await?;
        }

        db.get_schema_registry("musiclib_rs::httpcache::db::*")
            .sync(&db)
            .await?;
        Ok(db)
    }
}

impl DbHttpCache {
    fn map_response(model: cache_entry::Model) -> Result<RawResponse<'static>, Error> {
        let status = u16::try_from(model.status)
            .map_err(|_| Error::InvalidStatusCode(model.status))
            .and_then(|code| {
                StatusCode::from_u16(code).map_err(|_| Error::InvalidStatusCode(code as i32))
            })?;

        let body = reqwest::Body::from(model.body);
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
            body: Set(value
                .body_to_bytes()
                .await
                .map_err(|err| Error::BodyRead(Box::new(err)))?
                .to_vec()),
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
