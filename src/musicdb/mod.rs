use sea_orm::{
    ActiveValue::Set, ColumnTrait, Database, DatabaseConnection, DbErr, EntityTrait, QueryFilter,
    sea_query,
};

use crate::providers::types::{Alias, Contribution, EntrySpecificData, EntryType};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Database error: {0}")]
    Database(#[from] DbErr),
}

// Pair-centric schema. Metadata lives on `entry_source` (keyed by the
// (source, identifier) pair); `entry` is a grouping primitive whose id is
// referenced from `entry_source.entry_id` only. Aliases, child edges, and
// contributions are pair-keyed, so a DB-level entry merge only needs to
// re-point `entry_source` rows.

mod entry {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "entry")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub entry_type: String,
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
        pub source: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub identifier: String,
        pub entry_id: i64,
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

mod entry_alias {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "entry_alias")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub source: String,
        pub identifier: String,
        pub name: String,
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
        pub parent_source: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub parent_identifier: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub child_source: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub child_identifier: String,
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
        pub source: String,
        pub identifier: String,
        pub artist_source: String,
        pub artist_identifier: String,
        pub role: String,
        pub main_artist: bool,
        pub extra: Option<String>,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

pub struct MusicDb {
    db: DatabaseConnection,
}

impl MusicDb {
    pub async fn new(db_url: &str) -> Result<Self, Error> {
        let db = Database::connect(db_url).await?;
        sea_orm::ConnectionTrait::execute_unprepared(
            &db,
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;",
        )
        .await?;
        db.get_schema_registry("musiclib_rs::musicdb::*")
            .sync(&db)
            .await?;
        Ok(Self { db })
    }

    /// Look up the entry_id grouping a given (source, identifier) pair, if any.
    pub async fn find_entry_id_by_pair(
        &self,
        source: &str,
        identifier: &str,
    ) -> Result<Option<i64>, Error> {
        Ok(
            entry_source::Entity::find_by_id((source.to_string(), identifier.to_string()))
                .one(&self.db)
                .await?
                .map(|m| m.entry_id),
        )
    }

    /// Allocate a fresh `entry` row and return its auto-assigned id.
    pub async fn insert_entry(&self, entry_type: Option<EntryType>) -> Result<i64, Error> {
        let result = entry::Entity::insert(entry::ActiveModel {
            id: sea_orm::ActiveValue::NotSet,
            entry_type: Set(entry_type_str(entry_type).to_string()),
        })
        .exec(&self.db)
        .await?;
        Ok(result.last_insert_id)
    }

    /// Update the `entry_type` column for an existing entry.
    pub async fn set_entry_type(&self, entry_id: i64, entry_type: EntryType) -> Result<(), Error> {
        entry::Entity::update_many()
            .col_expr(
                entry::Column::EntryType,
                sea_query::Expr::value(entry_type_str(Some(entry_type))),
            )
            .filter(entry::Column::Id.eq(entry_id))
            .exec(&self.db)
            .await?;
        Ok(())
    }

    /// Re-point every `entry_source` row from `loser` to `winner` and delete the
    /// loser `entry` row. No other tables reference `entry_id`, so this is the
    /// entirety of a DB-level entry merge.
    pub async fn merge_entries(&self, loser: i64, winner: i64) -> Result<(), Error> {
        if loser == winner {
            return Ok(());
        }
        entry_source::Entity::update_many()
            .col_expr(
                entry_source::Column::EntryId,
                sea_query::Expr::value(winner),
            )
            .filter(entry_source::Column::EntryId.eq(loser))
            .exec(&self.db)
            .await?;
        entry::Entity::delete_by_id(loser).exec(&self.db).await?;
        Ok(())
    }

    /// Insert or update the metadata row for a pair. Updates every metadata
    /// column on conflict — the importer is the only writer and its final
    /// observation wins.
    pub async fn upsert_pair(
        &self,
        source: &str,
        identifier: &str,
        entry_id: i64,
        release_date: Option<&str>,
        extra: Option<String>,
        specific_data: &EntrySpecificData,
    ) -> Result<(), Error> {
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

        entry_source::Entity::insert(entry_source::ActiveModel {
            source: Set(source.to_string()),
            identifier: Set(identifier.to_string()),
            entry_id: Set(entry_id),
            release_date: Set(release_date.map(|s| s.to_string())),
            extra: Set(extra),
            fetched_at: Set(now),
            duration_ms: Set(duration_ms),
            release_type: Set(release_type),
            num_discs: Set(num_discs),
            num_tracks: Set(num_tracks),
            primary_type: Set(primary_type),
        })
        .on_conflict(
            sea_query::OnConflict::columns([
                entry_source::Column::Source,
                entry_source::Column::Identifier,
            ])
            .update_columns([
                entry_source::Column::EntryId,
                entry_source::Column::ReleaseDate,
                entry_source::Column::Extra,
                entry_source::Column::FetchedAt,
                entry_source::Column::DurationMs,
                entry_source::Column::ReleaseType,
                entry_source::Column::NumDiscs,
                entry_source::Column::NumTracks,
                entry_source::Column::PrimaryType,
            ])
            .to_owned(),
        )
        .exec(&self.db)
        .await?;
        Ok(())
    }

