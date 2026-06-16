//! Flow endpoints: CRUD over a cube's flows and flow tests, flow preview
//! (strip + parse validation), running a flow over uploaded data, a guided CSV
//! import, and running the flow test suite. All AuthPrincipal-gated. A flow's
//! staged outcome is applied through the engine: elements/edges first (so new
//! members exist), then cell writes.

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use epiphany_core::{ElementKind, ElementSpec, Flow, FlowTest};
use epiphany_engine::CellWrite;
use epiphany_flow::{
    parse_csv, run_flow, run_flow_tests, validate_flow, FlowError, FlowOutcome, FlowTestError,
    PlannedCell,
};
use epiphany_security::{AccessLevel, AuditAction, ObjectKind, ObjectRef};

use crate::auth::AuthPrincipal;
use crate::authz::{
    audit, deny_if_element_restricted, require_cube_access, require_element_write,
    require_kind_access,
};
use crate::connection_routes::fetch_connection_rows;
use crate::dto::{from_cell, to_cell, FailureDto, TestCellDto, TestOutcomeDto, TestReportDto};
use crate::routes::{broadcast, build_write, map_batch_error, snapshot};
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

/// Apply a flow's staged outcome through the engine: add elements/edges, then
/// write the cells. Returns `(elements_added, cells_written)`.
///
/// Everything is pre-validated against a clone of the cube (with the new
/// elements applied) before anything is committed, so an unresolvable cell or a
/// rejected schema change commits nothing. On success the elements are committed
/// first (so the new members exist) and then the cells as one batch.
pub(crate) fn apply_outcome(
    state: &AppState,
    cube: &str,
    outcome: &FlowOutcome,
) -> Result<(usize, usize), ApiError> {
    let snap = snapshot(state, cube)?;
    // Stage the schema growth on a clone and resolve every cell against it, so a
    // resolution failure surfaces before any commit.
    let mut preview = snap.cube().clone();
    preview
        .extend_schema(&outcome.elements, &outcome.edges)
        .map_err(|e| ApiError::unprocessable("FLOW_SCHEMA_ERROR", e.to_string()))?;
    let writes: Vec<CellWrite> = outcome
        .cells
        .iter()
        .map(|cell| build_write(&preview, &cell.coord, &cell.value))
        .collect::<Result<_, _>>()?;

    // All valid: commit elements first, then the cell batch.
    let mut elements_added = 0;
    if !outcome.elements.is_empty() || !outcome.edges.is_empty() {
        let (_, added) = state
            .engine
            .define_elements(cube, None, &outcome.elements, &outcome.edges)
            .map_err(map_batch_error)?;
        elements_added = added;
    }
    let cells_written = writes.len();
    if !writes.is_empty() {
        state
            .engine
            .apply_batch(cube, None, &writes)
            .map_err(map_batch_error)?;
    }
    Ok((elements_added, cells_written))
}

/// Authorize a flow/import outcome AS THE RUNNER (ADR-0023): a flow is never a
/// privilege-escalation path, so every effect it stages must be something the
/// runner could do directly. Structure changes (new elements/edges) require
/// `Dimension:Write` on the cube; cell writes require `Cube:Write` and that every
/// target cell is element-writable by the runner. Holding only `Flow:Write` lets
/// a user author and launch flows, but never edit a cube or dimension they lack
/// access to.
pub(crate) fn authorize_outcome(
    state: &AppState,
    auth: &AuthPrincipal,
    cube: &str,
    outcome: &FlowOutcome,
) -> Result<(), ApiError> {
    if !outcome.elements.is_empty() || !outcome.edges.is_empty() {
        require_kind_access(
            state,
            auth,
            ObjectKind::Dimension,
            Some(cube),
            AccessLevel::Write,
        )?;
    }
    if !outcome.cells.is_empty() {
        // Cell writes use the same gate as the direct cell-write endpoint (the
        // cube-level grant during the migration), plus per-cell element access, so
        // a flow can write exactly what the runner could write by hand.
        require_cube_access(state, auth, cube, AccessLevel::Write)?;
        for cell in &outcome.cells {
            require_element_write(state, auth, cube, &cell.coord)?;
        }
    }
    Ok(())
}

// ---- flow CRUD ----

#[derive(Serialize, Deserialize)]
pub(crate) struct FlowDto {
    pub name: String,
    pub source: String,
}

#[derive(Serialize)]
pub(crate) struct FlowListDto {
    pub flows: Vec<FlowDto>,
}

