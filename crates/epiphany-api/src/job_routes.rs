//! Scheduled-job endpoints (ADR-0013): CRUD over a cube's jobs, a manual kick,
//! and run queries. Jobs are cube-scoped secured objects; defining or running one
//! requires cube `Write`, reading requires `Read`. A manual kick runs the job
//! through the same path the reconcile loop uses and records it in the durable
//! run ledger.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use epiphany_core::{Job, Trigger};
use epiphany_flow::{Firing, RunRecord};
use epiphany_security::{AccessLevel, AuditAction, ObjectKind, ObjectRef};

use crate::auth::AuthPrincipal;
use crate::authz::{audit, require_cube_access};
use crate::routes::map_batch_error;
use crate::scheduler::Scheduler;
use crate::ws::ChangeEvent;
use crate::{ApiError, AppState};

fn snapshot(state: &AppState, cube: &str) -> Result<epiphany_engine::ReadSnapshot, ApiError> {
    state
        .engine
        .snapshot(cube)
        .ok_or_else(|| ApiError::not_found(format!("unknown cube '{cube}'")))
}

fn broadcast(state: &AppState, cube: &str) {
    if let Some(version) = state.engine.version(cube) {
        let _ = state.events.send(ChangeEvent::ObjectsChanged {
            cube: cube.to_string(),
            version,
        });
    }
}

// ---- DTOs ----

#[derive(Serialize, Deserialize)]
pub(crate) struct JobDto {
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

/// `GET /cubes/{cube}/jobs` -> the cube's jobs.
pub(crate) async fn list_jobs(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
) -> Result<Json<JobListDto>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    let snap = snapshot(&state, &cube)?;
    Ok(Json(JobListDto {
        jobs: snap.model().jobs.values().map(job_dto).collect(),
    }))
}

/// `GET /cubes/{cube}/jobs/{name}` -> one job.
pub(crate) async fn get_job(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
) -> Result<Json<JobDto>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    let snap = snapshot(&state, &cube)?;
    let job = snap
        .model()
        .job(&name)
        .ok_or_else(|| ApiError::not_found(format!("unknown job '{name}'")))?;
    Ok(Json(job_dto(job)))
}

/// `PUT /cubes/{cube}/jobs/{name}` -> validate and store a job. Every step must
/// name an existing flow of the cube.
pub(crate) async fn put_job(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
    Json(body): Json<JobDto>,
) -> Result<Json<JobDto>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Write)?;
    let existed = {
        let snap = snapshot(&state, &cube)?;
        for step in &body.steps {
            if !snap.model().flows.contains_key(step) {
                return Err(ApiError::unprocessable(
                    "UNKNOWN_FLOW",
                    format!("job step references unknown flow '{step}'"),
                ));
            }
        }
        snap.model().job(&name).is_some()
    };
    let job = Job {
        name: name.clone(),
        steps: body.steps.clone(),
        trigger: Trigger::Interval {
            every_millis: body.every_millis,
        },
        enabled: body.enabled,
    };
    state
        .engine
        .define_job(&cube, None, job.clone())
        .map_err(map_batch_error)?;
    audit(
        &state,
        &auth.principal.username,
        if existed {
            AuditAction::ObjectUpdate
        } else {
            AuditAction::ObjectCreate
        },
        Some(&ObjectRef::in_cube(ObjectKind::Job, &cube, &name)),
        true,
    );
    broadcast(&state, &cube);
    Ok(Json(job_dto(&job)))
}

/// `DELETE /cubes/{cube}/jobs/{name}` -> delete a job.
pub(crate) async fn delete_job(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Write)?;
    state
        .engine
        .delete_job(&cube, None, &name)
        .map_err(map_batch_error)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectDelete,
        Some(&ObjectRef::in_cube(ObjectKind::Job, &cube, &name)),
        true,
    );
    broadcast(&state, &cube);
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /cubes/{cube}/jobs/{name}/run` -> run a job now (manual kick), through
/// the same path the reconcile loop uses, and return the resulting run record.
pub(crate) async fn run_job(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
) -> Result<Json<RunDto>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Write)?;
    {
        let snap = snapshot(&state, &cube)?;
        if snap.model().job(&name).is_none() {
            return Err(ApiError::not_found(format!("unknown job '{name}'")));
        }
    }
    // A manual firing: the fire time is the current clock, the id is distinct from
    // the scheduled-id scheme so the two never collide.
    let fire_millis = state.clock.now_millis();
    let run_id = format!("manual:{cube}:{name}:{fire_millis}");
    let firing = Firing {
        cube: cube.clone(),
        job: name.clone(),
        fire_millis,
        run_id: run_id.clone(),
        coalesced: false,
    };
    Scheduler::new(state.clone()).execute(&firing, &auth.principal.username);
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

/// `GET /cubes/{cube}/runs` -> recent runs for the cube (newest first).
pub(crate) async fn list_runs(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
) -> Result<Json<RunListDto>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    snapshot(&state, &cube)?;
    let runs = state
        .runs
        .lock()
        .expect("run ledger mutex")
        .recent(&cube, 200)
        .iter()
        .map(run_dto)
        .collect();
    Ok(Json(RunListDto { runs }))
}

/// `GET /cubes/{cube}/runs/{id}` -> one run by id.
pub(crate) async fn get_run(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, id)): Path<(String, String)>,
) -> Result<Json<RunDto>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    let record = state
        .runs
        .lock()
        .expect("run ledger mutex")
        .get(&id)
        .filter(|r| r.cube == cube)
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("unknown run '{id}'")))?;
    Ok(Json(run_dto(&record)))
}
