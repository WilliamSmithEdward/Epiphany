//! The model testing framework: deterministic rule unit tests.
//!
//! A [`RuleTest`] sets fixture leaves and asserts derived cell values.
//! [`run_rule_tests`] runs them against a model with the same code path the REST
//! runner uses (Phase 4K): it compiles the model's rules once, then for each test
//! clones the cube, applies the fixtures, and evaluates each assertion through a
//! one-shot resolver, comparing exact [`Fixed`] values. No clock or RNG, and the
//! cube clone isolates fixtures from the live model, so results are deterministic.

use std::collections::BTreeMap;
use std::fmt;

use epiphany_core::{Cube, Fixed, Model, ModelError, RuleTest};

use crate::compile::compile;
use crate::compiled::CompileError;
use crate::compiled::CompiledModel;
use crate::eval::{CalcEngine, CalcError, EvalRegistry};
use crate::registry::SingleCube;
use crate::rules::{parse, RuleParseError};

/// The result of running one rule test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestOutcome {
    /// The test name.
    pub name: String,
    /// Whether every assertion held.
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

/// A failure that prevents the tests from running at all (malformed rules or
/// test coordinates), as opposed to an assertion failure.
#[derive(Debug, Clone)]
pub enum TestRunError {
    /// The model's rules did not parse.
    Parse(RuleParseError),
    /// The model's rules did not compile.
    Compile(CompileError),
    /// A test coordinate or fixture was invalid.
    Model(ModelError),
    /// A rule failed to evaluate during a test.
    Calc(CalcError),
    /// A test coordinate named a dimension or member that does not resolve.
    BadCoord(String),
}

impl fmt::Display for TestRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TestRunError::Parse(e) => write!(f, "rule parse error: {e}"),
            TestRunError::Compile(e) => write!(f, "rule compile error: {e}"),
            TestRunError::Model(e) => write!(f, "{e}"),
            TestRunError::Calc(e) => write!(f, "{e}"),
            TestRunError::BadCoord(m) => write!(f, "invalid test coordinate: {m}"),
        }
    }
}

impl std::error::Error for TestRunError {}

impl From<RuleParseError> for TestRunError {
    fn from(e: RuleParseError) -> Self {
        TestRunError::Parse(e)
    }
}
impl From<CompileError> for TestRunError {
    fn from(e: CompileError) -> Self {
        TestRunError::Compile(e)
    }
}
impl From<ModelError> for TestRunError {
    fn from(e: ModelError) -> Self {
        TestRunError::Model(e)
    }
}
impl From<CalcError> for TestRunError {
    fn from(e: CalcError) -> Self {
        TestRunError::Calc(e)
    }
}

struct TestRegistry<'a> {
    cube: Cube,
    model: &'a CompiledModel,
}
impl EvalRegistry for TestRegistry<'_> {
    fn cube(&self, o: u32) -> Option<&Cube> {
        (o == 0).then_some(&self.cube)
    }
    fn compiled(&self, o: u32) -> Option<&CompiledModel> {
        (o == 0).then_some(self.model)
    }
    fn ordinal(&self, name: &str) -> Option<u32> {
        (name == self.cube.name()).then_some(0)
    }
}

/// Resolve a `{dimension: member}` coordinate to element indices in dimension
/// order against `cube`.
fn resolve_coord(cube: &Cube, coord: &BTreeMap<String, String>) -> Result<Vec<u32>, TestRunError> {
    let mut out = Vec::with_capacity(cube.rank());
    for d in 0..cube.rank() {
        let dim = cube.dimension(d);
        let member = coord
            .get(dim.name())
            .ok_or_else(|| TestRunError::BadCoord(format!("missing dimension '{}'", dim.name())))?;
        let idx = dim.resolve(member).ok_or_else(|| {
            TestRunError::BadCoord(format!("unknown member '{member}' in '{}'", dim.name()))
        })?;
        out.push(idx);
    }
    Ok(out)
}

/// Run every rule test in a model, in name order.
///
/// Compiles the rules once (a parse/compile error fails the whole run); then for
/// each test clones the cube, applies fixtures, and checks assertions. A mismatch
/// is recorded as an [`AssertionFailure`]; a malformed coordinate or a rule
/// evaluation error is a [`TestRunError`].
pub fn run_rule_tests(model: &Model) -> Result<Vec<TestOutcome>, TestRunError> {
    let doc = parse(&model.rules.source)?;
    let compiled = compile(&model.cube, &SingleCube::new(&model.cube), &doc, 0)?;

    let mut outcomes = Vec::with_capacity(model.tests.len());
    for test in model.tests.values() {
        outcomes.push(run_one(model, &compiled, test)?);
    }
    Ok(outcomes)
}

