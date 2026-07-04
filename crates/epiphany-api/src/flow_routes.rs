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
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::Duration;

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
use crate::routes::{build_write, map_batch_error, map_persist_error};
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

// ---- flow-run wall-clock watchdog (F1) ----
//
// The flow interpreter has a DETERMINISTIC loop/recursion budget (epiphany-flow
// run.rs), but no wall-clock ceiling: a pathological-but-in-budget computation (a
// catastrophically backtracking regex, a tight arithmetic loop just under the
// iteration cap) can pin a worker for an unbounded time. This adds a configurable
// wall-clock DEADLINE around a flow run so such a wedge is aborted with a clear
// error instead of pinning a thread forever.
//
// SCOPE / limitation (documented deliberately, matching epiphany-flow's own doc):
// this is a LIVENESS bound, not OS-level isolation. boa runs synchronously and
// cannot be preempted from outside, so on a timeout the run thread is DETACHED and
// keeps running to completion (or forever) in the background — the watchdog frees
// the *caller* (the HTTP worker or the scheduler tick), returns a clear error, and
// stops the run's effects from ever being applied, but it cannot reclaim the wedged
// CPU. True CPU/RSS reclamation needs subprocess isolation, which is out of scope
// for the single-binary design (ADR-0004 / epiphany-flow run.rs). The deadline is a
// real wall-clock read: it is a deliberate liveness bound and is NOT part of any
// flow's deterministic output (a run that finishes within the deadline behaves
// identically regardless of the deadline value), so it does not violate the
// determinism mandate.

/// Default wall-clock ceiling for a single flow run (5 minutes). Generous for a
/// legitimate ETL over a large input, tight enough that a wedged flow does not pin
/// a worker indefinitely.
const DEFAULT_FLOW_DEADLINE_MILLIS: u64 = 300_000;

/// The configured flow wall-clock deadline, read ONCE from
/// `EPIPHANY_FLOW_DEADLINE_MILLIS` (defaulting to [`DEFAULT_FLOW_DEADLINE_MILLIS`]).
/// `0` or an unparseable value disables the watchdog (unbounded, the pre-F1
/// behavior) so an operator can opt out. Cached in a `OnceLock` because it is a
/// process-wide operational knob, not per-request state (it deliberately does not
/// live on `AppState`, which the composition root owns).
fn flow_deadline() -> Option<Duration> {
    static DEADLINE: OnceLock<Option<Duration>> = OnceLock::new();
    *DEADLINE.get_or_init(|| {
        let millis = std::env::var("EPIPHANY_FLOW_DEADLINE_MILLIS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_FLOW_DEADLINE_MILLIS);
        (millis > 0).then(|| Duration::from_millis(millis))
    })
}

/// Why a watchdog-supervised flow run did not produce an outcome.
pub(crate) enum FlowRunError {
    /// The flow itself failed (strip/runtime error), as before.
    Flow(FlowError),
    /// The run exceeded the wall-clock deadline and was abandoned (its thread is
    /// detached and its effects are never applied). Carries the deadline for the
    /// error message.
    TimedOut(Duration),
    /// The run thread panicked (a bug in the interpreter host); surfaced as a clean
    /// internal error rather than propagating the panic.
    Panicked,
}

/// Run a flow under the wall-clock watchdog (F1). All inputs are owned so the run
/// executes on its own thread (the non-`Send` `FlowReader` is built INSIDE the
/// thread, so nothing non-`Send` crosses the boundary); the caller waits up to the
/// deadline for the result. On a timeout the flow thread is detached (see the
/// module note) and [`FlowRunError::TimedOut`] is returned, so the run's staged
/// effects are never applied. With the watchdog disabled (deadline `None`) the flow
/// runs inline exactly as before.
///
/// This is a SYNC, blocking helper: the HTTP handler calls it from within
/// `spawn_blocking` (so a wait never occupies an async worker), and the scheduler —
/// already on a blocking thread — calls it directly.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_flow_watchdog(
    state: AppState,
    username: String,
    source: String,
    default_cube: Option<String>,
    cube_names: Vec<String>,
    inputs: BTreeMap<String, Vec<Row>>,
    params: BTreeMap<String, String>,
    now_millis: u64,
) -> Result<FlowOutcome, FlowRunError> {
    let run = move || {
        let reader = ApiFlowReader::new(state, &username);
        run_flow(
            &source,
            default_cube.as_deref(),
            &cube_names,
            inputs,
            &params,
            now_millis,
            Box::new(reader),
        )
    };

    match run_with_deadline(flow_deadline(), run) {
        Ok(result) => result.map_err(FlowRunError::Flow),
        Err(DeadlineError::TimedOut(deadline)) => Err(FlowRunError::TimedOut(deadline)),
        Err(DeadlineError::Panicked) => Err(FlowRunError::Panicked),
    }
}

