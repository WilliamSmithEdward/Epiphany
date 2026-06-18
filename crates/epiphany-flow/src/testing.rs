//! The flow testing framework: deterministic flow unit tests.
//!
//! A [`FlowTest`] pins a flow's data sources (inline content) and parameters and
//! asserts the resulting cell values in one target cube. [`run_flow_tests`] runs
//! each test against a throwaway clone of its target cube so a test never mutates
//! the live model: it parses the pinned inputs, runs the named flow with a
//! [`NullReader`] (tests do not read live state), applies that cube's staged
//! elements and cells to the clone, and checks each assertion's exact [`Fixed`]
//! value. No clock or RNG, so results are deterministic.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use epiphany_core::{Automation, Cube, ElementKind, Fixed, ModelError};
use epiphany_determinism::Deterministic;

use crate::csv::parse_csv;
use crate::run::{run_flow, CubeChanges, FlowError, NullReader};

/// The result of running one flow test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowTestOutcome {
    /// The test name.
    pub name: String,
    /// Whether every assertion held and the flow ran cleanly.
    pub passed: bool,
    /// The assertions that failed (empty when passed).
    pub failures: Vec<AssertionFailure>,
}

/// One failed assertion: where, what was expected, and what was computed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssertionFailure {
    /// The asserted coordinate (dimension -> member).
    pub coord: BTreeMap<String, String>,
    /// The expected value (decimal string).
    pub expected: String,
    /// The actual computed value (decimal string).
    pub actual: String,
}

/// A failure that prevents the tests from running at all, as opposed to an
/// assertion failure.
#[derive(Debug, Clone)]
pub enum FlowTestError {
    /// The test names a flow the automation model does not define.
    UnknownFlow { test: String, flow: String },
    /// The test does not name a target cube and its flow has no default cube.
    NoTargetCube { test: String },
    /// The test's target cube is not known to the runner.
    UnknownCube { test: String, cube: String },
    /// The test's input could not be parsed as CSV.
    BadInput { test: String, message: String },
    /// The flow failed to run.
    Flow { test: String, message: String },
    /// Applying the flow's output to the cube failed (bad coordinate, etc.).
    Apply { test: String, message: String },
    /// An assertion coordinate or value was invalid.
    Model(ModelError),
}

