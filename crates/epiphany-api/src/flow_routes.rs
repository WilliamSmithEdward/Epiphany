//! Flow endpoints (ADR-0035): CRUD over the server-global flows and flow tests,
//! flow preview (strip + parse validation), running a flow over its declared and
//! ad-hoc inputs against any mix of cubes and global dimensions, a cube-scoped
//! guided CSV import, and running the flow test suite. All AuthPrincipal-gated.
//! Authoring is gated by the global `Flow:Write` grant; a run is never a
//! privilege-escalation path, so every staged effect is re-authorized against the
//! running principal's object and element security per target before it is
//! applied. A flow's staged outcome is applied through the engine per target cube
//! (elements/edges first, then cells) and per target global dimension
//! (`grow_dimension`).

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use epiphany_core::{
    Connection, ConnectionSpec, ElementKind, ElementSpec, Flow, FlowInput, FlowInputBinding,
    FlowTest,
};
use epiphany_engine::CellWrite;
use epiphany_flow::FlowTestError;
use epiphany_flow::{
    parse_csv, run_flow, run_flow_tests, validate_flow, CubeChanges, FlowError, FlowOutcome,
    PlannedCell, Row,
};
use epiphany_security::{AccessLevel, AuditAction, ObjectKind, ObjectRef};

use crate::auth::AuthPrincipal;
use crate::authz::{audit, require_cube_access, require_element_write, require_kind_access};
use crate::connection_routes::{fetch_connection_rows, spec_from_dto, ConnectionDto};
use crate::dto::{from_cell, to_cell, FailureDto, TestCellDto, TestOutcomeDto, TestReportDto};
use crate::flow_reader::ApiFlowReader;
use crate::routes::{broadcast, build_write, map_batch_error, map_persist_error};
use crate::{ApiError, AppState};

/// Map a flow run/validate failure to the API envelope, attaching line/column
/// for a type-strip error.
fn map_flow_error(err: FlowError) -> ApiError {
    match err {
        FlowError::Strip(e) => ApiError::unprocessable("FLOW_STRIP_ERROR", e.message.clone())
            .with_details(json!({ "line": e.line, "column": e.column })),
        FlowError::Runtime { message } => ApiError::unprocessable("FLOW_RUNTIME_ERROR", message),
    }
}

/// Apply a flow's multi-target staged outcome through the engine (ADR-0035). For
/// each target cube, schema growth is pre-validated against a clone (and every
/// cell resolved against it) before any commit, then elements/edges are committed
/// (so new members exist) and the cell batch applied. For each target global
/// dimension, the registry dimension is grown (which fans out to its cubes).
/// Returns `(elements_added, cells_written)` aggregated across all targets. Each
/// per-cube write and each dimension grow is transactional; across targets it is
/// sequential after the per-cube pre-validation pass (cross-target atomicity is a
/// documented future item, ADR-0035 decision 3).
pub(crate) fn apply_outcome(
    state: &AppState,
    outcome: &FlowOutcome,
) -> Result<(usize, usize), ApiError> {
    let mut elements_added = 0usize;
    let mut cells_written = 0usize;

    // ---- per-global-dimension growth (applied first) ----
    // Grow global dimensions before cube cell writes so a flow can add a member to
    // a global dimension and, in the same run, write a cube cell at that member:
    // grow_dimension fans the member out to every referencing cube, and the
    // per-cube snapshot taken below then sees it (otherwise the cell would fail to
    // resolve, an order-dependent surprise).
    for (dim, changes) in &outcome.dimensions {
        let id = state.engine.dimension_id_by_name(dim).ok_or_else(|| {
            ApiError::unprocessable(
                "FLOW_UNKNOWN_TARGET",
                format!("flow targets unknown global dimension '{dim}'"),
            )
        })?;
        if !changes.elements.is_empty() || !changes.edges.is_empty() {
            let _ = state
                .engine
                .grow_dimension(id, &changes.elements, &changes.edges)
                .map_err(map_batch_error)?;
            // grow_dimension does not report a per-call count, so count the staged
            // new members (idempotent; an exact delta is a future refinement).
            elements_added += changes.elements.len();
        }
    }

    // ---- per-cube writes ----
    for (cube, changes) in &outcome.cubes {
        // A snapshot for an unknown target cube is a 422 (the flow named a cube
        // that does not exist), distinct from the 404 of an unknown route cube.
        // Taken after dimension growth so any fanned-out members are visible.
        let snap = state.engine.snapshot(cube).ok_or_else(|| {
            ApiError::unprocessable(
                "FLOW_UNKNOWN_TARGET",
                format!("flow targets unknown cube '{cube}'"),
            )
        })?;
        // Stage the schema growth on a clone and resolve every cell against it, so a
        // resolution failure surfaces before any commit.
        let mut preview = snap.cube().clone();
        preview
            .extend_schema(&changes.elements, &changes.edges)
            .map_err(|e| ApiError::unprocessable("FLOW_SCHEMA_ERROR", e.to_string()))?;
        let writes: Vec<CellWrite> = changes
            .cells
            .iter()
            .map(|cell| build_write(&preview, &cell.coord, &cell.value))
            .collect::<Result<_, _>>()?;

        // All valid for this cube: commit elements first, then the cell batch.
        if !changes.elements.is_empty() || !changes.edges.is_empty() {
            let (_, added) = state
                .engine
                .define_elements(cube, None, &changes.elements, &changes.edges)
                .map_err(map_batch_error)?;
            elements_added += added;
        }
        if !writes.is_empty() {
            cells_written += writes.len();
            state
                .engine
                .apply_batch(cube, None, &writes)
                .map_err(map_batch_error)?;
        }
    }

    Ok((elements_added, cells_written))
}

