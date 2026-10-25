//! The library update as a job (`pipeline::update`), and providers that
//! refresh listings for jobs that ask to. See docs/plan-update-roots.md.
//!
//! - `POST /api/update` `{"root_ids"?, "quota_budget"?}` starts the update:
//!   `202` with a `job_id`, polled at `GET /api/jobs/:id` like an ingest.
//!   Without `root_ids` it runs every enabled root; with them, those roots
//!   (disabled ones too). `quota_budget` defaults to 8000 YouTube units for
//!   the day. Only one update runs at a time: `409` while one is running.
//!   The job's result is `pipeline::update::UpdateReport`.

use std::{
    sync::{Arc, atomic::Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use hyper::{Method, StatusCode};
use musiclib_rs::{
    fetch_presets,
    http::HttpClient,
    pipeline::update::{UpdateContext, UpdateOptions, run_update},
    providers::{
        FetchProvider,
        listing::{ListingMode, ListingStore},
        registry::build_providers_with_listings,
    },
};
use serde::Deserialize;
use serde_json::json;

use crate::{
    ingest_progress::JobProgressSink,
    respond::{self, HyperResponse},
    state::AppState,
};

const PATH: &str = "/api/update";

pub fn handles(path: &str) -> bool {
    path == PATH
}

pub async fn route(state: &Arc<AppState>, method: &Method, body: &[u8]) -> HyperResponse {
    match *method {
        Method::POST => start(state, body).await,
        _ => respond::method_not_allowed(),
    }
}

/// Providers over a fresh listing store in refresh mode, with no budget:
/// for an ingest that asks to see new items now.
pub async fn refreshing_providers(
    state: &AppState,
) -> anyhow::Result<Arc<Vec<Arc<dyn FetchProvider>>>> {
    let listings = Arc::new(
        ListingStore::new(state.listing_db.clone(), ListingMode::Auto)
            .await?
            .with_kind_modes(state.registry.listings.clone()),
    );
    let http: Arc<dyn HttpClient> = state.activity.clone();
    Ok(Arc::new(build_providers_with_listings(
        &state.registry,
        http,
        Some(listings),
    )?))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct UpdateBody {
    #[serde(default)]
    root_ids: Option<Vec<i64>>,
    #[serde(default)]
    quota_budget: Option<u64>,
}

/// Clears `AppState::updating` however the job ends (including a cancel,
/// which drops the task).
struct Running(Arc<AppState>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.updating.store(false, Ordering::SeqCst);
    }
}

async fn start(state: &Arc<AppState>, body: &[u8]) -> HyperResponse {
    let req: UpdateBody = if body.iter().all(u8::is_ascii_whitespace) {
        UpdateBody::default()
    } else {
        match serde_json::from_slice(body) {
            Ok(r) => r,
            Err(e) => {
                return respond::error(StatusCode::BAD_REQUEST, "invalid_input", &e.to_string());
            }
        }
    };
    let all = match state.db.update_roots().await {
        Ok(roots) => roots,
        Err(e) => {
            return respond::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                &e.to_string(),
            );
        }
    };
    let roots: Vec<_> = match &req.root_ids {
        None => all.into_iter().filter(|r| r.enabled).collect(),
        Some(ids) => {
            if let Some(missing) = ids.iter().find(|id| !all.iter().any(|r| r.id == **id)) {
                return respond::error(
                    StatusCode::BAD_REQUEST,
                    "invalid_input",
                    &format!("no root {missing}"),
                );
            }
            all.into_iter().filter(|r| ids.contains(&r.id)).collect()
        }
    };
    if roots.is_empty() {
        return respond::error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "no roots to update",
        );
    }
    if state.updating.swap(true, Ordering::SeqCst) {
        return respond::error(
            StatusCode::CONFLICT,
            "conflict",
            "an update is already running",
        );
    }
    let running = Running(Arc::clone(state));

    let job_state = Arc::clone(state);
    let opts = UpdateOptions {
        quota_budget: Some(req.quota_budget.unwrap_or(8000)),
        ..UpdateOptions::default()
    };
    let job_id = state.jobs.spawn("update", move |handle| async move {
        let _running = running;
        handle.progress(
            "updating",
            format!(
                "syncing the MusicBrainz mirror, then {} root(s)",
                roots.len()
            ),
        );
        let sink = JobProgressSink::new(
            handle.clone(),
            Arc::clone(&job_state.activity),
            Arc::clone(&job_state.youtube_quota),
        );
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let result = run_update(
            UpdateContext {
                db: &job_state.db,
                registry: &job_state.registry,
                http: job_state.activity.clone(),
                listing_db: job_state.listing_db.clone(),
                ledger: Arc::clone(&job_state.ledger),
                youtube_quota: Arc::clone(&job_state.youtube_quota),
                soft_cfg: job_state.soft_cfg.as_ref(),
                progress: Some(sink),
                fetch_options_dir: fetch_presets::dir(),
            },
            &roots,
            &opts,
            now,
        )
        .await;
        match result {
            Ok(report) => handle.succeed(serde_json::to_value(&report).unwrap_or_default()),
            Err(e) => handle.fail("update_failed", format!("{e:#}")),
        }
    });

    respond::json(
        StatusCode::ACCEPTED,
        json!({ "job_id": job_id, "op": "update", "status": "pending" }),
    )
}
