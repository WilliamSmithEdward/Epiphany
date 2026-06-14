//! Rule endpoints: CRUD over a cube's rules and rule tests, rule preview
//! (parse/compile validation with span errors), cell explain (provenance), and
//! feeder diagnostics. All AuthPrincipal-gated; calc failures map to the shared
//! ApiError envelope with stable codes.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use epiphany_calc::{
    explain_with, infer_feeders, run_rule_tests, validate_feeders, EvalRegistry, SandboxOverlay,
};
use epiphany_core::{CellTrace, Cube, ExplainDepth, RuleTest, TestCell, TraceKind};
use epiphany_engine::ReadSnapshot;

use crate::auth::AuthPrincipal;
use crate::calc_factory::{compile_source, OwnedOverlay, PinnedRegistry, ValidateError};
use crate::dto::CoordMap;
use crate::resolve::resolve;
use crate::routes::map_batch_error;
use crate::sandbox_routes::{resolve_sandbox, SandboxSelector};
use crate::ws::ChangeEvent;
use crate::{ApiError, AppState};

// ---- shared helpers ----

fn snapshot(state: &AppState, cube: &str) -> Result<ReadSnapshot, ApiError> {
    state
        .engine
        .snapshot(cube)
        .ok_or_else(|| ApiError::not_found(format!("unknown cube '{cube}'")))
}

fn coord_names(cube: &Cube, coord: &[u32]) -> Vec<String> {
    coord
        .iter()
        .enumerate()
        .map(|(d, &idx)| {
            cube.dimension(d)
                .element(idx)
                .map(|e| e.name.clone())
                .unwrap_or_default()
        })
        .collect()
}

/// Map a rule validation failure to the API envelope, attaching line/column.
fn map_validate(err: ValidateError, source: &str) -> ApiError {
    match err {
        ValidateError::Parse(e) => {
            let (line, column) = e.span.line_col(source);
            ApiError::unprocessable("RULE_PARSE_ERROR", e.to_string())
                .with_details(json!({ "line": line, "column": column }))
        }
        ValidateError::Compile(e) => {
            let (line, column) = e.span().line_col(source);
            ApiError::unprocessable("RULE_COMPILE_ERROR", e.to_string())
                .with_details(json!({ "line": line, "column": column }))
        }
        ValidateError::UnknownCube(name) => ApiError::not_found(format!("unknown cube '{name}'")),
    }
}

fn broadcast(state: &AppState, cube: &str, version: u64) {
    let _ = state.events.send(ChangeEvent::ObjectsChanged {
        cube: cube.to_string(),
        version,
    });
}

// ---- rules CRUD ----

#[derive(Serialize, Deserialize)]
pub(crate) struct RulesDto {
    pub source: String,
}

/// `GET /cubes/{cube}/rules` -> the cube's rule source.
pub(crate) async fn get_rules(
    _auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
) -> Result<Json<RulesDto>, ApiError> {
    let snap = snapshot(&state, &cube)?;
    Ok(Json(RulesDto {
        source: snap.rules().source.clone(),
    }))
}

/// `PUT /cubes/{cube}/rules` -> validate and set the cube's rule source.
pub(crate) async fn put_rules(
    _auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    Json(body): Json<RulesDto>,
) -> Result<Json<RulesDto>, ApiError> {
    // Validate (parse + compile) before persisting; bad rules never get stored.
    compile_source(&state.engine, &cube, &body.source)
        .map_err(|e| map_validate(e, &body.source))?;
    let outcome = state
        .engine
        .define_rules(&cube, None, body.source.clone())
        .map_err(map_batch_error)?;
    broadcast(&state, &cube, outcome.version);
    Ok(Json(body))
}