/// Authorize a flow/import outcome AS THE RUNNER (ADR-0023 + ADR-0035): a flow is
/// never a privilege-escalation path, so every effect it stages must be something
/// the runner could do directly. For each target cube, structure changes (new
/// elements/edges) require `Dimension:Write` on the cube; cell writes require
/// `Cube:Write` and that every target cell is element-writable by the runner. For
/// each target global dimension, growth requires the global `Dimension:Write`.
/// Holding only `Flow:Write` lets a user author and launch flows, but never edit a
/// cube or dimension they lack access to.
pub(crate) fn authorize_outcome(
    state: &AppState,
    auth: &AuthPrincipal,
    outcome: &FlowOutcome,
) -> Result<(), ApiError> {
    for (cube, changes) in &outcome.cubes {
        if !changes.elements.is_empty() || !changes.edges.is_empty() {
            require_kind_access(
                state,
                auth,
                ObjectKind::Dimension,
                Some(cube),
                AccessLevel::Write,
            )?;
        }
        if !changes.cells.is_empty() {
            // Cell writes use the same gate as the direct cell-write endpoint (the
            // cube-level grant), plus per-cell element access, so a flow can write
            // exactly what the runner could write by hand.
            require_cube_access(state, auth, cube, AccessLevel::Write)?;
            for cell in &changes.cells {
                require_element_write(state, auth, cube, &cell.coord)?;
            }
        }
    }
    // Growing a global dimension is gated by the global `Dimension:Write` grant.
    for _dim in outcome.dimensions.keys() {
        if !outcome.dimensions.is_empty() {
            require_kind_access(state, auth, ObjectKind::Dimension, None, AccessLevel::Write)?;
        }
    }
    Ok(())
}

/// As [`authorize_outcome`], but for an arbitrary principal by username (ADR-0035):
/// a scheduled run executes as the flow's recorded owner, so its effects are gated
/// by the owner's rights, re-resolved from the live store. Fail-closed: an unknown
/// owner has no access.
pub(crate) fn authorize_outcome_as(
    state: &AppState,
    username: &str,
    outcome: &FlowOutcome,
) -> Result<(), ApiError> {
    let auth = AuthPrincipal::synthetic(username);
    authorize_outcome(state, &auth, outcome)
}

// ---- flow inputs ----

