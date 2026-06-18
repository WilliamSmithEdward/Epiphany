//! Connection endpoints: CRUD over the server-global data-source connections
//! (ADR-0035). Reading a connection requires `Connection:Read`; defining or
//! deleting one requires `Connection:Write` (ADR-0023), and a command
//! (process-execution) connection additionally requires the server to have opted
//! in (ADR-0012 decision 6): two independent gates before the host can ever run a
//! program. Connections live in the global automation store, not a cube model.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use epiphany_core::{
    CommandSpec, Connection, ConnectionSpec, HttpAuth, HttpAuthKind, HttpSpec, SqlEngine, SqlSpec,
    SqlSslMode,
};
use epiphany_flow::Row;
use epiphany_security::{AccessLevel, AuditAction, ObjectKind, ObjectRef};

use crate::auth::AuthPrincipal;
use crate::authz::{audit, require_kind_access};
use crate::http_connector::{
    format_token, parse_format, require_http_host_allowed, require_sql_host_allowed,
    resolve_auth_header,
};
use crate::routes::map_persist_error;
use crate::{ApiError, AppState};

/// The securable reference for a global connection (ADR-0035).
fn connection_ref(name: &str) -> ObjectRef {
    ObjectRef::global(ObjectKind::Connection, name)
}

/// A connection in JSON form (flat). The command fields apply when `kind` is
/// `"command"`; the http fields when `kind` is `"http"` (ADR-0030).
#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct ConnectionDto {
    /// Ignored on a request (the name comes from the path); always set on a
    /// response.
    #[serde(default)]
    pub name: String,
    pub kind: String,
    // ---- command fields ----
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
    // ---- http fields (ADR-0030) ----
    /// The URL to fetch. Redacted for non-admins (it may embed a token).
    #[serde(default)]
    pub url: String,
    /// Static request headers. Redacted for non-admins (may embed a token).
    #[serde(default)]
    pub headers: Vec<HeaderDto>,
    /// Optional credential, referencing a secret by NAME (never a value).
    #[serde(default)]
    pub auth: Option<AuthDto>,
    // ---- sql fields (ADR-0034) ----
    /// The database engine ("postgres"). Defaults to postgres when blank.
    #[serde(default)]
    pub engine: String,
    /// The database host. Redacted for non-admins.
    #[serde(default)]
    pub host: String,
    /// The database port.
    #[serde(default)]
    pub port: u16,
    /// The database (catalog) name. Redacted for non-admins.
    #[serde(default)]
    pub database: String,
    /// The connecting user. Redacted for non-admins.
    #[serde(default)]
    pub user: String,
    /// The NAME of the secret holding the password (never the value).
    #[serde(default)]
    pub password_secret: Option<String>,
    /// The SQL query to run. Redacted for non-admins (it is operator-authored).
    #[serde(default)]
    pub query: String,
    /// TLS mode: "verify-full" (default), "require", or "disable".
    #[serde(default)]
    pub ssl_mode: String,
}

/// One static HTTP request header.
#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct HeaderDto {
    pub name: String,
    pub value: String,
}

/// An HTTP credential reference: scheme plus the NAME of the secret holding the
/// value (a token, or `user:password` for basic). The value is never in the DTO.
#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct AuthDto {
    pub kind: String,
    pub secret: String,
}

#[derive(Serialize)]
pub(crate) struct ConnectionListDto {
    pub connections: Vec<ConnectionDto>,
}

fn auth_kind_token(kind: HttpAuthKind) -> &'static str {
    match kind {
        HttpAuthKind::Bearer => "bearer",
        HttpAuthKind::Basic => "basic",
    }
}

fn auth_dto(auth: &HttpAuth) -> AuthDto {
    AuthDto {
        kind: auth_kind_token(auth.kind).to_string(),
        secret: auth.secret.clone(),
    }
}

fn sql_engine_token(engine: SqlEngine) -> &'static str {
    match engine {
        SqlEngine::Postgres => "postgres",
        SqlEngine::MySql => "mysql",
    }
}

