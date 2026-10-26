//! Durable playback identity, per-device listening reports, and scrobble outbox.
//! Scrobbles are whole plays, independent of the incremental `play_export` ledger.

use super::*;

mod play_session {
    use sea_orm::entity::prelude::*;
    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "play_session")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub session_key: String,
        pub play_id: i64,
    }
    impl ActiveModelBehavior for ActiveModel {}
}

mod play_report {
    use sea_orm::entity::prelude::*;
    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "play_report")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub play_id: i64,
        #[sea_orm(primary_key, auto_increment = false)]
        pub reporter: String,
        pub listened_seconds: f64,
    }
    impl ActiveModelBehavior for ActiveModel {}
}

mod scrobble_submission {
    use sea_orm::entity::prelude::*;
    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "scrobble_submission")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub account: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub play_id: i64,
        /// Frozen metadata and original start timestamp, reused on every retry.
        pub payload: String,
        pub status: String,
        pub attempts: i64,
        pub next_attempt_at: i64,
        pub last_error: Option<String>,
    }
    impl ActiveModelBehavior for ActiveModel {}
}

#[derive(Debug)]
pub struct Submission {
    pub play_id: i64,
    pub payload: String,
    pub attempts: i64,
}

fn play_row(row: play_event::Model) -> PlayRow {
    PlayRow {
        id: row.id,
        entry_id: row.entry_id,
        started_at: row.started_at,
        listened_seconds: row.listened_seconds,
    }
}

impl MusicDb {
    /// Idempotent across browser reloads and device transfers. A replay gets a
    /// new session key from the playback hub, while a resume keeps the old key.
    pub async fn start_play_session(
        &self,
        entry_id: i64,
        started_at: i64,
        key: &str,
    ) -> Result<PlayRow, Error> {
        if key.is_empty() || key.len() > 128 {
            return Err(Error::InvalidInput("invalid playback session key".into()));
        }
        let _guard = self.write_lock.lock().await;
        let txn = begin_write(&self.db).await?;
        if let Some(session) = play_session::Entity::find_by_id(key.to_owned())
            .one(&txn)
            .await?
        {
            let play = play_event::Entity::find_by_id(session.play_id)
                .one(&txn)
                .await?
                .ok_or_else(|| Error::InvalidInput("missing play for session".into()))?;
            if play.entry_id != entry_id {
                return Err(Error::InvalidInput(
                    "playback session belongs to another entry".into(),
                ));
            }
            return Ok(play_row(play));
        }
        let result = play_event::Entity::insert(play_event::ActiveModel {
            id: sea_orm::ActiveValue::NotSet,
            entry_id: Set(entry_id),
            started_at: Set(started_at),
            listened_seconds: Set(0.0),
        })
        .exec(&txn)
        .await?;
        play_session::Entity::insert(play_session::ActiveModel {
            session_key: Set(key.to_owned()),
            play_id: Set(result.last_insert_id),
        })
        .exec(&txn)
        .await?;
        txn.commit().await?;
        Ok(PlayRow {
            id: result.last_insert_id,
            entry_id,
            started_at,
            listened_seconds: 0.0,
        })
    }