/// Why [`run_with_deadline`] did not return the closure's value.
#[derive(Debug)]
enum DeadlineError {
    /// The closure did not finish within the deadline; its thread is detached.
    TimedOut(Duration),
    /// The closure's thread panicked (or the OS refused to spawn it).
    Panicked,
}

/// Run `f` on a dedicated thread, returning its value if it finishes within
/// `deadline`, else abandoning the thread and returning [`DeadlineError::TimedOut`]
/// (F1). With `deadline` `None` the closure runs INLINE (no extra thread), matching
/// the unbounded pre-watchdog behavior. The thread is intentionally detached on a
/// timeout: a wedged computation cannot be interrupted, so joining it would defeat
/// the whole liveness guarantee — the value it may eventually produce is dropped
/// (the caller never applies it). Generic over the payload so the mechanism is unit-
/// testable without a full flow run.
fn run_with_deadline<T: Send + 'static>(
    deadline: Option<Duration>,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, DeadlineError> {
    let Some(deadline) = deadline else {
        return Ok(f());
    };
    // A bounded channel of one carries the result back. If the run finishes after we
    // have already timed out, the send fails (the receiver is gone) and the value is
    // dropped — never applied.
    let (tx, rx) = mpsc::sync_channel::<T>(1);
    std::thread::Builder::new()
        .name("epiphany-flow-run".to_string())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .map_err(|_| DeadlineError::Panicked)?;
    match rx.recv_timeout(deadline) {
        Ok(value) => Ok(value),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(DeadlineError::TimedOut(deadline)),
        // The sender hung up without sending: the run thread panicked.
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(DeadlineError::Panicked),
    }
}

