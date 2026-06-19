//! Running a flow on the embedded JavaScript engine (boa, ADR-0004).
//!
//! [`run_flow`] strips the flow's TypeScript to JavaScript, runs it on a fresh,
//! sandboxed boa context against a host API, and returns a [`FlowOutcome`]: the
//! dimension elements/edges and cell writes the flow staged at each target cube
//! and global dimension, plus a report. The runner is pure with respect to the
//! model write path: it never mutates the engine. The caller (the API layer)
//! applies the outcome transactionally per target. Reads are served by an
//! injected, principal-masked [`FlowReader`], pinned for the run, so reads stay
//! deterministic and never expose masked cells. This keeps `epiphany-flow`
//! dependent on `epiphany-core` only, and makes a flow unit-testable without a
//! running server.
//!
//! ## The flow programming model (ADR-0035)
//! A flow declares up to four top-level functions, called in order with a single
//! `ctx` argument: `init`, `schema`, `rows`, `finalize` (all optional). Outputs
//! are named in code; there is no output-object picker. The host API on `ctx`:
//! - data sources: `ctx.input(name)` -> a named source's rows (an array of
//!   `{column: value}`); `ctx.input()` -> the sole source (errors when several);
//!   `ctx.sources()` -> the source names.
//! - `ctx.param(name)`, `ctx.now()`, `ctx.log(msg)`, `ctx.cubes()`.
//! - `ctx.cube(name)` -> a cube handle: `ensureElements`/`ensureElement`/
//!   `ensureConsolidated`/`addChild`, `writeCells`/`writeCell` (staged), and the
//!   reads `readCell`/`readText`/`members`/`property` (live, masked).
//! - `ctx.dimension(name)` -> a global-dimension handle that grows the registry
//!   dimension and fans out to its cubes: `ensureElements`/...`/`addChild`, and
//!   the read `members`.
//! - legacy cube-less ops (`ctx.writeCells`, `ctx.ensureElements`, ...) target a
//!   flow's `default_cube` if one is set (migration shim).
//!
//! ## Determinism and sandboxing
//! `Date` is removed and `Math.random` throws (a flow re-run on the same input is
//! byte-identical; ADR-0009). `ctx.now()` returns the injected clock. There is no
//! filesystem or network access (the host exposes only the API above). A bounded
//! loop-iteration and recursion budget caps runaway scripts deterministically.

use std::cell::RefCell;
use std::collections::BTreeMap;

use boa_engine::object::ObjectInitializer;
use boa_engine::property::Attribute;
use boa_engine::{js_string, Context, JsNativeError, JsResult, JsValue, NativeFunction, Source};
use epiphany_core::{EdgeSpec, ElementKind, ElementSpec};

use crate::csv::Row;
use crate::strip::{strip_types, StripError};

/// Loop-iteration budget: bounds a runaway flow deterministically.
const LOOP_LIMIT: u64 = 50_000_000;
/// Recursion-depth budget.
const RECURSION_LIMIT: usize = 800;
/// Cap on the total staged changes (elements + edges + cells) a single run may
/// accumulate, so a flow cannot exhaust memory by staging unboundedly (which
/// would make the outcome depend on available RAM). Exceeding it throws.
const MAX_STAGED: usize = 5_000_000;

fn over_budget(s: &FlowState) -> bool {
    s.staged > MAX_STAGED
}

fn budget_error() -> boa_engine::JsError {
    JsNativeError::error()
        .with_message("flow exceeded the staged-change budget")
        .into()
}

/// A cell value read live during a run (ADR-0035), masked for the run principal.
/// Both representations may be present (a numeric cell has `numeric`; a string
/// cell has `text`); an empty cell has neither.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlowCell {
    /// The numeric value as an exact decimal string, if the cell is numeric.
    pub numeric: Option<String>,
    /// The string value, if the cell is a string cell.
    pub text: Option<String>,
}

/// Why a live read failed. Surfaced to the flow as a thrown error (fail-closed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowReadError {
    /// No such cube (or the cube is not readable by the run principal).
    UnknownCube(String),
    /// The run principal may not read the coordinate (element security).
    AccessDenied,
    /// The coordinate, dimension, or key was invalid.
    Invalid(String),
}

impl std::fmt::Display for FlowReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FlowReadError::UnknownCube(c) => write!(f, "unknown or unreadable cube '{c}'"),
            FlowReadError::AccessDenied => write!(f, "read access denied"),
            FlowReadError::Invalid(m) => write!(f, "{m}"),
        }
    }
}

/// The live, principal-masked read view a flow run reads through (ADR-0035). The
/// engine/API supplies an implementation pinned to a single snapshot per cube for
/// the run, so reads are deterministic and honor element security. Tests and
/// preview pass [`NullReader`], which has no live state.
pub trait FlowReader {
    /// The value at a cube coordinate (`{dimension: member}`), masked for the run
    /// principal. An empty cell yields an empty [`FlowCell`].
    fn read_cell(
        &self,
        cube: &str,
        coord: &BTreeMap<String, String>,
    ) -> Result<FlowCell, FlowReadError>;

    /// The current member names of a cube's embedded dimension.
    fn cube_members(&self, cube: &str, dimension: &str) -> Result<Vec<String>, FlowReadError>;

    /// The current member names of a global (registry) dimension.
    fn dimension_members(&self, dimension: &str) -> Result<Vec<String>, FlowReadError>;

