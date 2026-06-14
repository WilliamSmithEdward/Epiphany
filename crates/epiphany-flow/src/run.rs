//! Running a flow on the embedded JavaScript engine (boa, ADR-0004).
//!
//! [`run_flow`] strips the flow's TypeScript to JavaScript, runs it on a fresh,
//! sandboxed boa context against a host API, and returns a [`FlowOutcome`]: the
//! dimension elements/edges and cell writes the flow staged, plus a report. The
//! runner is pure with respect to the model: it never touches the engine. The
//! caller (the API layer) applies the outcome's element and cell changes
//! transactionally. This keeps `epiphany-flow` dependent on `epiphany-core` only,
//! and makes a flow unit-testable without a running server.
//!
//! ## The flow programming model
//! A flow declares up to four top-level functions, called in order with a single
//! `ctx` argument: `init`, `schema`, `rows`, `finalize` (all optional; most flows
//! implement `rows`). The host API on `ctx` is vectorized so the script is never
//! on the per-cell hot path:
//! - `ctx.input()` -> the data-source rows (an array of `{column: value}`).
//! - `ctx.param(name)`, `ctx.cubeName()`, `ctx.now()`, `ctx.log(msg)`.
//! - `ctx.ensureElements(dim, names)`, `ctx.ensureElement(dim, name)`,
//!   `ctx.ensureConsolidated(dim, name)`, `ctx.addChild(dim, parent, child, w)`.
//! - `ctx.writeCells([{coord: {Dim: Member, ...}, value}])`, `ctx.writeCell(...)`.
//!
//! ## Determinism and sandboxing
//! `Date` is removed and `Math.random` throws (a flow re-run on the same input is
//! byte-identical; ADR-0009). `ctx.now()` returns the injected clock. There is no
//! filesystem or network access (the host exposes only the API above). A bounded
//! loop-iteration and recursion budget caps runaway scripts deterministically
//! (no wall-clock timeout, which would be nondeterministic).

use std::cell::RefCell;
use std::collections::BTreeMap;

use boa_engine::object::ObjectInitializer;
use boa_engine::property::Attribute;
use boa_engine::{js_string, Context, JsResult, JsValue, NativeFunction, Source};
use epiphany_core::{EdgeSpec, ElementKind, ElementSpec};

use crate::csv::Row;
use crate::strip::{strip_types, StripError};

/// Loop-iteration budget: bounds a runaway flow deterministically.
const LOOP_LIMIT: u64 = 50_000_000;
/// Recursion-depth budget.
const RECURSION_LIMIT: usize = 800;

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
    /// Number of input rows the flow was given.
    pub rows_read: usize,
    /// Number of cell writes the flow staged.
    pub cells_written: usize,
    /// `ctx.log(...)` lines, in order.
    pub logs: Vec<String>,
}

