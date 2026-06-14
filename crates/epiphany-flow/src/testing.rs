//! The flow testing framework: deterministic flow unit tests.
//!
//! A [`FlowTest`] pins a flow's input (CSV text) and parameters and asserts the
//! resulting cell values. [`run_flow_tests`] runs them against a model the same
//! way the REST runner does, but on a throwaway cube clone so a test never
//! mutates the live model: for each test it parses the input, runs the named
//! flow, applies the staged elements and cells to the clone, and checks each
//! assertion's exact [`Fixed`] value. No clock or RNG, so results are
//! deterministic.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use epiphany_core::{Cube, ElementKind, Fixed, Model, ModelError};
use epiphany_determinism::Deterministic;

use crate::csv::parse_csv;
use crate::run::{run_flow, FlowError, FlowOutcome};

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
    /// The test names a flow the model does not define.
    UnknownFlow { test: String, flow: String },
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

/// Run every flow test in a model, in name order. A malformed test (unknown
/// flow, bad input, a flow that errors, or a bad coordinate) is a
/// [`FlowTestError`]; a value mismatch is recorded as an [`AssertionFailure`].
pub fn run_flow_tests(model: &Model) -> Result<Vec<FlowTestOutcome>, FlowTestError> {
    let mut outcomes = Vec::with_capacity(model.flow_tests.len());
    for test in model.flow_tests.values() {
        let flow = model
            .flows
            .get(&test.flow)
            .ok_or_else(|| FlowTestError::UnknownFlow {
                test: test.name.clone(),
                flow: test.flow.clone(),
            })?;

        let rows = parse_csv(&test.input).map_err(|e| FlowTestError::BadInput {
            test: test.name.clone(),
            message: e.to_string(),
        })?;

        let outcome = run_flow(
            &flow.source,
            model.cube.name(),
            rows,
            &test.params,
            Deterministic::EPOCH_2020_MILLIS,
        )
        .map_err(|e: FlowError| FlowTestError::Flow {
            test: test.name.clone(),
            message: e.to_string(),
        })?;

        // Apply to a fresh clone so tests never touch the live model.
        let mut cube = model.cube.clone();
        apply_outcome(&mut cube, &outcome).map_err(|e| FlowTestError::Apply {
            test: test.name.clone(),
            message: e.to_string(),
        })?;

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

/// Apply a flow's staged outcome to a cube: add its elements and edges, then
/// write its cells. Returns the number of cells written. Used by the test runner
/// (on a clone); the REST runner applies the same outcome through the engine.
pub fn apply_outcome(cube: &mut Cube, outcome: &FlowOutcome) -> Result<usize, ModelError> {
    cube.extend_schema(&outcome.elements, &outcome.edges)?;
    let mut written = 0;
    for cell in &outcome.cells {
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

    /// Region(Total over its leaves) x Measure(Sales) cube, with the flow free to
    /// add region leaves under Total.
    fn model_with_flow(source: &str, test: FlowTest) -> Model {
        let mut region = Dimension::new("Region");
        region.add_consolidated("Total");
        let mut measure = Dimension::new("Measure");
        measure.add_leaf("Sales");
        let cube = Cube::new("Sales", vec![region, measure]).unwrap();
        let mut model = Model::new(cube);
        model.flows.insert(
            "load".to_string(),
            Flow {
                name: "load".to_string(),
                source: source.to_string(),
            },
        );
        model.flow_tests.insert(test.name.clone(), test);
        model
    }

    const LOADER: &str = "\
function rows(ctx) {
  const data = ctx.input();
  const regions = data.map(function (r) { return r.Region; });
  ctx.ensureElements('Region', regions);
  ctx.addChild('Region', 'Total', regions[0], 1);
  regions.forEach(function (r) { ctx.addChild('Region', 'Total', r, 1); });
  ctx.writeCells(data.map(function (r) {
    return { coord: { Region: r.Region, Measure: 'Sales' }, value: r.Value };
  }));
}";

    #[test]
    fn flow_test_passes_on_matching_assertions() {
        let test = FlowTest {
            name: "load_test".to_string(),
            flow: "load".to_string(),
            input: "Region,Value\nNorth,100\nSouth,200\n".to_string(),
            params: BTreeMap::new(),
            assertions: vec![
                cell("North", "Sales", "100"),
                cell("South", "Sales", "200"),
                cell("Total", "Sales", "300"),
            ],
        };
        let model = model_with_flow(LOADER, test);
        let outcomes = run_flow_tests(&model).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].passed, "failures: {:?}", outcomes[0].failures);
    }

    #[test]
    fn flow_test_reports_a_mismatch() {
        let test = FlowTest {
            name: "bad".to_string(),
            flow: "load".to_string(),
            input: "Region,Value\nNorth,100\n".to_string(),
            params: BTreeMap::new(),
            assertions: vec![cell("North", "Sales", "999")],
        };
        let model = model_with_flow(LOADER, test);
        let outcomes = run_flow_tests(&model).unwrap();
        assert!(!outcomes[0].passed);
        assert_eq!(outcomes[0].failures.len(), 1);
        assert_eq!(outcomes[0].failures[0].expected, "999");
        assert_eq!(outcomes[0].failures[0].actual, "100");
    }

    #[test]
    fn unknown_flow_is_an_error() {
        let test = FlowTest {
            name: "t".to_string(),
            flow: "ghost".to_string(),
            input: String::new(),
            params: BTreeMap::new(),
            assertions: vec![],
        };
        let model = model_with_flow(LOADER, test);
        assert!(matches!(
            run_flow_tests(&model),
            Err(FlowTestError::UnknownFlow { .. })
        ));
    }

    #[test]
    fn outcomes_are_deterministic() {
        let test = FlowTest {
            name: "load_test".to_string(),
            flow: "load".to_string(),
            input: "Region,Value\nNorth,100\n".to_string(),
            params: BTreeMap::new(),
            assertions: vec![cell("Total", "Sales", "100")],
        };
        let model = model_with_flow(LOADER, test);
        assert_eq!(
            run_flow_tests(&model).unwrap(),
            run_flow_tests(&model).unwrap()
        );
    }
}
