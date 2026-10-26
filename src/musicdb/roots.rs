//! Update roots: the URLs the library update walks, each with the fetch
//! options to walk it with (docs/plan-update-roots.md). A root is keyed by
//! its canonical pair, so the same channel entered as two URL forms is one
//! root; the URL as entered is kept for display.
//!
//! Removing a root only stops updating it: what it imported stays.

use sea_orm::{ActiveValue::Set, ConnectionTrait, EntityTrait, Statement};

use super::{Error, MusicDb};

pub(super) mod update_root {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "update_root")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        /// As entered.
        pub url: String,
        /// The canonical pair; unique (`idx_update_root_pair`).
        pub source: String,
        pub identifier: String,
        /// A fetch-options file id (`<config_dir>/fetch_options/<id>.yaml`)
        /// and the entry point in it to start from.
        pub fetch_options_file: String,
        pub entry_point: String,
        pub label: Option<String>,
        pub enabled: bool,
        /// Unix seconds.
        pub added_at: i64,
        /// The last update that ran this root: when, `"ok"` or `"failed"`,
        /// the error, and its summary as JSON.
        pub last_run_at: Option<i64>,
        pub last_status: Option<String>,
        pub last_error: Option<String>,
        pub last_stats: Option<String>,
        /// The last full walk of this root (unix seconds), and the hash of
        /// its fetch options then. Between full walks, updates prune what
        /// earlier runs fetched (docs/plan-fast-update.md, Phase 3b).
        pub full_walk_at: Option<i64>,
        pub full_walk_hash: Option<String>,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

/// Longest label accepted, in characters.
const MAX_LABEL_CHARS: usize = 200;

/// One root, with the library entry its pair is in, if it's been imported.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateRootRow {
    pub id: i64,
    pub url: String,
    pub source: String,
    pub identifier: String,
    pub fetch_options_file: String,
    pub entry_point: String,
    pub label: Option<String>,
    pub enabled: bool,
    pub added_at: i64,
    pub last_run_at: Option<i64>,
    pub last_status: Option<String>,
    pub last_error: Option<String>,
    pub last_stats: Option<String>,
    pub full_walk_at: Option<i64>,
    pub full_walk_hash: Option<String>,
    pub entry_id: Option<i64>,
}

/// A root to add. `source`/`identifier` must already be canonical.
#[derive(Debug, Clone, PartialEq)]
pub struct NewUpdateRoot {
    pub url: String,
    pub source: String,
    pub identifier: String,
    pub fetch_options_file: String,
    pub entry_point: String,
    pub label: Option<String>,
}

/// Changes to a root; `None` leaves a field as it is. `label: Some(None)`
/// clears the label.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UpdateRootEdit {
    pub fetch_options: Option<(String, String)>,
    pub label: Option<Option<String>>,
    pub enabled: Option<bool>,
}

fn clean_label(label: Option<&str>) -> Result<Option<String>, Error> {
    let Some(label) = label.map(str::trim).filter(|l| !l.is_empty()) else {
        return Ok(None);
    };
    if label.chars().count() > MAX_LABEL_CHARS {
        return Err(Error::InvalidInput(format!(
            "label is longer than {MAX_LABEL_CHARS} characters"
        )));
    }
    Ok(Some(label.to_owned()))
}

const ROOT_COLUMNS: &str = "r.id, r.url, r.source, r.identifier, r.fetch_options_file, \
     r.entry_point, r.label, r.enabled, r.added_at, r.last_run_at, r.last_status, \
     r.last_error, r.last_stats, r.full_walk_at, r.full_walk_hash, s.entry_id";

const ROOT_FROM: &str = "update_root r \
     LEFT JOIN entry_source s ON s.source = r.source AND s.identifier = r.identifier";

fn root_row(row: &sea_orm::QueryResult) -> Result<UpdateRootRow, Error> {
    Ok(UpdateRootRow {
        id: row.try_get("", "id")?,
        url: row.try_get("", "url")?,
        source: row.try_get("", "source")?,
        identifier: row.try_get("", "identifier")?,
        fetch_options_file: row.try_get("", "fetch_options_file")?,
        entry_point: row.try_get("", "entry_point")?,
        label: row.try_get("", "label")?,
        enabled: row.try_get("", "enabled")?,
        added_at: row.try_get("", "added_at")?,
        last_run_at: row.try_get("", "last_run_at")?,
        last_status: row.try_get("", "last_status")?,
        last_error: row.try_get("", "last_error")?,
        last_stats: row.try_get("", "last_stats")?,
        full_walk_at: row.try_get("", "full_walk_at")?,
        full_walk_hash: row.try_get("", "full_walk_hash")?,
        entry_id: row.try_get("", "entry_id")?,
    })
}

