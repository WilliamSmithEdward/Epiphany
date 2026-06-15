//! Connection endpoints: CRUD over a cube's admin-defined data-source
//! connections. Defining or deleting a connection requires an admin, and a
//! command (process-execution) connection additionally requires the server to
//! have opted in (ADR-0012 decision 6): two independent gates before the host
//! can ever run a program.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use epiphany_core::{CommandSpec, Connection, ConnectionSpec, SourceFormat};
use epiphany_security::{AccessLevel, AuditAction, ObjectKind, ObjectRef};

use crate::auth::AuthPrincipal;
use crate::authz::{audit, require_access, require_cube_access};
use crate::routes::map_batch_error;
use crate::ws::ChangeEvent;
use crate::{ApiError, AppState};

/// The securable reference for a cube's connection.
fn connection_ref(cube: &str, name: &str) -> ObjectRef {
    ObjectRef::in_cube(ObjectKind::Connection, cube, name)
}

/// A connection in JSON form (flat; the command fields apply when `kind` is
/// `"command"`, the only kind today).
#[derive(Serialize, Deserialize)]
pub(crate) struct ConnectionDto {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub json_path: Option<String>,
    #[serde(default)]
    pub timeout_ms: u64,
}

#[derive(Serialize)]
pub(crate) struct ConnectionListDto {
    pub connections: Vec<ConnectionDto>,
}

/// Render a connection as a DTO. For a non-admin the command line (program,
/// args, json_path) is redacted, since it is an admin artifact that may embed
/// sensitive paths or tokens; the name/kind/format remain so a modeler can still
/// pick the connection as a flow source.
fn to_dto(conn: &Connection, is_admin: bool) -> ConnectionDto {
    match &conn.spec {
        ConnectionSpec::Command(cmd) => ConnectionDto {
            name: conn.name.clone(),
            kind: "command".to_string(),
            program: if is_admin {
                cmd.program.clone()
            } else {
                String::new()
            },
            args: if is_admin {
                cmd.args.clone()
            } else {
                Vec::new()
            },
            format: match cmd.format {
                SourceFormat::Csv => "csv".to_string(),
                SourceFormat::Json => "json".to_string(),
            },
            json_path: if is_admin {
                cmd.json_path.clone()
            } else {
                None
            },
            timeout_ms: cmd.timeout_ms,
        },
    }
}

fn snapshot(state: &AppState, cube: &str) -> Result<epiphany_engine::ReadSnapshot, ApiError> {
    state
        .engine
        .snapshot(cube)
        .ok_or_else(|| ApiError::not_found(format!("unknown cube '{cube}'")))
}

/// `GET /cubes/{cube}/connections` -> the cube's connections (command lines
/// redacted for non-admins).
pub(crate) async fn list_connections(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
) -> Result<Json<ConnectionListDto>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    let snap = snapshot(&state, &cube)?;
    let is_admin = auth.principal.is_admin;
    Ok(Json(ConnectionListDto {
        connections: snap
            .model()
            .connections
            .values()
            .map(|c| to_dto(c, is_admin))
            .collect(),
    }))
}

/// `GET /cubes/{cube}/connections/{name}` -> one connection (command line
/// redacted for non-admins).
pub(crate) async fn get_connection(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
) -> Result<Json<ConnectionDto>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    let snap = snapshot(&state, &cube)?;
    let conn = snap
        .model()
        .connections
        .get(&name)
        .ok_or_else(|| ApiError::not_found(format!("unknown connection '{name}'")))?;
    Ok(Json(to_dto(conn, auth.principal.is_admin)))
}

/// `PUT /cubes/{cube}/connections/{name}` -> define a connection (admin only).
pub(crate) async fn put_connection(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
    Json(body): Json<ConnectionDto>,
) -> Result<Json<ConnectionDto>, ApiError> {
    let obj = connection_ref(&cube, &name);
    require_access(&state, &auth, &obj, AccessLevel::Admin, None, false)?;
    if body.kind != "command" {
        return Err(ApiError::bad_request(format!(
            "unsupported connection kind '{}'",
            body.kind
        )));
    }
    // A command connection is host code execution: require the server opt-in.
    if !state.command_connectors_enabled {
        return Err(ApiError::forbidden(
            "command connectors are disabled on this server (set EPIPHANY_ENABLE_COMMAND_CONNECTORS)",
        ));
    }
    if body.program.trim().is_empty() {
        return Err(ApiError::bad_request(
            "a command connection needs a program",
        ));
    }
    let format = match body.format.as_str() {
        "csv" | "" => SourceFormat::Csv,
        "json" => SourceFormat::Json,
        other => {
            return Err(ApiError::bad_request(format!(
                "unknown output format '{other}' (expected 'csv' or 'json')"
            )))
        }
    };
    let spec = CommandSpec {
        program: body.program.clone(),
        args: body.args.clone(),
        format,
        json_path: body.json_path.clone(),
        // Default to 30s when unset, so a misconfigured connection cannot hang.
        timeout_ms: if body.timeout_ms == 0 {
            30_000
        } else {
            body.timeout_ms
        },
    };
    let connection = Connection {
        name: name.clone(),
        spec: ConnectionSpec::Command(spec),
    };
    // The caller is an admin (checked above), so the echoed DTO is unredacted.
    let response = to_dto(&connection, true);
    let outcome = state
        .engine
        .define_connection(&cube, None, connection)
        .map_err(map_batch_error)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectUpdate,
        Some(&obj),
        true,
    );
    let _ = state.events.send(ChangeEvent::ObjectsChanged {
        cube: cube.clone(),
        version: outcome.version,
    });
    Ok(Json(response))
}

/// `DELETE /cubes/{cube}/connections/{name}` -> delete a connection (admin only).
pub(crate) async fn delete_connection(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let obj = connection_ref(&cube, &name);
    require_access(&state, &auth, &obj, AccessLevel::Admin, None, false)?;
    let outcome = state
        .engine
        .delete_connection(&cube, None, &name)
        .map_err(map_batch_error)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectDelete,
        Some(&obj),
        true,
    );
    let _ = state.events.send(ChangeEvent::ObjectsChanged {
        cube: cube.clone(),
        version: outcome.version,
    });
    Ok(StatusCode::NO_CONTENT)
}
