//! On-demand evaluation of compiled rules (ADR-0007).
//!
//! [`CalcEngine`] evaluates rule-derived cell values lazily over an immutable
//! snapshot, overlaying rules on stored leaves and consolidation. It owns one
//! per-query memo keyed by `(cube ordinal, coordinate)`, so a value is computed
//! at most once per query, cycles are caught precisely (re-seeing a `Computing`
//! entry), and cross-cube reads share the same memo and cycle machinery.
//! Consolidation math stays in `epiphany-core`: a consolidated read calls
//! [`Cube::consolidate_with`] with a closure that pulls each contributing leaf
//! back through the resolver, so rule-derived leaves fold into rollups through
//! the same exact i128 algebra. Invalidation-on-write is free: a new published
//! version yields a fresh engine and memo.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;

use epiphany_core::{AttributeValue, CellResolver, Cube, Fixed, ModelError, QueryError, SCALE};

use crate::compiled::{AddrSlot, CCell, CCond, CExpr, CompiledModel};
use crate::rules::{ArithOp, CmpOp};

/// The eval-time view of the model set: each cube plus its compiled rules.
pub trait EvalRegistry {
    /// The cube at an ordinal.
    fn cube(&self, ordinal: u32) -> Option<&Cube>;
    /// The compiled rules for a cube (a cube may have none).
    fn compiled(&self, ordinal: u32) -> Option<&CompiledModel>;
    /// The ordinal of a cube by name.
    fn ordinal(&self, name: &str) -> Option<u32>;
}

/// A failure while evaluating rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalcError {
    /// A rule's dependency chain returned to a cell already being computed.
    Cycle {
        /// The cube the cycle was detected in.
        cube: String,
        /// The coordinate (element indices) at the cycle point.
        coord: Vec<u32>,
    },
    /// A division by zero in a rule.
    DivByZero,
    /// Fixed-point arithmetic overflowed.
    Overflow,
    /// A referenced cube ordinal was not in the registry.
    UnknownCube(u32),
    /// An underlying core model error.
    Model(ModelError),
}

impl fmt::Display for CalcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CalcError::Cycle { cube, coord } => {
                write!(f, "rule cycle at coordinate {coord:?} in cube '{cube}'")
            }
            CalcError::DivByZero => write!(f, "division by zero in a rule"),
            CalcError::Overflow => write!(f, "fixed-point overflow in a rule"),
            CalcError::UnknownCube(o) => write!(f, "unknown cube ordinal {o}"),
            CalcError::Model(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CalcError {}

impl From<ModelError> for CalcError {
    fn from(e: ModelError) -> Self {
        CalcError::Model(e)
    }
}

impl From<CalcError> for QueryError {
    fn from(e: CalcError) -> Self {
        QueryError::Calc {
            message: e.to_string(),
        }
    }
}

#[derive(Clone, Copy)]
enum CellState {
    Computing,
    Done(Fixed),
}

/// Per-query memo: `(cube ordinal, coordinate) -> state`.
type Memo = HashMap<(u32, Box<[u32]>), CellState>;

/// A pull-based rule evaluator over one query's pinned snapshot.
pub struct CalcEngine<'a> {
    registry: &'a dyn EvalRegistry,
    memo: RefCell<Memo>,
}

impl std::fmt::Debug for CalcEngine<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CalcEngine")
            .field("memoized", &self.memo.borrow().len())
            .finish_non_exhaustive()
    }
}

impl<'a> CalcEngine<'a> {
    /// Create an engine over a registry. Cheap: the memo starts empty.
    pub fn new(registry: &'a dyn EvalRegistry) -> Self {
        Self {
            registry,
            memo: RefCell::new(HashMap::new()),
        }
    }

