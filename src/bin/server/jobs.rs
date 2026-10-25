//! Generic async job resource — store-protocol/specification.md §7 (in the
//! `nrs` spec repository). Every long-running operation (an ingest, the
//! library update, the source finder's `attach_source`) creates one of these
//! instead of blocking the request that started it; the client polls
//! `GET /jobs/:id` or follows `GET /api/jobs/events` until it reaches a
//! terminal status.
//!
//! Each job carries what it's about (`JobMeta`: a label, what started it,
//! its parameters), is recorded in `musiclib.db`'s `job_run` when it starts
//! and when it ends (so finished jobs outlive the process, and one cut off
//! by a restart shows as `interrupted`), and announces every change on a
//! broadcast channel (progress at most about once a second per job). See
//! docs/plan-jobs-ui.md.

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use dashmap::DashMap;
use musiclib_rs::musicdb::{JobRunRow, MusicDb};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::{
    sync::{broadcast, mpsc},
    task::{AbortHandle, JoinError},
};

/// A `JoinError` is always either "cancelled" or "panicked" — never both, and
/// nothing else. `into_panic()` panics if called on the former, hence the
/// `is_cancelled` guard.
fn join_error_message(e: JoinError) -> String {
    if e.is_cancelled() {
        return "canceled".to_string();
    }
    let payload = e.into_panic();
    if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "job task panicked".to_string()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    /// Part of the wire contract (spec §7.2) but never emitted by this
    /// implementation: cancellation here has no cooperative-interruption
    /// mechanism to report an interim state for, so `cancel()` transitions
    /// straight to `Canceled` instead of pausing here.
    #[allow(dead_code)]
    Canceling,
    Canceled,
    /// A recorded job a server restart found unfinished.
    Interrupted,
}

impl JobStatus {
    fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending | Self::Running | Self::Canceling)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceling => "canceling",
            Self::Canceled => "canceled",
            Self::Interrupted => "interrupted",
        }
    }
}

/// What started a job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// The player.
    Manual,
    /// The update scheduler.
    #[allow(dead_code)]
    Schedule,
    /// An NRS store-protocol request.
    StoreProtocol,
}

impl Trigger {
    fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Schedule => "schedule",
            Self::StoreProtocol => "store_protocol",
        }
    }
}

