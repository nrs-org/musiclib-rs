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

// Soft-dedup relation edges. Each row is a heuristic (or manual) assertion
// that two entries are related in some way. `entry_a < entry_b` is enforced
// at insertion time so there is at most one row per (entry_a, entry_b, kind)
// triple regardless of which side was passed first. Disabling a row (enabled=0)
// tombstones the decision without losing history.
mod entry_relation {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "entry_relation")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub entry_a: i64,
        #[sea_orm(primary_key, auto_increment = false)]
        pub entry_b: i64,
        #[sea_orm(primary_key, auto_increment = false)]
        pub kind: String,
        pub confidence: f64,
        pub origin: String,
        pub enabled: bool,
        pub extra: Option<String>,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

// Dedup bookkeeping. `dedup_state` records the last-applied mtime of each barrier
// config file (the cheap "did it change" gate). `entry_dedup` is a many-to-many
// tag table recording which config files currently affect which entries, so when
// a config is changed or removed we can recover the entries it used to touch
// (needed to re-merge after a barrier is relaxed — its anchors are gone from the
// new config, so only the persisted tag knows which entries to re-import).
mod dedup_state {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "dedup_state")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub config_path: String,
        pub mtime_ns: i64,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

mod entry_dedup {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "entry_dedup")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub entry_id: i64,
        #[sea_orm(primary_key, auto_increment = false)]
        pub config_path: String,
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

    /// Every `(source, identifier)` pair belonging to one of `entry_ids`. Used
    /// by the lazy dedup pass to re-import only the entries that contain a
    /// declared barrier anchor.
    pub async fn pairs_by_entry_ids(
        &self,
        entry_ids: &[i64],
    ) -> Result<Vec<(String, String)>, Error> {
        if entry_ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(entry_source::Entity::find()
            .filter(entry_source::Column::EntryId.is_in(entry_ids.iter().copied()))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|m| (m.source, m.identifier))
            .collect())
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

    /// Re-point a single existing pair to a different entry. Used by the
    /// importer's split path to move a stub pair (no fresh metadata) off a
    /// contaminated entry; `upsert_pair` already re-points pairs that do carry
    /// fresh metadata. No-op if the pair row doesn't exist.
    pub async fn set_pair_entry(
        &self,
        source: &str,
        identifier: &str,
        entry_id: i64,
    ) -> Result<(), Error> {
        entry_source::Entity::update_many()
            .col_expr(
                entry_source::Column::EntryId,
                sea_query::Expr::value(entry_id),
            )
            .filter(entry_source::Column::Source.eq(source))
            .filter(entry_source::Column::Identifier.eq(identifier))
            .exec(&self.db)
            .await?;
        Ok(())
    }

    /// Delete `entry` rows no longer referenced by any `entry_source` row — e.g.
    /// an entry emptied when a split moved all its pairs elsewhere. Idempotent.
    pub async fn delete_orphan_entries(&self) -> Result<(), Error> {
        let referenced = sea_query::Query::select()
            .distinct()
            .column(entry_source::Column::EntryId)
            .from(entry_source::Entity)
            .to_owned();
        entry::Entity::delete_many()
            .filter(entry::Column::Id.not_in_subquery(referenced))
            .exec(&self.db)
            .await?;
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

    /// Last-applied mtime (ns since epoch) of a barrier config file, if recorded.
    pub async fn get_dedup_mtime(&self, config_path: &str) -> Result<Option<i64>, Error> {
        Ok(dedup_state::Entity::find_by_id(config_path.to_string())
            .one(&self.db)
            .await?
            .map(|m| m.mtime_ns))
    }

    /// Record (upsert) the last-applied mtime for a barrier config file.
    pub async fn set_dedup_mtime(&self, config_path: &str, mtime_ns: i64) -> Result<(), Error> {
        dedup_state::Entity::insert(dedup_state::ActiveModel {
            config_path: Set(config_path.to_string()),
            mtime_ns: Set(mtime_ns),
        })
        .on_conflict(
            sea_query::OnConflict::column(dedup_state::Column::ConfigPath)
                .update_column(dedup_state::Column::MtimeNs)
                .to_owned(),
        )
        .exec(&self.db)
        .await?;
        Ok(())
    }

    /// Forget a barrier config file's recorded mtime (used when the file is gone).
    pub async fn delete_dedup_mtime(&self, config_path: &str) -> Result<(), Error> {
        dedup_state::Entity::delete_by_id(config_path.to_string())
            .exec(&self.db)
            .await?;
        Ok(())
    }

    /// Entry ids currently tagged as affected by a given barrier config file.
    pub async fn entries_for_config(&self, config_path: &str) -> Result<Vec<i64>, Error> {
        Ok(entry_dedup::Entity::find()
            .filter(entry_dedup::Column::ConfigPath.eq(config_path))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|m| m.entry_id)
            .collect())
    }

    /// Every config path that currently tags at least one entry. Used to spot
    /// config files that were removed since the last run.
    pub async fn tagged_config_paths(&self) -> Result<Vec<String>, Error> {
        let mut paths: std::collections::HashSet<String> = std::collections::HashSet::new();
        for m in entry_dedup::Entity::find().all(&self.db).await? {
            paths.insert(m.config_path);
        }
        Ok(paths.into_iter().collect())
    }

    /// Replace the set of entries tagged for `config_path` with `entry_ids`.
    /// Called after every flush so the tags reflect the latest grouping.
    pub async fn set_config_entries(
        &self,
        config_path: &str,
        entry_ids: &[i64],
    ) -> Result<(), Error> {
        entry_dedup::Entity::delete_many()
            .filter(entry_dedup::Column::ConfigPath.eq(config_path))
            .exec(&self.db)
            .await?;
        if entry_ids.is_empty() {
            return Ok(());
        }
        let models: Vec<entry_dedup::ActiveModel> = entry_ids
            .iter()
            .map(|id| entry_dedup::ActiveModel {
                entry_id: Set(*id),
                config_path: Set(config_path.to_string()),
            })
            .collect();
        entry_dedup::Entity::insert_many(models)
            .exec(&self.db)
            .await?;
        Ok(())
    }

    // ── Bulk-fetch helpers (used by the soft-match pipeline) ──────────────────

    /// Every entry row (id, entry_type). Used to seed the match candidate set.
    pub async fn all_entry_rows(&self) -> Result<Vec<EntryRow>, Error> {
        Ok(entry::Entity::find()
            .all(&self.db)
            .await?
            .into_iter()
            .map(|m| EntryRow {
                id: m.id,
                entry_type: m.entry_type,
            })
            .collect())
    }

    /// Every entry_source row. Used to build alias / metadata views per entry.
    pub async fn all_source_rows(&self) -> Result<Vec<SourceRow>, Error> {
        Ok(entry_source::Entity::find()
            .all(&self.db)
            .await?
            .into_iter()
            .map(|m| SourceRow {
                source: m.source,
                identifier: m.identifier,
                entry_id: m.entry_id,
                duration_ms: m.duration_ms,
                release_type: m.release_type,
                primary_type: m.primary_type,
                release_date: m.release_date,
            })
            .collect())
    }

    /// Every alias row. Used to build the alias list per pair.
    pub async fn all_alias_rows(&self) -> Result<Vec<AliasRow>, Error> {
        Ok(entry_alias::Entity::find()
            .all(&self.db)
            .await?
            .into_iter()
            .map(|m| AliasRow {
                source: m.source,
                identifier: m.identifier,
                name: m.name,
                locale: m.locale,
                primary_alias: m.primary,
            })
            .collect())
    }

    /// Every contribution row. Used to build credited-artist sets per entry.
    pub async fn all_contrib_rows(&self) -> Result<Vec<ContribRow>, Error> {
        Ok(contribution::Entity::find()
            .all(&self.db)
            .await?
            .into_iter()
            .map(|m| ContribRow {
                source: m.source,
                identifier: m.identifier,
                artist_source: m.artist_source,
                artist_identifier: m.artist_identifier,
            })
            .collect())
    }

    // ── entry_relation ────────────────────────────────────────────────────────

    /// Insert or update a relation between two entries. Enforces `entry_a <
    /// entry_b` so there is exactly one row per unordered pair + kind.
    pub async fn upsert_relation(
        &self,
        entry_a: i64,
        entry_b: i64,
        kind: &str,
        confidence: f64,
        origin: &str,
        extra: Option<&str>,
    ) -> Result<(), Error> {
        let (a, b) = if entry_a < entry_b {
            (entry_a, entry_b)
        } else {
            (entry_b, entry_a)
        };
        entry_relation::Entity::insert(entry_relation::ActiveModel {
            entry_a: Set(a),
            entry_b: Set(b),
            kind: Set(kind.to_string()),
            confidence: Set(confidence),
            origin: Set(origin.to_string()),
            enabled: Set(true),
            extra: Set(extra.map(|s| s.to_string())),
        })
        .on_conflict(
            sea_query::OnConflict::columns([
                entry_relation::Column::EntryA,
                entry_relation::Column::EntryB,
                entry_relation::Column::Kind,
            ])
            .update_columns([
                entry_relation::Column::Confidence,
                entry_relation::Column::Origin,
                entry_relation::Column::Enabled,
                entry_relation::Column::Extra,
            ])
            .to_owned(),
        )
        .exec(&self.db)
        .await?;
        Ok(())
    }

    /// Every entry_child row. Used by soft-match to compute release-position features.
    pub async fn all_child_rows(&self) -> Result<Vec<ChildRow>, Error> {
        Ok(entry_child::Entity::find()
            .all(&self.db)
            .await?
            .into_iter()
            .map(|m| ChildRow {
                parent_source: m.parent_source,
                parent_identifier: m.parent_identifier,
                child_source: m.child_source,
                child_identifier: m.child_identifier,
                disc_no: m.disc_no,
                track_no: m.track_no,
            })
            .collect())
    }

    /// All enabled relation rows, for reporting.
    pub async fn all_relations(&self) -> Result<Vec<RelationRow>, Error> {
        Ok(entry_relation::Entity::find()
            .all(&self.db)
            .await?
            .into_iter()
            .map(|m| RelationRow {
                entry_a: m.entry_a,
                entry_b: m.entry_b,
                kind: m.kind,
                confidence: m.confidence,
                origin: m.origin,
                enabled: m.enabled,
            })
            .collect())
    }
}

// ── Public data structs for bulk queries ──────────────────────────────────────

pub struct EntryRow {
    pub id: i64,
    pub entry_type: String,
}

pub struct SourceRow {
    pub source: String,
    pub identifier: String,
    pub entry_id: i64,
    pub duration_ms: Option<i64>,
    pub release_type: Option<String>,
    pub primary_type: Option<String>,
    pub release_date: Option<String>,
}

pub struct AliasRow {
    pub source: String,
    pub identifier: String,
    pub name: String,
    pub locale: Option<String>,
    pub primary_alias: bool,
}

pub struct ContribRow {
    pub source: String,
    pub identifier: String,
    pub artist_source: String,
    pub artist_identifier: String,
}

pub struct ChildRow {
    pub parent_source: String,
    pub parent_identifier: String,
    pub child_source: String,
    pub child_identifier: String,
    pub disc_no: Option<i32>,
    pub track_no: Option<i32>,
}

pub struct RelationRow {
    pub entry_a: i64,
    pub entry_b: i64,
    pub kind: String,
    pub confidence: f64,
    pub origin: String,
    pub enabled: bool,
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