    /// A [`CellResolver`] view bound to one cube ordinal (for `execute_view` /
    /// `read_cells`). Multiple views share the engine's single memo.
    pub fn view(&'a self, ordinal: u32) -> CalcView<'a> {
        CalcView {
            engine: self,
            ordinal,
        }
    }

    /// The rule-aware value at a coordinate in cube `ordinal`.
    pub fn value(&self, ordinal: u32, coord: &[u32]) -> Result<Fixed, CalcError> {
        let key = (ordinal, coord.to_vec().into_boxed_slice());
        {
            let mut memo = self.memo.borrow_mut();
            match memo.get(&key) {
                Some(CellState::Done(v)) => return Ok(*v),
                Some(CellState::Computing) => {
                    return Err(CalcError::Cycle {
                        cube: self
                            .registry
                            .cube(ordinal)
                            .map(|c| c.name().to_string())
                            .unwrap_or_default(),
                        coord: coord.to_vec(),
                    })
                }
                None => {
                    memo.insert(key.clone(), CellState::Computing);
                }
            }
        }
        let result = self.compute(ordinal, coord);
        match result {
            Ok(v) => {
                self.memo.borrow_mut().insert(key, CellState::Done(v));
                Ok(v)
            }
            Err(e) => {
                // Drop the Computing marker so the failure is reported once.
                self.memo.borrow_mut().remove(&key);
                Err(e)
            }
        }
    }

    fn compute(&self, ordinal: u32, coord: &[u32]) -> Result<Fixed, CalcError> {
        let cube = self
            .registry
            .cube(ordinal)
            .ok_or(CalcError::UnknownCube(ordinal))?;
        let compiled = self.registry.compiled(ordinal);

        // A matching rule fires for a leaf (rule-derived leaf) or, when it
        // explicitly names the consolidated element, as a consolidation override.
        if let Some(rid) = compiled.and_then(|cm| cm.matching_rule(cube, coord)) {
            let cm = compiled.expect("compiled present when a rule matched");
            return self.eval_expr(&cm.rules[rid.0].expr, ordinal, coord);
        }

        let all_leaf = coord.iter().enumerate().all(|(d, &i)| {
            cube.dimension(d)
                .element(i)
                .map(|e| e.kind.is_leaf())
                .unwrap_or(false)
        });
        if all_leaf {
            // No rule at this leaf: the stored value.
            Ok(cube.get(coord)?)
        } else {
            // Consolidate, pulling each contributing leaf back through the
            // resolver so rule-derived leaves are included with correct weights.
            cube.consolidate_with::<CalcError, _>(coord, |lc| self.value(ordinal, lc))
        }
    }

    fn eval_expr(&self, expr: &CExpr, ordinal: u32, target: &[u32]) -> Result<Fixed, CalcError> {
        match expr {
            CExpr::Num(f) => Ok(*f),
            CExpr::Undef => Ok(Fixed::ZERO),
            CExpr::AttrNum { dim_pos, attr } => {
                let cube = self
                    .registry
                    .cube(ordinal)
                    .ok_or(CalcError::UnknownCube(ordinal))?;
                let dim = cube.dimension(*dim_pos);
                let attr_name = &dim.attribute_defs()[*attr as usize].name;
                Ok(match dim.attribute(target[*dim_pos], attr_name) {
                    Some(AttributeValue::Numeric(f)) => *f,
                    // A missing or non-numeric attribute reads as zero.
                    _ => Fixed::ZERO,
                })
            }
            CExpr::Cell(cell) => self.eval_cell(cell, target),
            CExpr::Neg(e) => {
                let v = self.eval_expr(e, ordinal, target)?;
                v.to_scaled()
                    .checked_neg()
                    .map(Fixed::from_scaled)
                    .ok_or(CalcError::Overflow)
            }
            CExpr::Bin { op, left, right } => {
                let a = self.eval_expr(left, ordinal, target)?;
                let b = self.eval_expr(right, ordinal, target)?;
                arith(*op, a, b)
            }
            CExpr::If {
                cond,
                then,
                otherwise,
            } => {
                if self.eval_cond(cond, ordinal, target)? {
                    self.eval_expr(then, ordinal, target)
                } else {
                    match otherwise {
                        Some(o) => self.eval_expr(o, ordinal, target),
                        None => Ok(Fixed::ZERO),
                    }
                }
            }
        }
    }

    fn eval_cell(&self, cell: &CCell, target: &[u32]) -> Result<Fixed, CalcError> {
        let mut abs = Vec::with_capacity(cell.addr.len());
        for slot in &cell.addr {
            abs.push(match slot {
                AddrSlot::Pinned(idx) => *idx,
                AddrSlot::FromTarget(pos) => target[*pos],
            });
        }
        self.value(cell.cube, &abs)
    }

    fn eval_cond(&self, cond: &CCond, ordinal: u32, target: &[u32]) -> Result<bool, CalcError> {
        match cond {
            CCond::And(a, b) => {
                Ok(self.eval_cond(a, ordinal, target)? && self.eval_cond(b, ordinal, target)?)
            }
            CCond::Or(a, b) => {
                Ok(self.eval_cond(a, ordinal, target)? || self.eval_cond(b, ordinal, target)?)
            }
            CCond::Not(c) => Ok(!self.eval_cond(c, ordinal, target)?),
            CCond::Compare { left, op, right } => {
                let a = self.eval_expr(left, ordinal, target)?;
                let b = self.eval_expr(right, ordinal, target)?;
                Ok(compare(a.to_scaled(), b.to_scaled(), *op))
            }
        }
    }
}

/// A [`CellResolver`] bound to one cube ordinal, backed by a shared engine.
#[derive(Debug)]
pub struct CalcView<'a> {
    engine: &'a CalcEngine<'a>,
    ordinal: u32,
}

