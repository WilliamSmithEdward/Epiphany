//! Connection endpoints: CRUD over a cube's data-source connections. Reading a
//! connection requires `Connection:Read`; defining or deleting one requires
//! `Connection:Write` (ADR-0023), and a command (process-execution) connection
//! additionally requires the server to have opted in (ADR-0012 decision 6): two
//! independent gates before the host can ever run a program.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use epiphany_core::{CommandSpec, Connection, ConnectionSpec, SourceFormat};
use epiphany_security::{AccessLevel, AuditAction, ObjectKind, ObjectRef};

use crate::auth::AuthPrincipal;
use crate::authz::{audit, require_kind_access};
use crate::routes::{map_batch_error, snapshot};
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
    /// Optional working directory the program runs in (must be an absolute path
    /// with no `..` traversal). Redacted for non-admins like the command line.
    #[serde(default)]
    pub working_dir: Option<String>,
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
            working_dir: if is_admin {
                cmd.working_dir.clone()
            } else {
                None
            },
        },
    }
}

/// Validate an optional connector working directory (ADR-0012 addendum): if
/// present it must be an absolute path with no `..` traversal component.
/// Fail-closed: a relative path or a traversal is rejected (422). Validation is
/// lexical (not canonicalized); a `..`-free absolute path can still resolve
/// through a symlink, which is acceptable since the program/args are already
/// admin-defined arbitrary code (ADR-0012 decision 6).
fn validate_working_dir(dir: &Option<String>) -> Result<(), ApiError> {
    let Some(dir) = dir.as_deref().filter(|d| !d.is_empty()) else {
        return Ok(());
    };
    let path = std::path::Path::new(dir);
    if !path.is_absolute() {
        return Err(ApiError::unprocessable(
            "INVALID_WORKING_DIR",
            "working_dir must be an absolute path",
        ));
    }
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(ApiError::unprocessable(
            "INVALID_WORKING_DIR",
            "working_dir must not contain a '..' traversal",
        ));
    }
    Ok(())
}

/// `GET /cubes/{cube}/connections` -> the cube's connections (command lines
/// redacted for non-admins).
pub(crate) async fn list_connections(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
) -> Result<Json<ConnectionListDto>, ApiError> {
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Connection,
        Some(&cube),
        AccessLevel::Read,
    )?;
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
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Connection,
        Some(&cube),
        AccessLevel::Read,
    )?;
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
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Connection,
        Some(&cube),
        AccessLevel::Write,
    )?;
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
    validate_working_dir(&body.working_dir)?;
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
        working_dir: body.working_dir.as_ref().filter(|d| !d.is_empty()).cloned(),
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
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Connection,
        Some(&cube),
        AccessLevel::Write,
    )?;
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

/// The first rows a connection emits, for the wizard's "Test connection" button.
#[derive(Serialize)]
pub(crate) struct ConnectionPreviewDto {
    /// Column names, taken from the first row's keys (in order).
    columns: Vec<String>,
    /// Up to `PREVIEW_ROW_LIMIT` sample rows, each aligned to `columns`.
    rows: Vec<Vec<String>>,
    /// Total rows the connection produced (may exceed the sample shown).
    row_count: usize,
}

/// How many rows the preview returns; the rest are counted but not sent.
const PREVIEW_ROW_LIMIT: usize = 20;

/// `POST /cubes/{cube}/connections/{name}/preview` -> run the connection and
/// return a small sample of its parsed rows (ADR-0027). Requires `Connection:Write`
/// and the command-connector enable flag; never stages a model change.
pub(crate) async fn preview_connection(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
) -> Result<Json<ConnectionPreviewDto>, ApiError> {
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Connection,
        Some(&cube),
        AccessLevel::Write,
    )?;
    // Running a program is host code execution: require the server opt-in.
    if !state.command_connectors_enabled {
        return Err(ApiError::forbidden(
            "command connectors are disabled on this server (set EPIPHANY_ENABLE_COMMAND_CONNECTORS)",
        ));
    }
    let cmd = {
        let snap = snapshot(&state, &cube)?;
        let conn = snap
            .model()
            .connections
            .get(&name)
            .ok_or_else(|| ApiError::not_found(format!("unknown connection '{name}'")))?;
        let ConnectionSpec::Command(cmd) = &conn.spec;
        cmd.clone()
    };
    let rows = epiphany_connect::run_command(&cmd)
        .map_err(|e| ApiError::unprocessable("CONNECTOR_ERROR", e.to_string()))?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectUpdate,
        Some(&connection_ref(&cube, &name)),
        true,
    );
    let columns: Vec<String> = rows
        .first()
        .map(|r| r.iter().map(|(k, _)| k.clone()).collect())
        .unwrap_or_default();
    let sample: Vec<Vec<String>> = rows
        .iter()
        .take(PREVIEW_ROW_LIMIT)
        .map(|r| r.iter().map(|(_, v)| v.clone()).collect())
        .collect();
    Ok(Json(ConnectionPreviewDto {
        columns,
        rows: sample,
        row_count: rows.len(),
    }))
}