/// Resolve a flow's inputs to a `{address: rows}` map (ADR-0035). Each declared
/// input is fetched: a `Global` binding reads the named global connection; a
/// `Local` binding reads its embedded connection. Ad-hoc `inline` content (parsed
/// as CSV, keyed by address) is merged on top, and a legacy single source
/// (`legacy_single`, when non-empty) is keyed under the flow's first declared
/// source address (or `"data"`). All fetches go through the shared connection
/// fetcher, so a flow-scoped connection obeys the same connector controls as a
/// global one.
pub(crate) fn resolve_flow_inputs(
    state: &AppState,
    flow: &Flow,
    inline: &BTreeMap<String, String>,
    legacy_single: &str,
) -> Result<BTreeMap<String, Vec<Row>>, ApiError> {
    let mut inputs: BTreeMap<String, Vec<Row>> = BTreeMap::new();

    // (a) the flow's declared inputs, fetched from their connections.
    for input in &flow.inputs {
        let rows = match &input.binding {
            FlowInputBinding::Global => {
                let conn = {
                    let store = state.automation.lock().expect("automation store mutex");
                    store
                        .automation()
                        .connections
                        .get(&input.name)
                        .cloned()
                        .ok_or_else(|| {
                            ApiError::unprocessable(
                                "UNKNOWN_CONNECTION",
                                format!(
                                    "flow input references unknown connection '{}'",
                                    input.name
                                ),
                            )
                        })?
                };
                fetch_connection_rows(state, &conn)?
            }
            FlowInputBinding::Local(spec) => {
                let conn = Connection {
                    name: input.name.clone(),
                    spec: spec.clone(),
                };
                fetch_connection_rows(state, &conn)?
            }
        };
        inputs.insert(input.address(), rows);
    }

    // (b) ad-hoc inline sources from the run body, parsed as CSV, by address.
    for (address, content) in inline {
        let rows = parse_csv(content)
            .map_err(|e| ApiError::unprocessable("FLOW_INPUT_ERROR", e.to_string()))?;
        inputs.insert(address.clone(), rows);
    }

    // (c) the legacy single source, keyed under the flow's first declared source
    // address (or "data"), so a single-source flow's `ctx.input()` still resolves.
    if !legacy_single.is_empty() {
        let key = flow
            .inputs
            .first()
            .map(|i| i.address())
            .unwrap_or_else(|| "data".to_string());
        let rows = parse_csv(legacy_single)
            .map_err(|e| ApiError::unprocessable("FLOW_INPUT_ERROR", e.to_string()))?;
        inputs.insert(key, rows);
    }

    Ok(inputs)
}

// ---- flow DTOs ----

/// A flow's data input in JSON form: a named source bound either to a global
/// connection (`scope: "global"`, locked to the connection name) or to an inline
/// flow-scoped connection (`scope: "local"`, carrying the connection definition).
#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct FlowInputDto {
    pub name: String,
    /// `"global"` (reference a global connection by name) or `"local"` (inline).
    pub scope: String,
    /// The embedded connection definition for a `"local"` scope; ignored for
    /// `"global"`.
    #[serde(default)]
    pub connection: Option<ConnectionDto>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct FlowDto {
    /// Ignored on a `PUT` (the name comes from the path); always set on responses.
    /// Optional in a request body for parity with the connection/schedule DTOs.
    #[serde(default)]
    pub name: String,
    pub source: String,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub default_cube: Option<String>,
    #[serde(default)]
    pub inputs: Vec<FlowInputDto>,
}

#[derive(Serialize)]
pub(crate) struct FlowListDto {
    pub flows: Vec<FlowDto>,
}

/// Render a flow input as its DTO. A local connection's target (command line,
/// URL, SQL host/query) is redacted for a non-admin, matching the global
/// connection surface: a `Flow:Read` holder is not necessarily a connection
/// admin, so a flow-scoped connector's target is not echoed in full to them. The
/// referenced secret name (never a value) is not sensitive.
fn input_dto(input: &FlowInput, is_admin: bool) -> FlowInputDto {
    match &input.binding {
        FlowInputBinding::Global => FlowInputDto {
            name: input.name.clone(),
            scope: "global".to_string(),
            connection: None,
        },
        FlowInputBinding::Local(spec) => FlowInputDto {
            name: input.name.clone(),
            scope: "local".to_string(),
            connection: Some(crate::connection_routes::spec_to_dto(
                &input.name,
                spec,
                is_admin,
            )),
        },
    }
}