/// What a job is about, given when it's spawned.
pub struct JobMeta {
    /// For people: "Import https://…", "Library update: 52 roots".
    pub label: String,
    pub trigger: Trigger,
    /// The request, as JSON.
    pub params: Value,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct JobProgress {
    pub stage: String,
    pub message: String,
    /// Op-specific structured payload, opaque to `JobManager` itself — e.g.
    /// the `ingest` job attaches a live fetch feed and per-domain HTTP
    /// activity here (see `server`'s `ingest_progress` module) so the
    /// frontend can render something closer to the `import` CLI's display
    /// than `stage`/`message` alone allow. `None` for jobs that don't use it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct JobError {
    pub code: String,
    pub message: String,
}

struct JobEntry {
    op: String,
    meta: JobMeta,
    status: JobStatus,
    /// Unix seconds.
    created_at: i64,
    started_at: Option<i64>,
    finished_at: Option<i64>,
    updated_at: i64,
    progress: Option<JobProgress>,
    result: Option<Value>,
    error: Option<JobError>,
    handle: Option<AbortHandle>,
    /// When a change was last announced, and whether one since is waiting
    /// for the next tick (progress is announced at most every
    /// `EVENT_INTERVAL`).
    announced_at: Option<Instant>,
    unannounced: bool,
}

#[derive(Clone, Serialize)]
pub struct JobView {
    pub job_id: String,
    pub op: String,
    pub label: String,
    pub trigger: Trigger,
    pub params: Value,
    pub status: JobStatus,
    pub created_at: String,
    pub updated_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub progress: Option<JobProgress>,
    pub result: Option<Value>,
    pub error: Option<JobError>,
}

/// Progress is announced at most this often per job.
const EVENT_INTERVAL: Duration = Duration::from_secs(1);
/// Finished jobs kept in memory besides the running ones; older ones are
/// read back from `job_run`.
const KEEP_FINISHED: usize = 50;
/// Events a slow subscriber may fall behind by before it gets a snapshot.
const EVENT_BUFFER: usize = 256;

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn rfc3339(unix: i64) -> String {
    OffsetDateTime::from_unix_timestamp(unix)
        .ok()
        .and_then(|t| t.format(&Rfc3339).ok())
        .unwrap_or_default()
}

/// The progress kept in history: without the rolling activity log and
/// per-domain HTTP counts, which only mean anything while a job runs.
fn lasting_progress(progress: &JobProgress) -> JobProgress {
    let mut p = progress.clone();
    if let Some(Value::Object(detail)) = &mut p.detail {
        detail.remove("recent");
        detail.remove("http");
    }
    p
}

pub struct JobManager {
    jobs: DashMap<String, JobEntry>,
    next_id: AtomicU64,
    /// The process's start time: job ids stay unique across restarts, which
    /// matters once they're recorded.
    id_prefix: String,
    db: Option<MusicDb>,
    /// One writer, so a job's rows are written in the order they change.
    writes: Option<mpsc::UnboundedSender<JobRunRow>>,
    events: broadcast::Sender<JobView>,
}

impl JobManager {
    /// `db` records jobs in `job_run`; without it (tests) they live in
    /// memory only. Needs a Tokio runtime: it starts the history writer and
    /// the event ticker.
    pub fn new(db: Option<MusicDb>) -> Arc<Self> {
        let writes = db.clone().map(|db| {
            let (tx, mut rx) = mpsc::unbounded_channel::<JobRunRow>();
            tokio::spawn(async move {
                while let Some(run) = rx.recv().await {
                    if let Err(e) = db.save_job_run(&run).await {
                        tracing::warn!("recording job {} failed: {e}", run.id);
                    }
                }
            });
            tx
        });
        let this = Arc::new(Self {
            jobs: DashMap::new(),
            next_id: AtomicU64::new(1),
            id_prefix: format!("{:x}", unix_now()),
            db,
            writes,
            events: broadcast::channel(EVENT_BUFFER).0,
        });
        let weak = Arc::downgrade(&this);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(EVENT_INTERVAL);
            loop {
                tick.tick().await;
                let Some(this) = weak.upgrade() else { break };
                this.announce_waiting();
            }
        });
        this
    }

    /// Marks rows a previous process left unfinished `interrupted`, and
    /// drops history beyond its retention.
    pub async fn close_stale(&self) {
        let Some(db) = &self.db else { return };
        match db.close_stale_job_runs(unix_now()).await {
            Ok(0) => {}
            Ok(n) => tracing::info!("{n} job(s) were cut off by the last shutdown"),
            Err(e) => tracing::warn!("closing unfinished job records failed: {e}"),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<JobView> {
        self.events.subscribe()
    }

    /// Create a job in `pending` status and spawn `make_task` to run it via
    /// `tokio::task::spawn_local` — this binary runs a single-threaded
    /// runtime under a `LocalSet` (see `main.rs`) specifically so job
    /// futures don't need to be `Send`: the dedup scoring pipeline holds a
    /// `rhai::Engine` (not `Sync`, by construction — see `CustomSyntax`)
    /// across `.await` points, which `tokio::spawn` can't accept.
    /// `make_task` is handed a [`JobHandle`] to report progress and the
    /// terminal outcome through; the job manager marks it `running` right
    /// before the task starts.
    ///
    /// If `make_task`'s future panics (a provider backend bug, say) without
    /// ever reaching `succeed`/`fail`, a second watcher task (below) notices
    /// the underlying `JoinHandle` came back `Err` and fails the job itself
    /// — otherwise it would sit at its last reported progress forever, which
    /// looks indistinguishable from a slow job that's still working.
    pub fn spawn<F, Fut>(self: &Arc<Self>, op: &str, meta: JobMeta, make_task: F) -> String
    where
        F: FnOnce(JobHandle) -> Fut + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let id = format!(
            "{}-{:x}",
            self.id_prefix,
            self.next_id.fetch_add(1, Ordering::Relaxed)
        );
        let now = unix_now();
        self.jobs.insert(
            id.clone(),
            JobEntry {
                op: op.to_string(),
                meta,
                status: JobStatus::Pending,
                created_at: now,
                started_at: None,
                finished_at: None,
                updated_at: now,
                progress: None,
                result: None,
                error: None,
                handle: None,
                announced_at: None,
                unannounced: false,
            },
        );
        self.changed(&id, true);

        let this = Arc::clone(self);
        let task_id = id.clone();
        let join_handle = tokio::task::spawn_local(async move {
            this.update(&task_id, |e| {
                e.status = JobStatus::Running;
                e.started_at = Some(unix_now());
            });
            make_task(JobHandle {
                manager: Arc::clone(&this),
                id: task_id,
            })
            .await;
        });
        if let Some(mut entry) = self.jobs.get_mut(&id) {
            entry.handle = Some(join_handle.abort_handle());
        }

        let watch_this = Arc::clone(self);
        let watch_id = id.clone();
        tokio::task::spawn_local(async move {
            if let Err(join_err) = join_handle.await {
                watch_this.fail_if_unfinished(&watch_id, &join_error_message(join_err));
            }
        });
        id
    }

    /// Apply `f` to job `id` unless it's already terminal, then record and
    /// announce the change. Returns false for an unknown or finished job.
    fn update(&self, id: &str, f: impl FnOnce(&mut JobEntry)) -> bool {
        {
            let Some(mut entry) = self.jobs.get_mut(id) else {
                return false;
            };
            if entry.status.is_terminal() {
                return false;
            }
            f(&mut entry);
            entry.updated_at = unix_now();
            if entry.status.is_terminal() {
                entry.finished_at = Some(entry.updated_at);
                entry.handle = None;
            }
        }
        self.changed(id, true);
        true
    }

    /// Record (when `record`) and announce job `id`'s current state; a
    /// terminal job is also pruned from memory down to `KEEP_FINISHED`.
    fn changed(&self, id: &str, record: bool) {
        let Some((view, terminal, row)) = self.jobs.get_mut(id).map(|mut entry| {
            entry.announced_at = Some(Instant::now());
            entry.unannounced = false;
            let view = Self::view_of(id, &entry);
            (view, entry.status.is_terminal(), Self::row_of(id, &entry))
        }) else {
            return;
        };
        if record && let Some(writes) = &self.writes {
            let _ = writes.send(row);
        }
        let _ = self.events.send(view);
        if terminal {
            self.prune();
        }
    }

    /// Announce progress that `progress_detail` held back.
    fn announce_waiting(&self) {
        let waiting: Vec<String> = self
            .jobs
            .iter()
            .filter(|e| e.unannounced)
            .map(|e| e.key().clone())
            .collect();
        for id in waiting {
            self.changed(&id, false);
        }
    }

    fn prune(&self) {
        let mut finished: Vec<(i64, String)> = self
            .jobs
            .iter()
            .filter(|e| e.status.is_terminal())
            .map(|e| (e.updated_at, e.key().clone()))
            .collect();
        if finished.len() <= KEEP_FINISHED {
            return;
        }
        finished.sort();
        for (_, id) in &finished[..finished.len() - KEEP_FINISHED] {
            self.jobs.remove(id);
        }
    }

    /// Marks `id` `failed` with `message` unless it already reached a
    /// terminal status — used by the panic watcher above, and harmless
    /// no-op for the ordinary case where `cancel()` already set `Canceled`
    /// (a cancelled task's `JoinHandle` also resolves `Err`, just for
    /// "cancelled" rather than "panicked").
    fn fail_if_unfinished(&self, id: &str, message: &str) {
        self.update(id, |e| {
            e.status = JobStatus::Failed;
            e.error = Some(JobError {
                code: "panicked".to_string(),
                message: message.to_string(),
            });
        });
    }

    fn view_of(id: &str, e: &JobEntry) -> JobView {
        JobView {
            job_id: id.to_string(),
            op: e.op.clone(),
            label: e.meta.label.clone(),
            trigger: e.meta.trigger,
            params: e.meta.params.clone(),
            status: e.status,
            created_at: rfc3339(e.created_at),
            updated_at: rfc3339(e.updated_at),
            started_at: e.started_at.map(rfc3339),
            finished_at: e.finished_at.map(rfc3339),
            progress: e.progress.clone(),
            result: e.result.clone(),
            error: e.error.clone(),
        }
    }

    fn row_of(id: &str, e: &JobEntry) -> JobRunRow {
        let json = |v: &Option<Value>| v.as_ref().map(Value::to_string);
        JobRunRow {
            id: id.to_string(),
            op: e.op.clone(),
            label: e.meta.label.clone(),
            trigger: e.meta.trigger.as_str().to_string(),
            params: Some(e.meta.params.to_string()),
            status: e.status.as_str().to_string(),
            created_at: e.created_at,
            started_at: e.started_at,
            finished_at: e.finished_at,
            progress: e
                .progress
                .as_ref()
                .and_then(|p| serde_json::to_string(&lasting_progress(p)).ok()),
            result: json(&e.result),
            error: e.error.as_ref().and_then(|x| serde_json::to_string(x).ok()),
        }
    }

    fn view_of_row(r: JobRunRow) -> JobView {
        fn parse<T: serde::de::DeserializeOwned>(s: &Option<String>) -> Option<T> {
            s.as_deref().and_then(|s| serde_json::from_str(s).ok())
        }
        JobView {
            status: serde_json::from_value(Value::String(r.status.clone()))
                .unwrap_or(JobStatus::Interrupted),
            trigger: serde_json::from_value(Value::String(r.trigger.clone()))
                .unwrap_or(Trigger::Manual),
            params: parse(&r.params).unwrap_or(Value::Null),
            progress: parse(&r.progress),
            result: parse(&r.result),
            error: parse(&r.error),
            created_at: rfc3339(r.created_at),
            updated_at: rfc3339(r.finished_at.or(r.started_at).unwrap_or(r.created_at)),
            started_at: r.started_at.map(rfc3339),
            finished_at: r.finished_at.map(rfc3339),
            job_id: r.id,
            op: r.op,
            label: r.label,
        }
    }

    /// A job this process knows: running, or among the latest finished.
    pub fn view(&self, id: &str) -> Option<JobView> {
        self.jobs.get(id).map(|e| Self::view_of(id, &e))
    }

    /// Any job: in memory, else from history.
    pub async fn find(&self, id: &str) -> Option<JobView> {
        if let Some(view) = self.view(id) {
            return Some(view);
        }
        let db = self.db.as_ref()?;
        match db.job_run(id).await {
            Ok(row) => row.map(Self::view_of_row),
            Err(e) => {
                tracing::warn!("reading job {id} failed: {e}");
                None
            }
        }
    }

    /// Pending and running jobs, oldest first.
    pub fn running(&self) -> Vec<JobView> {
        let mut out: Vec<(i64, JobView)> = self
            .jobs
            .iter()
            .filter(|e| !e.status.is_terminal())
            .map(|e| (e.created_at, Self::view_of(e.key(), &e)))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.job_id.cmp(&b.1.job_id)));
        out.into_iter().map(|(_, v)| v).collect()
    }

    /// Finished jobs, newest first: up to `limit` created before `before`
    /// (unix seconds). From history; without one, those still in memory.
    pub async fn finished(&self, limit: u64, before: Option<i64>) -> Vec<JobView> {
        if let Some(db) = &self.db {
            return match db.job_runs(limit, before).await {
                Ok(rows) => rows
                    .into_iter()
                    .map(Self::view_of_row)
                    .filter(|v| v.status.is_terminal())
                    .collect(),
                Err(e) => {
                    tracing::warn!("reading job history failed: {e}");
                    Vec::new()
                }
            };
        }
        let mut out: Vec<(i64, JobView)> = self
            .jobs
            .iter()
            .filter(|e| e.status.is_terminal() && before.is_none_or(|b| e.created_at < b))
            .map(|e| (e.created_at, Self::view_of(e.key(), &e)))
            .collect();
        out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.job_id.cmp(&a.1.job_id)));
        out.into_iter()
            .take(limit as usize)
            .map(|(_, v)| v)
            .collect()
    }

    /// Best-effort cancellation (spec §7.3): aborts the task at its next
    /// await point and marks the job `canceled` immediately, rather than
    /// waiting for the abort to actually land — `JoinHandle::abort` doesn't
    /// give us a completion signal to wait on, and every job here is
    /// network-I/O-bound so the task will in fact stop promptly. Returns
    /// `false` only if the job id is unknown.
    pub fn cancel(&self, id: &str) -> bool {
        if !self.jobs.contains_key(id) {
            return false;
        }
        self.update(id, |e| {
            if let Some(handle) = &e.handle {
                handle.abort();
            }
            e.status = JobStatus::Canceled;
        });
        true
    }
}