fn parse_sql_engine(token: &str) -> Result<SqlEngine, ApiError> {
    match token {
        "postgres" | "postgresql" | "" => Ok(SqlEngine::Postgres),
        "mysql" | "mariadb" => Ok(SqlEngine::MySql),
        other => Err(ApiError::bad_request(format!(
            "unsupported SQL engine '{other}' (expected 'postgres' or 'mysql')"
        ))),
    }
}

fn ssl_mode_token(mode: SqlSslMode) -> &'static str {
    match mode {
        SqlSslMode::VerifyFull => "verify-full",
        SqlSslMode::Require => "require",
        SqlSslMode::Disable => "disable",
    }
}

fn parse_ssl_mode(token: &str) -> Result<SqlSslMode, ApiError> {
    match token {
        "verify-full" | "" => Ok(SqlSslMode::VerifyFull),
        "require" => Ok(SqlSslMode::Require),
        "disable" => Ok(SqlSslMode::Disable),
        other => Err(ApiError::bad_request(format!(
            "unknown ssl_mode '{other}' (expected 'verify-full', 'require', or 'disable')"
        ))),
    }
}

/// Render a connection as a DTO. For a non-admin the operator-authored detail
/// (the command line, or the URL and headers) is redacted, since it may embed
/// sensitive paths or tokens; the name/kind/format (and the credential's secret
/// NAME, which is not itself sensitive) remain so a modeler can still pick the
/// connection as a flow source.
fn to_dto(conn: &Connection, is_admin: bool) -> ConnectionDto {
    let base = ConnectionDto {
        name: conn.name.clone(),
        kind: String::new(),
        program: String::new(),
        args: Vec::new(),
        format: String::new(),
        json_path: None,
        timeout_ms: 0,
        working_dir: None,
        url: String::new(),
        headers: Vec::new(),
        auth: None,
        engine: String::new(),
        host: String::new(),
        port: 0,
        database: String::new(),
        user: String::new(),
        password_secret: None,
        query: String::new(),
        ssl_mode: String::new(),
    };
    match &conn.spec {
        ConnectionSpec::Command(cmd) => ConnectionDto {
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
            format: format_token(cmd.format).to_string(),
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
            ..base
        },
        ConnectionSpec::Http(http) => ConnectionDto {
            kind: "http".to_string(),
            format: format_token(http.format).to_string(),
            json_path: http.json_path.clone(),
            timeout_ms: http.timeout_ms,
            url: if is_admin {
                http.url.clone()
            } else {
                String::new()
            },
            headers: if is_admin {
                http.headers
                    .iter()
                    .map(|(name, value)| HeaderDto {
                        name: name.clone(),
                        value: value.clone(),
                    })
                    .collect()
            } else {
                Vec::new()
            },
            auth: http.auth.as_ref().map(auth_dto),
            ..base
        },
        ConnectionSpec::Sql(sql) => ConnectionDto {
            kind: "sql".to_string(),
            engine: sql_engine_token(sql.engine).to_string(),
            // Connection target + query are operator-authored; redact for a
            // non-admin (the secret NAME, port, and ssl mode are not sensitive).
            host: if is_admin {
                sql.host.clone()
            } else {
                String::new()
            },
            port: sql.port,
            database: if is_admin {
                sql.database.clone()
            } else {
                String::new()
            },
            user: if is_admin {
                sql.user.clone()
            } else {
                String::new()
            },
            password_secret: sql.password_secret.clone(),
            query: if is_admin {
                sql.query.clone()
            } else {
                String::new()
            },
            ssl_mode: ssl_mode_token(sql.ssl_mode).to_string(),
            timeout_ms: sql.timeout_ms,
            ..base
        },
    }
}