/// `DELETE /cubes/{cube}/rules` -> clear the cube's rules.
pub(crate) async fn delete_rules(
    _auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
) -> Result<StatusCode, ApiError> {
    let outcome = state
        .engine
        .delete_rules(&cube, None)
        .map_err(map_batch_error)?;
    broadcast(&state, &cube, outcome.version);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub(crate) struct PreviewResult {
    pub ok: bool,
}

/// `POST /cubes/{cube}/rules/preview` -> validate a rule source without saving.
pub(crate) async fn preview_rules(
    _auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    Json(body): Json<RulesDto>,
) -> Result<Json<PreviewResult>, ApiError> {
    compile_source(&state.engine, &cube, &body.source)
        .map_err(|e| map_validate(e, &body.source))?;
    Ok(Json(PreviewResult { ok: true }))
}

// ---- explain ----

#[derive(Deserialize)]
pub(crate) struct ExplainRequest {
    pub coord: CoordMap,
    #[serde(default)]
    pub depth: Option<String>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum TraceKindDto {
    Stored,
    Rule {
        rule: usize,
        span_start: usize,
        span_end: usize,
    },
    Consolidation {
        contributions: usize,
    },
}

#[derive(Serialize)]
pub(crate) struct TraceDto {
    pub cube: String,
    pub coord: Vec<String>,
    pub value: String,
    #[serde(flatten)]
    pub kind: TraceKindDto,
    pub inputs: Vec<TraceDto>,
}

fn trace_dto(trace: CellTrace) -> TraceDto {
    let kind = match trace.kind {
        TraceKind::Stored => TraceKindDto::Stored,
        TraceKind::Rule { rule, span } => TraceKindDto::Rule {
            rule,
            span_start: span.0,
            span_end: span.1,
        },
        TraceKind::Consolidation { contributions } => TraceKindDto::Consolidation { contributions },
    };
    TraceDto {
        cube: trace.cube,
        coord: trace.coord,
        value: trace.value.to_string(),
        kind,
        inputs: trace.inputs.into_iter().map(trace_dto).collect(),
    }
}

/// `POST /cubes/{cube}/cells/explain` -> a provenance trace for a cell.
pub(crate) async fn explain_cell(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    selector: SandboxSelector,
    Json(req): Json<ExplainRequest>,
) -> Result<Json<TraceDto>, ApiError> {
    let snap = snapshot(&state, &cube)?;
    let resolved = resolve(snap.cube(), &req.coord)?;
    let depth = match req.depth.as_deref() {
        None | Some("full") => ExplainDepth::Full,
        Some("immediate") => ExplainDepth::Immediate,
        Some(other) => ExplainDepth::Levels(
            other
                .parse::<u32>()
                .map_err(|_| ApiError::bad_request(format!("invalid depth '{other}'")))?,
        ),
    };
    let registry = PinnedRegistry::build(&state.engine);
    let ordinal = registry
        .ordinal_of(&cube)
        .ok_or_else(|| ApiError::not_found(format!("unknown cube '{cube}'")))?;
    // Explain over the same what-if overlay as the read, so provenance matches a
    // sandboxed value rather than base (ADR-0014).
    let sandbox_name = resolve_sandbox(&snap, &auth.principal, &selector)?;
    let overlay = sandbox_name
        .as_deref()
        .and_then(|n| snap.model().sandbox(n))
        .map(|sb| OwnedOverlay::new(ordinal, sb));
    let trace = explain_with(
        &registry,
        ordinal,
        &resolved.indices,
        depth,
        overlay.as_ref().map(|o| o as &dyn SandboxOverlay),
    )
    .map_err(|e| ApiError::unprocessable("CALC_ERROR", e.to_string()))?;
    Ok(Json(trace_dto(trace)))
}

// ---- feeder diagnostics ----

#[derive(Serialize)]
pub(crate) struct OpaqueDto {
    pub rule: usize,
    pub reason: String,
}

#[derive(Serialize)]
pub(crate) struct FeederReportDto {
    pub fed_cell_count: usize,
    pub under_fed: Vec<Vec<String>>,
    pub over_fed: Vec<Vec<String>>,
    pub estimated_over_fed_bytes: usize,
    pub opaque_rules: Vec<OpaqueDto>,
}

/// `GET /cubes/{cube}/feeders/diagnostics` -> auto-inferred feeders + validation.
pub(crate) async fn feeder_diagnostics(
    _auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
) -> Result<Json<FeederReportDto>, ApiError> {
    let registry = PinnedRegistry::build(&state.engine);
    let ordinal = registry
        .ordinal_of(&cube)
        .ok_or_else(|| ApiError::not_found(format!("unknown cube '{cube}'")))?;
    let cube_ref = registry.cube(ordinal).ok_or_else(ApiError::internal)?;
    let model = registry.compiled(ordinal).ok_or_else(ApiError::internal)?;
    let inference = infer_feeders(cube_ref, model, ordinal);
    let diag = validate_feeders(&registry, ordinal, &inference.index)
        .map_err(|e| ApiError::unprocessable("CALC_ERROR", e.to_string()))?;
    let names = |coords: Vec<Vec<u32>>| -> Vec<Vec<String>> {
        coords.iter().map(|c| coord_names(cube_ref, c)).collect()
    };
    Ok(Json(FeederReportDto {
        fed_cell_count: diag.fed_cell_count,
        under_fed: names(diag.under_fed),
        over_fed: names(diag.over_fed),
        estimated_over_fed_bytes: diag.estimated_over_fed_bytes,
        opaque_rules: inference
            .opaque
            .into_iter()
            .map(|o| OpaqueDto {
                rule: o.rule.0,
                reason: o.reason,
            })
            .collect(),
    }))
}

// ---- rule tests ----

#[derive(Serialize, Deserialize)]
pub(crate) struct TestCellDto {
    pub coord: CoordMap,
    pub value: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct RuleTestDto {
    pub name: String,
    #[serde(default)]
    pub fixtures: Vec<TestCellDto>,
    #[serde(default)]
    pub assertions: Vec<TestCellDto>,
}

#[derive(Serialize)]
pub(crate) struct RuleTestListDto {
    pub tests: Vec<RuleTestDto>,
}

fn to_cell(c: TestCellDto) -> TestCell {
    TestCell {
        coord: c.coord,
        value: c.value,
    }
}

fn from_cell(c: &TestCell) -> TestCellDto {
    TestCellDto {
        coord: c.coord.clone(),
        value: c.value.clone(),
    }
}

fn test_dto(t: &RuleTest) -> RuleTestDto {
    RuleTestDto {
        name: t.name.clone(),
        fixtures: t.fixtures.iter().map(from_cell).collect(),
        assertions: t.assertions.iter().map(from_cell).collect(),
    }
}

/// `GET /cubes/{cube}/rules/tests` -> the cube's rule tests.
pub(crate) async fn list_rule_tests(
    _auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
) -> Result<Json<RuleTestListDto>, ApiError> {
    let snap = snapshot(&state, &cube)?;
    Ok(Json(RuleTestListDto {
        tests: snap.tests().values().map(test_dto).collect(),
    }))
}

/// `POST /cubes/{cube}/rules/tests` -> create or replace a rule test.
pub(crate) async fn put_rule_test(
    _auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    Json(body): Json<RuleTestDto>,
) -> Result<(StatusCode, Json<RuleTestDto>), ApiError> {
    let test = RuleTest {
        name: body.name.clone(),
        fixtures: body.fixtures.into_iter().map(to_cell).collect(),
        assertions: body.assertions.into_iter().map(to_cell).collect(),
    };
    let response = test_dto(&test);
    let outcome = state
        .engine
        .define_rule_test(&cube, None, test)
        .map_err(map_batch_error)?;
    broadcast(&state, &cube, outcome.version);
    Ok((StatusCode::CREATED, Json(response)))
}

/// `DELETE /cubes/{cube}/rules/tests/{name}` -> delete a rule test.
pub(crate) async fn delete_rule_test(
    _auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let outcome = state
        .engine
        .delete_rule_test(&cube, None, &name)
        .map_err(map_batch_error)?;
    broadcast(&state, &cube, outcome.version);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub(crate) struct FailureDto {
    pub coord: CoordMap,
    pub expected: String,
    pub actual: String,
}

#[derive(Serialize)]
pub(crate) struct TestOutcomeDto {
    pub name: String,
    pub passed: bool,
    pub failures: Vec<FailureDto>,
}

#[derive(Serialize)]
pub(crate) struct TestReportDto {
    pub all_passed: bool,
    pub outcomes: Vec<TestOutcomeDto>,
}

/// `POST /cubes/{cube}/rules/tests/run` -> run the cube's rule tests.
pub(crate) async fn run_rule_tests_handler(
    _auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
) -> Result<Json<TestReportDto>, ApiError> {
    let snap = snapshot(&state, &cube)?;
    let outcomes = run_rule_tests(snap.model())
        .map_err(|e| ApiError::unprocessable("RULE_TEST_ERROR", e.to_string()))?;
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
