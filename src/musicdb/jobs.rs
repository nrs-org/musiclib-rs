//! Job history: one row per server job (an ingest, the library update, …),
//! written when the job starts and again when it ends, so the player can
//! list past jobs and a job cut off by a restart shows as `interrupted`
//! (docs/plan-jobs-ui.md). The server's `JobManager` owns running jobs; this
//! is only their record.

use sea_orm::{ConnectionTrait, Statement, Value};

use super::{Error, MusicDb};

pub(super) mod job_run {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "job_run")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub op: String,
        pub label: String,
        /// `manual`, `schedule` or `store_protocol`.
        pub trigger: String,
        /// The request, as JSON.
        pub params: Option<String>,
        /// The job's status at the last write (`interrupted` once a restart
        /// found it unfinished).
        pub status: String,
        /// Unix seconds.
        #[sea_orm(indexed)]
        pub created_at: i64,
        pub started_at: Option<i64>,
        pub finished_at: Option<i64>,
        /// The last progress report, the result and the error, as JSON.
        pub progress: Option<String>,
        pub result: Option<String>,
        pub error: Option<String>,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

#[derive(Debug, Clone, PartialEq)]
pub struct JobRunRow {
    pub id: String,
    pub op: String,
    pub label: String,
    pub trigger: String,
    pub params: Option<String>,
    pub status: String,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub progress: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
}

/// Rows kept: the newest this many, none older than `MAX_AGE_SECS`.
const KEEP: i64 = 500;
const MAX_AGE_SECS: i64 = 90 * 24 * 60 * 60;

const COLUMNS: &str = "id, op, label, \"trigger\", params, status, created_at, started_at, \
     finished_at, progress, result, error";

fn row(r: &sea_orm::QueryResult) -> Result<JobRunRow, Error> {
    Ok(JobRunRow {
        id: r.try_get("", "id")?,
        op: r.try_get("", "op")?,
        label: r.try_get("", "label")?,
        trigger: r.try_get("", "trigger")?,
        params: r.try_get("", "params")?,
        status: r.try_get("", "status")?,
        created_at: r.try_get("", "created_at")?,
        started_at: r.try_get("", "started_at")?,
        finished_at: r.try_get("", "finished_at")?,
        progress: r.try_get("", "progress")?,
        result: r.try_get("", "result")?,
        error: r.try_get("", "error")?,
    })
}

impl MusicDb {
    /// Writes `run` over any row with its id.
    pub async fn save_job_run(&self, run: &JobRunRow) -> Result<(), Error> {
        let values: Vec<Value> = vec![
            run.id.clone().into(),
            run.op.clone().into(),
            run.label.clone().into(),
            run.trigger.clone().into(),
            run.params.clone().into(),
            run.status.clone().into(),
            run.created_at.into(),
            run.started_at.into(),
            run.finished_at.into(),
            run.progress.clone().into(),
            run.result.clone().into(),
            run.error.clone().into(),
        ];
        self.db
            .execute_raw(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                format!(
                    "INSERT OR REPLACE INTO job_run ({COLUMNS}) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
                ),
                values,
            ))
            .await?;
        Ok(())
    }

    pub async fn job_run(&self, id: &str) -> Result<Option<JobRunRow>, Error> {
        let r = self
            .db
            .query_one_raw(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                format!("SELECT {COLUMNS} FROM job_run WHERE id = ?"),
                [id.into()],
            ))
            .await?;
        r.as_ref().map(row).transpose()
    }

    /// Newest first: up to `limit` rows created before `before` (unix
    /// seconds), or the newest when `None`.
    pub async fn job_runs(&self, limit: u64, before: Option<i64>) -> Result<Vec<JobRunRow>, Error> {
        let rows = self
            .db
            .query_all_raw(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                format!(
                    "SELECT {COLUMNS} FROM job_run WHERE created_at < ? \
                     ORDER BY created_at DESC, id DESC LIMIT ?"
                ),
                [before.unwrap_or(i64::MAX).into(), (limit as i64).into()],
            ))
            .await?;
        rows.iter().map(row).collect()
    }

    /// At startup: every row a previous process left unfinished becomes
    /// `interrupted`, and old rows beyond the retention are dropped. Returns
    /// how many were interrupted.
    pub async fn close_stale_job_runs(&self, now: i64) -> Result<u64, Error> {
        let backend = self.db.get_database_backend();
        let interrupted = self
            .db
            .execute_raw(Statement::from_sql_and_values(
                backend,
                "UPDATE job_run SET status = 'interrupted', finished_at = ? \
                 WHERE status IN ('pending', 'running', 'canceling')",
                [now.into()],
            ))
            .await?
            .rows_affected();
        self.db
            .execute_raw(Statement::from_sql_and_values(
                backend,
                "DELETE FROM job_run WHERE created_at < ? OR id NOT IN \
                 (SELECT id FROM job_run ORDER BY created_at DESC, id DESC LIMIT ?)",
                [(now - MAX_AGE_SECS).into(), KEEP.into()],
            ))
            .await?;
        Ok(interrupted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: &str, created_at: i64, status: &str) -> JobRunRow {
        JobRunRow {
            id: id.into(),
            op: "ingest".into(),
            label: format!("Import {id}"),
            trigger: "manual".into(),
            params: Some(r#"{"query":"x"}"#.into()),
            status: status.into(),
            created_at,
            started_at: Some(created_at),
            finished_at: None,
            progress: None,
            result: None,
            error: None,
        }
    }

    #[tokio::test]
    async fn save_list_and_close() {
        let mdb = MusicDb::new("sqlite::memory:").await.unwrap();
        let now = 1_800_000_000;
        mdb.save_job_run(&run("a", now - 30, "succeeded"))
            .await
            .unwrap();
        mdb.save_job_run(&run("b", now - 20, "running"))
            .await
            .unwrap();
        mdb.save_job_run(&run("c", now - 10, "pending"))
            .await
            .unwrap();
        // An update of a row replaces it.
        let mut done = run("a", now - 30, "succeeded");
        done.finished_at = Some(now - 25);
        mdb.save_job_run(&done).await.unwrap();

        let ids = |rows: Vec<JobRunRow>| rows.into_iter().map(|r| r.id).collect::<Vec<_>>();
        assert_eq!(ids(mdb.job_runs(10, None).await.unwrap()), ["c", "b", "a"]);
        assert_eq!(ids(mdb.job_runs(1, Some(now - 10)).await.unwrap()), ["b"]);
        assert_eq!(
            mdb.job_run("a").await.unwrap().unwrap().finished_at,
            Some(now - 25)
        );

        assert_eq!(mdb.close_stale_job_runs(now).await.unwrap(), 2);
        let b = mdb.job_run("b").await.unwrap().unwrap();
        assert_eq!(
            (b.status.as_str(), b.finished_at),
            ("interrupted", Some(now))
        );
        assert_eq!(mdb.job_run("a").await.unwrap().unwrap().status, "succeeded");

        // Rows past the retention age go.
        mdb.save_job_run(&run("old", now - MAX_AGE_SECS - 1, "succeeded"))
            .await
            .unwrap();
        mdb.close_stale_job_runs(now).await.unwrap();
        assert!(mdb.job_run("old").await.unwrap().is_none());
    }
}