/// Render a connection's name + spec as a DTO (ADR-0035): used to echo a flow's
/// embedded (local) connection. Same redaction policy as [`to_dto`].
pub(crate) fn spec_to_dto(name: &str, spec: &ConnectionSpec, is_admin: bool) -> ConnectionDto {
    to_dto(
        &Connection {
            name: name.to_string(),
            spec: spec.clone(),
        },
        is_admin,
    )
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

/// `GET /connections` -> the global connections (command lines redacted for
/// non-admins).
pub(crate) async fn list_connections(
    auth: AuthPrincipal,
    State(state): State<AppState>,
) -> Result<Json<ConnectionListDto>, ApiError> {
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Connection,
        None,
        AccessLevel::Read,
    )?;
    let is_admin = auth.principal.is_admin;
    let store = state.automation.lock().expect("automation store mutex");
    Ok(Json(ConnectionListDto {
        connections: store
            .automation()
            .connections
            .values()
            .map(|c| to_dto(c, is_admin))
            .collect(),
    }))
}

/// `GET /connections/{name}` -> one connection (command line redacted for
/// non-admins).
pub(crate) async fn get_connection(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<ConnectionDto>, ApiError> {
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Connection,
        None,
        AccessLevel::Read,
    )?;
    let store = state.automation.lock().expect("automation store mutex");
    let conn = store
        .automation()
        .connections
        .get(&name)
        .ok_or_else(|| ApiError::not_found(format!("unknown connection '{name}'")))?;
    Ok(Json(to_dto(conn, auth.principal.is_admin)))
}

/// Build a [`ConnectionSpec`] from a DTO, applying the per-kind controls (command
/// opt-in, HTTP/SQL build feature plus enable flag plus host allowlist, and a
/// referenced secret must already exist). Shared by the connection PUT handler and
/// the flow-scoped (local) connection on a flow input (ADR-0035), so a
/// flow-scoped connection obeys the same gates as a global one. `timeout_ms` of 0
/// defaults to 30s so a misconfigured connection cannot hang.
pub(crate) fn spec_from_dto(
    state: &AppState,
    body: &ConnectionDto,
) -> Result<ConnectionSpec, ApiError> {
    // Default an unset timeout to 30s so a misconfigured connection cannot hang.
    let timeout_ms = if body.timeout_ms == 0 {
        30_000
    } else {
        body.timeout_ms
    };
    match body.kind.as_str() {
        "command" => {
            // A command connection is host code execution: require the opt-in.
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
            Ok(ConnectionSpec::Command(CommandSpec {
                program: body.program.clone(),
                args: body.args.clone(),
                format: parse_format(&body.format)?,
                json_path: body.json_path.clone(),
                timeout_ms,
                working_dir: body.working_dir.as_ref().filter(|d| !d.is_empty()).cloned(),
            }))
        }
        "http" => {
            // An HTTP connection fetches an external URL: require the opt-in and
            // an allowlisted host (ADR-0030 SSRF control), and a referenced
            // secret must already exist (fail-closed).
            if !state.http.enabled {
                return Err(ApiError::forbidden(
                    "HTTP connectors are disabled on this server (set EPIPHANY_ENABLE_HTTP_CONNECTORS)",
                ));
            }
            if body.url.trim().is_empty() {
                return Err(ApiError::bad_request("an http connection needs a url"));
            }
            require_http_host_allowed(state, &body.url)?;
            let auth = match &body.auth {
                None => None,
                Some(a) => {
                    let kind = match a.kind.as_str() {
                        "bearer" => HttpAuthKind::Bearer,
                        "basic" => HttpAuthKind::Basic,
                        other => {
                            return Err(ApiError::bad_request(format!(
                                "unknown auth kind '{other}' (expected 'bearer' or 'basic')"
                            )))
                        }
                    };
                    if a.secret.trim().is_empty() {
                        return Err(ApiError::bad_request("auth needs a secret name"));
                    }
                    if !state
                        .secrets
                        .lock()
                        .expect("secret store")
                        .contains(&a.secret)
                    {
                        return Err(ApiError::unprocessable(
                            "UNKNOWN_SECRET",
                            format!("no secret named '{}'", a.secret),
                        ));
                    }
                    Some(HttpAuth {
                        kind,
                        secret: a.secret.clone(),
                    })
                }
            };
            Ok(ConnectionSpec::Http(HttpSpec {
                url: body.url.clone(),
                headers: body
                    .headers
                    .iter()
                    .map(|h| (h.name.clone(), h.value.clone()))
                    .collect(),
                auth,
                format: parse_format(&body.format)?,
                json_path: body.json_path.clone(),
                timeout_ms,
            }))
        }
        "sql" => {
            // A SQL connection queries an external database: require the opt-in
            // and an allowlisted host (ADR-0034), and a referenced password
            // secret must already exist (fail-closed).
            if !state.sql.enabled {
                return Err(ApiError::forbidden(
                    "SQL connectors are disabled on this server (set EPIPHANY_ENABLE_SQL_CONNECTORS)",
                ));
            }
            if body.host.trim().is_empty() {
                return Err(ApiError::bad_request("a sql connection needs a host"));
            }
            if body.database.trim().is_empty() {
                return Err(ApiError::bad_request("a sql connection needs a database"));
            }
            if body.query.trim().is_empty() {
                return Err(ApiError::bad_request("a sql connection needs a query"));
            }
            require_sql_host_allowed(state, &body.host)?;
            let password_secret = match body
                .password_secret
                .as_ref()
                .filter(|s| !s.trim().is_empty())
            {
                None => None,
                Some(name) => {
                    if !state.secrets.lock().expect("secret store").contains(name) {
                        return Err(ApiError::unprocessable(
                            "UNKNOWN_SECRET",
                            format!("no secret named '{name}'"),
                        ));
                    }
                    Some(name.clone())
                }
            };
            Ok(ConnectionSpec::Sql(SqlSpec {
                engine: parse_sql_engine(&body.engine)?,
                host: body.host.clone(),
                port: body.port,
                database: body.database.clone(),
                user: body.user.clone(),
                password_secret,
                query: body.query.clone(),
                ssl_mode: parse_ssl_mode(&body.ssl_mode)?,
                timeout_ms,
            }))
        }
        other => Err(ApiError::bad_request(format!(
            "unsupported connection kind '{other}'"
        ))),
    }
}