    /// A read-only cube property by key (an allowlisted field; `None` if unset or
    /// the key is not exposed).
    fn cube_property(&self, cube: &str, key: &str) -> Result<Option<String>, FlowReadError>;
}

/// A reader with no live state: every read fails (fail-closed). Used by the flow
/// test runner and the preview path, which do not read live model state.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullReader;

impl FlowReader for NullReader {
    fn read_cell(
        &self,
        cube: &str,
        _coord: &BTreeMap<String, String>,
    ) -> Result<FlowCell, FlowReadError> {
        Err(FlowReadError::UnknownCube(cube.to_string()))
    }
    fn cube_members(&self, cube: &str, _dimension: &str) -> Result<Vec<String>, FlowReadError> {
        Err(FlowReadError::UnknownCube(cube.to_string()))
    }
    fn dimension_members(&self, dimension: &str) -> Result<Vec<String>, FlowReadError> {
        Err(FlowReadError::Invalid(format!(
            "no live state to read dimension '{dimension}'"
        )))
    }
    fn cube_property(&self, cube: &str, _key: &str) -> Result<Option<String>, FlowReadError> {
        Err(FlowReadError::UnknownCube(cube.to_string()))
    }
}

/// A cell write a flow staged, addressed by member names (resolved to indices by
/// the caller once any new elements exist).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedCell {
    /// Dimension name -> member name.
    pub coord: BTreeMap<String, String>,
    /// The value as a decimal/text string (exact; never an `f64`).
    pub value: String,
}

/// A flow run's summary counts and log lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlowReport {
    /// Number of input rows the flow was given (across all sources).
    pub rows_read: usize,
    /// Number of cell writes the flow staged.
    pub cells_written: usize,
    /// `ctx.log(...)` lines, in order.
    pub logs: Vec<String>,
}

/// The staged schema + cell changes a flow targets at ONE cube (ADR-0035).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CubeChanges {
    /// Cube-embedded dimension elements to ensure (append-only, idempotent).
    pub elements: Vec<ElementSpec>,
    /// Consolidation edges to ensure on the cube's embedded dimensions.
    pub edges: Vec<EdgeSpec>,
    /// Cell writes to apply (after elements exist).
    pub cells: Vec<PlannedCell>,
}

/// The staged growth a flow targets at ONE global dimension (ADR-0035): members
/// and edges added to the registry dimension, which fan out to its cubes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DimChanges {
    /// Members to ensure on the global dimension (the `dimension` field of each
    /// spec is the dimension itself).
    pub elements: Vec<ElementSpec>,
    /// Consolidation edges to ensure on the global dimension.
    pub edges: Vec<EdgeSpec>,
}

/// The result of running a flow (ADR-0035): changes keyed by target cube and by
/// target global dimension, plus a report. A flow may touch any number of cubes
/// and dimensions in one run, owned by none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlowOutcome {
    /// Per-cube staged changes, keyed by cube name.
    pub cubes: BTreeMap<String, CubeChanges>,
    /// Per-global-dimension staged growth, keyed by dimension name.
    pub dimensions: BTreeMap<String, DimChanges>,
    /// The run report.
    pub report: FlowReport,
}

impl FlowOutcome {
    /// Total cells staged across all target cubes.
    pub fn total_cells(&self) -> usize {
        self.cubes.values().map(|c| c.cells.len()).sum()
    }
}

/// Why a flow did not run to completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowError {
    /// The TypeScript could not be stripped (unsupported construct).
    Strip(StripError),
    /// The JavaScript failed to parse or a stage threw.
    Runtime { message: String },
}

impl std::fmt::Display for FlowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FlowError::Strip(e) => write!(f, "{e}"),
            FlowError::Runtime { message } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for FlowError {}

impl From<StripError> for FlowError {
    fn from(e: StripError) -> Self {
        FlowError::Strip(e)
    }
}

/// Per-run host state, accumulated by the native host functions during a run.
/// Kept in a thread-local so plain `fn`-pointer host functions can reach it
/// without capturing (boa's GC-traced captures would require unsafe, which the
/// workspace forbids); a run is fully synchronous on one thread.
struct FlowState {
    /// `{ address: [ {col: value}, ... ] }` for every resolved data source.
    inputs_json: String,
    params_json: String,
    /// The flow's optional default target cube (ADR-0035): the cube the legacy
    /// cube-less `ctx.writeCells`/`ctx.ensureElements`/... calls target. Empty
    /// when the flow has no default (those calls then error and the flow must
    /// name a cube via `ctx.cube(name)`).
    default_cube: String,
    /// Names of the cubes available, for `ctx.cubes()`.
    cube_names_json: String,
    now_millis: u64,
    /// The live, principal-masked read view (ADR-0035).
    reader: Box<dyn FlowReader>,
    /// Per-target-cube staged changes, keyed by cube name.
    cubes: BTreeMap<String, CubeChanges>,
    /// Per-target-dimension staged growth, keyed by dimension name.
    dimensions: BTreeMap<String, DimChanges>,
    /// Running total of staged items (for the budget check).
    staged: usize,
    logs: Vec<String>,
    rows_read: usize,
}

thread_local! {
    static FLOW: RefCell<Option<FlowState>> = const { RefCell::new(None) };
}