impl MusicDb {
    /// Every root, in the order they were added.
    pub async fn update_roots(&self) -> Result<Vec<UpdateRootRow>, Error> {
        let rows = self
            .db
            .query_all_raw(Statement::from_string(
                self.db.get_database_backend(),
                format!("SELECT {ROOT_COLUMNS} FROM {ROOT_FROM} ORDER BY r.id"),
            ))
            .await?;
        rows.iter().map(root_row).collect()
    }

    pub async fn update_root(&self, id: i64) -> Result<Option<UpdateRootRow>, Error> {
        let row = self
            .db
            .query_one_raw(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                format!("SELECT {ROOT_COLUMNS} FROM {ROOT_FROM} WHERE r.id = ?"),
                [id.into()],
            ))
            .await?;
        row.as_ref().map(root_row).transpose()
    }

    /// Adds a root, enabled; returns its id. `InvalidInput` if a root with
    /// the same pair exists. Roots in one library entry are allowed: an
    /// artist's two channels both need walking.
    pub async fn add_update_root(&self, root: &NewUpdateRoot, now: i64) -> Result<i64, Error> {
        let label = clean_label(root.label.as_deref())?;
        let _guard = self.write_lock.lock().await;
        let existing = self
            .db
            .query_one_raw(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "SELECT id FROM update_root WHERE source = ? AND identifier = ?",
                [root.source.clone().into(), root.identifier.clone().into()],
            ))
            .await?;
        if let Some(row) = existing {
            let id: i64 = row.try_get("", "id")?;
            return Err(Error::InvalidInput(format!(
                "{} is already root {id}",
                root.identifier
            )));
        }
        let result = update_root::Entity::insert(update_root::ActiveModel {
            id: sea_orm::ActiveValue::NotSet,
            url: Set(root.url.clone()),
            source: Set(root.source.clone()),
            identifier: Set(root.identifier.clone()),
            fetch_options_file: Set(root.fetch_options_file.clone()),
            entry_point: Set(root.entry_point.clone()),
            label: Set(label),
            enabled: Set(true),
            added_at: Set(now),
            last_run_at: Set(None),
            last_status: Set(None),
            last_error: Set(None),
            last_stats: Set(None),
            full_walk_at: Set(None),
            full_walk_hash: Set(None),
        })
        .exec(&self.db)
        .await?;
        Ok(result.last_insert_id)
    }

    /// Returns false for an unknown root.
    pub async fn edit_update_root(&self, id: i64, edit: &UpdateRootEdit) -> Result<bool, Error> {
        let mut sets = Vec::new();
        let mut values: Vec<sea_orm::Value> = Vec::new();
        if let Some((file, entry_point)) = &edit.fetch_options {
            sets.push("fetch_options_file = ?, entry_point = ?");
            values.push(file.clone().into());
            values.push(entry_point.clone().into());
        }
        if let Some(label) = &edit.label {
            sets.push("label = ?");
            values.push(clean_label(label.as_deref())?.into());
        }
        if let Some(enabled) = edit.enabled {
            sets.push("enabled = ?");
            values.push(enabled.into());
        }
        let _guard = self.write_lock.lock().await;
        if sets.is_empty() {
            return Ok(self.update_root(id).await?.is_some());
        }
        values.push(id.into());
        let result = self
            .db
            .execute_raw(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                format!("UPDATE update_root SET {} WHERE id = ?", sets.join(", ")),
                values,
            ))
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Returns false for an unknown root.
    pub async fn delete_update_root(&self, id: i64) -> Result<bool, Error> {
        let _guard = self.write_lock.lock().await;
        let result = self
            .db
            .execute_raw(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "DELETE FROM update_root WHERE id = ?",
                [id.into()],
            ))
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Records the outcome of an update run on a root.
    /// Records that `ids` were walked in full at `at` with fetch options
    /// hashing to `hash`.
    pub async fn record_full_walk(&self, ids: &[i64], at: i64, hash: &str) -> Result<(), Error> {
        let _guard = self.write_lock.lock().await;
        for id in ids {
            self.db
                .execute_raw(Statement::from_sql_and_values(
                    self.db.get_database_backend(),
                    "UPDATE update_root SET full_walk_at = ?, full_walk_hash = ? WHERE id = ?",
                    [at.into(), hash.into(), (*id).into()],
                ))
                .await?;
        }
        Ok(())
    }

    pub async fn record_update_root_run(
        &self,
        id: i64,
        at: i64,
        result: Result<&str, &str>,
    ) -> Result<(), Error> {
        let (status, error, stats) = match result {
            Ok(stats) => ("ok", None, Some(stats.to_owned())),
            Err(error) => ("failed", Some(error.to_owned()), None),
        };
        let _guard = self.write_lock.lock().await;
        self.db
            .execute_raw(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "UPDATE update_root SET last_run_at = ?, last_status = ?, last_error = ?, \
                 last_stats = ? WHERE id = ?",
                [
                    at.into(),
                    status.into(),
                    error.into(),
                    stats.into(),
                    id.into(),
                ],
            ))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::types::EntryType;

    async fn mem_db() -> MusicDb {
        MusicDb::new("sqlite::memory:").await.unwrap()
    }

    fn root(identifier: &str) -> NewUpdateRoot {
        NewUpdateRoot {
            url: format!("{identifier}?as=entered"),
            source: "youtube".into(),
            identifier: identifier.into(),
            fetch_options_file: "vtuber_fetch_discography".into(),
            entry_point: "main".into(),
            label: None,
        }
    }

    #[tokio::test]
    async fn add_list_and_resolve_entry() {
        let mdb = mem_db().await;
        let id = mdb.add_update_root(&root("chan/UC1"), 10).await.unwrap();
        let roots = mdb.update_roots().await.unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].id, id);
        assert!(roots[0].enabled);
        assert_eq!(roots[0].url, "chan/UC1?as=entered");
        assert_eq!(roots[0].entry_id, None);

        let entry = mdb.insert_entry(Some(EntryType::Artist)).await.unwrap();
        mdb.upsert_stub_pair("youtube", "chan/UC1", entry, None, None)
            .await
            .unwrap();
        assert_eq!(
            mdb.update_root(id).await.unwrap().unwrap().entry_id,
            Some(entry)
        );
    }

    #[tokio::test]
    async fn one_root_per_pair() {
        let mdb = mem_db().await;
        mdb.add_update_root(&root("chan/UC1"), 10).await.unwrap();
        let mut again = root("chan/UC1");
        again.fetch_options_file = "fetch_discography".into();
        assert!(matches!(
            mdb.add_update_root(&again, 11).await,
            Err(Error::InvalidInput(_))
        ));
    }

    /// Two pairs in one entry (an artist's two channels) are two roots.
    #[tokio::test]
    async fn roots_may_share_an_entry() {
        let mdb = mem_db().await;
        let entry = mdb.insert_entry(Some(EntryType::Artist)).await.unwrap();
        for identifier in ["chan/UC1", "chan/UC2"] {
            mdb.upsert_stub_pair("youtube", identifier, entry, None, None)
                .await
                .unwrap();
        }
        mdb.add_update_root(&root("chan/UC1"), 10).await.unwrap();
        mdb.add_update_root(&root("chan/UC2"), 11).await.unwrap();
        assert_eq!(mdb.update_roots().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn edit_record_and_delete() {
        let mdb = mem_db().await;
        let id = mdb.add_update_root(&root("chan/UC1"), 10).await.unwrap();
        let edit = UpdateRootEdit {
            fetch_options: Some(("fetch_discography".into(), "track".into())),
            label: Some(Some("  Watame  ".into())),
            enabled: Some(false),
        };
        assert!(mdb.edit_update_root(id, &edit).await.unwrap());
        mdb.record_update_root_run(id, 20, Err("boom"))
            .await
            .unwrap();
        let r = mdb.update_root(id).await.unwrap().unwrap();
        assert_eq!(
            (r.fetch_options_file.as_str(), r.entry_point.as_str()),
            ("fetch_discography", "track")
        );
        assert_eq!(r.label.as_deref(), Some("Watame"));
        assert!(!r.enabled);
        assert_eq!(
            (
                r.last_run_at,
                r.last_status.as_deref(),
                r.last_error.as_deref()
            ),
            (Some(20), Some("failed"), Some("boom"))
        );

        let clear = UpdateRootEdit {
            label: Some(None),
            ..Default::default()
        };
        assert!(mdb.edit_update_root(id, &clear).await.unwrap());
        assert_eq!(mdb.update_root(id).await.unwrap().unwrap().label, None);

        assert!(mdb.delete_update_root(id).await.unwrap());
        assert!(!mdb.delete_update_root(id).await.unwrap());
        assert!(
            !mdb.edit_update_root(id, &UpdateRootEdit::default())
                .await
                .unwrap()
        );
    }
}