/// `PUT /connections/{name}` -> define a global connection (admin or a holder of
/// a global `Connection:Write` grant).
pub(crate) async fn put_connection(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<ConnectionDto>,
) -> Result<Json<ConnectionDto>, ApiError> {
    let obj = connection_ref(&name);
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Connection,
        None,
        AccessLevel::Write,
    )?;
    let spec = spec_from_dto(&state, &body)?;
    let connection = Connection {
        name: name.clone(),
        spec,
    };
    // The echoed DTO is unredacted for the admin/grant-holder who just defined it.
    let response = to_dto(&connection, auth.principal.is_admin);
    state
        .automation
        .lock()
        .expect("automation store mutex")
        .define_connection(connection)
        .map_err(map_persist_error)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectUpdate,
        Some(&obj),
        true,
    );
    Ok(Json(response))
}

/// `DELETE /connections/{name}` -> delete a global connection.
pub(crate) async fn delete_connection(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    let obj = connection_ref(&name);
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Connection,
        None,
        AccessLevel::Write,
    )?;
    let removed = state
        .automation
        .lock()
        .expect("automation store mutex")
        .delete_connection(&name)
        .map_err(map_persist_error)?;
    if !removed {
        return Err(ApiError::not_found(format!("unknown connection '{name}'")));
    }
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectDelete,
        Some(&obj),
        true,
    );
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

/// `POST /connections/{name}/preview` -> run the global connection and return a
/// small sample of its parsed rows (ADR-0027). Requires `Connection:Write` and
/// the per-kind enable flags; never stages a model change.
pub(crate) async fn preview_connection(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<ConnectionPreviewDto>, ApiError> {
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Connection,
        None,
        AccessLevel::Write,
    )?;
    let conn = {
        let store = state.automation.lock().expect("automation store mutex");
        store
            .automation()
            .connections
            .get(&name)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("unknown connection '{name}'")))?
    };
    // Per-kind gates (command opt-in, or HTTP opt-in + host allowlist) live in the
    // shared fetcher, so preview and a flow run apply the same controls.
    let rows = fetch_connection_rows(&state, &conn)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectUpdate,
        Some(&connection_ref(&name)),
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