/// `GET /cubes/{cube}/flows` -> the cube's flows.
pub(crate) async fn list_flows(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
) -> Result<Json<FlowListDto>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    let snap = snapshot(&state, &cube)?;
    Ok(Json(FlowListDto {
        flows: snap
            .model()
            .flows
            .values()
            .map(|f| FlowDto {
                name: f.name.clone(),
                source: f.source.clone(),
            })
            .collect(),
    }))
}

/// `GET /cubes/{cube}/flows/{name}` -> one flow.
pub(crate) async fn get_flow(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
) -> Result<Json<FlowDto>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    let snap = snapshot(&state, &cube)?;
    let flow = snap
        .model()
        .flows
        .get(&name)
        .ok_or_else(|| ApiError::not_found(format!("unknown flow '{name}'")))?;
    Ok(Json(FlowDto {
        name: flow.name.clone(),
        source: flow.source.clone(),
    }))
}

/// `PUT /cubes/{cube}/flows/{name}` -> validate and store a flow.
pub(crate) async fn put_flow(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
    Json(body): Json<FlowDto>,
) -> Result<Json<FlowDto>, ApiError> {
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Flow,
        Some(&cube),
        AccessLevel::Write,
    )?;
    // Validate (strip + parse) before persisting; a bad flow is never stored.
    if body.source.trim().is_empty() {
        return Err(ApiError::unprocessable(
            "FLOW_EMPTY",
            "flow source is empty",
        ));
    }
    validate_flow(&body.source).map_err(map_flow_error)?;
    let flow = Flow {
        name: name.clone(),
        source: body.source.clone(),
    };
    state
        .engine
        .define_flow(&cube, None, flow)
        .map_err(map_batch_error)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectUpdate,
        Some(&ObjectRef::in_cube(ObjectKind::Flow, &cube, &name)),
        true,
    );
    broadcast(&state, &cube);
    Ok(Json(FlowDto {
        name,
        source: body.source,
    }))
}

/// `DELETE /cubes/{cube}/flows/{name}` -> delete a flow.
pub(crate) async fn delete_flow(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Flow,
        Some(&cube),
        AccessLevel::Write,
    )?;
    state
        .engine
        .delete_flow(&cube, None, &name)
        .map_err(map_batch_error)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectDelete,
        Some(&ObjectRef::in_cube(ObjectKind::Flow, &cube, &name)),
        true,
    );
    broadcast(&state, &cube);
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

/// `POST /cubes/{cube}/flows/preview` -> validate a flow source without saving.
pub(crate) async fn preview_flow(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    Json(body): Json<PreviewBody>,
) -> Result<Json<PreviewResult>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    validate_flow(&body.source).map_err(map_flow_error)?;
    Ok(Json(PreviewResult { ok: true }))
}

// ---- running a flow ----

#[derive(Deserialize)]
pub(crate) struct RunBody {
    /// Inline data-source content (CSV text). Used when `connection` is unset;
    /// empty for a source-less flow.
    #[serde(default)]
    pub input: String,
    /// The name of a configured connection to fetch the input rows from. When
    /// set, it supplies the rows instead of `input`.
    #[serde(default)]
    pub connection: Option<String>,
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

/// `POST /cubes/{cube}/flows/{name}/run` -> run a stored flow over uploaded data.
pub(crate) async fn run_flow_handler(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
    Json(body): Json<RunBody>,
) -> Result<Json<RunReport>, ApiError> {
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Flow,
        Some(&cube),
        AccessLevel::Write,
    )?;
    // Resolve the flow source and (if a connection is named) the connection
    // itself from one snapshot.
    let (source, connection) = {
        let snap = snapshot(&state, &cube)?;
        let model = snap.model();
        let source = model
            .flows
            .get(&name)
            .ok_or_else(|| ApiError::not_found(format!("unknown flow '{name}'")))?
            .source
            .clone();
        let connection = match &body.connection {
            Some(conn_name) => {
                Some(model.connections.get(conn_name).cloned().ok_or_else(|| {
                    ApiError::not_found(format!("unknown connection '{conn_name}'"))
                })?)
            }
            None => None,
        };
        (source, connection)
    };

    // Fetch input rows: from the connection (impure edge, with its per-kind
    // gates applied by the shared fetcher), else inline CSV.
    let rows = match connection {
        Some(conn) => fetch_connection_rows(&state, &conn)?,
        None => parse_csv(&body.input)
            .map_err(|e| ApiError::unprocessable("FLOW_INPUT_ERROR", e.to_string()))?,
    };
    let now = state.clock.now_millis();
    let outcome = run_flow(&source, &cube, rows, &body.params, now).map_err(map_flow_error)?;