impl CellResolver for CalcView<'_> {
    fn value(&self, coord: &[u32]) -> Result<Fixed, QueryError> {
        Ok(self.engine.value(self.ordinal, coord)?)
    }

    fn string_value(&self, coord: &[u32]) -> Result<Option<String>, QueryError> {
        // Rules are numeric for M4; string cells pass through to stored values.
        let cube = self
            .engine
            .registry
            .cube(self.ordinal)
            .ok_or(QueryError::Calc {
                message: format!("unknown cube ordinal {}", self.ordinal),
            })?;
        Ok(cube.get_string(coord)?.map(str::to_string))
    }
}

fn compare(a: i64, b: i64, op: CmpOp) -> bool {
    match op {
        CmpOp::Eq => a == b,
        CmpOp::Ne => a != b,
        CmpOp::Lt => a < b,
        CmpOp::Le => a <= b,
        CmpOp::Gt => a > b,
        CmpOp::Ge => a >= b,
    }
}

/// Exact fixed-point arithmetic on the rule path (ADR-0008): add/sub are checked
/// i64; multiply and divide go through i128 with round-half-to-even, the pinned
/// rounding contract. No floating point.
fn arith(op: ArithOp, a: Fixed, b: Fixed) -> Result<Fixed, CalcError> {
    let (sa, sb) = (a.to_scaled(), b.to_scaled());
    match op {
        ArithOp::Add => sa
            .checked_add(sb)
            .map(Fixed::from_scaled)
            .ok_or(CalcError::Overflow),
        ArithOp::Sub => sa
            .checked_sub(sb)
            .map(Fixed::from_scaled)
            .ok_or(CalcError::Overflow),
        ArithOp::Mul => {
            let scaled = div_round_half_even(sa as i128 * sb as i128, SCALE as i128);
            to_fixed(scaled)
        }
        ArithOp::Div => {
            if sb == 0 {
                return Err(CalcError::DivByZero);
            }
            let scaled = div_round_half_even(sa as i128 * SCALE as i128, sb as i128);
            to_fixed(scaled)
        }
    }
}

fn to_fixed(scaled: i128) -> Result<Fixed, CalcError> {
    i64::try_from(scaled)
        .map(Fixed::from_scaled)
        .map_err(|_| CalcError::Overflow)
}