/// Handed to a job's task so it can report progress and its terminal
/// outcome. Dropping it without calling `succeed`/`fail` leaves the job
/// stuck at its last reported status — every job implementation must call
/// exactly one of them. `Clone` is cheap (an `Arc` and a `String`) and lets a
/// job hand a copy to a progress observer (e.g. `ImportProgress`) that only
/// ever calls `progress`/`progress_detail`, while keeping the original to
/// call `succeed`/`fail` itself.
#[derive(Clone)]
pub struct JobHandle {
    manager: Arc<JobManager>,
    id: String,
}

impl JobHandle {
    pub fn progress(&self, stage: &str, message: impl Into<String>) {
        self.progress_detail(stage, message, None);
    }

    /// Same as `progress`, plus an op-specific `detail` payload — see
    /// `JobProgress::detail`. Announced at most every `EVENT_INTERVAL`; the
    /// manager's ticker announces whatever was held back.
    pub fn progress_detail(&self, stage: &str, message: impl Into<String>, detail: Option<Value>) {
        let announce = {
            let Some(mut entry) = self.manager.jobs.get_mut(&self.id) else {
                return;
            };
            if entry.status.is_terminal() {
                return;
            }
            entry.progress = Some(JobProgress {
                stage: stage.to_string(),
                message: message.into(),
                detail,
            });
            entry.updated_at = unix_now();
            let due = entry
                .announced_at
                .is_none_or(|t| t.elapsed() >= EVENT_INTERVAL);
            entry.unannounced = !due;
            due
        };
        if announce {
            self.manager.changed(&self.id, false);
        }
    }

