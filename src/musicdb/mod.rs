use std::collections::HashMap;

use sea_orm::{
    ActiveValue::Set, ColumnTrait, Condition, ConnectionTrait, Database, DatabaseConnection, DbErr,
    EntityTrait, QueryFilter, TransactionTrait, sea_query,
};
use tracing::warn;

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

/// Combine the `extra` payloads of two relations being merged into one. Both are
/// preserved when present and different: JSON values are unioned into a flat,
/// deduplicated, sorted array (so repeated merges converge), and a lone present
/// payload is kept verbatim. Returns `None` only when both are absent.
///
/// This is the focal point for handling `extra` on a merge and is deliberately
/// kept small so it can later be replaced by richer, possibly user-defined
/// (e.g. Rhai) logic.
fn combine_extra(a: &Option<String>, b: &Option<String>) -> Option<String> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some(x.clone()),
        (Some(x), Some(y)) if x == y => Some(x.clone()),
        (Some(x), Some(y)) => {
            let mut items: Vec<serde_json::Value> = Vec::new();
            for raw in [x, y] {
                match serde_json::from_str::<serde_json::Value>(raw) {
                    Ok(serde_json::Value::Array(arr)) => items.extend(arr),
                    Ok(v) => items.push(v),
                    Err(_) => items.push(serde_json::Value::String(raw.clone())),
                }
            }
            items.sort_by_key(|v| v.to_string());
            items.dedup_by_key(|v| v.to_string());
            Some(serde_json::Value::Array(items).to_string())
        }
    }
}

/// Combine two relations that share the same `(entry_a, entry_b, kind)` into one.
///
/// This is the single seam where duplicate relations are reconciled when entries
/// merge. The current policy is intentionally simple — keep the higher-confidence
/// relation's scalar fields (deterministic tie-break on origin/enabled) and union
/// the two `extra` payloads via [`combine_extra`]. It is **lossy** on the scalar
/// fields of the discarded relation; callers log when they invoke it. Replace
/// this with richer logic (e.g. a user-provided Rhai hook) when needed.
fn combine_relations(
    a: &entry_relation::Model,
    b: &entry_relation::Model,
) -> entry_relation::Model {
    let a_wins = match a.confidence.partial_cmp(&b.confidence) {
        Some(std::cmp::Ordering::Greater) => true,
        Some(std::cmp::Ordering::Less) => false,
        _ => (&a.origin, a.enabled) >= (&b.origin, b.enabled),
    };
    let primary = if a_wins { a } else { b };
    entry_relation::Model {
        entry_a: primary.entry_a,
        entry_b: primary.entry_b,
        kind: primary.kind.clone(),
        confidence: primary.confidence,
        origin: primary.origin.clone(),
        enabled: primary.enabled,
        extra: combine_extra(&a.extra, &b.extra),
    }
}