    // A flow runs as the caller: it may only make changes the caller could make
    // directly (ADR-0023). Authorize the staged effects before applying them, so
    // Flow:Write alone can never edit a cube or dimension the runner lacks.
    authorize_outcome(&state, &auth, &cube, &outcome)?;
    let (elements_added, cells_written) = apply_outcome(&state, &cube, &outcome)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::FlowExec,
        Some(&ObjectRef::in_cube(ObjectKind::Flow, &cube, &name)),
        true,
    );
    broadcast(&state, &cube);
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

/// `POST /cubes/{cube}/flows/import` -> a guided CSV load: build the dimension
/// members the CSV references and write its values, without writing a flow by
/// hand. Equivalent to a generated load flow, applied through the same path.
pub(crate) async fn import_csv(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    Json(body): Json<ImportBody>,
) -> Result<Json<RunReport>, ApiError> {
    let rows = parse_csv(&body.csv)
        .map_err(|e| ApiError::unprocessable("FLOW_INPUT_ERROR", e.to_string()))?;
    let outcome = plan_import(&rows, &body)?;
    // Authorize the import's effects as the caller (ADR-0023): a CSV import builds
    // members (Dimension:Write) and writes cells (Cube:Write + element write), so
    // it can never load past the caller's own access.
    authorize_outcome(&state, &auth, &cube, &outcome)?;
    let (elements_added, cells_written) = apply_outcome(&state, &cube, &outcome)?;
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

/// Build the flow outcome for a guided CSV import: ensure each mapped column's
/// values as leaf members, then write the value column at the row's coordinate.
fn plan_import(rows: &[epiphany_flow::Row], body: &ImportBody) -> Result<FlowOutcome, ApiError> {
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
    Ok(FlowOutcome {
        elements,
        edges: Vec::new(),
        cells,
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
        params: t.params.clone(),
        assertions: t.assertions.iter().map(from_cell).collect(),
    }
}

/// `GET /cubes/{cube}/flows/tests` -> the cube's flow tests.
pub(crate) async fn list_flow_tests(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
) -> Result<Json<FlowTestListDto>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    let snap = snapshot(&state, &cube)?;
    Ok(Json(FlowTestListDto {
        tests: snap
            .model()
            .flow_tests
            .values()
            .map(flow_test_dto)
            .collect(),
    }))
}

/// `POST /cubes/{cube}/flows/tests` -> create or replace a flow test.
pub(crate) async fn put_flow_test(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    Json(body): Json<FlowTestDto>,
) -> Result<(StatusCode, Json<FlowTestDto>), ApiError> {
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Flow,
        Some(&cube),
        AccessLevel::Write,
    )?;
    let test = FlowTest {
        name: body.name.clone(),
        flow: body.flow.clone(),
        input: body.input.clone(),
        params: body.params.clone(),
        assertions: body.assertions.into_iter().map(to_cell).collect(),
    };
    let response = flow_test_dto(&test);
    state
        .engine
        .define_flow_test(&cube, None, test)
        .map_err(map_batch_error)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectUpdate,
        Some(&ObjectRef::in_cube(ObjectKind::Flow, &cube, &body.name)),
        true,
    );
    broadcast(&state, &cube);
    Ok((StatusCode::CREATED, Json(response)))
}

/// `DELETE /cubes/{cube}/flows/tests/{name}` -> delete a flow test.
pub(crate) async fn delete_flow_test(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Flow,
        Some(&cube),
        AccessLevel::Write,
    )?;
    state
        .engine
        .delete_flow_test(&cube, None, &name)
        .map_err(map_batch_error)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectDelete,
        Some(&ObjectRef::in_cube(ObjectKind::Flow, &cube, &name)),
        true,
    );
    broadcast(&state, &cube);
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /cubes/{cube}/flows/tests/run` -> run the cube's flow tests.
pub(crate) async fn run_flow_tests_handler(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
) -> Result<Json<TestReportDto>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    let snap = snapshot(&state, &cube)?;
    // Tests evaluate over a clone of the live cube; an element-restricted caller
    // is denied so they cannot observe a denied member's value (ADR-0015).
    deny_if_element_restricted(&state, &auth, &snap)?;
    let outcomes = run_flow_tests(snap.model()).map_err(map_flow_test_error)?;
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
