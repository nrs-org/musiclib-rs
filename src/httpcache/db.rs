use std::{str::FromStr, sync::Arc};

use async_trait::async_trait;
use reqwest::StatusCode;
use sea_orm::{
    ActiveValue::Set, Database, DatabaseConnection, DbErr, EntityTrait, FromJsonQueryResult,
    sea_query,
};
use serde::{Deserialize, Serialize};

use crate::{
    http::{BodyExtractor, HeaderName, HeaderValue, Response},
    httpcache::HttpCache,
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Invalid status code in cache entry: {0}")]
    InvalidStatusCode(i32),
    #[error("Database error: {0}")]
    DatabaseError(#[from] DbErr),
    #[error("Response construction error: {0}")]
    ResponseError(#[from] http::Error),
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
        let db = Database::connect(db_url).await.unwrap();
        db.get_schema_registry("musiclib_rs::httpcache::db::*")
            .sync(&db)
            .await?;
        Ok(db)
    }
}

impl DbHttpCache {
    fn map_response(model: cache_entry::Model) -> Result<reqwest::Response, Error> {
        let status = u16::try_from(model.status)
            .map_err(|_| Error::InvalidStatusCode(model.status))
            .and_then(|code| {
                StatusCode::from_u16(code).map_err(|_| Error::InvalidStatusCode(code as i32))
            })?;

        let mut response = http::Response::builder().status(status).body(model.body)?;

        for (k, v) in model.headers.0 {
            if let Ok(k) = HeaderName::from_str(&k)
                && let Ok(v) = HeaderValue::from_str(&v)
            {
                response.headers_mut().insert(k, v);
            }
        }

        Ok(reqwest::Response::from(response))
    }
}

#[async_trait]
impl HttpCache for DbHttpCache {
    async fn set(&self, key: String, value: Arc<Response>) -> Result<(), super::Error> {
        let now = time::OffsetDateTime::now_utc();
        let entry = cache_entry::ActiveModel {
            key: Set(key),
            method: Set(value.status.as_u16().to_string()),
            url: Set("".to_string()), // URL is not used in this implementation
            status: Set(value.status.as_u16() as i32),
            headers: Set(HeaderMap(
                value
                    .headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                    .collect(),
            )),
            body: Set(value.body.to_bytes()?),
            created_at: Set(now),
            expires_at: Set(now + time::Duration::days(7)), // Example expiration
            stale_at: Set(now + time::Duration::days(3)),   // Example staleness
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
                    ])
                    .to_owned(),
            )
            .exec(&self.db)
            .await
            .map_err(Error::DatabaseError)?;

        Ok(())
    }

    async fn get(
        &self,
        key: &str,
        extractor: &dyn BodyExtractor,
    ) -> Result<Option<Arc<Response>>, super::Error> {
        match cache_entry::Entity::find_by_id(key)
            .one(&self.db)
            .await
            .map_err(Error::DatabaseError)?
        {
            None => Ok(None),
            Some(model) => {
                let response = Self::map_response(model)?;
                Ok(Some(Arc::new(extractor.extract_response(response).await?)))
            }
        }
    }
}