fn flow_dto(flow: &Flow, is_admin: bool) -> FlowDto {
    FlowDto {
        name: flow.name.clone(),
        source: flow.source.clone(),
        owner: flow.owner.clone(),
        default_cube: flow.default_cube.clone(),
        inputs: flow.inputs.iter().map(|i| input_dto(i, is_admin)).collect(),
    }
}

/// Build the core [`FlowInput`] list from the DTOs, parsing each local connection
/// through the shared connector-gated parser (ADR-0035).
fn inputs_from_dtos(state: &AppState, dtos: &[FlowInputDto]) -> Result<Vec<FlowInput>, ApiError> {
    let mut inputs = Vec::with_capacity(dtos.len());
    for dto in dtos {
        let binding = match dto.scope.as_str() {
            "global" => FlowInputBinding::Global,
            "local" => {
                let conn = dto.connection.as_ref().ok_or_else(|| {
                    ApiError::bad_request(format!(
                        "flow input '{}' is local but carries no connection",
                        dto.name
                    ))
                })?;
                let spec: ConnectionSpec = spec_from_dto(state, conn)?;
                FlowInputBinding::Local(spec)
            }
            other => {
                return Err(ApiError::bad_request(format!(
                    "unknown flow input scope '{other}' (expected 'global' or 'local')"
                )))
            }
        };
        inputs.push(FlowInput {
            name: dto.name.clone(),
            binding,
        });
    }
    Ok(inputs)
}

// ---- flow CRUD ----

/// `GET /flows` -> the global flows.
pub(crate) async fn list_flows(
    auth: AuthPrincipal,
    State(state): State<AppState>,
) -> Result<Json<FlowListDto>, ApiError> {
    require_kind_access(&state, &auth, ObjectKind::Flow, None, AccessLevel::Read)?;
    let is_admin = auth.principal.is_admin;
    let store = state.automation.lock().expect("automation store mutex");
    Ok(Json(FlowListDto {
        flows: store
            .automation()
            .flows
            .values()
            .map(|f| flow_dto(f, is_admin))
            .collect(),
    }))
}

/// `GET /flows/{name}` -> one flow.
pub(crate) async fn get_flow(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<FlowDto>, ApiError> {
    require_kind_access(&state, &auth, ObjectKind::Flow, None, AccessLevel::Read)?;
    let store = state.automation.lock().expect("automation store mutex");
    let flow = store
        .automation()
        .flows
        .get(&name)
        .ok_or_else(|| ApiError::not_found(format!("unknown flow '{name}'")))?;
    Ok(Json(flow_dto(flow, auth.principal.is_admin)))
}

/// `PUT /flows/{name}` -> validate and store a global flow. The owner is set to
/// the caller on first authoring and preserved on replace, unless the caller is an
/// admin overriding it.
pub(crate) async fn put_flow(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<FlowDto>,
) -> Result<Json<FlowDto>, ApiError> {
    require_kind_access(&state, &auth, ObjectKind::Flow, None, AccessLevel::Write)?;
    // Validate (strip + parse) before persisting; a bad flow is never stored.
    if body.source.trim().is_empty() {
        return Err(ApiError::unprocessable(
            "FLOW_EMPTY",
            "flow source is empty",
        ));
    }
    validate_flow(&body.source).map_err(map_flow_error)?;
    let inputs = inputs_from_dtos(&state, &body.inputs)?;

    // Resolve the owner: preserve the existing owner on replace; on first authoring
    // (or an explicit admin override) stamp the requested or calling owner. A
    // non-admin may never set the owner to someone else (fail-closed).
    let flow = {
        let mut store = state.automation.lock().expect("automation store mutex");
        let existing_owner = store
            .automation()
            .flows
            .get(&name)
            .and_then(|f| f.owner.clone());
        let owner = if auth.principal.is_admin {
            // An admin may set or override the owner; default to the existing owner,
            // then the request body, then the admin themselves.
            body.owner
                .clone()
                .or(existing_owner)
                .or_else(|| Some(auth.principal.username.clone()))
        } else {
            // A non-admin keeps the existing owner, or becomes the owner on first
            // authoring; they cannot reassign it.
            existing_owner.or_else(|| Some(auth.principal.username.clone()))
        };
        let flow = Flow {
            name: name.clone(),
            source: body.source.clone(),
            owner,
            default_cube: body.default_cube.clone(),
            inputs,
        };
        store.define_flow(flow.clone()).map_err(map_persist_error)?;
        flow
    };
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectUpdate,
        Some(&ObjectRef::global(ObjectKind::Flow, &name)),
        true,
    );
    // Echo the flow the caller just authored at full detail: it is their own
    // submission, so this is never a cross-principal disclosure.
    Ok(Json(flow_dto(&flow, true)))
}

