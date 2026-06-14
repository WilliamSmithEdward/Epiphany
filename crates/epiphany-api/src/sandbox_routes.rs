//! Sandbox lifecycle endpoints and the per-request sandbox selector (ADR-0014).
//!
//! A sandbox is a per-cube, per-user what-if overlay. Lifecycle (create, list,
//! get, discard, commit) is addressed by path under `/cubes/{cube}/sandboxes`;
//! the data endpoints (read, execute, explain, write) select a sandbox to
//! overlay with the `X-Epiphany-Sandbox` header via [`SandboxSelector`]. A
//! non-admin may use only sandboxes they own; an admin may use any.

use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use epiphany_core::Sandbox;
use epiphany_engine::ReadSnapshot;
use epiphany_security::Principal;

use crate::auth::AuthPrincipal;
use crate::routes::map_batch_error;
use crate::{ApiError, AppState};

/// The HTTP header naming the sandbox a data request should overlay.
const SANDBOX_HEADER: &str = "x-epiphany-sandbox";

/// The sandbox selected for a data request, from the `X-Epiphany-Sandbox`
/// header. `None` when the header is absent or blank (base behavior, fully
/// back-compatible). Carries only the name; existence and ownership are checked
/// per-request against the cube snapshot by [`resolve_sandbox`].
pub struct SandboxSelector(pub Option<String>);

impl FromRequestParts<AppState> for SandboxSelector {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &AppState) -> Result<Self, ApiError> {
        let name = parts
            .headers
            .get(SANDBOX_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Ok(SandboxSelector(name))
    }
}

/// Find a sandbox in a cube snapshot and authorize the caller as its owner (an
/// admin may access any). A missing sandbox is 404; a foreign one is 403.
fn authorize_sandbox<'a>(
    snap: &'a ReadSnapshot,
    principal: &Principal,
    name: &str,
) -> Result<&'a Sandbox, ApiError> {
    let sb = snap
        .model()
        .sandbox(name)
        .ok_or_else(|| ApiError::not_found(format!("no sandbox '{name}'")))?;
    if !principal.is_admin && sb.owner != principal.username {
        return Err(ApiError::forbidden("not your sandbox"));
    }
    Ok(sb)
}

/// Resolve a data request's [`SandboxSelector`] to the sandbox name to overlay,
/// authorizing ownership. `None` means base (no header). Used by the read,
/// execute, explain, and write paths.
pub(crate) fn resolve_sandbox(
    snap: &ReadSnapshot,
    principal: &Principal,
    selector: &SandboxSelector,
) -> Result<Option<String>, ApiError> {
    match &selector.0 {
        None => Ok(None),
        Some(name) => {
            authorize_sandbox(snap, principal, name)?;
            Ok(Some(name.clone()))
        }
    }
}

/// A sandbox in a list or detail response.
#[derive(Serialize)]
pub(crate) struct SandboxDto {
    name: String,
    owner: String,
    created: u64,
    updated: u64,
    cell_count: usize,
}

fn dto(sb: &Sandbox) -> SandboxDto {
    SandboxDto {
        name: sb.name.clone(),
        owner: sb.owner.clone(),
        created: sb.created,
        updated: sb.updated,
        cell_count: sb.len(),
    }
}

fn snapshot(state: &AppState, cube: &str) -> Result<ReadSnapshot, ApiError> {
    state
        .engine
        .snapshot(cube)
        .ok_or_else(|| ApiError::not_found(format!("unknown cube '{cube}'")))
}

#[derive(Serialize)]
pub(crate) struct SandboxList {
    sandboxes: Vec<SandboxDto>,
}

/// `GET /cubes/{cube}/sandboxes` -> the caller's sandboxes (an admin sees all).
pub(crate) async fn list_sandboxes(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
) -> Result<Json<SandboxList>, ApiError> {
    let snap = snapshot(&state, &cube)?;
    let p = &auth.principal;
    let sandboxes = snap
        .model()
        .sandboxes
        .values()
        .filter(|sb| p.is_admin || sb.owner == p.username)
        .map(dto)
        .collect();
    Ok(Json(SandboxList { sandboxes }))
}

#[derive(Deserialize)]
pub struct CreateSandboxBody {
    name: String,
}

/// `POST /cubes/{cube}/sandboxes` -> create a sandbox owned by the caller.
pub(crate) async fn create_sandbox(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    Json(body): Json<CreateSandboxBody>,
) -> Result<(StatusCode, Json<SandboxDto>), ApiError> {
    let snap = snapshot(&state, &cube)?;
    let name = body.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("sandbox name is required"));
    }
    if snap.model().sandbox(name).is_some() {
        return Err(ApiError::conflict(format!(
            "sandbox '{name}' already exists"
        )));
    }
    state
        .engine
        .create_sandbox(&cube, None, name, &auth.principal.username)
        .map_err(map_batch_error)?;
    let snap = snapshot(&state, &cube)?;
    let sb = snap.model().sandbox(name).ok_or_else(ApiError::internal)?;
    Ok((StatusCode::CREATED, Json(dto(sb))))
}

/// `GET /cubes/{cube}/sandboxes/{name}` -> one sandbox (owner or admin).
pub(crate) async fn get_sandbox(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
) -> Result<Json<SandboxDto>, ApiError> {
    let snap = snapshot(&state, &cube)?;
    let sb = authorize_sandbox(&snap, &auth.principal, &name)?;
    Ok(Json(dto(sb)))
}

/// `DELETE /cubes/{cube}/sandboxes/{name}` -> discard a sandbox (owner or admin).
pub(crate) async fn delete_sandbox(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let snap = snapshot(&state, &cube)?;
    authorize_sandbox(&snap, &auth.principal, &name)?;
    state
        .engine
        .discard_sandbox(&cube, None, &name)
        .map_err(map_batch_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub(crate) struct CommitResponse {
    version: u64,
    committed: usize,
}

/// Optional commit body: the base version the client last synced against, for
/// the optimistic concurrency check. Absent means last-writer-wins.
#[derive(Deserialize)]
pub(crate) struct CommitBody {
    #[serde(default)]
    base_version: Option<u64>,
}

/// `POST /cubes/{cube}/sandboxes/{name}/commit` -> merge the sandbox's what-if
/// values into base (owner or admin), clearing the deltas. An optional
/// `base_version` enables the optimistic check: if base moved past it, the
/// commit conflicts (409) and base is unchanged (ADR-0014).
pub(crate) async fn commit_sandbox(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
    body: Option<Json<CommitBody>>,
) -> Result<Json<CommitResponse>, ApiError> {
    let snap = snapshot(&state, &cube)?;
    let committed = authorize_sandbox(&snap, &auth.principal, &name)?.len();
    let base = body.and_then(|Json(b)| b.base_version);
    let outcome = state
        .engine
        .commit_sandbox(&cube, base, &name)
        .map_err(map_batch_error)?;
    Ok(Json(CommitResponse {
        version: outcome.version,
        committed,
    }))
}