/// Integer division of `num/den` rounded half to even (banker's rounding).
fn div_round_half_even(num: i128, den: i128) -> i128 {
    let q = num / den;
    let r = num % den;
    if r == 0 {
        return q;
    }
    let twice = r.unsigned_abs() * 2;
    let aden = den.unsigned_abs();
    let round_away = twice > aden || (twice == aden && q % 2 != 0);
    if round_away {
        // q truncates toward zero; step it toward the true quotient.
        if (num < 0) ^ (den < 0) {
            q - 1
        } else {
            q + 1
        }
    } else {
        q
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::compile;
    use crate::registry::SingleCube;
    use crate::rules::parse;
    use epiphany_core::{Cube, Dimension};
    use epiphany_determinism::DeterministicRng;

    /// Sales: Region(North,South,Total) x Measure(Sales,Cost,Margin).
    fn sales_cube() -> Cube {
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
        Cube::new("Sales", vec![region, measure]).unwrap()
    }

    /// A single-cube eval registry (target at ordinal 0).
    struct OneCube {
        cube: Cube,
        model: CompiledModel,
    }
    impl EvalRegistry for OneCube {
        fn cube(&self, o: u32) -> Option<&Cube> {
            (o == 0).then_some(&self.cube)
        }
        fn compiled(&self, o: u32) -> Option<&CompiledModel> {
            (o == 0).then_some(&self.model)
        }
        fn ordinal(&self, name: &str) -> Option<u32> {
            (name == self.cube.name()).then_some(0)
        }
    }

    fn build(cube: Cube, rules: &str) -> OneCube {
        let model = compile(&cube, &SingleCube::new(&cube), &parse(rules).unwrap(), 1).unwrap();
        OneCube { cube, model }
    }

    fn coord(reg: &OneCube, region: &str, measure: &str) -> Vec<u32> {
        vec![
            reg.cube.dimension(0).resolve(region).unwrap(),
            reg.cube.dimension(1).resolve(measure).unwrap(),
        ]
    }

    #[test]
    fn leaf_rule_and_rollup_of_rule_derived_leaves() {
        let mut cube = sales_cube();
        let (n, s) = (
            cube.dimension(0).resolve("North").unwrap(),
            cube.dimension(0).resolve("South").unwrap(),
        );
        let (sales, cost) = (
            cube.dimension(1).resolve("Sales").unwrap(),
            cube.dimension(1).resolve("Cost").unwrap(),
        );
        cube.set_leaf(&[n, sales], Fixed::from(100)).unwrap();
        cube.set_leaf(&[n, cost], Fixed::from(60)).unwrap();
        cube.set_leaf(&[s, sales], Fixed::from(200)).unwrap();
        cube.set_leaf(&[s, cost], Fixed::from(150)).unwrap();
        let reg = build(
            cube,
            "['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];",
        );
        let engine = CalcEngine::new(&reg);
        // Leaf Margins.
        assert_eq!(
            engine.value(0, &coord(&reg, "North", "Margin")).unwrap(),
            Fixed::from(40)
        );
        assert_eq!(
            engine.value(0, &coord(&reg, "South", "Margin")).unwrap(),
            Fixed::from(50)
        );
        // The Total Margin consolidates the RULE-DERIVED leaf margins: 40 + 50.
        assert_eq!(
            engine.value(0, &coord(&reg, "Total", "Margin")).unwrap(),
            Fixed::from(90)
        );
        // A stored consolidation is unaffected: Total Sales = 300.
        assert_eq!(
            engine.value(0, &coord(&reg, "Total", "Sales")).unwrap(),
            Fixed::from(300)
        );
    }

    #[test]
    fn consolidation_override_replaces_rollup() {
        let mut cube = sales_cube();
        let (n, s) = (
            cube.dimension(0).resolve("North").unwrap(),
            cube.dimension(0).resolve("South").unwrap(),
        );
        let sales = cube.dimension(1).resolve("Sales").unwrap();
        cube.set_leaf(&[n, sales], Fixed::from(100)).unwrap();
        cube.set_leaf(&[s, sales], Fixed::from(200)).unwrap();
        // Override Total/Sales to a fixed 1000 instead of the 300 rollup.
        let reg = build(cube, "['Region':'Total', 'Measure':'Sales'] = 1000;");
        let engine = CalcEngine::new(&reg);
        assert_eq!(
            engine.value(0, &coord(&reg, "Total", "Sales")).unwrap(),
            Fixed::from(1000)
        );
        // Leaves untouched.
        assert_eq!(
            engine.value(0, &coord(&reg, "North", "Sales")).unwrap(),
            Fixed::from(100)
        );
    }

    #[test]
    fn if_then_else_and_divide() {
        let mut cube = sales_cube();
        let (n, sales, cost) = (
            cube.dimension(0).resolve("North").unwrap(),
            cube.dimension(1).resolve("Sales").unwrap(),
            cube.dimension(1).resolve("Cost").unwrap(),
        );
        cube.set_leaf(&[n, sales], Fixed::from(100)).unwrap();
        cube.set_leaf(&[n, cost], Fixed::from(40)).unwrap();
        // Margin% = IF Sales > 0 THEN (Sales - Cost) / Sales ELSE 0.
        let reg = build(
            cube,
            "['Measure':'Margin'] = IF value['Measure':'Sales'] > 0 THEN (value['Measure':'Sales'] - value['Measure':'Cost']) / value['Measure':'Sales'] ELSE 0;",
        );
        let engine = CalcEngine::new(&reg);
        // (100-40)/100 = 0.6
        assert_eq!(
            engine.value(0, &coord(&reg, "North", "Margin")).unwrap(),
            Fixed::from_scaled(6000)
        );
    }

    #[test]
    fn division_by_zero_and_cycle_are_errors() {
        let cube = sales_cube();
        let reg = build(
            cube,
            "['Measure':'Margin'] = value['Measure':'Sales'] / value['Measure':'Cost'];",
        );
        let engine = CalcEngine::new(&reg);
        // Cost is zero -> DivByZero.
        assert_eq!(
            engine.value(0, &coord(&reg, "North", "Margin")),
            Err(CalcError::DivByZero)
        );

        // A self-referential rule -> Cycle.
        let cube2 = sales_cube();
        let reg2 = build(
            cube2,
            "['Measure':'Margin'] = value['Measure':'Margin'] + 1;",
        );
        let engine2 = CalcEngine::new(&reg2);
        assert!(matches!(
            engine2.value(0, &coord(&reg2, "North", "Margin")),
            Err(CalcError::Cycle { .. })
        ));
    }

    #[test]
    fn multiply_rounds_half_to_even() {
        // 0.0001 * 0.5 = 0.00005 -> rounds to 0.0000 (even); 1.5*1 stays 1.5.
        assert_eq!(
            arith(
                ArithOp::Mul,
                Fixed::from_scaled(1),
                Fixed::from_scaled(5000)
            )
            .unwrap(),
            Fixed::from_scaled(0)
        );
        assert_eq!(
            arith(
                ArithOp::Mul,
                Fixed::from_scaled(3),
                Fixed::from_scaled(5000)
            )
            .unwrap(),
            Fixed::from_scaled(2)
        );
        // 2.5 (as 25000 scaled) * 1 = 2.5 exact.
        assert_eq!(
            arith(ArithOp::Mul, Fixed::from(2), Fixed::from(3)).unwrap(),
            Fixed::from(6)
        );
    }

    #[test]
    fn no_rules_resolver_matches_stored() {
        let mut cube = sales_cube();
        let (n, sales) = (
            cube.dimension(0).resolve("North").unwrap(),
            cube.dimension(1).resolve("Sales").unwrap(),
        );
        cube.set_leaf(&[n, sales], Fixed::from(42)).unwrap();
        let reg = build(cube, "");
        let engine = CalcEngine::new(&reg);
        let region_total = reg.cube.dimension(0).resolve("Total").unwrap();
        let sales_i = reg.cube.dimension(1).resolve("Sales").unwrap();
        for c in [[n, sales], [region_total, sales_i]] {
            assert_eq!(engine.value(0, &c).unwrap(), reg.cube.get(&c).unwrap());
        }
    }

    #[test]
    fn children_sum_to_parent_with_rule_leaves_randomized() {
        let mut cube = sales_cube();
        let mut rng = DeterministicRng::new(99);
        let (n, s) = (
            cube.dimension(0).resolve("North").unwrap(),
            cube.dimension(0).resolve("South").unwrap(),
        );
        let (sales, cost) = (
            cube.dimension(1).resolve("Sales").unwrap(),
            cube.dimension(1).resolve("Cost").unwrap(),
        );
        for &r in &[n, s] {
            cube.set_leaf(
                &[r, sales],
                Fixed::from_scaled((rng.next_u64() % 1000) as i64),
            )
            .unwrap();
            cube.set_leaf(
                &[r, cost],
                Fixed::from_scaled((rng.next_u64() % 1000) as i64),
            )
            .unwrap();
        }
        let reg = build(
            cube,
            "['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];",
        );
        let engine = CalcEngine::new(&reg);
        let total = engine.value(0, &coord(&reg, "Total", "Margin")).unwrap();
        let north = engine.value(0, &coord(&reg, "North", "Margin")).unwrap();
        let south = engine.value(0, &coord(&reg, "South", "Margin")).unwrap();
        assert_eq!(total.to_scaled(), north.to_scaled() + south.to_scaled());
        // Determinism: identical run-to-run.
        let engine2 = CalcEngine::new(&reg);
        assert_eq!(
            engine2.value(0, &coord(&reg, "Total", "Margin")).unwrap(),
            total
        );
    }

    #[test]
    fn cross_cube_reference_evaluates() {
        // Sales.Revenue = Units * FX!Rate (a fixed cross-cube scalar).
        let mut sales = {
            let mut region = Dimension::new("Region");
            let n = region.add_leaf("North");
            let mut measure = Dimension::new("Measure");
            measure.add_leaf("Units");
            measure.add_leaf("Revenue");
            let cube = Cube::new("Sales", vec![region, measure]).unwrap();
            let _ = n;
            cube
        };
        let units = sales.dimension(1).resolve("Units").unwrap();
        let north = sales.dimension(0).resolve("North").unwrap();
        sales.set_leaf(&[north, units], Fixed::from(10)).unwrap();

        let mut pair = Dimension::new("Pair");
        let usd = pair.add_leaf("USD");
        let mut fx = Cube::new("FX", vec![pair]).unwrap();
        fx.set_leaf(&[usd], Fixed::from(3)).unwrap();

        // Multi-cube eval registry.
        struct TwoCubes {
            cubes: Vec<Cube>,
            models: Vec<CompiledModel>,
        }
        impl EvalRegistry for TwoCubes {
            fn cube(&self, o: u32) -> Option<&Cube> {
                self.cubes.get(o as usize)
            }
            fn compiled(&self, o: u32) -> Option<&CompiledModel> {
                self.models.get(o as usize)
            }
            fn ordinal(&self, name: &str) -> Option<u32> {
                self.cubes
                    .iter()
                    .position(|c| c.name() == name)
                    .map(|i| i as u32)
            }
        }

        let reg_for_compile = crate::registry::VecRegistry::new(vec![sales.clone(), fx.clone()]);
        let sales_model = compile(
            &sales,
            &reg_for_compile,
            &parse("['Measure':'Revenue'] = value['Measure':'Units'] * 'FX'!['Pair':'USD'];")
                .unwrap(),
            1,
        )
        .unwrap();
        let fx_model = compile(&fx, &reg_for_compile, &parse("").unwrap(), 1).unwrap();
        let eval_reg = TwoCubes {
            cubes: vec![sales, fx],
            models: vec![sales_model, fx_model],
        };
        let engine = CalcEngine::new(&eval_reg);
        // Revenue(North) = Units(10) * Rate(3) = 30.
        assert_eq!(
            engine.value(0, &[north, units + 1]).unwrap(),
            Fixed::from(30)
        );
    }
}