/// `DELETE /flows/{name}` -> delete a flow.
pub(crate) async fn delete_flow(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_kind_access(&state, &auth, ObjectKind::Flow, None, AccessLevel::Write)?;
    let removed = state
        .automation
        .lock()
        .expect("automation store mutex")
        .delete_flow(&name)
        .map_err(map_persist_error)?;
    if !removed {
        return Err(ApiError::not_found(format!("unknown flow '{name}'")));
    }
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectDelete,
        Some(&ObjectRef::global(ObjectKind::Flow, &name)),
        true,
    );
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize, Deserialize)]
pub(crate) struct PreviewBody {
    pub source: String,
}

#[derive(Serialize)]
pub(crate) struct PreviewResult {
    pub ok: bool,
}

/// `POST /flows/preview` -> validate a flow source without saving.
pub(crate) async fn preview_flow(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Json(body): Json<PreviewBody>,
) -> Result<Json<PreviewResult>, ApiError> {
    require_kind_access(&state, &auth, ObjectKind::Flow, None, AccessLevel::Read)?;
    validate_flow(&body.source).map_err(map_flow_error)?;
    Ok(Json(PreviewResult { ok: true }))
}

// ---- running a flow ----

#[derive(Deserialize)]
pub(crate) struct RunBody {
    /// Inline data-source content (CSV text) for the legacy single source. Used
    /// when `connection` is unset; empty for a source-less flow.
    #[serde(default)]
    pub input: String,
    /// The name of a configured global connection to fetch the legacy single
    /// source's rows from. When set, it supplies the rows instead of `input`.
    #[serde(default)]
    pub connection: Option<String>,
    /// Ad-hoc inline content for named sources (ADR-0035): address -> CSV text,
    /// merged over the flow's declared inputs.
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
    /// Flow parameters.
    #[serde(default)]
    pub params: BTreeMap<String, String>,
}

#[derive(Serialize)]
pub(crate) struct RunReport {
    pub rows_read: usize,
    pub cells_written: usize,
    pub elements_added: usize,
    pub logs: Vec<String>,
}