impl fmt::Display for FlowTestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FlowTestError::UnknownFlow { test, flow } => {
                write!(f, "test '{test}' references unknown flow '{flow}'")
            }
            FlowTestError::NoTargetCube { test } => {
                write!(
                    f,
                    "test '{test}' names no target cube and its flow has no default cube"
                )
            }
            FlowTestError::UnknownCube { test, cube } => {
                write!(f, "test '{test}' targets unknown cube '{cube}'")
            }
            FlowTestError::BadInput { test, message } => {
                write!(f, "test '{test}' input: {message}")
            }
            FlowTestError::Flow { test, message } => write!(f, "test '{test}': {message}"),
            FlowTestError::Apply { test, message } => write!(f, "test '{test}': {message}"),
            FlowTestError::Model(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for FlowTestError {}

/// Run every flow test in the global automation model, in name order. Each test
/// asserts cell values in one target cube (the test's `cube`, else the flow's
/// `default_cube`), resolved to a fresh cube clone by `cube_for`. A malformed
/// test (unknown flow, missing/unknown target cube, bad input, a flow that
/// errors, or a bad coordinate) is a [`FlowTestError`]; a value mismatch is
/// recorded as an [`AssertionFailure`].
pub fn run_flow_tests(
    automation: &Automation,
    cube_for: impl Fn(&str) -> Option<Cube>,
) -> Result<Vec<FlowTestOutcome>, FlowTestError> {
    let mut outcomes = Vec::with_capacity(automation.flow_tests.len());
    for test in automation.flow_tests.values() {
        let flow = automation
            .flows
            .get(&test.flow)
            .ok_or_else(|| FlowTestError::UnknownFlow {
                test: test.name.clone(),
                flow: test.flow.clone(),
            })?;

        let target = test
            .cube
            .clone()
            .or_else(|| flow.default_cube.clone())
            .ok_or_else(|| FlowTestError::NoTargetCube {
                test: test.name.clone(),
            })?;
        let mut cube = cube_for(&target).ok_or_else(|| FlowTestError::UnknownCube {
            test: test.name.clone(),
            cube: target.clone(),
        })?;

        // Build the named-source map: pinned per-source content, with the
        // sole-source `input` keyed under the flow's first declared source (or a
        // default name) so `ctx.input()` returns it.
        let mut inputs = BTreeMap::new();
        for (addr, content) in &test.inputs {
            let rows = parse_csv(content).map_err(|e| FlowTestError::BadInput {
                test: test.name.clone(),
                message: e.to_string(),
            })?;
            inputs.insert(addr.clone(), rows);
        }
        if !test.input.is_empty() && inputs.is_empty() {
            let key = flow
                .inputs
                .first()
                .map(|i| i.address())
                .unwrap_or_else(|| "data".to_string());
            let rows = parse_csv(&test.input).map_err(|e| FlowTestError::BadInput {
                test: test.name.clone(),
                message: e.to_string(),
            })?;
            inputs.insert(key, rows);
        }

        let outcome = run_flow(
            &flow.source,
            Some(&target),
            std::slice::from_ref(&target),
            inputs,
            &test.params,
            Deterministic::EPOCH_2020_MILLIS,
            Box::new(NullReader),
        )
        .map_err(|e: FlowError| FlowTestError::Flow {
            test: test.name.clone(),
            message: e.to_string(),
        })?;

        // Apply only the target cube's staged changes to the clone.
        if let Some(changes) = outcome.cubes.get(&target) {
            apply_cube_changes(&mut cube, changes).map_err(|e| FlowTestError::Apply {
                test: test.name.clone(),
                message: e.to_string(),
            })?;
        }

        let mut failures = Vec::new();
        for assertion in &test.assertions {
            let coord = resolve_coord(&cube, &assertion.coord).map_err(FlowTestError::Model)?;
            let actual = cube.get(&coord).map_err(FlowTestError::Model)?;
            let expected = Fixed::from_str(&assertion.value).map_err(FlowTestError::Model)?;
            if actual != expected {
                failures.push(AssertionFailure {
                    coord: assertion.coord.clone(),
                    expected: expected.to_string(),
                    actual: actual.to_string(),
                });
            }
        }
        outcomes.push(FlowTestOutcome {
            name: test.name.clone(),
            passed: failures.is_empty(),
            failures,
        });
    }
    Ok(outcomes)
}

/// Apply one cube's staged changes to a cube clone: add its elements and edges,
/// then write its cells. Returns the number of cells written. Used by the test
/// runner (on a clone); the REST runner applies the same changes through the
/// engine, per target cube.
pub fn apply_cube_changes(cube: &mut Cube, changes: &CubeChanges) -> Result<usize, ModelError> {
    cube.extend_schema(&changes.elements, &changes.edges)?;
    let mut written = 0;
    for cell in &changes.cells {
        let coord = resolve_coord(cube, &cell.coord)?;
        if coord_has_string(cube, &coord) {
            cube.set_string(&coord, &cell.value)?;
        } else {
            let value = Fixed::from_str(&cell.value)?;
            cube.set_leaf(&coord, value)?;
        }
        written += 1;
    }
    Ok(written)
}

/// Resolve a `{dimension: member}` map to element indices in dimension order.
/// Every dimension of the cube must be addressed.
fn resolve_coord(cube: &Cube, coord: &BTreeMap<String, String>) -> Result<Vec<u32>, ModelError> {
    let mut out = Vec::with_capacity(cube.rank());
    for d in 0..cube.rank() {
        let dim = cube.dimension(d);
        let member = coord
            .get(dim.name())
            .ok_or_else(|| ModelError::ElementNotFound {
                dimension: dim.name().to_string(),
                element: "<missing>".to_string(),
            })?;
        let idx = dim
            .resolve(member)
            .ok_or_else(|| ModelError::ElementNotFound {
                dimension: dim.name().to_string(),
                element: member.clone(),
            })?;
        out.push(idx);
    }
    Ok(out)
}

/// Whether any element in the coordinate is a string leaf (so the cell is a
/// string cell, not a numeric one).
fn coord_has_string(cube: &Cube, coord: &[u32]) -> bool {
    coord.iter().enumerate().any(|(d, &idx)| {
        cube.dimension(d)
            .element(idx)
            .map(|e| e.kind == ElementKind::String)
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use epiphany_core::{Dimension, Flow, FlowTest, TestCell};

    fn cell(region: &str, measure: &str, value: &str) -> TestCell {
        let mut coord = BTreeMap::new();
        coord.insert("Region".to_string(), region.to_string());
        coord.insert("Measure".to_string(), measure.to_string());
        TestCell {
            coord,
            value: value.to_string(),
        }
    }

    fn sales_cube() -> Cube {
        let mut region = Dimension::new("Region");
        region.add_consolidated("Total");
        let mut measure = Dimension::new("Measure");
        measure.add_leaf("Sales");
        Cube::new("Sales", vec![region, measure]).unwrap()
    }

    /// An automation model with one flow named "load" (default cube "Sales") and
    /// the given test. The flow is free to add region leaves under Total.
    fn automation_with_flow(source: &str, test: FlowTest) -> Automation {
        let mut automation = Automation::new();
        automation.flows.insert(
            "load".to_string(),
            Flow {
                name: "load".to_string(),
                source: source.to_string(),
                owner: None,
                default_cube: Some("Sales".to_string()),
                inputs: Vec::new(),
            },
        );
        automation.flow_tests.insert(test.name.clone(), test);
        automation
    }

    fn run(automation: &Automation) -> Result<Vec<FlowTestOutcome>, FlowTestError> {
        run_flow_tests(automation, |name| (name == "Sales").then(sales_cube))
    }

    fn flow_test(name: &str, input: &str, assertions: Vec<TestCell>) -> FlowTest {
        FlowTest {
            name: name.to_string(),
            flow: "load".to_string(),
            input: input.to_string(),
            inputs: BTreeMap::new(),
            cube: None,
            params: BTreeMap::new(),
            assertions,
        }
    }

    const LOADER: &str = "\
function rows(ctx) {
  const data = ctx.input();
  const regions = data.map(function (r) { return r.Region; });
  ctx.ensureElements('Region', regions);
  regions.forEach(function (r) { ctx.addChild('Region', 'Total', r, 1); });
  ctx.writeCells(data.map(function (r) {
    return { coord: { Region: r.Region, Measure: 'Sales' }, value: r.Value };
  }));
}";

    #[test]
    fn flow_test_passes_on_matching_assertions() {
        let test = flow_test(
            "load_test",
            "Region,Value\nNorth,100\nSouth,200\n",
            vec![
                cell("North", "Sales", "100"),
                cell("South", "Sales", "200"),
                cell("Total", "Sales", "300"),
            ],
        );
        let automation = automation_with_flow(LOADER, test);
        let outcomes = run(&automation).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].passed, "failures: {:?}", outcomes[0].failures);
    }

    #[test]
    fn flow_test_reports_a_mismatch() {
        let test = flow_test(
            "bad",
            "Region,Value\nNorth,100\n",
            vec![cell("North", "Sales", "999")],
        );
        let automation = automation_with_flow(LOADER, test);
        let outcomes = run(&automation).unwrap();
        assert!(!outcomes[0].passed);
        assert_eq!(outcomes[0].failures.len(), 1);
        assert_eq!(outcomes[0].failures[0].expected, "999");
        assert_eq!(outcomes[0].failures[0].actual, "100");
    }

    #[test]
    fn unknown_flow_is_an_error() {
        let mut test = flow_test("t", "", vec![]);
        test.flow = "ghost".to_string();
        let automation = automation_with_flow(LOADER, test);
        assert!(matches!(
            run(&automation),
            Err(FlowTestError::UnknownFlow { .. })
        ));
    }

    #[test]
    fn explicit_target_cube_is_honored() {
        let mut test = flow_test(
            "load_test",
            "Region,Value\nNorth,100\n",
            vec![cell("Total", "Sales", "100")],
        );
        test.cube = Some("Sales".to_string());
        let automation = automation_with_flow(LOADER, test);
        assert!(run(&automation).unwrap()[0].passed);
    }

    #[test]
    fn outcomes_are_deterministic() {
        let test = flow_test(
            "load_test",
            "Region,Value\nNorth,100\n",
            vec![cell("Total", "Sales", "100")],
        );
        let automation = automation_with_flow(LOADER, test);
        assert_eq!(run(&automation).unwrap(), run(&automation).unwrap());
    }
}
