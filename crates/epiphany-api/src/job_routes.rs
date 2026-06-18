//! Scheduled-job endpoints (ADR-0013/0035): CRUD over the server-global jobs
//! (exposed as "schedules"), a manual kick, and the global run queries. Jobs are
//! server-global secured objects; defining or running one requires `Job:Write`,
//! reading requires `Job:Read`. A job's steps reference global flows by name. A
//! manual kick runs the job through the same path the reconcile loop uses,
//! executing each step as the flow's owner (ADR-0035), and records it in the
//! durable run ledger.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use epiphany_core::{Job, Trigger};
use epiphany_flow::{Firing, RunRecord};
use epiphany_security::{AccessLevel, AuditAction, ObjectKind, ObjectRef};

use crate::auth::AuthPrincipal;
use crate::authz::{audit, require_admin, require_kind_access};
use crate::routes::map_persist_error;
use crate::scheduler::Scheduler;
use crate::{ApiError, AppState};

// ---- DTOs ----

#[derive(Serialize, Deserialize)]
pub(crate) struct JobDto {
    /// Ignored on `PUT` (the name comes from the path); always set on responses.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub steps: Vec<String>,
    pub every_millis: u64,
    pub enabled: bool,
}

#[derive(Serialize)]
pub(crate) struct JobListDto {
    pub jobs: Vec<JobDto>,
}

fn job_dto(job: &Job) -> JobDto {
    let Trigger::Interval { every_millis } = job.trigger;
    JobDto {
        name: job.name.clone(),
        steps: job.steps.clone(),
        every_millis,
        enabled: job.enabled,
    }
}

#[derive(Serialize)]
pub(crate) struct RunDto {
    pub id: String,
    /// The cube a run wrote. Empty for a global flow/job run (ADR-0035): a global
    /// run is labelled by its flow/job, not a cube.
    pub cube: String,
    pub target: String,
    pub is_job: bool,
    pub fire_millis: u64,
    pub state: &'static str,
    pub rows_read: u64,
    pub cells_written: u64,
    pub elements_added: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub principal: String,
}

#[derive(Serialize)]
pub(crate) struct RunListDto {
    pub runs: Vec<RunDto>,
}

fn run_dto(r: &RunRecord) -> RunDto {
    RunDto {
        id: r.id.clone(),
        cube: r.cube.clone(),
        target: r.target.clone(),
        is_job: r.is_job,
        fire_millis: r.fire_millis,
        state: r.state.as_str(),
        rows_read: r.rows_read,
        cells_written: r.cells_written,
        elements_added: r.elements_added,
        error: (!r.error.is_empty()).then(|| r.error.clone()),
        principal: r.principal.clone(),
    }
}

// ---- job CRUD ----

/// `GET /schedules` -> the global jobs.
pub(crate) async fn list_jobs(
    auth: AuthPrincipal,
    State(state): State<AppState>,
) -> Result<Json<JobListDto>, ApiError> {
    require_kind_access(&state, &auth, ObjectKind::Job, None, AccessLevel::Read)?;
    let store = state.automation.lock().expect("automation store mutex");
    Ok(Json(JobListDto {
        jobs: store.automation().jobs.values().map(job_dto).collect(),
    }))
}

