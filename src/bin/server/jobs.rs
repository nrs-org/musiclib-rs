//! Generic async job resource — store-protocol/specification.md §7 (in the
//! `nrs` spec repository). Every long-running store-protocol operation
//! (currently just `ingest`) creates one of these instead of blocking the
//! request that started it; the client polls `GET /jobs/:id` (or streams,
//! once that's implemented) until it reaches a terminal status.

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use dashmap::DashMap;
use serde::Serialize;
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::task::{AbortHandle, JoinError};

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
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
}

#[derive(Clone, Serialize)]
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

#[derive(Clone, Serialize)]
pub struct JobError {
    pub code: String,
    pub message: String,
}

struct JobEntry {
    op: String,
    status: JobStatus,
    created_at: String,
    updated_at: String,
    progress: Option<JobProgress>,
    result: Option<Value>,
    error: Option<JobError>,
    handle: Option<AbortHandle>,
}

#[derive(Serialize)]
pub struct JobView {
    pub job_id: String,
    pub op: String,
    pub status: JobStatus,
    pub created_at: String,
    pub updated_at: String,
    pub progress: Option<JobProgress>,
    pub result: Option<Value>,
    pub error: Option<JobError>,
}

pub struct JobManager {
    jobs: DashMap<String, JobEntry>,
    next_id: AtomicU64,
}

impl JobManager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            jobs: DashMap::new(),
            next_id: AtomicU64::new(1),
        })
    }

    fn now() -> String {
        OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_default()
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
    pub fn spawn<F, Fut>(self: &Arc<Self>, op: &str, make_task: F) -> String
    where
        F: FnOnce(JobHandle) -> Fut + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let id = format!("{:x}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let now = Self::now();
        self.jobs.insert(
            id.clone(),
            JobEntry {
                op: op.to_string(),
                status: JobStatus::Pending,
                created_at: now.clone(),
                updated_at: now,
                progress: None,
                result: None,
                error: None,
                handle: None,
            },
        );

        let this = Arc::clone(self);
        let task_id = id.clone();
        let join_handle = tokio::task::spawn_local(async move {
            this.set_status(&task_id, JobStatus::Running);
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

    /// Marks `id` `failed` with `message` unless it already reached a
    /// terminal status — used by the panic watcher above, and harmless
    /// no-op for the ordinary case where `cancel()` already set `Canceled`
    /// (a cancelled task's `JoinHandle` also resolves `Err`, just for
    /// "cancelled" rather than "panicked").
    fn fail_if_unfinished(&self, id: &str, message: &str) {
        if let Some(mut entry) = self.jobs.get_mut(id) {
            if matches!(
                entry.status,
                JobStatus::Succeeded | JobStatus::Failed | JobStatus::Canceled
            ) {
                return;
            }
            entry.status = JobStatus::Failed;
            entry.error = Some(JobError {
                code: "panicked".to_string(),
                message: message.to_string(),
            });
            entry.updated_at = Self::now();
        }
    }

    fn set_status(&self, id: &str, status: JobStatus) {
        if let Some(mut entry) = self.jobs.get_mut(id) {
            entry.status = status;
            entry.updated_at = Self::now();
        }
    }

    pub fn view(&self, id: &str) -> Option<JobView> {
        self.jobs.get(id).map(|entry| JobView {
            job_id: id.to_string(),
            op: entry.op.clone(),
            status: entry.status,
            created_at: entry.created_at.clone(),
            updated_at: entry.updated_at.clone(),
            progress: entry.progress.clone(),
            result: entry.result.clone(),
            error: entry.error.clone(),
        })
    }

    /// Best-effort cancellation (spec §7.3): aborts the task at its next
    /// await point and marks the job `canceled` immediately, rather than
    /// waiting for the abort to actually land — `JoinHandle::abort` doesn't
    /// give us a completion signal to wait on, and every job here is
    /// network-I/O-bound so the task will in fact stop promptly. Returns
    /// `false` only if the job id is unknown.
    pub fn cancel(&self, id: &str) -> bool {
        let Some(mut entry) = self.jobs.get_mut(id) else {
            return false;
        };
        if matches!(
            entry.status,
            JobStatus::Succeeded | JobStatus::Failed | JobStatus::Canceled
        ) {
            return true;
        }
        if let Some(handle) = &entry.handle {
            handle.abort();
        }
        entry.status = JobStatus::Canceled;
        entry.updated_at = Self::now();
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
    /// `JobProgress::detail`.
    pub fn progress_detail(&self, stage: &str, message: impl Into<String>, detail: Option<Value>) {
        if let Some(mut entry) = self.manager.jobs.get_mut(&self.id) {
            entry.progress = Some(JobProgress {
                stage: stage.to_string(),
                message: message.into(),
                detail,
            });
            entry.updated_at = JobManager::now();
        }
    }

    pub fn succeed(&self, result: Value) {
        if let Some(mut entry) = self.manager.jobs.get_mut(&self.id) {
            if entry.status == JobStatus::Canceled {
                return;
            }
            entry.status = JobStatus::Succeeded;
            entry.result = Some(result);
            entry.updated_at = JobManager::now();
        }
    }

    pub fn fail(&self, code: &str, message: impl Into<String>) {
        if let Some(mut entry) = self.manager.jobs.get_mut(&self.id) {
            if entry.status == JobStatus::Canceled {
                return;
            }
            entry.status = JobStatus::Failed;
            entry.error = Some(JobError {
                code: code.to_string(),
                message: message.into(),
            });
            entry.updated_at = JobManager::now();
        }
    }
}