/// `POST /flows/{name}/run` -> run a stored flow over its declared and ad-hoc
/// inputs, fanning its staged outcome across the cubes and dimensions its body
/// names. The flow runs as the caller; its effects are authorized against the
/// caller's rights before they are applied (ADR-0023/0035).
pub(crate) async fn run_flow_handler(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<RunBody>,
) -> Result<Json<RunReport>, ApiError> {
    require_kind_access(&state, &auth, ObjectKind::Flow, None, AccessLevel::Write)?;
    // Resolve the flow from the global automation store.
    let flow = {
        let store = state.automation.lock().expect("automation store mutex");
        store
            .automation()
            .flows
            .get(&name)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("unknown flow '{name}'")))?
    };

    // The legacy single-source body (`input`/`connection`) is only for a flow with
    // no declared inputs. With declared inputs, the run fetches them and an ad-hoc
    // override goes through `inputs` keyed by source address; reject the ambiguous
    // mix rather than silently overwriting the first declared source.
    if !flow.inputs.is_empty() && (!body.input.is_empty() || body.connection.is_some()) {
        return Err(ApiError::bad_request(
            "this flow declares its data sources; override a named source with 'inputs' (address to content), not the single 'input'/'connection'",
        ));
    }

    // The legacy single source: either a named connection's rows or inline CSV. A
    // named connection's rows are fetched and keyed under the flow's first declared
    // source address (or "data") via the legacy path; an inline body uses the
    // same keying.
    let legacy_single = match &body.connection {
        Some(conn_name) => {
            let conn = {
                let store = state.automation.lock().expect("automation store mutex");
                store
                    .automation()
                    .connections
                    .get(conn_name)
                    .cloned()
                    .ok_or_else(|| {
                        ApiError::not_found(format!("unknown connection '{conn_name}'"))
                    })?
            };
            // Fetch now and stage under the legacy key in the inline map, so the
            // shared resolver does not re-parse it as CSV.
            let rows = fetch_connection_rows(&state, &conn)?;
            (Some(rows), String::new())
        }
        None => (None, body.input.clone()),
    };

    // Build the inputs map: declared inputs + ad-hoc inline + the legacy single.
    let mut inputs = resolve_flow_inputs(&state, &flow, &body.inputs, &legacy_single.1)?;
    if let Some(rows) = legacy_single.0 {
        let key = flow
            .inputs
            .first()
            .map(|i| i.address())
            .unwrap_or_else(|| "data".to_string());
        inputs.insert(key, rows);
    }

    let cube_names = state.engine.cube_names();
    let reader = ApiFlowReader::new(state.clone(), &auth.principal.username);
    let now = state.clock.now_millis();
    let outcome = run_flow(
        &flow.source,
        flow.default_cube.as_deref(),
        &cube_names,
        inputs,
        &body.params,
        now,
        Box::new(reader),
    )
    .map_err(map_flow_error)?;

    // A flow runs as the caller: it may only make changes the caller could make
    // directly (ADR-0023/0035). Authorize the staged effects before applying.
    authorize_outcome(&state, &auth, &outcome)?;
    let (elements_added, cells_written) = apply_outcome(&state, &outcome)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::FlowExec,
        Some(&ObjectRef::global(ObjectKind::Flow, &name)),
        true,
    );
    // Notify every cube the run wrote (a global flow has no single cube).
    for cube in outcome.cubes.keys() {
        broadcast(&state, cube);
    }
    Ok(Json(RunReport {
        rows_read: outcome.report.rows_read,
        cells_written,
        elements_added,
        logs: outcome.report.logs,
    }))
}

// ---- guided CSV import (generates the equivalent of a load flow) ----

#[derive(Deserialize)]
pub(crate) struct ImportBody {
    /// The CSV text to load.
    pub csv: String,
    /// CSV column name -> dimension name. Each column's values become leaf
    /// members of its dimension, and form the coordinate.
    pub columns: BTreeMap<String, String>,
    /// The CSV column holding the numeric value to write.
    pub value_column: String,
    /// Fixed members for dimensions not mapped to a column (dimension -> member).
    #[serde(default)]
    pub fixed: BTreeMap<String, String>,
}

/// `POST /cubes/{cube}/import` -> a guided CSV load into one cube: build the
/// dimension members the CSV references and write its values, without writing a
/// flow by hand. Equivalent to a generated single-cube load flow, applied through
/// the same multi-target path.
pub(crate) async fn import_csv(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    Json(body): Json<ImportBody>,
) -> Result<Json<RunReport>, ApiError> {
    let rows = parse_csv(&body.csv)
        .map_err(|e| ApiError::unprocessable("FLOW_INPUT_ERROR", e.to_string()))?;
    let outcome = plan_import(&cube, &rows, &body)?;
    // Authorize the import's effects as the caller (ADR-0023): a CSV import builds
    // members (Dimension:Write) and writes cells (Cube:Write + element write), so
    // it can never load past the caller's own access.
    authorize_outcome(&state, &auth, &outcome)?;
    let (elements_added, cells_written) = apply_outcome(&state, &outcome)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::FlowExec,
        Some(&ObjectRef::in_cube(ObjectKind::Flow, &cube, "import")),
        true,
    );
    broadcast(&state, &cube);
    Ok(Json(RunReport {
        rows_read: rows.len(),
        cells_written,
        elements_added,
        logs: Vec::new(),
    }))
}