/// `GET /schedules/{name}` -> one job.
pub(crate) async fn get_job(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<JobDto>, ApiError> {
    require_kind_access(&state, &auth, ObjectKind::Job, None, AccessLevel::Read)?;
    let store = state.automation.lock().expect("automation store mutex");
    let job = store
        .automation()
        .job(&name)
        .ok_or_else(|| ApiError::not_found(format!("unknown job '{name}'")))?;
    Ok(Json(job_dto(job)))
}

/// `PUT /schedules/{name}` -> validate and store a job. Every step must name an
/// existing global flow.
pub(crate) async fn put_job(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<JobDto>,
) -> Result<Json<JobDto>, ApiError> {
    require_kind_access(&state, &auth, ObjectKind::Job, None, AccessLevel::Write)?;
    let job = Job {
        name: name.clone(),
        steps: body.steps.clone(),
        trigger: Trigger::Interval {
            every_millis: body.every_millis,
        },
        enabled: body.enabled,
    };
    let existed = {
        let mut store = state.automation.lock().expect("automation store mutex");
        for step in &body.steps {
            if !store.automation().flows.contains_key(step) {
                return Err(ApiError::unprocessable(
                    "UNKNOWN_FLOW",
                    format!("job step references unknown flow '{step}'"),
                ));
            }
        }
        let existed = store.automation().job(&name).is_some();
        store.define_job(job.clone()).map_err(map_persist_error)?;
        existed
    };
    audit(
        &state,
        &auth.principal.username,
        if existed {
            AuditAction::ObjectUpdate
        } else {
            AuditAction::ObjectCreate
        },
        Some(&ObjectRef::global(ObjectKind::Job, &name)),
        true,
    );
    Ok(Json(job_dto(&job)))
}

/// `DELETE /schedules/{name}` -> delete a job.
pub(crate) async fn delete_job(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_kind_access(&state, &auth, ObjectKind::Job, None, AccessLevel::Write)?;
    let removed = state
        .automation
        .lock()
        .expect("automation store mutex")
        .delete_job(&name)
        .map_err(map_persist_error)?;
    if !removed {
        return Err(ApiError::not_found(format!("unknown job '{name}'")));
    }
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectDelete,
        Some(&ObjectRef::global(ObjectKind::Job, &name)),
        true,
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /schedules/{name}/run` -> run a job now (manual kick), through the same
/// path the reconcile loop uses, and return the resulting run record. The kick is
/// authorized as the caller (`Job:Write`); each step still executes as the flow's
/// owner and is gated by that owner's data access (ADR-0035), so a kick is never a
/// privilege-escalation path.
pub(crate) async fn run_job(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<RunDto>, ApiError> {
    require_kind_access(&state, &auth, ObjectKind::Job, None, AccessLevel::Write)?;
    {
        let store = state.automation.lock().expect("automation store mutex");
        if store.automation().job(&name).is_none() {
            return Err(ApiError::not_found(format!("unknown job '{name}'")));
        }
    }
    // A manual firing: the fire time is the current clock, the id is distinct from
    // the scheduled-id scheme so the two never collide. A global job has no cube.
    let fire_millis = state.clock.now_millis();
    let run_id = format!("manual:{name}:{fire_millis}");
    let firing = Firing {
        cube: String::new(),
        job: name.clone(),
        fire_millis,
        run_id: run_id.clone(),
        coalesced: false,
    };
    // The flow engine (boa) is blocking, so run the job off the async worker
    // threads, mirroring the scheduler loop (ADR-0013 decision 4).
    let principal = auth.principal.username.clone();
    let exec_state = state.clone();
    tokio::task::spawn_blocking(move || Scheduler::new(exec_state).execute(&firing, &principal))
        .await
        .map_err(|_| ApiError::internal())?;
    let record = state
        .runs
        .lock()
        .expect("run ledger mutex")
        .get(&run_id)
        .cloned()
        .ok_or_else(ApiError::internal)?;
    Ok(Json(run_dto(&record)))
}

// ---- run queries ----

/// Query for the global runs listing.
#[derive(Deserialize)]
pub(crate) struct RunsQuery {
    #[serde(default)]
    limit: Option<usize>,
}

/// `GET /runs` -> recent runs across the server (admin only), for the server
/// overview dashboard. `limit` defaults to 50, capped at 500.
pub(crate) async fn list_all_runs(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Query(q): Query<RunsQuery>,
) -> Result<Json<RunListDto>, ApiError> {
    require_admin(&state, &auth)?;
    let limit = q.limit.unwrap_or(50).min(500);
    let runs = state
        .runs
        .lock()
        .expect("run ledger mutex")
        .recent_global(limit)
        .iter()
        .map(run_dto)
        .collect();
    Ok(Json(RunListDto { runs }))
}

/// `GET /runs/{id}` -> one run by id (admin only). Global: no cube filter.
pub(crate) async fn get_run(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<RunDto>, ApiError> {
    require_admin(&state, &auth)?;
    let record = state
        .runs
        .lock()
        .expect("run ledger mutex")
        .get(&id)
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("unknown run '{id}'")))?;
    Ok(Json(run_dto(&record)))
}