/// Map a [`FlowRunError`] to the API envelope: a flow error keeps its existing
/// mapping; a timeout is a clear 503 (the server could not complete the run in
/// time), and a panic is a clean 500.
fn map_flow_run_error(err: FlowRunError) -> ApiError {
    match err {
        FlowRunError::Flow(e) => map_flow_error(e),
        FlowRunError::TimedOut(deadline) => ApiError::service_unavailable(format!(
            "flow run exceeded the {}s wall-clock deadline and was aborted",
            deadline.as_secs()
        )),
        FlowRunError::Panicked => ApiError::internal(),
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

/// Gate a flow run's *input* connections as the runner (ADR-0035 decision 7):
/// authoring/running a flow must not grant data access the principal lacks, and a
/// **global** connection's output rows (a SQL result, an HTTP body) are such data,
/// gated by `Connection:Read`. Require it once per run before any global
/// connection is fetched, so a `Flow:Write` holder who lacks `Connection:Read`
/// cannot surface a global connection's rows via `ctx.log`. Flow-scoped (`Local`)
/// connections are the runner's own inline definition and are unaffected; the
/// legacy single `connection` field is a global reference and is covered by the
/// `legacy_global` flag.
fn require_connection_read_for_inputs(
    state: &AppState,
    auth: &AuthPrincipal,
    flow: &Flow,
    legacy_global: bool,
) -> Result<(), ApiError> {
    let uses_global = legacy_global
        || flow
            .inputs
            .iter()
            .any(|i| i.binding == FlowInputBinding::Global);
    if uses_global {
        require_kind_access(state, auth, ObjectKind::Connection, None, AccessLevel::Read)?;
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

    // Reading a GLOBAL connection's rows is data access gated by `Connection:Read`
    // (ADR-0035 decision 7): a `Flow:Write` holder without it may not fetch a
    // global connection's output through a flow. Checked before any fetch.
    require_connection_read_for_inputs(&state, &auth, &flow, body.connection.is_some())?;

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
    let now = state.clock.now_millis();
    // Run the flow under the wall-clock watchdog (F1) AND off the async workers (A2,
    // boa is blocking): a wedged or long-running flow can no longer pin an async
    // worker, and one that overruns the deadline is aborted with a clear 503 rather
    // than running forever. The reader is built inside the watchdog thread, so no
    // non-`Send` value crosses the boundary.
    let outcome = {
        let state = state.clone();
        let username = auth.principal.username.clone();
        let source = flow.source.clone();
        let default_cube = flow.default_cube.clone();
        let params = body.params.clone();
        tokio::task::spawn_blocking(move || {
            run_flow_watchdog(
                state,
                username,
                source,
                default_cube,
                cube_names,
                inputs,
                params,
                now,
            )
        })
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(map_flow_run_error)?
    };

    // A flow runs as the caller: it may only make changes the caller could make
    // directly (ADR-0023/0035). Authorize the staged effects before applying.
    authorize_outcome(&state, &auth, &outcome)?;
    // Apply the staged outcome (per-cube writer locks + fsyncs) off the async
    // workers (A2): a flow can write many cubes, each a blocking commit, so running
    // it inline would starve readers. The engine + outcome are cheap to clone into
    // the task; the scheduler path already runs `apply_outcome` under spawn_blocking.
    let (elements_added, cells_written) = {
        let state = state.clone();
        let outcome = outcome.clone();
        tokio::task::spawn_blocking(move || apply_outcome(&state, &outcome))
            .await
            .map_err(|_| ApiError::internal())??
    };
    audit(
        &state,
        &auth.principal.username,
        AuditAction::FlowExec,
        Some(&ObjectRef::global(ObjectKind::Flow, &name)),
        true,
    );
    // A1: each per-cube write inside `apply_outcome` fires the commit-ordered change
    // feed from within the engine commit, so the handler no longer broadcasts per
    // cube here.
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
    // Apply the import (writer lock + fsync) off the async workers (A2); `outcome` is
    // not needed after, so it moves into the task.
    let (elements_added, cells_written) = {
        let state = state.clone();
        tokio::task::spawn_blocking(move || apply_outcome(&state, &outcome))
            .await
            .map_err(|_| ApiError::internal())??
    };
    audit(
        &state,
        &auth.principal.username,
        AuditAction::FlowExec,
        Some(&ObjectRef::in_cube(ObjectKind::Flow, &cube, "import")),
        true,
    );
    // A1: the import's per-cube write fires the commit-ordered change feed inside the
    // engine commit; the handler no longer broadcasts.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// F1: the wall-clock watchdog aborts a run that overruns the deadline and
    /// returns a timeout — proving the liveness bound. The overrunning closure's
    /// thread is detached (we never join it), so the test does not hang on it.
    #[test]
    fn deadline_aborts_an_overrunning_run() {
        let deadline = Duration::from_millis(20);
        let result = run_with_deadline(Some(deadline), || {
            // Simulate a wedged, uninterruptible computation.
            std::thread::sleep(Duration::from_secs(30));
            42
        });
        match result {
            Err(DeadlineError::TimedOut(d)) => assert_eq!(d, deadline),
            Ok(_) => panic!("the overrunning run must time out, not complete"),
            Err(DeadlineError::Panicked) => panic!("the run did not panic; it overran"),
        }
    }

    /// A run that finishes within the deadline returns its value normally.
    #[test]
    fn a_fast_run_completes_within_the_deadline() {
        let value = run_with_deadline(Some(Duration::from_secs(5)), || 7)
            .unwrap_or_else(|_| panic!("a fast run must not time out"));
        assert_eq!(value, 7);
    }

    /// With the watchdog disabled (deadline `None`) the closure runs inline and its
    /// value is returned unchanged (the unbounded pre-F1 behavior).
    #[test]
    fn a_disabled_watchdog_runs_inline() {
        let value = run_with_deadline(None, || 9).expect("inline run never times out");
        assert_eq!(value, 9);
    }
}