/// The result of running a flow: the staged schema and cell changes plus a report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlowOutcome {
    /// Dimension elements to ensure (append-only, idempotent).
    pub elements: Vec<ElementSpec>,
    /// Consolidation edges to ensure.
    pub edges: Vec<EdgeSpec>,
    /// Cell writes to apply (after elements exist).
    pub cells: Vec<PlannedCell>,
    /// The run report.
    pub report: FlowReport,
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
#[derive(Default)]
struct FlowState {
    input_json: String,
    params_json: String,
    cube: String,
    now_millis: u64,
    elements: Vec<ElementSpec>,
    edges: Vec<EdgeSpec>,
    cells: Vec<PlannedCell>,
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

/// Run `source` (a flow's TypeScript) over `rows`, with `params`, against cube
/// `cube`, using `now_millis` for `ctx.now()`. Returns the staged outcome.
pub fn run_flow(
    source: &str,
    cube: &str,
    rows: Vec<Row>,
    params: &BTreeMap<String, String>,
    now_millis: u64,
) -> Result<FlowOutcome, FlowError> {
    let js = strip_types(source)?;

    let state = FlowState {
        input_json: rows_to_json(&rows),
        params_json: params_to_json(params),
        cube: cube.to_string(),
        now_millis,
        rows_read: rows.len(),
        ..Default::default()
    };
    FLOW.with(|cell| *cell.borrow_mut() = Some(state));
    // Always clear the thread-local on the way out, even on error.
    let result = run_inner(&js);
    let finished = FLOW.with(|cell| cell.borrow_mut().take());

    match result {
        Ok(()) => {
            let s = finished.unwrap_or_default();
            let mut outcome = FlowOutcome {
                report: FlowReport {
                    rows_read: s.rows_read,
                    cells_written: s.cells.len(),
                    logs: s.logs,
                },
                elements: s.elements,
                edges: s.edges,
                cells: s.cells,
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
            NativeFunction::from_fn_ptr(host_input_json),
            js_string!("inputJson"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(host_params_json),
            js_string!("paramsJson"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(host_cube_name),
            js_string!("cubeName"),
            0,
        )
        .function(NativeFunction::from_fn_ptr(host_now), js_string!("now"), 0)
        .function(NativeFunction::from_fn_ptr(host_log), js_string!("log"), 1)
        .function(
            NativeFunction::from_fn_ptr(host_ensure_elements),
            js_string!("ensureElementsJson"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(host_add_child),
            js_string!("addChildJson"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(host_write_cells),
            js_string!("writeCellsJson"),
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

fn host_input_json(_t: &JsValue, _a: &[JsValue], _c: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(js_string!(with_flow(|s| s
        .input_json
        .clone()))))
}

fn host_params_json(_t: &JsValue, _a: &[JsValue], _c: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(js_string!(with_flow(|s| s
        .params_json
        .clone()))))
}

fn host_cube_name(_t: &JsValue, _a: &[JsValue], _c: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(js_string!(with_flow(|s| s.cube.clone()))))
}

fn host_now(_t: &JsValue, _a: &[JsValue], _c: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(with_flow(|s| s.now_millis as f64)))
}

fn host_log(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let msg = arg_string(args, 0, ctx)?;
    with_flow(|s| s.logs.push(msg));
    Ok(JsValue::undefined())
}

fn host_ensure_elements(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let json = arg_string(args, 0, ctx)?;
    let arr: serde_json::Value = serde_json::from_str(&json).unwrap_or(serde_json::Value::Null);
    if let Some(items) = arr.as_array() {
        with_flow(|s| {
            for item in items {
                let dimension = str_field(item, "dimension");
                let name = str_field(item, "name");
                let kind = match str_field(item, "kind").as_str() {
                    "consolidated" => ElementKind::Consolidated,
                    "string" => ElementKind::String,
                    _ => ElementKind::Leaf,
                };
                if !dimension.is_empty() && !name.is_empty() {
                    s.elements.push(ElementSpec {
                        dimension,
                        name,
                        kind,
                    });
                }
            }
        });
    }
    Ok(JsValue::undefined())
}

fn host_add_child(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let json = arg_string(args, 0, ctx)?;
    let arr: serde_json::Value = serde_json::from_str(&json).unwrap_or(serde_json::Value::Null);
    if let Some(items) = arr.as_array() {
        with_flow(|s| {
            for item in items {
                let dimension = str_field(item, "dimension");
                let parent = str_field(item, "parent");
                let child = str_field(item, "child");
                let weight = item.get("weight").and_then(|w| w.as_i64()).unwrap_or(1);
                if !dimension.is_empty() && !parent.is_empty() && !child.is_empty() {
                    s.edges.push(EdgeSpec {
                        dimension,
                        parent,
                        child,
                        weight,
                    });
                }
            }
        });
    }
    Ok(JsValue::undefined())
}

fn host_write_cells(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let json = arg_string(args, 0, ctx)?;
    let arr: serde_json::Value = serde_json::from_str(&json).unwrap_or(serde_json::Value::Null);
    if let Some(items) = arr.as_array() {
        with_flow(|s| {
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
                    s.cells.push(PlannedCell { coord, value });
                }
            }
        });
    }
    Ok(JsValue::undefined())
}

fn str_field(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn rows_to_json(rows: &[Row]) -> String {
    let arr: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            let mut m = serde_json::Map::new();
            for (k, v) in row {
                m.insert(k.clone(), serde_json::Value::String(v.clone()));
            }
            serde_json::Value::Object(m)
        })
        .collect();
    serde_json::Value::Array(arr).to_string()
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
fn dedup_specs(outcome: &mut FlowOutcome) {
    let mut seen_el = std::collections::HashSet::new();
    outcome
        .elements
        .retain(|e| seen_el.insert((e.dimension.clone(), e.name.clone(), e.kind)));
    let mut seen_edge = std::collections::HashSet::new();
    outcome
        .edges
        .retain(|e| seen_edge.insert((e.dimension.clone(), e.parent.clone(), e.child.clone())));
}

const DETERMINISM_PRELUDE: &str = "\
delete globalThis.Date;
Math.random = function () { throw new Error('Math.random is forbidden in flows (use ctx.now or seeded values)'); };
";

const CTX_PRELUDE: &str = "\
var __input = JSON.parse(__host.inputJson());
var __params = JSON.parse(__host.paramsJson());
function __cell(coord, value) { return { coord: coord, value: String(value) }; }
var ctx = {
  input: function () { return __input; },
  param: function (name) { return __params[name]; },
  cubeName: function () { return __host.cubeName(); },
  now: function () { return __host.now(); },
  log: function (msg) { __host.log(String(msg)); },
  ensureElement: function (dim, name) {
    __host.ensureElementsJson(JSON.stringify([{ dimension: dim, name: String(name), kind: 'leaf' }]));
  },
  ensureElements: function (dim, names) {
    __host.ensureElementsJson(JSON.stringify(names.map(function (n) {
      return { dimension: dim, name: String(n), kind: 'leaf' };
    })));
  },
  ensureConsolidated: function (dim, name) {
    __host.ensureElementsJson(JSON.stringify([{ dimension: dim, name: String(name), kind: 'consolidated' }]));
  },
  addChild: function (dim, parent, child, weight) {
    __host.addChildJson(JSON.stringify([{ dimension: dim, parent: String(parent), child: String(child), weight: (weight === undefined ? 1 : weight) }]));
  },
  writeCell: function (coord, value) { __host.writeCellsJson(JSON.stringify([__cell(coord, value)])); },
  writeCells: function (arr) {
    __host.writeCellsJson(JSON.stringify(arr.map(function (c) { return __cell(c.coord, c.value); })));
  }
};
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csv::parse_csv;

    fn run(src: &str, csv: &str) -> Result<FlowOutcome, FlowError> {
        let rows = parse_csv(csv).unwrap();
        run_flow(src, "Sales", rows, &BTreeMap::new(), 1_577_836_800_000)
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
        assert_eq!(out.elements.len(), 2);
        assert!(out
            .elements
            .iter()
            .any(|e| e.name == "North" && e.kind == ElementKind::Leaf));
        let north = out
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
        assert_eq!(out.elements.len(), 1);
        assert_eq!(out.elements[0].name, "North");
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
        assert_eq!(out.edges.len(), 2);
        assert!(out
            .elements
            .iter()
            .any(|e| e.name == "Total" && e.kind == ElementKind::Consolidated));
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
}