    /// Each engine mount reports its own cumulative time. Summing reporters
    /// preserves a late final report from the previous device after a transfer;
    /// MAX per reporter makes retries and reordered reports harmless.
    pub async fn report_play_listened(
        &self,
        play_id: i64,
        reporter: &str,
        seconds: f64,
    ) -> Result<bool, Error> {
        if reporter.is_empty() || reporter.len() > 128 || !(0.0..=86_400.0).contains(&seconds) {
            return Err(Error::InvalidInput("invalid listening report".into()));
        }
        let _guard = self.write_lock.lock().await;
        let txn = begin_write(&self.db).await?;
        if play_event::Entity::find_by_id(play_id)
            .one(&txn)
            .await?
            .is_none()
        {
            return Ok(false);
        }
        txn.execute_raw(Statement::from_sql_and_values(
            txn.get_database_backend(),
            "INSERT INTO play_report (play_id, reporter, listened_seconds) VALUES (?, ?, ?)
             ON CONFLICT (play_id, reporter) DO UPDATE
             SET listened_seconds = MAX(listened_seconds, excluded.listened_seconds)",
            [play_id.into(), reporter.into(), seconds.into()],
        ))
        .await?;
        txn.execute_raw(Statement::from_sql_and_values(txn.get_database_backend(),
            "UPDATE play_event SET listened_seconds = MAX(listened_seconds,
                MIN(86400.0, (SELECT SUM(listened_seconds) FROM play_report WHERE play_id = ?))) WHERE id = ?",
            [play_id.into(), play_id.into()])).await?;
        txn.commit().await?;
        Ok(true)
    }

    /// Unqueued plays for one Last.fm account. Eligibility and metadata are
    /// evaluated by that exporter, independently of NRS's ten-second cutoff.
    pub async fn unqueued_scrobbles(
        &self,
        account: &str,
        since: i64,
    ) -> Result<Vec<PlayRow>, Error> {
        let rows = self
            .db
            .query_all_raw(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "SELECT p.id, p.entry_id, p.started_at, p.listened_seconds FROM play_event p
             LEFT JOIN scrobble_submission s ON s.play_id = p.id AND s.account = ?
             WHERE p.started_at >= ? AND p.listened_seconds > 15.0 AND s.play_id IS NULL
             ORDER BY p.started_at, p.id",
                [account.into(), since.into()],
            ))
            .await?;
        rows.iter()
            .map(|r| {
                Ok(PlayRow {
                    id: r.try_get("", "id")?,
                    entry_id: r.try_get("", "entry_id")?,
                    started_at: r.try_get("", "started_at")?,
                    listened_seconds: r.try_get("", "listened_seconds")?,
                })
            })
            .collect()
    }

    pub async fn queue_scrobble(
        &self,
        account: &str,
        play_id: i64,
        payload: &str,
    ) -> Result<(), Error> {
        let _guard = self.write_lock.lock().await;
        self.db.execute_raw(Statement::from_sql_and_values(self.db.get_database_backend(),
            "INSERT INTO scrobble_submission (account, play_id, payload, status, attempts, next_attempt_at)
             VALUES (?, ?, ?, 'pending', 0, 0) ON CONFLICT (account, play_id) DO NOTHING",
            [account.into(), play_id.into(), payload.into()])).await?;
        Ok(())
    }

    /// Oldest first. A backed-off oldest item blocks newer ones, preserving
    /// Last.fm's required order for cached submissions.
    pub async fn pending_scrobbles(
        &self,
        account: &str,
        now: i64,
    ) -> Result<Vec<Submission>, Error> {
        let rows = scrobble_submission::Entity::find()
            .filter(scrobble_submission::Column::Account.eq(account))
            .filter(scrobble_submission::Column::Status.eq("pending"))
            .order_by_asc(scrobble_submission::Column::PlayId)
            .limit(50)
            .all(&self.db)
            .await?;
        Ok(rows
            .into_iter()
            .take_while(|r| r.next_attempt_at <= now)
            .map(|r| Submission {
                play_id: r.play_id,
                payload: r.payload,
                attempts: r.attempts,
            })
            .collect())
    }

    pub async fn finish_scrobble(
        &self,
        account: &str,
        play_id: i64,
        status: &str,
        error: Option<&str>,
        retry_at: i64,
    ) -> Result<(), Error> {
        let _guard = self.write_lock.lock().await;
        self.db
            .execute_raw(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "UPDATE scrobble_submission SET status = ?, attempts = attempts + 1,
             next_attempt_at = ?, last_error = ? WHERE account = ? AND play_id = ?",
                [
                    status.into(),
                    retry_at.into(),
                    error.into(),
                    account.into(),
                    play_id.into(),
                ],
            ))
            .await?;
        Ok(())
    }

    pub async fn scrobble_counts(&self, account: &str) -> Result<BTreeMap<String, i64>, Error> {
        let rows = self.db.query_all_raw(Statement::from_sql_and_values(self.db.get_database_backend(),
            "SELECT status, COUNT(*) AS n FROM scrobble_submission WHERE account = ? GROUP BY status", [account.into()])).await?;
        rows.iter()
            .map(|r| Ok((r.try_get("", "status")?, r.try_get("", "n")?)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn transfer_and_reload_preserve_play_and_late_reports() {
        let db = MusicDb::new("sqlite::memory:").await.unwrap();
        let first = db.start_play_session(7, 100, "session").await.unwrap();
        db.report_play_listened(first.id, "old-device", 80.0)
            .await
            .unwrap();
        let resumed = db.start_play_session(7, 200, "session").await.unwrap();
        assert_eq!(
            (resumed.id, resumed.started_at, resumed.listened_seconds),
            (first.id, 100, 80.0)
        );
        db.report_play_listened(first.id, "new-device", 50.0)
            .await
            .unwrap();
        db.report_play_listened(first.id, "old-device", 90.0)
            .await
            .unwrap();
        db.report_play_listened(first.id, "old-device", 80.0)
            .await
            .unwrap();
        let row = db.start_play_session(7, 300, "session").await.unwrap();
        assert_eq!(row.listened_seconds, 140.0);
        assert!(db.start_play_session(8, 300, "session").await.is_err());
        assert_ne!(
            db.start_play_session(7, 300, "replay").await.unwrap().id,
            first.id
        );
        assert!(
            db.report_play_listened(first.id, "bad", f64::NAN)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn imported_entry_merge_preserves_history_and_resumable_session() {
        let db = MusicDb::new("sqlite::memory:").await.unwrap();
        let original = db.insert_entry(Some(EntryType::Track)).await.unwrap();
        let catalog = db.insert_entry(Some(EntryType::Track)).await.unwrap();
        let play = db
            .start_play_session(original, 100, "session")
            .await
            .unwrap();
        db.report_play_listened(play.id, "browser", 120.0)
            .await
            .unwrap();
        db.merge_entries(original, catalog).await.unwrap();
        let resumed = db
            .start_play_session(catalog, 200, "session")
            .await
            .unwrap();
        assert_eq!(
            (resumed.id, resumed.entry_id, resumed.listened_seconds),
            (play.id, catalog, 120.0)
        );
    }

    #[tokio::test]
    async fn outbox_survives_restart_is_account_scoped_and_only_sends_once() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("music.db").display()
        );
        let db = MusicDb::new(&url).await.unwrap();
        let play = db.start_play(1, 100).await.unwrap();
        db.set_play_listened(play, 100.0).await.unwrap();
        db.queue_scrobble("alice", play, "original").await.unwrap();
        db.queue_scrobble("alice", play, "changed").await.unwrap();
        drop(db);
        let db = MusicDb::new(&url).await.unwrap();
        assert_eq!(
            db.pending_scrobbles("alice", 100).await.unwrap()[0].payload,
            "original"
        );
        assert_eq!(db.unqueued_scrobbles("bob", 0).await.unwrap().len(), 1);
        db.finish_scrobble("alice", play, "pending", Some("offline"), 200)
            .await
            .unwrap();
        assert!(db.pending_scrobbles("alice", 199).await.unwrap().is_empty());
        db.finish_scrobble("alice", play, "accepted", None, 0)
            .await
            .unwrap();
        db.set_play_listened(play, 200.0).await.unwrap();
        assert!(db.unqueued_scrobbles("alice", 0).await.unwrap().is_empty());
        assert!(db.pending_scrobbles("alice", 300).await.unwrap().is_empty());
    }
}