/// Build the flow outcome for a guided CSV import into `cube`: ensure each mapped
/// column's values as leaf members, then write the value column at the row's
/// coordinate. The result is a single-target outcome keyed by `cube`.
fn plan_import(cube: &str, rows: &[Row], body: &ImportBody) -> Result<FlowOutcome, ApiError> {
    if body.columns.is_empty() {
        return Err(ApiError::bad_request(
            "import needs at least one column mapping",
        ));
    }
    let mut elements = Vec::new();
    let mut cells = Vec::new();
    for row in rows {
        let lookup = |col: &str| -> Option<&str> {
            row.iter().find(|(k, _)| k == col).map(|(_, v)| v.as_str())
        };
        let mut coord: BTreeMap<String, String> = body.fixed.clone();
        for (column, dimension) in &body.columns {
            let member = lookup(column).ok_or_else(|| {
                ApiError::unprocessable(
                    "FLOW_INPUT_ERROR",
                    format!("CSV row missing column '{column}'"),
                )
            })?;
            elements.push(ElementSpec {
                dimension: dimension.clone(),
                name: member.to_string(),
                kind: ElementKind::Leaf,
            });
            coord.insert(dimension.clone(), member.to_string());
        }
        let value = lookup(&body.value_column).ok_or_else(|| {
            ApiError::unprocessable(
                "FLOW_INPUT_ERROR",
                format!("CSV row missing value column '{}'", body.value_column),
            )
        })?;
        // A blank value is "no data" for that cell: build the member but skip the
        // write (so a CSV with some empty values loads cleanly, sparse).
        if !value.trim().is_empty() {
            cells.push(PlannedCell {
                coord,
                value: value.to_string(),
            });
        }
    }
    // Dedup element specs (idempotent anyway).
    let mut seen = std::collections::HashSet::new();
    elements.retain(|e| seen.insert((e.dimension.clone(), e.name.clone())));
    let mut cubes = BTreeMap::new();
    cubes.insert(
        cube.to_string(),
        CubeChanges {
            elements,
            edges: Vec::new(),
            cells,
        },
    );
    Ok(FlowOutcome {
        cubes,
        dimensions: BTreeMap::new(),
        report: Default::default(),
    })
}

// ---- flow tests ----

#[derive(Serialize, Deserialize)]
pub(crate) struct FlowTestDto {
    pub name: String,
    pub flow: String,
    #[serde(default)]
    pub input: String,
    /// Named-source contents for a multi-source flow (ADR-0035): address -> CSV.
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
    /// The target cube whose staged cells the assertions check; `None` uses the
    /// flow's default cube.
    #[serde(default)]
    pub cube: Option<String>,
    #[serde(default)]
    pub params: BTreeMap<String, String>,
    #[serde(default)]
    pub assertions: Vec<TestCellDto>,
}

#[derive(Serialize)]
pub(crate) struct FlowTestListDto {
    pub tests: Vec<FlowTestDto>,
}

fn flow_test_dto(t: &FlowTest) -> FlowTestDto {
    FlowTestDto {
        name: t.name.clone(),
        flow: t.flow.clone(),
        input: t.input.clone(),
        inputs: t.inputs.clone(),
        cube: t.cube.clone(),
        params: t.params.clone(),
        assertions: t.assertions.iter().map(from_cell).collect(),
    }
}

/// `GET /flows/tests` -> the global flow tests.
pub(crate) async fn list_flow_tests(
    auth: AuthPrincipal,
    State(state): State<AppState>,
) -> Result<Json<FlowTestListDto>, ApiError> {
    require_kind_access(&state, &auth, ObjectKind::Flow, None, AccessLevel::Read)?;
    let store = state.automation.lock().expect("automation store mutex");
    Ok(Json(FlowTestListDto {
        tests: store
            .automation()
            .flow_tests
            .values()
            .map(flow_test_dto)
            .collect(),
    }))
}