fn with_flow<R>(f: impl FnOnce(&mut FlowState) -> R) -> R {
    FLOW.with(|cell| {
        f(cell
            .borrow_mut()
            .as_mut()
            .expect("flow state set during a run"))
    })
}

/// Run `source` (a flow's TypeScript) over its named data `inputs` (address ->
/// rows), with `params`, using `now_millis` for `ctx.now()`. `default_cube` (if
/// any) receives the legacy cube-less calls; `cube_names` populates `ctx.cubes()`;
/// `reader` serves live, principal-masked reads. Returns the staged outcome.
pub fn run_flow(
    source: &str,
    default_cube: Option<&str>,
    cube_names: &[String],
    inputs: BTreeMap<String, Vec<Row>>,
    params: &BTreeMap<String, String>,
    now_millis: u64,
    reader: Box<dyn FlowReader>,
) -> Result<FlowOutcome, FlowError> {
    let js = strip_types(source)?;

    let cube_names_json = serde_json::Value::Array(
        cube_names
            .iter()
            .map(|c| serde_json::Value::String(c.clone()))
            .collect(),
    )
    .to_string();
    let rows_read = inputs.values().map(Vec::len).sum();
    let state = FlowState {
        inputs_json: inputs_to_json(&inputs),
        params_json: params_to_json(params),
        default_cube: default_cube.unwrap_or_default().to_string(),
        cube_names_json,
        now_millis,
        reader,
        cubes: BTreeMap::new(),
        dimensions: BTreeMap::new(),
        staged: 0,
        logs: Vec::new(),
        rows_read,
    };
    FLOW.with(|cell| *cell.borrow_mut() = Some(state));
    // Always clear the thread-local on the way out, even on error.
    let result = run_inner(&js);
    let finished = FLOW.with(|cell| cell.borrow_mut().take());

    match result {
        Ok(()) => {
            let s = finished.expect("flow state set during a run");
            let mut outcome = FlowOutcome {
                report: FlowReport {
                    rows_read: s.rows_read,
                    cells_written: s.cubes.values().map(|c| c.cells.len()).sum(),
                    logs: s.logs,
                },
                cubes: s.cubes,
                dimensions: s.dimensions,
            };
            dedup_specs(&mut outcome);
            Ok(outcome)
        }
        Err(message) => Err(FlowError::Runtime { message }),
    }
}

/// Validate a flow without running it: strip its TypeScript and parse the
/// resulting JavaScript. Catches unsupported constructs and syntax errors (the
/// preview path), with no side effects and no stage execution.
pub fn validate_flow(source: &str) -> Result<(), FlowError> {
    let js = strip_types(source)?;
    let mut ctx = Context::default();
    boa_engine::Script::parse(Source::from_bytes(&js), None, &mut ctx)
        .map(|_| ())
        .map_err(|e| FlowError::Runtime {
            message: js_err(&mut ctx, e),
        })
}

fn run_inner(js: &str) -> Result<(), String> {
    let mut ctx = Context::default();
    ctx.runtime_limits_mut()
        .set_loop_iteration_limit(LOOP_LIMIT);
    ctx.runtime_limits_mut()
        .set_recursion_limit(RECURSION_LIMIT);

    // Determinism guards.
    eval(&mut ctx, DETERMINISM_PRELUDE)?;
    register_host(&mut ctx).map_err(|e| js_err(&mut ctx, e))?;
    eval(&mut ctx, CTX_PRELUDE)?;

    // The flow itself (defines top-level stage functions).
    eval(&mut ctx, js)?;

    // The host API object passed to each stage.
    let ctx_value = ctx
        .global_object()
        .get(js_string!("ctx"), &mut ctx)
        .map_err(|e| js_err(&mut ctx, e))?;

    for stage in ["init", "schema", "rows", "finalize"] {
        call_stage(&mut ctx, stage, &ctx_value)?;
    }
    Ok(())
}

fn eval(ctx: &mut Context, code: &str) -> Result<(), String> {
    match ctx.eval(Source::from_bytes(code)) {
        Ok(_) => Ok(()),
        Err(e) => Err(js_err(ctx, e)),
    }
}

fn js_err(ctx: &mut Context, e: boa_engine::JsError) -> String {
    // Prefer the thrown value's message; fall back to the error's Display.
    e.try_native(ctx)
        .map(|n| n.to_string())
        .unwrap_or_else(|_| e.to_string())
}

/// Call a global stage function with `ctx_value` if it exists and is callable.
fn call_stage(ctx: &mut Context, name: &str, ctx_value: &JsValue) -> Result<(), String> {
    let func = ctx
        .global_object()
        .get(js_string!(name), ctx)
        .map_err(|e| js_err(ctx, e))?;
    let Some(callable) = func.as_callable() else {
        return Ok(()); // stage not defined
    };
    let callable = callable.clone();
    callable
        .call(&JsValue::undefined(), std::slice::from_ref(ctx_value), ctx)
        .map(|_| ())
        .map_err(|e| js_err(ctx, e))
}

