use sea_orm::{
    ActiveValue::Set, ColumnTrait, Database, DatabaseConnection, DbErr, EntityTrait, InsertResult,
    QueryFilter, sea_query,
};

use crate::providers::types::{Alias, Contribution, EntrySpecificData, EntryType};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Database error: {0}")]
    Database(#[from] DbErr),
}

mod entry {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "entry")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub entry_type: String,
        pub release_date: Option<String>,
        pub extra: Option<String>,
        pub fetched_at: i64,
        // Track-specific
        pub duration_ms: Option<i64>,
        // Release-specific
        pub release_type: Option<String>,
        pub num_discs: Option<i32>,
        pub num_tracks: Option<i32>,
        // ReleaseGroup-specific
        pub primary_type: Option<String>,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

mod entry_source {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "entry_source")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub entry_id: i64,
        #[sea_orm(primary_key, auto_increment = false)]
        pub source: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub identifier: String,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

mod entry_alias {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "entry_alias")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub entry_id: i64,
        pub name: String,
        pub source: String,
        pub locale: Option<String>,
        pub extra: Option<String>,
        pub primary: bool,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

mod entry_child {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "entry_child")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub parent_id: i64,
        #[sea_orm(primary_key, auto_increment = false)]
        pub child_id: i64,
        pub disc_no: Option<i32>,
        pub track_no: Option<i32>,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

mod contribution {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "contribution")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub entry_id: i64,
        pub artist_id: i64,
        pub role: String,
        pub main_artist: bool,
        pub extra: Option<String>,
        pub source: String,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

pub struct MusicDb {
    db: DatabaseConnection,
}

impl MusicDb {
    pub async fn new(db_url: &str) -> Result<Self, Error> {
        let db = Database::connect(db_url).await?;
        db.get_schema_registry("musiclib_rs::musicdb::*")
            .sync(&db)
            .await?;
        Ok(Self { db })
    }

    /// Find an existing entry by a known (source, identifier) pair.
    pub async fn find_entry_by_source(
        &self,
        source: &str,
        identifier: &str,
    ) -> Result<Option<i64>, Error> {
        Ok(entry_source::Entity::find()
            .filter(entry_source::Column::Source.eq(source))
            .filter(entry_source::Column::Identifier.eq(identifier))
            .one(&self.db)
            .await?
            .map(|m| m.entry_id))
    }

    /// Insert a new entry row and return its auto-assigned id.
    pub async fn insert_entry(
        &self,
        entry_type: EntryType,
        release_date: Option<&str>,
        extra: Option<String>,
        specific_data: &EntrySpecificData,
    ) -> Result<i64, Error> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let (duration_ms, release_type, num_discs, num_tracks, primary_type) = match specific_data {
            EntrySpecificData::Track { duration_ms, .. } => (*duration_ms, None, None, None, None),
            EntrySpecificData::Release {
                release_type,
                num_discs,
                num_tracks,
            } => (None, release_type.clone(), *num_discs, *num_tracks, None),
            EntrySpecificData::ReleaseGroup { primary_type } => {
                (None, None, None, None, primary_type.clone())
            }
            EntrySpecificData::Artist => (None, None, None, None, None),
        };

        let entry_type_str = match entry_type {
            EntryType::Artist => "artist",
            EntryType::ReleaseGroup => "release_group",
            EntryType::Release => "release",
            EntryType::Track => "track",
        };

        let result = entry::Entity::insert(entry::ActiveModel {
            id: sea_orm::ActiveValue::NotSet,
            entry_type: Set(entry_type_str.to_string()),
            release_date: Set(release_date.map(|s| s.to_string())),
            extra: Set(extra),
            fetched_at: Set(now),
            duration_ms: Set(duration_ms),
            release_type: Set(release_type),
            num_discs: Set(num_discs),
            num_tracks: Set(num_tracks),
            primary_type: Set(primary_type),
        })
        .exec(&self.db)
        .await?;

        Ok(result.last_insert_id)
    }

    /// Insert a (source, identifier) pair for an entry. Silently ignores duplicates.
    pub async fn insert_source(
        &self,
        entry_id: i64,
        source: &str,
        identifier: &str,
    ) -> Result<(), Error> {
        ignore_not_inserted(
            entry_source::Entity::insert(entry_source::ActiveModel {
                entry_id: Set(entry_id),
                source: Set(source.to_string()),
                identifier: Set(identifier.to_string()),
            })
            .on_conflict(
                sea_query::OnConflict::columns([
                    entry_source::Column::EntryId,
                    entry_source::Column::Source,
                    entry_source::Column::Identifier,
                ])
                .do_nothing()
                .to_owned(),
            )
            .exec(&self.db)
            .await,
        )
    }

    pub async fn insert_alias(&self, entry_id: i64, alias: &Alias) -> Result<(), Error> {
        entry_alias::Entity::insert(entry_alias::ActiveModel {
            id: sea_orm::ActiveValue::NotSet,
            entry_id: Set(entry_id),
            name: Set(alias.name.clone()),
            source: Set(alias.source.clone()),
            locale: Set(alias.locale.clone()),
            extra: Set(Some(alias.extra.to_string())),
            primary: Set(alias.primary),
        })
        .exec(&self.db)
        .await?;
        Ok(())
    }

    /// Insert a parent→child edge. Silently ignores duplicate edges.
    pub async fn insert_child_edge(
        &self,
        parent_id: i64,
        child_id: i64,
        disc_no: Option<i32>,
        track_no: Option<i32>,
    ) -> Result<(), Error> {
        ignore_not_inserted(
            entry_child::Entity::insert(entry_child::ActiveModel {
                parent_id: Set(parent_id),
                child_id: Set(child_id),
                disc_no: Set(disc_no),
                track_no: Set(track_no),
            })
            .on_conflict(
                sea_query::OnConflict::columns([
                    entry_child::Column::ParentId,
                    entry_child::Column::ChildId,
                ])
                .do_nothing()
                .to_owned(),
            )
            .exec(&self.db)
            .await,
        )
    }

    /// Insert an artist contribution to an entry.
    #[allow(dead_code)] // may be unused depending on provider output
    pub async fn insert_contribution(
        &self,
        entry_id: i64,
        artist_id: i64,
        contrib: &Contribution,
    ) -> Result<(), Error> {
        contribution::Entity::insert(contribution::ActiveModel {
            id: sea_orm::ActiveValue::NotSet,
            entry_id: Set(entry_id),
            artist_id: Set(artist_id),
            role: Set(contrib.role.clone()),
            main_artist: Set(contrib.main_artist),
            extra: Set(Some(contrib.extra.to_string())),
            source: Set(contrib.source.to_string()),
        })
        .exec(&self.db)
        .await?;
        Ok(())
    }
}

/// SeaORM returns `DbErr::RecordNotInserted` when `ON CONFLICT DO NOTHING` skips a row.
/// Treat that as success for idempotent inserts.
fn ignore_not_inserted<T: sea_orm::ActiveModelTrait>(
    result: Result<InsertResult<T>, DbErr>,
) -> Result<(), Error> {
    match result {
        Ok(_) | Err(DbErr::RecordNotInserted) => Ok(()),
        Err(e) => Err(Error::Database(e)),
    }
}
