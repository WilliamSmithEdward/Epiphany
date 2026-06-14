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

use crate::auth::AuthPrincipal;
use crate::routes::map_batch_error;
use crate::ws::ChangeEvent;
use crate::{ApiError, AppState};

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

fn to_dto(conn: &Connection) -> ConnectionDto {
    match &conn.spec {
        ConnectionSpec::Command(cmd) => ConnectionDto {
            name: conn.name.clone(),
            kind: "command".to_string(),
            program: cmd.program.clone(),
            args: cmd.args.clone(),
            format: match cmd.format {
                SourceFormat::Csv => "csv".to_string(),
                SourceFormat::Json => "json".to_string(),
            },
            json_path: cmd.json_path.clone(),
            timeout_ms: cmd.timeout_ms,
        },
    }
}

fn require_admin(auth: &AuthPrincipal) -> Result<(), ApiError> {
    if auth.principal.is_admin {
        Ok(())
    } else {
        Err(ApiError::forbidden(
            "defining connections requires an administrator",
        ))
    }
}

fn snapshot(state: &AppState, cube: &str) -> Result<epiphany_engine::ReadSnapshot, ApiError> {
    state
        .engine
        .snapshot(cube)
        .ok_or_else(|| ApiError::not_found(format!("unknown cube '{cube}'")))
}

/// `GET /cubes/{cube}/connections` -> the cube's connections.
pub(crate) async fn list_connections(
    _auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
) -> Result<Json<ConnectionListDto>, ApiError> {
    let snap = snapshot(&state, &cube)?;
    Ok(Json(ConnectionListDto {
        connections: snap.model().connections.values().map(to_dto).collect(),
    }))
}

/// `GET /cubes/{cube}/connections/{name}` -> one connection.
pub(crate) async fn get_connection(
    _auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
) -> Result<Json<ConnectionDto>, ApiError> {
    let snap = snapshot(&state, &cube)?;
    let conn = snap
        .model()
        .connections
        .get(&name)
        .ok_or_else(|| ApiError::not_found(format!("unknown connection '{name}'")))?;
    Ok(Json(to_dto(conn)))
}

/// `PUT /cubes/{cube}/connections/{name}` -> define a connection (admin only).
pub(crate) async fn put_connection(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
    Json(body): Json<ConnectionDto>,
) -> Result<Json<ConnectionDto>, ApiError> {
    require_admin(&auth)?;
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
    let spec = CommandSpec {
        program: body.program.clone(),
        args: body.args.clone(),
        format: if body.format == "json" {
            SourceFormat::Json
        } else {
            SourceFormat::Csv
        },
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
    let response = to_dto(&connection);
    let outcome = state
        .engine
        .define_connection(&cube, None, connection)
        .map_err(map_batch_error)?;
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
    require_admin(&auth)?;
    let outcome = state
        .engine
        .delete_connection(&cube, None, &name)
        .map_err(map_batch_error)?;
    let _ = state.events.send(ChangeEvent::ObjectsChanged {
        cube: cube.clone(),
        version: outcome.version,
    });
    Ok(StatusCode::NO_CONTENT)
}