/// Re-point every `entry_relation` row referencing `loser` onto `winner` and keep
/// the table referentially consistent. Endpoint order is normalized
/// (`entry_a < entry_b`). The two lossy steps are made explicit with `warn!`: a
/// relation that becomes `winner ↔ winner` (both endpoints merged into one entry)
/// is **dropped**, and rows that collide on `(entry_a, entry_b, kind)` are
/// **merged** into one via [`combine_relations`]. Winner rows are included so a
/// remapped loser relation merges against any pre-existing winner relation.
async fn remap_relations<C: ConnectionTrait>(
    txn: &C,
    loser: i64,
    winner: i64,
) -> Result<(), Error> {
    use entry_relation::Column as Col;

    let touching = Condition::any()
        .add(Col::EntryA.eq(loser))
        .add(Col::EntryB.eq(loser))
        .add(Col::EntryA.eq(winner))
        .add(Col::EntryB.eq(winner));

    let rows = entry_relation::Entity::find()
        .filter(touching.clone())
        .all(txn)
        .await?;
    if rows.is_empty() {
        return Ok(());
    }

    // Delete the affected rows; the canonicalized survivors are re-inserted.
    entry_relation::Entity::delete_many()
        .filter(touching)
        .exec(txn)
        .await?;

    let mut merged: HashMap<(i64, i64, String), entry_relation::Model> = HashMap::new();
    for mut r in rows {
        if r.entry_a == loser {
            r.entry_a = winner;
        }
        if r.entry_b == loser {
            r.entry_b = winner;
        }
        if r.entry_a == r.entry_b {
            // Lossy: a relationship that is now internal to a single entry is
            // dropped. Surface it so the loss is never silent.
            warn!(
                entry = r.entry_a,
                kind = %r.kind,
                confidence = r.confidence,
                origin = %r.origin,
                "merge dropped self-relation (both endpoints merged into one entry)"
            );
            continue;
        }
        if r.entry_a > r.entry_b {
            std::mem::swap(&mut r.entry_a, &mut r.entry_b);
        }
        let key = (r.entry_a, r.entry_b, r.kind.clone());
        match merged.remove(&key) {
            Some(existing) => {
                // Lossy: two relations collapse into one. Surface it.
                warn!(
                    entry_a = key.0,
                    entry_b = key.1,
                    kind = %key.2,
                    "merge combined duplicate relations into one (see combine_relations)"
                );
                merged.insert(key, combine_relations(&existing, &r));
            }
            None => {
                merged.insert(key, r);
            }
        }
    }

    for (_, r) in merged {
        entry_relation::Entity::insert(entry_relation::ActiveModel {
            entry_a: Set(r.entry_a),
            entry_b: Set(r.entry_b),
            kind: Set(r.kind),
            confidence: Set(r.confidence),
            origin: Set(r.origin),
            enabled: Set(r.enabled),
            extra: Set(r.extra),
        })
        .exec(txn)
        .await?;
    }

    Ok(())
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
        let txn = self.db.begin().await?;

        // Re-point all of the loser's source rows to the winner.
        entry_source::Entity::update_many()
            .col_expr(
                entry_source::Column::EntryId,
                sea_query::Expr::value(winner),
            )
            .filter(entry_source::Column::EntryId.eq(loser))
            .exec(&txn)
            .await?;

        // Keep entry_relation referentially consistent: the loser entry is about
        // to be deleted, so re-point every relation that referenced it onto the
        // winner, drop the resulting self-relations, and collapse duplicates.
        remap_relations(&txn, loser, winner).await?;

        entry::Entity::delete_by_id(loser).exec(&txn).await?;

        txn.commit().await?;
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
            // Note: `Extra` is intentionally NOT updated on conflict. It may hold
            // a payload combined by `combine_relations` when entries merged; a
            // later re-score refreshes confidence/origin/enabled but must not wipe
            // that combined `extra`.
            .update_columns([
                entry_relation::Column::Confidence,
                entry_relation::Column::Origin,
                entry_relation::Column::Enabled,
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn mem_db() -> MusicDb {
        MusicDb::new("sqlite::memory:").await.unwrap()
    }

    async fn insert_entry(db: &DatabaseConnection, id: i64) {
        entry::Entity::insert(entry::ActiveModel {
            id: Set(id),
            entry_type: Set("track".to_string()),
        })
        .exec(db)
        .await
        .unwrap();
    }

    async fn insert_rel(db: &DatabaseConnection, a: i64, b: i64, kind: &str, conf: f64) {
        entry_relation::Entity::insert(entry_relation::ActiveModel {
            entry_a: Set(a),
            entry_b: Set(b),
            kind: Set(kind.to_string()),
            confidence: Set(conf),
            origin: Set("heuristic".to_string()),
            enabled: Set(true),
            extra: Set(None),
        })
        .exec(db)
        .await
        .unwrap();
    }

    /// merge_entries must leave entry_relation referentially consistent: no row
    /// may reference the merged-away loser; a relation that becomes winner↔winner
    /// is dropped; colliding rows are merged into one (higher confidence wins).
    /// Unrelated relations are untouched.
    #[tokio::test]
    async fn merge_entries_keeps_relations_consistent() {
        let mdb = mem_db().await;
        let db = &mdb.db;
        for id in [1i64, 2, 3, 4] {
            insert_entry(db, id).await;
        }
        // (1,3) and (2,3) both "alt" → after 1→2 both become (2,3) → merged.
        insert_rel(db, 1, 3, "alt", 0.9).await;
        insert_rel(db, 2, 3, "alt", 0.5).await;
        // (1,2) → becomes a self-relation after merge → dropped.
        insert_rel(db, 1, 2, "same", 0.8).await;
        // Unrelated relation, must survive verbatim.
        insert_rel(db, 3, 4, "alt", 0.7).await;

        mdb.merge_entries(1, 2).await.unwrap();

        let rows = entry_relation::Entity::find().all(db).await.unwrap();
        assert!(
            rows.iter().all(|r| r.entry_a != 1 && r.entry_b != 1),
            "no relation may reference the merged-away entry 1"
        );
        assert!(
            !rows.iter().any(|r| r.entry_a == r.entry_b),
            "self-relations dropped"
        );
        // Colliding (2,3,alt) merged into one, higher confidence wins.
        let r23: Vec<_> = rows
            .iter()
            .filter(|r| r.entry_a == 2 && r.entry_b == 3 && r.kind == "alt")
            .collect();
        assert_eq!(r23.len(), 1, "duplicate collapsed to one row");
        assert_eq!(r23[0].confidence, 0.9, "higher confidence wins");
        assert!(
            rows.iter()
                .any(|r| r.entry_a == 3 && r.entry_b == 4 && r.kind == "alt"),
            "unrelated relation intact"
        );
        assert!(
            entry::Entity::find_by_id(1)
                .one(db)
                .await
                .unwrap()
                .is_none(),
            "loser entry deleted"
        );
    }

    /// combine_relations is order-independent: the higher-confidence row's scalar
    /// fields win regardless of argument order.
    #[test]
    fn combine_relations_is_order_independent() {
        let mk = |conf: f64, origin: &str| entry_relation::Model {
            entry_a: 2,
            entry_b: 3,
            kind: "alt".to_string(),
            confidence: conf,
            origin: origin.to_string(),
            enabled: true,
            extra: None,
        };
        let a = mk(0.5, "heuristic");
        let b = mk(0.9, "manual");

        let r1 = combine_relations(&a, &b);
        let r2 = combine_relations(&b, &a);
        assert_eq!(r1, r2, "combine is order-independent");
        assert_eq!(r1.confidence, 0.9);
        assert_eq!(r1.origin, "manual", "higher-confidence scalar fields win");
    }

    /// combine_extra preserves both payloads when they differ, keeps a lone one
    /// verbatim, and converges (no growth) when re-combined.
    #[test]
    fn combine_extra_preserves_both_payloads() {
        assert_eq!(combine_extra(&None, &None), None);
        assert_eq!(
            combine_extra(&Some("{\"x\":1}".into()), &None),
            Some("{\"x\":1}".into()),
            "lone payload kept verbatim"
        );

        let a = Some("{\"src\":\"a\"}".to_string());
        let b = Some("{\"src\":\"b\"}".to_string());
        let combined = combine_extra(&a, &b).unwrap();
        let arr: serde_json::Value = serde_json::from_str(&combined).unwrap();
        assert_eq!(arr.as_array().unwrap().len(), 2, "both payloads kept");

        // Re-combining with one of the originals must not grow the array.
        let again = combine_extra(&Some(combined.clone()), &a).unwrap();
        let arr2: serde_json::Value = serde_json::from_str(&again).unwrap();
        assert_eq!(arr2.as_array().unwrap().len(), 2, "converges, no growth");
    }

    /// Endpoint order is normalized: a relation stored as (loser, x) with
    /// loser > x must come back as (x, winner) or (winner, x) with entry_a < entry_b.
    #[tokio::test]
    async fn merge_entries_normalizes_endpoint_order() {
        let mdb = mem_db().await;
        let db = &mdb.db;
        for id in [5i64, 10, 20] {
            insert_entry(db, id).await;
        }
        // (5, 20) where 5 will merge into 10 → becomes (10, 20).
        insert_rel(db, 5, 20, "alt", 0.6).await;
        mdb.merge_entries(5, 10).await.unwrap();

        let rows = entry_relation::Entity::find().all(db).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].entry_a, rows[0].entry_b), (10, 20));
        assert!(rows[0].entry_a < rows[0].entry_b, "endpoints normalized");
    }
}