    pub fn succeed(&self, result: Value) {
        self.manager.update(&self.id, |e| {
            e.status = JobStatus::Succeeded;
            e.result = Some(result);
        });
    }

    pub fn fail(&self, code: &str, message: impl Into<String>) {
        self.manager.update(&self.id, |e| {
            e.status = JobStatus::Failed;
            e.error = Some(JobError {
                code: code.to_string(),
                message: message.into(),
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(label: &str) -> JobMeta {
        JobMeta {
            label: label.into(),
            trigger: Trigger::Manual,
            params: serde_json::json!({ "query": label }),
        }
    }

    /// A job is recorded when it starts and when it ends, announced on every
    /// change, and still found once it's gone from memory.
    #[tokio::test]
    async fn jobs_are_recorded_announced_and_found() {
        let db = MusicDb::new("sqlite::memory:").await.unwrap();
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let jobs = JobManager::new(Some(db.clone()));
                let mut events = jobs.subscribe();
                let (tx, rx) = tokio::sync::oneshot::channel::<()>();
                let id = jobs.spawn("ingest", meta("Import x"), move |h| async move {
                    h.progress("fetching", "1 entry fetched");
                    rx.await.ok();
                    h.succeed(serde_json::json!({ "entry_id": 7 }));
                });
                assert_eq!(events.recv().await.unwrap().status, JobStatus::Pending);
                tokio::task::yield_now().await;
                assert_eq!(jobs.running().len(), 1);
                assert_eq!(jobs.running()[0].label, "Import x");

                tx.send(()).unwrap();
                let done = loop {
                    let v = events.recv().await.unwrap();
                    if v.status.is_terminal() {
                        break v;
                    }
                };
                assert_eq!(done.status, JobStatus::Succeeded);
                assert!(jobs.running().is_empty());

                // The writer catches up; history has the finished job.
                for _ in 0..50 {
                    if db
                        .job_run(&id)
                        .await
                        .unwrap()
                        .is_some_and(|r| r.status == "succeeded")
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                let finished = jobs.finished(10, None).await;
                assert_eq!(finished.len(), 1);
                assert_eq!(
                    finished[0].result,
                    Some(serde_json::json!({ "entry_id": 7 }))
                );
                jobs.jobs.remove(&id);
                assert_eq!(jobs.find(&id).await.unwrap().status, JobStatus::Succeeded);
            })
            .await;
    }

    /// Canceling marks the job canceled at once; its task's later outcome
    /// doesn't overwrite that.
    #[tokio::test]
    async fn cancel_wins_over_a_late_outcome() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let jobs = JobManager::new(None);
                let id = jobs.spawn("update", meta("Library update"), |h| async move {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    h.succeed(Value::Null);
                });
                tokio::task::yield_now().await;
                assert!(jobs.cancel(&id));
                assert_eq!(jobs.view(&id).unwrap().status, JobStatus::Canceled);
                assert!(!jobs.cancel("nope"));
                assert_eq!(jobs.finished(10, None).await.len(), 1);
            })
            .await;
    }
}