/// Fetch a connection's rows, applying the per-kind runtime gates. Shared by the
/// preview endpoint and a flow run so both enforce the same controls: a command
/// connection needs the command opt-in; an HTTP connection needs the HTTP opt-in,
/// an allowlisted host, and its credential resolved from the secret store.
pub(crate) fn fetch_connection_rows(
    state: &AppState,
    conn: &Connection,
) -> Result<Vec<Row>, ApiError> {
    match &conn.spec {
        ConnectionSpec::Command(cmd) => {
            if !state.command_connectors_enabled {
                return Err(ApiError::forbidden(
                    "command connectors are disabled on this server (set EPIPHANY_ENABLE_COMMAND_CONNECTORS)",
                ));
            }
            epiphany_connect::run_command(cmd)
                .map_err(|e| ApiError::unprocessable("CONNECTOR_ERROR", e.to_string()))
        }
        ConnectionSpec::Http(http) => fetch_http_rows(state, http),
        ConnectionSpec::Sql(sql) => fetch_sql_rows(state, sql),
    }
}

/// Fetch a SQL connection's rows: gate on the capability and host allowlist,
/// resolve the password from the secret store, then query (the query itself is
/// compiled only with the `postgres` feature; a default build returns 422).
fn fetch_sql_rows(state: &AppState, sql: &SqlSpec) -> Result<Vec<Row>, ApiError> {
    if !state.sql.enabled {
        return Err(ApiError::forbidden(
            "SQL connectors are disabled on this server (set EPIPHANY_ENABLE_SQL_CONNECTORS)",
        ));
    }
    require_sql_host_allowed(state, &sql.host)?;
    let password = match &sql.password_secret {
        None => None,
        Some(name) => {
            let secrets = state.secrets.lock().expect("secret store");
            let value = secrets.get(name).ok_or_else(|| {
                ApiError::unprocessable("UNKNOWN_SECRET", format!("no secret named '{name}'"))
            })?;
            Some(value.to_string())
        }
    };
    #[cfg(any(feature = "postgres", feature = "mysql"))]
    {
        epiphany_connect::fetch_sql(sql, password.as_deref())
            .map_err(|e| ApiError::unprocessable("CONNECTOR_ERROR", e.to_string()))
    }
    #[cfg(not(any(feature = "postgres", feature = "mysql")))]
    {
        let _ = password;
        Err(ApiError::unprocessable(
            "SQL_NOT_BUILT",
            "this server build does not include the SQL connector",
        ))
    }
}

/// Fetch an HTTP connection's rows: gate on the capability and host allowlist,
/// resolve the credential into an `Authorization` header from the secret store,
/// then fetch (the fetch itself is compiled only with the `http` feature).
fn fetch_http_rows(state: &AppState, http: &HttpSpec) -> Result<Vec<Row>, ApiError> {
    if !state.http.enabled {
        return Err(ApiError::forbidden(
            "HTTP connectors are disabled on this server (set EPIPHANY_ENABLE_HTTP_CONNECTORS)",
        ));
    }
    require_http_host_allowed(state, &http.url)?;
    let auth_header = match &http.auth {
        None => None,
        Some(auth) => {
            let secrets = state.secrets.lock().expect("secret store");
            Some(resolve_auth_header(&secrets, auth)?)
        }
    };
    #[cfg(feature = "http")]
    {
        epiphany_connect::fetch_http(http, auth_header.as_deref())
            .map_err(|e| ApiError::unprocessable("CONNECTOR_ERROR", e.to_string()))
    }
    #[cfg(not(feature = "http"))]
    {
        let _ = auth_header;
        Err(ApiError::unprocessable(
            "HTTP_NOT_BUILT",
            "this server build does not include the HTTP connector (build with --features http)",
        ))
    }
}