/// `POST /flows/tests` -> create or replace a global flow test.
pub(crate) async fn put_flow_test(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Json(body): Json<FlowTestDto>,
) -> Result<(StatusCode, Json<FlowTestDto>), ApiError> {
    require_kind_access(&state, &auth, ObjectKind::Flow, None, AccessLevel::Write)?;
    if body.name.trim().is_empty() {
        return Err(ApiError::unprocessable(
            "FLOW_TEST_EMPTY_NAME",
            "flow test name is empty",
        ));
    }
    if body.flow.trim().is_empty() {
        return Err(ApiError::unprocessable(
            "FLOW_TEST_EMPTY_FLOW",
            "flow test references no flow",
        ));
    }
    let test = FlowTest {
        name: body.name.clone(),
        flow: body.flow.clone(),
        input: body.input.clone(),
        inputs: body.inputs.clone(),
        cube: body.cube.clone(),
        params: body.params.clone(),
        assertions: body.assertions.into_iter().map(to_cell).collect(),
    };
    let response = flow_test_dto(&test);
    {
        let mut store = state.automation.lock().expect("automation store mutex");
        // A test must reference an existing flow; reject a dangling test up front
        // rather than letting it fail only when run (parity with put_job).
        if !store.automation().flows.contains_key(&test.flow) {
            return Err(ApiError::unprocessable(
                "UNKNOWN_FLOW",
                format!("flow test references unknown flow '{}'", test.flow),
            ));
        }
        store.define_flow_test(test).map_err(map_persist_error)?;
    }
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectUpdate,
        Some(&ObjectRef::global(ObjectKind::Flow, &body.name)),
        true,
    );
    Ok((StatusCode::CREATED, Json(response)))
}

/// `DELETE /flows/tests/{name}` -> delete a global flow test.
pub(crate) async fn delete_flow_test(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_kind_access(&state, &auth, ObjectKind::Flow, None, AccessLevel::Write)?;
    let removed = state
        .automation
        .lock()
        .expect("automation store mutex")
        .delete_flow_test(&name)
        .map_err(map_persist_error)?;
    if !removed {
        return Err(ApiError::not_found(format!("unknown flow test '{name}'")));
    }
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectDelete,
        Some(&ObjectRef::global(ObjectKind::Flow, &name)),
        true,
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /flows/tests/run` -> run the global flow tests. Tests evaluate over cube
/// clones and may target any cube, surfacing values across them, so an
/// element-restricted caller could otherwise observe a denied member; rather than
/// partially redact across an unknown set of cubes, the run requires a server
/// admin (fail-closed, ADR-0015/0035). A future increment may relax this to a
/// per-cube element-restriction check.
pub(crate) async fn run_flow_tests_handler(
    auth: AuthPrincipal,
    State(state): State<AppState>,
) -> Result<Json<TestReportDto>, ApiError> {
    require_kind_access(&state, &auth, ObjectKind::Flow, None, AccessLevel::Read)?;
    // The global flow-test run can read live cell values across many cubes via its
    // assertions, so it is admin-only (least-surprising fail-closed posture).
    crate::authz::require_admin(&state, &auth)?;
    let automation = {
        let store = state.automation.lock().expect("automation store mutex");
        store.automation().clone()
    };
    let outcomes = run_flow_tests(&automation, |name| {
        state.engine.snapshot(name).map(|s| s.cube().clone())
    })
    .map_err(map_flow_test_error)?;
    let all_passed = outcomes.iter().all(|o| o.passed);
    Ok(Json(TestReportDto {
        all_passed,
        outcomes: outcomes
            .into_iter()
            .map(|o| TestOutcomeDto {
                name: o.name,
                passed: o.passed,
                failures: o
                    .failures
                    .into_iter()
                    .map(|f| FailureDto {
                        coord: f.coord,
                        expected: f.expected,
                        actual: f.actual,
                    })
                    .collect(),
            })
            .collect(),
    }))
}

fn map_flow_test_error(err: FlowTestError) -> ApiError {
    ApiError::unprocessable("FLOW_TEST_ERROR", err.to_string())
}