fn register_host(ctx: &mut Context) -> JsResult<()> {
    let host = ObjectInitializer::new(ctx)
        .function(
            NativeFunction::from_fn_ptr(host_inputs_json),
            js_string!("inputsJson"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(host_params_json),
            js_string!("paramsJson"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(host_default_cube),
            js_string!("defaultCube"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(host_cube_names),
            js_string!("cubeNamesJson"),
            0,
        )
        .function(NativeFunction::from_fn_ptr(host_now), js_string!("now"), 0)
        .function(NativeFunction::from_fn_ptr(host_log), js_string!("log"), 1)
        .function(
            NativeFunction::from_fn_ptr(host_cube_ensure_elements),
            js_string!("cubeEnsureElementsJson"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(host_cube_add_child),
            js_string!("cubeAddChildJson"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(host_cube_write_cells),
            js_string!("cubeWriteCellsJson"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(host_dim_ensure_elements),
            js_string!("dimEnsureElementsJson"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(host_dim_add_child),
            js_string!("dimAddChildJson"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(host_cube_read_cell),
            js_string!("cubeReadCellJson"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(host_cube_read_text),
            js_string!("cubeReadTextJson"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(host_cube_members),
            js_string!("cubeMembersJson"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(host_cube_property),
            js_string!("cubePropertyJson"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(host_dim_members),
            js_string!("dimMembersJson"),
            1,
        )
        .build();
    ctx.register_global_property(js_string!("__host"), host, Attribute::all())
}

fn arg_string(args: &[JsValue], i: usize, ctx: &mut Context) -> JsResult<String> {
    Ok(args
        .get(i)
        .cloned()
        .unwrap_or_default()
        .to_string(ctx)?
        .to_std_string_escaped())
}

fn host_inputs_json(_t: &JsValue, _a: &[JsValue], _c: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(js_string!(with_flow(|s| s
        .inputs_json
        .clone()))))
}

fn host_params_json(_t: &JsValue, _a: &[JsValue], _c: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(js_string!(with_flow(|s| s
        .params_json
        .clone()))))
}

fn host_default_cube(_t: &JsValue, _a: &[JsValue], _c: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(js_string!(with_flow(|s| s
        .default_cube
        .clone()))))
}

fn host_cube_names(_t: &JsValue, _a: &[JsValue], _c: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(js_string!(with_flow(|s| s
        .cube_names_json
        .clone()))))
}

fn host_now(_t: &JsValue, _a: &[JsValue], _c: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(with_flow(|s| s.now_millis as f64)))
}

fn host_log(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let msg = arg_string(args, 0, ctx)?;
    with_flow(|s| s.logs.push(msg));
    Ok(JsValue::undefined())
}

fn parse_kind(item: &serde_json::Value) -> ElementKind {
    match str_field(item, "kind").as_str() {
        "consolidated" => ElementKind::Consolidated,
        "string" => ElementKind::String,
        _ => ElementKind::Leaf,
    }
}

/// Parse a JSON array argument (`args[idx]`) into its items, or `None`.
fn json_array(
    args: &[JsValue],
    idx: usize,
    ctx: &mut Context,
) -> JsResult<Option<Vec<serde_json::Value>>> {
    let json = arg_string(args, idx, ctx)?;
    Ok(serde_json::from_str::<serde_json::Value>(&json)
        .ok()
        .and_then(|v| v.as_array().cloned()))
}

/// Parse a `{dimension: member}` JSON object argument into a coordinate map.
fn parse_coord(json: &str) -> BTreeMap<String, String> {
    let mut coord = BTreeMap::new();
    if let Ok(serde_json::Value::Object(obj)) = serde_json::from_str::<serde_json::Value>(json) {
        for (dim, member) in obj {
            if let Some(m) = member.as_str() {
                coord.insert(dim, m.to_string());
            }
        }
    }
    coord
}

fn read_error(e: &FlowReadError) -> boa_engine::JsError {
    JsNativeError::error().with_message(e.to_string()).into()
}

/// Shared scaffold for the staging host functions (`cubeEnsureElementsJson`,
/// `cubeAddChildJson`, `cubeWriteCellsJson`, `dimEnsureElementsJson`,
/// `dimAddChildJson`): parse the target name (`args[0]`) and the JSON item array
/// (`args[1]`, a no-op when absent or not an array), then under the run state run
/// `stage` -- which pushes the parsed specs and returns how many it staged -- and
/// enforce the `MAX_STAGED` budget exactly (the running total is bumped by the
/// staged count and exceeding the cap throws [`budget_error`]). Each host function
/// supplies only its specific `stage` body.
fn stage_items(
    args: &[JsValue],
    ctx: &mut Context,
    stage: impl FnOnce(&mut FlowState, String, &[serde_json::Value]) -> usize,
) -> JsResult<JsValue> {
    let name = arg_string(args, 0, ctx)?;
    let Some(items) = json_array(args, 1, ctx)? else {
        return Ok(JsValue::undefined());
    };
    let over = with_flow(|s| {
        let added = stage(s, name, &items);
        s.staged += added;
        over_budget(s)
    });
    if over {
        return Err(budget_error());
    }
    Ok(JsValue::undefined())
}

fn host_cube_ensure_elements(
    _t: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    stage_items(args, ctx, |s, cube, items| {
        let mut added = 0;
        let entry = s.cubes.entry(cube).or_default();
        for item in items {
            let dimension = str_field(item, "dimension");
            let name = str_field(item, "name");
            if !dimension.is_empty() && !name.is_empty() {
                entry.elements.push(ElementSpec {
                    dimension,
                    name,
                    kind: parse_kind(item),
                });
                added += 1;
            }
        }
        added
    })
}

fn host_cube_add_child(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    stage_items(args, ctx, |s, cube, items| {
        let mut added = 0;
        let entry = s.cubes.entry(cube).or_default();
        for item in items {
            let dimension = str_field(item, "dimension");
            let parent = str_field(item, "parent");
            let child = str_field(item, "child");
            let weight = item.get("weight").and_then(|w| w.as_i64()).unwrap_or(1);
            if !dimension.is_empty() && !parent.is_empty() && !child.is_empty() {
                entry.edges.push(EdgeSpec {
                    dimension,
                    parent,
                    child,
                    weight,
                });
                added += 1;
            }
        }
        added
    })
}

fn host_cube_write_cells(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    stage_items(args, ctx, |s, cube, items| {
        let mut added = 0;
        let entry = s.cubes.entry(cube).or_default();
        for item in items {
            let value = str_field(item, "value");
            let mut coord = BTreeMap::new();
            if let Some(obj) = item.get("coord").and_then(|c| c.as_object()) {
                for (dim, member) in obj {
                    if let Some(m) = member.as_str() {
                        coord.insert(dim.clone(), m.to_string());
                    }
                }
            }
            if !coord.is_empty() {
                entry.cells.push(PlannedCell { coord, value });
                added += 1;
            }
        }
        added
    })
}

fn host_dim_ensure_elements(
    _t: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    stage_items(args, ctx, |s, dim, items| {
        let mut added = 0;
        let entry = s.dimensions.entry(dim.clone()).or_default();
        for item in items {
            let name = str_field(item, "name");
            if !name.is_empty() {
                entry.elements.push(ElementSpec {
                    dimension: dim.clone(),
                    name,
                    kind: parse_kind(item),
                });
                added += 1;
            }
        }
        added
    })
}

fn host_dim_add_child(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    stage_items(args, ctx, |s, dim, items| {
        let mut added = 0;
        let entry = s.dimensions.entry(dim.clone()).or_default();
        for item in items {
            let parent = str_field(item, "parent");
            let child = str_field(item, "child");
            let weight = item.get("weight").and_then(|w| w.as_i64()).unwrap_or(1);
            if !parent.is_empty() && !child.is_empty() {
                entry.edges.push(EdgeSpec {
                    dimension: dim.clone(),
                    parent,
                    child,
                    weight,
                });
                added += 1;
            }
        }
        added
    })
}

fn host_cube_read_cell(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let cube = arg_string(args, 0, ctx)?;
    let coord = parse_coord(&arg_string(args, 1, ctx)?);
    let result = with_flow(|s| s.reader.read_cell(&cube, &coord));
    match result {
        Ok(cell) => Ok(cell
            .numeric
            .map(|v| JsValue::from(js_string!(v)))
            .unwrap_or(JsValue::null())),
        Err(e) => Err(read_error(&e)),
    }
}

fn host_cube_read_text(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let cube = arg_string(args, 0, ctx)?;
    let coord = parse_coord(&arg_string(args, 1, ctx)?);
    let result = with_flow(|s| s.reader.read_cell(&cube, &coord));
    match result {
        Ok(cell) => Ok(cell
            .text
            .map(|v| JsValue::from(js_string!(v)))
            .unwrap_or(JsValue::null())),
        Err(e) => Err(read_error(&e)),
    }
}

fn host_cube_members(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let cube = arg_string(args, 0, ctx)?;
    let dim = arg_string(args, 1, ctx)?;
    let result = with_flow(|s| s.reader.cube_members(&cube, &dim));
    match result {
        Ok(members) => Ok(JsValue::from(js_string!(string_array_json(&members)))),
        Err(e) => Err(read_error(&e)),
    }
}

fn host_dim_members(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let dim = arg_string(args, 0, ctx)?;
    let result = with_flow(|s| s.reader.dimension_members(&dim));
    match result {
        Ok(members) => Ok(JsValue::from(js_string!(string_array_json(&members)))),
        Err(e) => Err(read_error(&e)),
    }
}

fn host_cube_property(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let cube = arg_string(args, 0, ctx)?;
    let key = arg_string(args, 1, ctx)?;
    let result = with_flow(|s| s.reader.cube_property(&cube, &key));
    match result {
        Ok(Some(v)) => Ok(JsValue::from(js_string!(v))),
        Ok(None) => Ok(JsValue::null()),
        Err(e) => Err(read_error(&e)),
    }
}

fn str_field(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn string_array_json(items: &[String]) -> String {
    serde_json::Value::Array(
        items
            .iter()
            .map(|s| serde_json::Value::String(s.clone()))
            .collect(),
    )
    .to_string()
}

fn rows_to_json_value(rows: &[Row]) -> serde_json::Value {
    serde_json::Value::Array(
        rows.iter()
            .map(|row| {
                let mut m = serde_json::Map::new();
                for (k, v) in row {
                    m.insert(k.clone(), serde_json::Value::String(v.clone()));
                }
                serde_json::Value::Object(m)
            })
            .collect(),
    )
}

fn inputs_to_json(inputs: &BTreeMap<String, Vec<Row>>) -> String {
    let mut m = serde_json::Map::new();
    for (name, rows) in inputs {
        m.insert(name.clone(), rows_to_json_value(rows));
    }
    serde_json::Value::Object(m).to_string()
}

fn params_to_json(params: &BTreeMap<String, String>) -> String {
    let mut m = serde_json::Map::new();
    for (k, v) in params {
        m.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    serde_json::Value::Object(m).to_string()
}

/// Drop duplicate element and edge specs (the cube layer is idempotent anyway,
/// but a deterministic, deduplicated outcome is cleaner to apply and report).
///
/// `Vec::retain` visits elements front-to-back and keeps the first occurrence of
/// each key, so the output order is the flow's staging order; the `HashSet` only
/// tracks membership, and its (randomized) iteration order never affects the
/// retained sequence. The result is therefore deterministic.
fn dedup_specs(outcome: &mut FlowOutcome) {
    for c in outcome.cubes.values_mut() {
        let mut seen_el = std::collections::HashSet::new();
        c.elements
            .retain(|e| seen_el.insert((e.dimension.clone(), e.name.clone(), e.kind)));
        let mut seen_edge = std::collections::HashSet::new();
        c.edges
            .retain(|e| seen_edge.insert((e.dimension.clone(), e.parent.clone(), e.child.clone())));
    }
    for d in outcome.dimensions.values_mut() {
        let mut seen_el = std::collections::HashSet::new();
        d.elements
            .retain(|e| seen_el.insert((e.name.clone(), e.kind)));
        let mut seen_edge = std::collections::HashSet::new();
        d.edges
            .retain(|e| seen_edge.insert((e.parent.clone(), e.child.clone())));
    }
}

const DETERMINISM_PRELUDE: &str = "\
delete globalThis.Date;
Math.random = function () { throw new Error('Math.random is forbidden in flows (use ctx.now or seeded values)'); };
";

const CTX_PRELUDE: &str = "\
var __inputs = JSON.parse(__host.inputsJson());
var __inputNames = Object.keys(__inputs);
var __params = JSON.parse(__host.paramsJson());
var __cubes = JSON.parse(__host.cubeNamesJson());
function __cell(coord, value) { return { coord: coord, value: String(value) }; }
function __w(weight) { return (weight === undefined ? 1 : weight); }
// A cube-scoped handle (ADR-0035): element/edge/cell ops stage against the cube;
// reads go through the live, principal-masked host reader.
function __cubeHandle(cube) {
  return {
    name: cube,
    ensureElement: function (dim, name) {
      __host.cubeEnsureElementsJson(cube, JSON.stringify([{ dimension: dim, name: String(name), kind: 'leaf' }]));
    },
    ensureElements: function (dim, names) {
      __host.cubeEnsureElementsJson(cube, JSON.stringify(names.map(function (n) {
        return { dimension: dim, name: String(n), kind: 'leaf' };
      })));
    },
    ensureConsolidated: function (dim, name) {
      __host.cubeEnsureElementsJson(cube, JSON.stringify([{ dimension: dim, name: String(name), kind: 'consolidated' }]));
    },
    addChild: function (dim, parent, child, weight) {
      __host.cubeAddChildJson(cube, JSON.stringify([{ dimension: dim, parent: String(parent), child: String(child), weight: __w(weight) }]));
    },
    writeCell: function (coord, value) { __host.cubeWriteCellsJson(cube, JSON.stringify([__cell(coord, value)])); },
    writeCells: function (arr) {
      __host.cubeWriteCellsJson(cube, JSON.stringify(arr.map(function (c) { return __cell(c.coord, c.value); })));
    },
    readCell: function (coord) { return __host.cubeReadCellJson(cube, JSON.stringify(coord)); },
    readText: function (coord) { return __host.cubeReadTextJson(cube, JSON.stringify(coord)); },
    members: function (dim) { return JSON.parse(__host.cubeMembersJson(cube, String(dim))); },
    property: function (key) { return __host.cubePropertyJson(cube, String(key)); }
  };
}
// A global-dimension handle (ADR-0035): grows the registry dimension `dimName`,
// fanning out to every cube that uses it.
function __dimHandle(dimName) {
  return {
    name: dimName,
    ensureElement: function (name) {
      __host.dimEnsureElementsJson(dimName, JSON.stringify([{ name: String(name), kind: 'leaf' }]));
    },
    ensureElements: function (names) {
      __host.dimEnsureElementsJson(dimName, JSON.stringify(names.map(function (n) {
        return { name: String(n), kind: 'leaf' };
      })));
    },
    ensureConsolidated: function (name) {
      __host.dimEnsureElementsJson(dimName, JSON.stringify([{ name: String(name), kind: 'consolidated' }]));
    },
    addChild: function (parent, child, weight) {
      __host.dimAddChildJson(dimName, JSON.stringify([{ parent: String(parent), child: String(child), weight: __w(weight) }]));
    },
    members: function () { return JSON.parse(__host.dimMembersJson(dimName)); }
  };
}
function __default() {
  var c = __host.defaultCube();
  if (!c) { throw new Error('this flow has no default cube; address one explicitly with ctx.cube(name)'); }
  return __cubeHandle(c);
}
var ctx = {
  input: function (name) {
    if (name === undefined) {
      if (__inputNames.length === 1) { return __inputs[__inputNames[0]]; }
      if (__inputNames.length === 0) { return []; }
      throw new Error('this flow has multiple data sources; name one with ctx.input(name): ' + __inputNames.join(', '));
    }
    var key = String(name);
    if (!Object.prototype.hasOwnProperty.call(__inputs, key)) { throw new Error('no data source named ' + key); }
    return __inputs[key];
  },
  sources: function () { return __inputNames.slice(); },
  param: function (name) { return __params[name]; },
  cubes: function () { return __cubes.slice(); },
  cube: function (name) { return __cubeHandle(String(name)); },
  dimension: function (name) { return __dimHandle(String(name)); },
  now: function () { return __host.now(); },
  log: function (msg) { __host.log(String(msg)); },
  // Legacy cube-less ops route to the flow's default cube (ADR-0035 back-compat).
  ensureElement: function (dim, name) { __default().ensureElement(dim, name); },
  ensureElements: function (dim, names) { __default().ensureElements(dim, names); },
  ensureConsolidated: function (dim, name) { __default().ensureConsolidated(dim, name); },
  addChild: function (dim, parent, child, weight) { __default().addChild(dim, parent, child, weight); },
  writeCell: function (coord, value) { __default().writeCell(coord, value); },
  writeCells: function (arr) { __default().writeCells(arr); }
};
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csv::parse_csv;

    /// Run `src` over a single unnamed CSV source against a default cube "Sales".
    fn run(src: &str, csv: &str) -> Result<FlowOutcome, FlowError> {
        let rows = parse_csv(csv).unwrap();
        let mut inputs = BTreeMap::new();
        inputs.insert("data".to_string(), rows);
        run_flow(
            src,
            Some("Sales"),
            &["Sales".to_string()],
            inputs,
            &BTreeMap::new(),
            1_577_836_800_000,
            Box::new(NullReader),
        )
    }

    /// The "Sales" cube's staged changes from an outcome (the tests' single target).
    fn sales(out: &FlowOutcome) -> &CubeChanges {
        out.cubes.get("Sales").expect("Sales target staged")
    }

    #[test]
    fn loads_csv_into_elements_and_cells() {
        let src = "\
function rows(ctx) {
  const data = ctx.input();
  const regions = data.map(function (r) { return r.Region; });
  ctx.ensureElements('Region', regions);
  const cells = data.map(function (r) {
    return { coord: { Region: r.Region, Measure: 'Sales' }, value: r.Value };
  });
  ctx.writeCells(cells);
}";
        let out = run(src, "Region,Value\nNorth,100\nSouth,200\n").unwrap();
        assert_eq!(out.report.rows_read, 2);
        assert_eq!(out.report.cells_written, 2);
        let s = sales(&out);
        assert_eq!(s.elements.len(), 2);
        assert!(s
            .elements
            .iter()
            .any(|e| e.name == "North" && e.kind == ElementKind::Leaf));
        let north = s
            .cells
            .iter()
            .find(|c| c.coord.get("Region") == Some(&"North".to_string()))
            .unwrap();
        assert_eq!(north.value, "100"); // exact, from the CSV text (not an f64)
    }

    #[test]
    fn stages_run_in_order_and_logs() {
        let src = "\
function init(ctx) { ctx.log('init'); }
function rows(ctx) { ctx.log('rows ' + ctx.input().length); }
function finalize(ctx) { ctx.log('done'); }";
        let out = run(src, "A\n1\n2\n3\n").unwrap();
        assert_eq!(out.report.logs, vec!["init", "rows 3", "done"]);
    }

    #[test]
    fn now_uses_injected_clock_and_date_is_gone() {
        let src =
            "function rows(ctx) { ctx.log(String(ctx.now())); ctx.log(String(typeof Date)); }";
        let out = run(src, "A\n1\n").unwrap();
        assert_eq!(out.report.logs[0], "1577836800000");
        assert_eq!(out.report.logs[1], "undefined");
    }

    #[test]
    fn math_random_is_forbidden() {
        let src = "function rows(ctx) { Math.random(); }";
        let err = run(src, "A\n1\n").unwrap_err();
        assert!(matches!(err, FlowError::Runtime { .. }));
        assert!(err.to_string().contains("forbidden"));
    }

    #[test]
    fn a_thrown_error_is_reported() {
        let src = "function rows(ctx) { throw new Error('boom'); }";
        let err = run(src, "A\n1\n").unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn typescript_is_stripped_before_running() {
        let src = "\
function rows(ctx: FlowContext): void {
  const regions: string[] = ctx.input().map(function (r) { return r.Region; });
  ctx.ensureElements('Region', regions);
}";
        let out = run(src, "Region\nNorth\n").unwrap();
        assert_eq!(sales(&out).elements.len(), 1);
        assert_eq!(sales(&out).elements[0].name, "North");
    }

    #[test]
    fn consolidation_and_edges() {
        let src = "\
function schema(ctx) {
  ctx.ensureElements('Region', ['North', 'South']);
  ctx.ensureConsolidated('Region', 'Total');
  ctx.addChild('Region', 'Total', 'North', 1);
  ctx.addChild('Region', 'Total', 'South', 1);
}";
        let out = run(src, "").unwrap();
        let s = sales(&out);
        assert_eq!(s.edges.len(), 2);
        assert!(s
            .elements
            .iter()
            .any(|e| e.name == "Total" && e.kind == ElementKind::Consolidated));
    }

    #[test]
    fn explicit_cube_handle_targets_a_named_cube() {
        let src = "\
function rows(ctx) {
  ctx.cube('Forecast').ensureElement('Region', 'West');
  ctx.cube('Forecast').writeCell({ Region: 'West', Measure: 'Sales' }, '42');
}";
        let out = run(src, "A\n1\n").unwrap();
        let f = out.cubes.get("Forecast").expect("Forecast target");
        assert_eq!(f.elements.len(), 1);
        assert_eq!(f.cells.len(), 1);
        assert_eq!(f.cells[0].value, "42");
        assert!(!out.cubes.contains_key("Sales"));
    }

    #[test]
    fn dimension_handle_stages_global_dimension_growth() {
        let src = "\
function schema(ctx) {
  const region = ctx.dimension('Region');
  region.ensureElements(['North', 'South']);
  region.ensureConsolidated('Total');
  region.addChild('Total', 'North', 1);
}";
        let out = run(src, "").unwrap();
        let d = out.dimensions.get("Region").expect("Region dim growth");
        assert_eq!(d.edges.len(), 1);
        assert!(d.elements.iter().any(|e| e.name == "Total"));
    }

    #[test]
    fn multiple_named_sources_are_addressable() {
        let src = "\
function rows(ctx) {
  ctx.log('sources ' + ctx.sources().sort().join(','));
  ctx.log('a ' + ctx.input('a').length);
  ctx.log('b ' + ctx.input('local.b').length);
}";
        let mut inputs = BTreeMap::new();
        inputs.insert("a".to_string(), parse_csv("X\n1\n2\n").unwrap());
        inputs.insert("local.b".to_string(), parse_csv("Y\n9\n").unwrap());
        let out = run_flow(
            src,
            None,
            &[],
            inputs,
            &BTreeMap::new(),
            0,
            Box::new(NullReader),
        )
        .unwrap();
        assert_eq!(out.report.logs[0], "sources a,local.b");
        assert_eq!(out.report.logs[1], "a 2");
        assert_eq!(out.report.logs[2], "b 1");
    }

    #[test]
    fn input_with_no_name_errors_when_several_sources() {
        let src = "function rows(ctx) { ctx.input(); }";
        let mut inputs = BTreeMap::new();
        inputs.insert("a".to_string(), Vec::new());
        inputs.insert("b".to_string(), Vec::new());
        let err = run_flow(
            src,
            None,
            &[],
            inputs,
            &BTreeMap::new(),
            0,
            Box::new(NullReader),
        )
        .unwrap_err();
        assert!(err.to_string().contains("multiple data sources"));
    }

    #[test]
    fn a_read_against_the_null_reader_throws() {
        let src = "function rows(ctx) { ctx.cube('Sales').readCell({ Region: 'North' }); }";
        let err = run(src, "A\n1\n").unwrap_err();
        assert!(matches!(err, FlowError::Runtime { .. }));
    }

    #[test]
    fn is_deterministic_run_to_run() {
        let src = "function rows(ctx) { ctx.input().forEach(function (r) { ctx.ensureElement('Region', r.Region); }); }";
        let a = run(src, "Region\nNorth\nSouth\n").unwrap();
        let b = run(src, "Region\nNorth\nSouth\n").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn runtime_limit_stops_an_infinite_loop() {
        let src = "function rows(ctx) { while (true) {} }";
        let err = run(src, "A\n1\n").unwrap_err();
        assert!(matches!(err, FlowError::Runtime { .. }));
    }

    #[test]
    fn unsupported_typescript_is_a_strip_error() {
        let src = "enum E { A, B }\nfunction rows(ctx) {}";
        let err = run(src, "").unwrap_err();
        assert!(matches!(err, FlowError::Strip(_)));
    }

    /// A fixture reader so a read path can be exercised deterministically.
    struct FixtureReader;
    impl FlowReader for FixtureReader {
        fn read_cell(
            &self,
            _cube: &str,
            coord: &BTreeMap<String, String>,
        ) -> Result<FlowCell, FlowReadError> {
            if coord.get("Region").map(String::as_str) == Some("North") {
                Ok(FlowCell {
                    numeric: Some("100".to_string()),
                    text: None,
                })
            } else {
                Ok(FlowCell::default())
            }
        }
        fn cube_members(&self, _cube: &str, _dim: &str) -> Result<Vec<String>, FlowReadError> {
            Ok(vec!["North".to_string(), "South".to_string()])
        }
        fn dimension_members(&self, _dim: &str) -> Result<Vec<String>, FlowReadError> {
            Ok(vec!["North".to_string()])
        }
        fn cube_property(&self, _cube: &str, key: &str) -> Result<Option<String>, FlowReadError> {
            Ok((key == "description").then(|| "the sales cube".to_string()))
        }
    }

    #[test]
    fn reads_go_through_the_injected_reader() {
        let src = "\
function rows(ctx) {
  const sales = ctx.cube('Sales');
  ctx.log('cell ' + sales.readCell({ Region: 'North' }));
  ctx.log('empty ' + sales.readCell({ Region: 'South' }));
  ctx.log('members ' + sales.members('Region').join(','));
  ctx.log('prop ' + sales.property('description'));
}";
        let rows = parse_csv("A\n1\n").unwrap();
        let mut inputs = BTreeMap::new();
        inputs.insert("data".to_string(), rows);
        let out = run_flow(
            src,
            Some("Sales"),
            &["Sales".to_string()],
            inputs,
            &BTreeMap::new(),
            0,
            Box::new(FixtureReader),
        )
        .unwrap();
        assert_eq!(out.report.logs[0], "cell 100");
        assert_eq!(out.report.logs[1], "empty null");
        assert_eq!(out.report.logs[2], "members North,South");
        assert_eq!(out.report.logs[3], "prop the sales cube");
    }
}