fn run_one(
    model: &Model,
    compiled: &CompiledModel,
    test: &RuleTest,
) -> Result<TestOutcome, TestRunError> {
    // A fresh cube clone isolates this test's fixtures from the live model.
    let mut cube = model.cube.clone();
    for fixture in &test.fixtures {
        let coord = resolve_coord(&cube, &fixture.coord)?;
        let value = Fixed::from_str_checked(&fixture.value)?;
        cube.set_leaf(&coord, value)?;
    }

    let registry = TestRegistry {
        cube,
        model: compiled,
    };
    let engine = CalcEngine::new(&registry);

    let mut failures = Vec::new();
    for assertion in &test.assertions {
        let coord = resolve_coord(&registry.cube, &assertion.coord)?;
        let actual = engine.value(0, &coord)?;
        let expected = Fixed::from_str_checked(&assertion.value)?;
        if actual != expected {
            failures.push(AssertionFailure {
                coord: assertion.coord.clone(),
                expected: expected.to_string(),
                actual: actual.to_string(),
            });
        }
    }
    Ok(TestOutcome {
        name: test.name.clone(),
        passed: failures.is_empty(),
        failures,
    })
}

/// Parse a decimal string into a `Fixed`, mapping the error into the test domain.
trait FixedFromStr: Sized {
    fn from_str_checked(s: &str) -> Result<Self, TestRunError>;
}
impl FixedFromStr for Fixed {
    fn from_str_checked(s: &str) -> Result<Self, TestRunError> {
        use std::str::FromStr;
        Fixed::from_str(s).map_err(TestRunError::Model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epiphany_core::{Dimension, RuleSet, TestCell};

    fn pnl_model(rules: &str) -> Model {
        let mut region = Dimension::new("Region");
        let n = region.add_leaf("North");
        let s = region.add_leaf("South");
        let t = region.add_consolidated("Total");
        region.add_child(t, n, 1).unwrap();
        region.add_child(t, s, 1).unwrap();
        let mut measure = Dimension::new("Measure");
        measure.add_leaf("Sales");
        measure.add_leaf("Cost");
        measure.add_leaf("Margin");
        let cube = Cube::new("Sales", vec![region, measure]).unwrap();
        let mut model = Model::new(cube);
        model.rules = RuleSet {
            source: rules.to_string(),
        };
        model
    }

    fn cell(region: &str, measure: &str, value: &str) -> TestCell {
        let mut coord = BTreeMap::new();
        coord.insert("Region".to_string(), region.to_string());
        coord.insert("Measure".to_string(), measure.to_string());
        TestCell {
            coord,
            value: value.to_string(),
        }
    }

    #[test]
    fn margin_rolls_up_passes() {
        let mut model =
            pnl_model("['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];");
        model.tests.insert(
            "margin".to_string(),
            RuleTest {
                name: "margin".to_string(),
                fixtures: vec![cell("North", "Sales", "100"), cell("North", "Cost", "60")],
                assertions: vec![cell("North", "Margin", "40"), cell("Total", "Margin", "40")],
            },
        );
        let outcomes = run_rule_tests(&model).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].passed, "failures: {:?}", outcomes[0].failures);
    }

    #[test]
    fn wrong_expectation_reports_failure() {
        let mut model =
            pnl_model("['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];");
        model.tests.insert(
            "bad".to_string(),
            RuleTest {
                name: "bad".to_string(),
                fixtures: vec![cell("North", "Sales", "100"), cell("North", "Cost", "60")],
                assertions: vec![cell("North", "Margin", "999")],
            },
        );
        let outcomes = run_rule_tests(&model).unwrap();
        assert!(!outcomes[0].passed);
        assert_eq!(outcomes[0].failures.len(), 1);
        assert_eq!(outcomes[0].failures[0].expected, "999");
        assert_eq!(outcomes[0].failures[0].actual, "40");
    }

    #[test]
    fn outcomes_are_in_name_order_and_deterministic() {
        let mut model = pnl_model("['Measure':'Margin'] = value['Measure':'Sales'];");
        for name in ["zebra", "alpha", "mid"] {
            model.tests.insert(
                name.to_string(),
                RuleTest {
                    name: name.to_string(),
                    fixtures: vec![],
                    assertions: vec![],
                },
            );
        }
        let outcomes = run_rule_tests(&model).unwrap();
        let names: Vec<&str> = outcomes.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mid", "zebra"]);
        assert_eq!(run_rule_tests(&model).unwrap(), outcomes);
    }

    #[test]
    fn a_bad_rule_fails_the_run() {
        let model = pnl_model("['Measure':'Ghost'] = 1;"); // unknown member
        assert!(matches!(
            run_rule_tests(&model),
            Err(TestRunError::Compile(_))
        ));
    }
}