    /// Insert a stub row for a pair we never fetched but which is referenced by
    /// an is_rel / has_rel / contribution. Binds the pair to its assigned
    /// `entry_id` so equivalence-class queries see it. Uses `entry_type =
    /// "unknown"` and leaves other metadata NULL. Does nothing on conflict so a
    /// stub will never overwrite a real metadata row.
    pub async fn insert_stub_pair(
        &self,
        source: &str,
        identifier: &str,
        entry_id: i64,
    ) -> Result<(), Error> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        ignore_not_inserted(
            entry_source::Entity::insert(entry_source::ActiveModel {
                source: Set(source.to_string()),
                identifier: Set(identifier.to_string()),
                entry_id: Set(entry_id),
                release_date: Set(None),
                extra: Set(None),
                fetched_at: Set(now),
                duration_ms: Set(None),
                release_type: Set(None),
                num_discs: Set(None),
                num_tracks: Set(None),
                primary_type: Set(None),
            })
            .on_conflict(
                sea_query::OnConflict::columns([
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

    /// Insert aliases for a pair. Caller is responsible for deduping if needed
    /// — the table has no uniqueness constraint on (pair, name, locale).
    pub async fn insert_aliases_for_pair(
        &self,
        source: &str,
        identifier: &str,
        aliases: &[Alias],
    ) -> Result<(), Error> {
        if aliases.is_empty() {
            return Ok(());
        }
        let models: Vec<entry_alias::ActiveModel> = aliases
            .iter()
            .map(|alias| entry_alias::ActiveModel {
                id: sea_orm::ActiveValue::NotSet,
                source: Set(source.to_string()),
                identifier: Set(identifier.to_string()),
                name: Set(alias.name.clone()),
                locale: Set(alias.locale.clone()),
                extra: Set(Some(alias.extra.to_string())),
                primary: Set(alias.primary),
            })
            .collect();
        entry_alias::Entity::insert_many(models)
            .exec(&self.db)
            .await?;
        Ok(())
    }

    /// Insert a parent→child edge between two pairs. Idempotent on the
    /// composite PK (parent_pair, child_pair).
    pub async fn insert_child_edge(
        &self,
        parent_source: &str,
        parent_identifier: &str,
        child_source: &str,
        child_identifier: &str,
        disc_no: Option<i32>,
        track_no: Option<i32>,
    ) -> Result<(), Error> {
        ignore_not_inserted(
            entry_child::Entity::insert(entry_child::ActiveModel {
                parent_source: Set(parent_source.to_string()),
                parent_identifier: Set(parent_identifier.to_string()),
                child_source: Set(child_source.to_string()),
                child_identifier: Set(child_identifier.to_string()),
                disc_no: Set(disc_no),
                track_no: Set(track_no),
            })
            .on_conflict(
                sea_query::OnConflict::columns([
                    entry_child::Column::ParentSource,
                    entry_child::Column::ParentIdentifier,
                    entry_child::Column::ChildSource,
                    entry_child::Column::ChildIdentifier,
                ])
                .do_nothing()
                .to_owned(),
            )
            .exec(&self.db)
            .await,
        )
    }

    /// Insert a contribution attached to a (parent_pair, artist_pair) edge.
    pub async fn insert_contribution(
        &self,
        source: &str,
        identifier: &str,
        artist_source: &str,
        artist_identifier: &str,
        contrib: &Contribution,
    ) -> Result<(), Error> {
        contribution::Entity::insert(contribution::ActiveModel {
            id: sea_orm::ActiveValue::NotSet,
            source: Set(source.to_string()),
            identifier: Set(identifier.to_string()),
            artist_source: Set(artist_source.to_string()),
            artist_identifier: Set(artist_identifier.to_string()),
            role: Set(contrib.role.clone()),
            main_artist: Set(contrib.main_artist),
            extra: Set(Some(contrib.extra.to_string())),
        })
        .exec(&self.db)
        .await?;
        Ok(())
    }
}

fn entry_type_str(entry_type: Option<EntryType>) -> &'static str {
    match entry_type {
        Some(EntryType::Artist) => "artist",
        Some(EntryType::ReleaseGroup) => "release_group",
        Some(EntryType::Release) => "release",
        Some(EntryType::Track) => "track",
        None => "unknown",
    }
}

fn ignore_not_inserted<T: sea_orm::ActiveModelTrait>(
    result: Result<sea_orm::InsertResult<T>, DbErr>,
) -> Result<(), Error> {
    match result {
        Ok(_) | Err(DbErr::RecordNotInserted) => Ok(()),
        Err(e) => Err(Error::Database(e)),
    }
}
