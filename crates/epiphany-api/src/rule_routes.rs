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
    explain_with, infer_feeders, run_rule_tests, validate_feeders, CalcError, EvalRegistry,
    SandboxOverlay,
};
use epiphany_core::{CellTrace, Cube, ExplainDepth, RuleTest, TraceKind};
use epiphany_security::{AccessLevel, AuditAction, ObjectKind, ObjectRef};

use crate::auth::AuthPrincipal;
use crate::authz::{
    audit, deny_if_element_restricted, element_mask, require_cube_access, require_kind_access,
};
use crate::calc_factory::{compile_source, OwnedOverlay, PinnedRegistry, ValidateError};
use crate::dto::{
    from_cell, to_cell, CoordMap, FailureDto, TestCellDto, TestOutcomeDto, TestReportDto,
};
use crate::resolve::resolve;
use crate::routes::{broadcast_with_version, map_batch_error, snapshot};
use crate::sandbox_routes::{resolve_sandbox, SandboxSelector};
use crate::{ApiError, AppState};

// ---- shared helpers ----

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

// ---- rules CRUD ----

#[derive(Serialize, Deserialize)]
pub(crate) struct RulesDto {
    pub source: String,
}

/// `GET /cubes/{cube}/rules` -> the cube's rule source.
pub(crate) async fn get_rules(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
) -> Result<Json<RulesDto>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    let snap = snapshot(&state, &cube)?;
    Ok(Json(RulesDto {
        source: snap.rules().source.clone(),
    }))
}

/// `PUT /cubes/{cube}/rules` -> validate and set the cube's rule source.
pub(crate) async fn put_rules(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    Json(body): Json<RulesDto>,
) -> Result<Json<RulesDto>, ApiError> {
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Rule,
        Some(&cube),
        AccessLevel::Write,
    )?;
    // Validate (parse + compile) before persisting; bad rules never get stored.
    compile_source(&state.engine, &cube, &body.source)
        .map_err(|e| map_validate(e, &body.source))?;
    let outcome = state
        .engine
        .define_rules(&cube, None, body.source.clone())
        .map_err(map_batch_error)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectUpdate,
        Some(&ObjectRef::in_cube(ObjectKind::Rule, &cube, "rules")),
        true,
    );
    broadcast_with_version(&state, &cube, outcome.version);
    Ok(Json(body))
}

/// `DELETE /cubes/{cube}/rules` -> clear the cube's rules.
pub(crate) async fn delete_rules(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Rule,
        Some(&cube),
        AccessLevel::Write,
    )?;
    let outcome = state
        .engine
        .delete_rules(&cube, None)
        .map_err(map_batch_error)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectDelete,
        Some(&ObjectRef::in_cube(ObjectKind::Rule, &cube, "rules")),
        true,
    );
    broadcast_with_version(&state, &cube, outcome.version);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub(crate) struct PreviewResult {
    pub ok: bool,
}

/// `POST /cubes/{cube}/rules/preview` -> validate a rule source without saving.
pub(crate) async fn preview_rules(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    Json(body): Json<RulesDto>,
) -> Result<Json<PreviewResult>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
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
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
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
    // Explaining a denied cell (or one rolling up a denied leaf) is itself a
    // direct read: it returns 403, not a provenance trace (ADR-0015).
    let mask = element_mask(&state, &auth, &snap);
    let trace = explain_with(
        &registry,
        ordinal,
        &resolved.indices,
        depth,
        overlay.as_ref().map(|o| o as &dyn SandboxOverlay),
        mask.as_ref(),
    )
    .map_err(|e| match e {
        CalcError::AccessDenied => ApiError::forbidden("you do not have access to this cell"),
        other => ApiError::unprocessable("CALC_ERROR", other.to_string()),
    })?;
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
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
) -> Result<Json<FeederReportDto>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    deny_if_element_restricted(&state, &auth, &snapshot(&state, &cube)?)?;
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

fn test_dto(t: &RuleTest) -> RuleTestDto {
    RuleTestDto {
        name: t.name.clone(),
        fixtures: t.fixtures.iter().map(from_cell).collect(),
        assertions: t.assertions.iter().map(from_cell).collect(),
    }
}

/// `GET /cubes/{cube}/rules/tests` -> the cube's rule tests.
pub(crate) async fn list_rule_tests(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
) -> Result<Json<RuleTestListDto>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    let snap = snapshot(&state, &cube)?;
    Ok(Json(RuleTestListDto {
        tests: snap.tests().values().map(test_dto).collect(),
    }))
}

/// `POST /cubes/{cube}/rules/tests` -> create or replace a rule test.
pub(crate) async fn put_rule_test(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    Json(body): Json<RuleTestDto>,
) -> Result<(StatusCode, Json<RuleTestDto>), ApiError> {
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Rule,
        Some(&cube),
        AccessLevel::Write,
    )?;
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
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectUpdate,
        Some(&ObjectRef::in_cube(ObjectKind::Rule, &cube, &body.name)),
        true,
    );
    broadcast_with_version(&state, &cube, outcome.version);
    Ok((StatusCode::CREATED, Json(response)))
}

/// `DELETE /cubes/{cube}/rules/tests/{name}` -> delete a rule test.
pub(crate) async fn delete_rule_test(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Rule,
        Some(&cube),
        AccessLevel::Write,
    )?;
    let outcome = state
        .engine
        .delete_rule_test(&cube, None, &name)
        .map_err(map_batch_error)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectDelete,
        Some(&ObjectRef::in_cube(ObjectKind::Rule, &cube, &name)),
        true,
    );
    broadcast_with_version(&state, &cube, outcome.version);
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /cubes/{cube}/rules/tests/run` -> run the cube's rule tests.
pub(crate) async fn run_rule_tests_handler(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
) -> Result<Json<TestReportDto>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    let snap = snapshot(&state, &cube)?;
    // Tests evaluate over a clone of the live cube, so they expose derived values
    // across the whole cube; an element-restricted caller is denied (ADR-0015).
    deny_if_element_restricted(&state, &auth, &snap)?;
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
