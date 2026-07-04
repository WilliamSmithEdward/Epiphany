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

use epiphany_core::{
    AttributeValue, CellResolver, Cube, ElementMask, Fixed, ModelError, QueryError, SCALE,
};

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

    /// A rendered compile/parse error for a cube whose rule source failed to
    /// compile against the (possibly since-edited) model, or `None` when the
    /// cube's rules are healthy (or it has none). Reading a cube in this state
    /// must FAIL LOUD rather than silently serve rule-less values: a structural
    /// edit can invalidate a previously-valid rule, and dropping the whole rule
    /// set would silently revert every derived cell to stored/aggregated numbers.
    /// [`CalcEngine::compute`] consults this before treating a cube as rule-less,
    /// so the failure propagates through consolidation rollups and cross-cube
    /// references too. The default returns `None`, so a registry that compiles
    /// eagerly (or has no failure notion) keeps its existing behavior.
    fn compile_error(&self, ordinal: u32) -> Option<&str> {
        let _ = ordinal;
        None
    }

    /// A precomputed, sorted set of rule-derived (fed) leaf coordinates for cube
    /// `ordinal` when — and ONLY when — the sparse consolidation path
    /// ([`Cube::consolidate_fed`]) is provably byte-identical to the dense
    /// [`Cube::consolidate_with`] for this cube (ADR-0005). `Some(fed)` licenses a
    /// consolidated read to union the stored cells with `fed` instead of
    /// enumerating the whole dense leaf space; `None` (the default) keeps the
    /// always-correct dense path.
    ///
    /// The gate that produces `Some` must guarantee completeness: the fed set
    /// covers every non-stored leaf whose rule value is non-zero, and no rule is
    /// opaque (feeders inference could not localize it). The returned set is a
    /// *base-read* property (stored data + rules only); it is invalid under a
    /// what-if overlay or an element mask, which introduce values the sparse scan
    /// cannot see, so [`CalcEngine::compute`] additionally requires no overlay and
    /// no mask before using it. The default returns `None`, so a registry with no
    /// feeder analysis (every unit-test registry, the explain/diagnostics path)
    /// stays on the dense path unchanged.
    fn fed_set(&self, ordinal: u32) -> Option<&[Box<[u32]>]> {
        let _ = ordinal;
        None
    }
}

/// A per-query what-if overlay consulted at the stored-leaf terminal (ADR-0014).
///
/// [`SandboxOverlay::leaf`] returns an override for a STORED leaf at
/// `(ordinal, coord)`, or `None` to fall through to the cube's stored value. It
/// is consulted only after a matching rule has been ruled out, so a rule-derived
/// leaf is never masked; rules and consolidations recompute over the overridden
/// leaves because they recurse through [`CalcEngine::value`]. [`scope_id`] is a
/// stable, non-zero id that partitions the memo so a base value and a what-if
/// value for the same coordinate can never alias.
///
/// [`scope_id`]: SandboxOverlay::scope_id
pub trait SandboxOverlay {
    /// The numeric override for a stored leaf, or `None` to use the stored value.
    fn leaf(&self, ordinal: u32, coord: &[u32]) -> Option<Fixed>;
    /// A stable, non-zero scope id identifying this overlay (a memo partition).
    fn scope_id(&self) -> u64;
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
    /// A cube whose stored rule source no longer compiles against the (edited)
    /// model was read: fail loud instead of silently reverting every rule-derived
    /// cell to stored/aggregated values. Carries the cube name and the rendered
    /// compile/parse error. Raised for a direct read of the cube AND for any
    /// rollup or cross-cube reference that pulls a value from it.
    RulesFailed {
        /// The cube whose rules failed to compile.
        cube: String,
        /// The rendered compile/parse error.
        error: String,
    },
    /// The caller may not read a cell this evaluation depends on: it names, or
    /// rolls up, an element denied to them (ADR-0015 element security). Carries no
    /// member identity (RG-13).
    AccessDenied,
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
            CalcError::RulesFailed { cube, error } => {
                write!(f, "cube '{cube}' rules fail to compile: {error}")
            }
            CalcError::AccessDenied => write!(f, "access denied"),
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
        match e {
            // Preserve the access-denied signal so the API renders it as 403
            // (suppress on an axis), not a generic calc 422.
            CalcError::AccessDenied => QueryError::AccessDenied,
            _ => QueryError::Calc {
                message: e.to_string(),
            },
        }
    }
}

#[derive(Clone, Copy)]
enum CellState {
    Computing,
    Done(Fixed),
}

/// Per-query memo, split as `(scope id, cube ordinal) -> coordinate -> state` so
/// a lookup borrows the queried coordinate (`&[u32]`) instead of boxing it: a
/// memo hit -- the hot case for every leaf a consolidation pulls more than once
/// -- allocates nothing, and the coordinate key is boxed exactly once, when the
/// `Computing` marker is first inserted. The scope id is 0 for base reads and the
/// sandbox's scope id for a sandboxed read, so the two never alias even if an
/// engine were shared across scopes (ADR-0014).
type Memo = HashMap<(u64, u32), HashMap<Box<[u32]>, CellState>>;

/// An engine's per-query memo, carried OUT of one [`CalcEngine`] and INTO the
/// next so a caller that must build a fresh engine per call (the `CellResolver`
/// seam builds one per `value`, since it cannot hold a self-referential engine
/// borrowing its own registry) still amortizes the ADR-0007 memo across the many
/// cells of a cellset. Opaque: it only holds computed `(scope, ordinal, coord)`
/// results, so reusing it is a pure cache and never changes a value (determinism
/// holds). Seed an engine with [`CalcEngine::with_memo`] /
/// [`CalcEngine::with_overlay_memo`] and reclaim it with
/// [`CalcEngine::into_memo`].
#[derive(Default)]
pub struct CalcMemo(Memo);

impl std::fmt::Debug for CalcMemo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CalcMemo")
            .field("memoized", &self.len())
            .finish()
    }
}

impl CalcMemo {
    /// A fresh, empty memo.
    pub fn new() -> Self {
        Self(HashMap::new())
    }

    /// How many cells are currently memoized (a behavioral proxy for reuse: a
    /// resolver that shares one memo across a cellset's cells retains a non-empty
    /// memo between reads, whereas a fresh-per-cell engine always starts at zero).
    pub fn len(&self) -> usize {
        self.0.values().map(HashMap::len).sum()
    }

    /// Whether nothing is memoized yet.
    pub fn is_empty(&self) -> bool {
        self.0.values().all(HashMap::is_empty)
    }
}

/// A pull-based rule evaluator over one query's pinned snapshot. An optional
/// what-if overlay (ADR-0014) replaces stored leaves beneath the rules.
pub struct CalcEngine<'a> {
    registry: &'a dyn EvalRegistry,
    overlay: Option<&'a dyn SandboxOverlay>,
    scope_id: u64,
    /// The per-request element deny mask (ADR-0015) and the cube ordinal it
    /// applies to. Absent (the common case) means no element check at all.
    mask: Option<&'a ElementMask>,
    mask_target: u32,
    memo: RefCell<Memo>,
}

impl std::fmt::Debug for CalcEngine<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let memoized: usize = self.memo.borrow().values().map(HashMap::len).sum();
        f.debug_struct("CalcEngine")
            .field("memoized", &memoized)
            .finish_non_exhaustive()
    }
}

impl<'a> CalcEngine<'a> {
    /// Create an engine over a registry. Cheap: the memo starts empty. No
    /// overlay, so reads are byte-identical to the no-sandbox behavior.
    pub fn new(registry: &'a dyn EvalRegistry) -> Self {
        Self {
            registry,
            overlay: None,
            scope_id: 0,
            mask: None,
            mask_target: 0,
            memo: RefCell::new(HashMap::new()),
        }
    }

    /// Create an engine that overlays a sandbox's what-if values beneath the
    /// rules (ADR-0014). The overlay's [`scope_id`](SandboxOverlay::scope_id)
    /// partitions this engine's memo.
    pub fn with_overlay(registry: &'a dyn EvalRegistry, overlay: &'a dyn SandboxOverlay) -> Self {
        let scope_id = overlay.scope_id();
        Self {
            registry,
            overlay: Some(overlay),
            scope_id,
            mask: None,
            mask_target: 0,
            memo: RefCell::new(HashMap::new()),
        }
    }

    /// Create an engine over a registry, SEEDED with an existing [`CalcMemo`]
    /// (ADR-0007). Identical to [`new`](Self::new) except the memo is carried in
    /// rather than started empty, so a caller building a fresh engine per read
    /// (the `CellResolver` seam) still shares one memo across a cellset's cells.
    /// Reclaim the (now-larger) memo with [`into_memo`](Self::into_memo). Because
    /// the memo only holds computed results, seeding it can never change a value.
    pub fn with_memo(registry: &'a dyn EvalRegistry, memo: CalcMemo) -> Self {
        Self {
            registry,
            overlay: None,
            scope_id: 0,
            mask: None,
            mask_target: 0,
            memo: RefCell::new(memo.0),
        }
    }

    /// Create an overlay engine (ADR-0014) SEEDED with an existing [`CalcMemo`].
    /// The overlay's [`scope_id`](SandboxOverlay::scope_id) partitions the memo,
    /// so reusing a memo built for the same overlay is sound; a base and a
    /// sandbox read never alias even through a shared memo.
    pub fn with_overlay_memo(
        registry: &'a dyn EvalRegistry,
        overlay: &'a dyn SandboxOverlay,
        memo: CalcMemo,
    ) -> Self {
        let scope_id = overlay.scope_id();
        Self {
            registry,
            overlay: Some(overlay),
            scope_id,
            mask: None,
            mask_target: 0,
            memo: RefCell::new(memo.0),
        }
    }

    /// Reclaim this engine's memo so the next per-cell engine can be seeded with
    /// it, amortizing computation across a cellset (ADR-0007).
    pub fn into_memo(self) -> CalcMemo {
        CalcMemo(self.memo.into_inner())
    }

    /// Attach a per-request element deny mask for cube ordinal `target`
    /// (ADR-0015). Every value the evaluation touches in that cube -- a direct
    /// read, a consolidation contribution, or a rule reference -- is checked at
    /// the cell terminal, so a denied leaf taints every rollup it feeds
    /// (deny-the-rollup). A `None` mask is a no-op.
    #[must_use]
    pub fn with_mask(mut self, mask: Option<&'a ElementMask>, target: u32) -> Self {
        self.mask = mask;
        self.mask_target = target;
        self
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
        {
            let mut memo = self.memo.borrow_mut();
            let inner = memo.entry((self.scope_id, ordinal)).or_default();
            // A borrowed `&[u32]` lookup: no allocation on the hit paths.
            match inner.get(coord) {
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
                    // The only allocation: box the coordinate once, for the
                    // marker that doubles as this cell's memo slot.
                    inner.insert(coord.to_vec().into_boxed_slice(), CellState::Computing);
                }
            }
        }
        let result = self.compute(ordinal, coord);
        let mut memo = self.memo.borrow_mut();
        // Both slots were created above, so these borrowed lookups always hit.
        if let Some(inner) = memo.get_mut(&(self.scope_id, ordinal)) {
            match &result {
                Ok(v) => {
                    if let Some(state) = inner.get_mut(coord) {
                        *state = CellState::Done(*v);
                    }
                }
                Err(_) => {
                    // Drop the Computing marker so the failure is reported once.
                    inner.remove(coord);
                }
            }
        }
        result
    }

    fn compute(&self, ordinal: u32, coord: &[u32]) -> Result<Fixed, CalcError> {
        // Element security (ADR-0015): deny before any value is produced. Every
        // cell -- a direct read, a leaf pulled into a consolidation, or a rule
        // reference -- reaches `compute`, so checking the queried element indices
        // here (a cheap per-component lookup, no expansion) denies a directly
        // named member and, via the consolidation recursion, every rollup that
        // includes a denied leaf. Scoped to the mask's target cube.
        if ordinal == self.mask_target {
            if let Some(mask) = self.mask {
                if mask.denies_leaf(coord) {
                    return Err(CalcError::AccessDenied);
                }
            }
        }
        let cube = self
            .registry
            .cube(ordinal)
            .ok_or(CalcError::UnknownCube(ordinal))?;

        // Fail loud on a cube whose stored rules no longer compile against the
        // (since-edited) model: serving stored/aggregated values here would
        // silently drop every rule-derived cell. Checked before the cube is
        // treated as rule-less, and reached by every read of the cube -- a direct
        // read, a leaf pulled into a rollup, or a cross-cube reference -- so the
        // whole dependent result errors rather than returning wrong numbers.
        if let Some(error) = self.registry.compile_error(ordinal) {
            return Err(CalcError::RulesFailed {
                cube: cube.name().to_string(),
                error: error.to_string(),
            });
        }

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
            // A what-if overlay replaces the STORED leaf value (a rule-derived
            // leaf was handled above, so it is never masked). Rules and
            // consolidations that read this leaf through `value` recompute over
            // the override (ADR-0014).
            if let Some(overlay) = self.overlay {
                if let Some(v) = overlay.leaf(ordinal, coord) {
                    return Ok(v);
                }
            }
            // No rule and no overlay at this leaf: the stored value.
            Ok(cube.get(coord)?)
        } else {
            // Consolidate, pulling each contributing leaf back through the
            // resolver so rule-derived leaves are included with correct weights.
            //
            // Sparse fast path (ADR-0005), used ONLY when it is provably
            // byte-identical to the dense enumeration:
            //   * the registry supplies a fed set for this cube (`fed_set` is
            //     `Some`), meaning the completeness gate proved the cube's feeders
            //     cover every non-stored non-zero rule leaf and no rule is opaque;
            //   * there is no what-if overlay and no element mask in play — both can
            //     produce a value (an overlaid leaf, an access-denied rollup) at a
            //     coordinate the sparse union scan never visits, so the two paths
            //     could otherwise diverge.
            // In every other case fall through to the always-correct dense path. The
            // sparse path unions the stored cells with the fed leaves and sums the
            // same exact i128 weighted algebra, so where it is used the result — and
            // the overflow-error behavior — is identical to `consolidate_with`.
            let sparse = self
                .registry
                .fed_set(ordinal)
                .filter(|_| self.overlay.is_none() && self.mask.is_none());
            match sparse {
                Some(fed) => {
                    cube.consolidate_fed::<CalcError, _>(coord, fed, |lc| self.value(ordinal, lc))
                }
                None => cube.consolidate_with::<CalcError, _>(coord, |lc| self.value(ordinal, lc)),
            }
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

    /// Evaluate a compiled condition against `target` in cube `ordinal`. Exposed
    /// to `provenance` so an explain trace can follow the branch the evaluator
    /// actually took (rather than force-evaluating both branches), using the exact
    /// same condition semantics as evaluation -- no duplicated logic to diverge.
    pub(crate) fn eval_condition(
        &self,
        cond: &CCond,
        ordinal: u32,
        target: &[u32],
    ) -> Result<bool, CalcError> {
        self.eval_cond(cond, ordinal, target)
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
        // Element security (ADR-0015): enforce the engine's deny mask here too, so a
        // string cell is never a bypass of the check the numeric path applies at the
        // cell terminal (`CalcEngine::compute`). Scoped to the mask's target cube.
        if self.ordinal == self.engine.mask_target {
            if let Some(mask) = self.engine.mask {
                if mask.denies_leaf(coord) {
                    return Err(QueryError::AccessDenied);
                }
            }
        }
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

    /// Populate the four Sales/Cost leaves used by the scope-marker ratio tests:
    /// North 120/60 (ratio 2.0), South 200/50 (ratio 4.0). Per-leaf ratios sum to
    /// 6.0; the ratio of totals is 320/110 ≈ 2.9091 — deliberately far apart so a
    /// summed total and a recomputed total can never be confused.
    fn ratio_cube() -> Cube {
        let mut cube = sales_cube();
        let (n, s) = (
            cube.dimension(0).resolve("North").unwrap(),
            cube.dimension(0).resolve("South").unwrap(),
        );
        let (sales, cost) = (
            cube.dimension(1).resolve("Sales").unwrap(),
            cube.dimension(1).resolve("Cost").unwrap(),
        );
        cube.set_leaf(&[n, sales], Fixed::from(120)).unwrap();
        cube.set_leaf(&[n, cost], Fixed::from(60)).unwrap();
        cube.set_leaf(&[s, sales], Fixed::from(200)).unwrap();
        cube.set_leaf(&[s, cost], Fixed::from(50)).unwrap();
        cube
    }

    #[test]
    fn c_scoped_ratio_recomputes_at_the_total_instead_of_summing_child_ratios() {
        // THE core ADR-0041 proof: a C: ratio rule fires AT the consolidated
        // coordinate and recomputes there, so the Total is Sales_total/Cost_total,
        // not the (meaningless) sum of the per-region ratios.
        let reg = build(
            ratio_cube(),
            "C:['Measure':'Margin'] = value['Measure':'Sales'] / value['Measure':'Cost'];",
        );
        let engine = CalcEngine::new(&reg);

        // The Total recomputes: 320 / 110 = 2.9091 (half-even at 4 dp), NOT the
        // summed child ratios 2.0 + 4.0 = 6.0. This is the whole point.
        let total = engine.value(0, &coord(&reg, "Total", "Margin")).unwrap();
        assert_eq!(total, "2.9091".parse::<Fixed>().unwrap());
        assert_ne!(
            total,
            Fixed::from(6),
            "a C: total must NOT be the sum of the child ratios"
        );

        // Cross-check against reading the operands directly at the Total: the C:
        // rule genuinely evaluated `value['Sales'] / value['Cost']` at Total.
        let sales_total = engine.value(0, &coord(&reg, "Total", "Sales")).unwrap();
        let cost_total = engine.value(0, &coord(&reg, "Total", "Cost")).unwrap();
        assert_eq!(sales_total, Fixed::from(320));
        assert_eq!(cost_total, Fixed::from(110));

        // The C: rule does NOT fire at the leaves (an Any dim under C: matches only
        // consolidations), so a leaf Margin has no rule and reads its stored value
        // (unpopulated -> zero). The marker moved the rule to the total, exactly.
        assert_eq!(
            engine.value(0, &coord(&reg, "North", "Margin")).unwrap(),
            Fixed::ZERO,
            "a C: rule does not fire at leaves"
        );
    }

    #[test]
    fn an_unmarked_ratio_stays_leaf_only_and_the_total_aggregates_unchanged() {
        // Backward compatibility (ADR-0041): the SAME ratio formula with NO marker
        // keeps today's exact meaning — it fires at the leaves, and the Total is
        // the ordinary consolidation of those leaf ratios (0.6 + 0.25 = 0.85).
        // Proves the default is untouched and the marker is purely additive.
        let reg = build(
            ratio_cube(),
            "['Measure':'Margin'] = value['Measure':'Sales'] / value['Measure':'Cost'];",
        );
        let engine = CalcEngine::new(&reg);
        // Leaf ratios fire (leaf-only default): 120/60 = 2.0, 200/50 = 4.0.
        assert_eq!(
            engine.value(0, &coord(&reg, "North", "Margin")).unwrap(),
            Fixed::from(2)
        );
        assert_eq!(
            engine.value(0, &coord(&reg, "South", "Margin")).unwrap(),
            Fixed::from(4)
        );
        // The Total is the SUM of the leaf ratios — unchanged from before the
        // feature existed (this is the very number a C: rule fixes, left intact
        // here to prove no existing rule's meaning changed).
        assert_eq!(
            engine.value(0, &coord(&reg, "Total", "Margin")).unwrap(),
            Fixed::from(6)
        );

        // An explicit N: marker is meaning-identical to unmarked: same leaf ratios,
        // same summed total.
        let reg_n = build(
            ratio_cube(),
            "N:['Measure':'Margin'] = value['Measure':'Sales'] / value['Measure':'Cost'];",
        );
        let engine_n = CalcEngine::new(&reg_n);
        assert_eq!(
            engine_n
                .value(0, &coord(&reg_n, "North", "Margin"))
                .unwrap(),
            Fixed::from(2)
        );
        assert_eq!(
            engine_n
                .value(0, &coord(&reg_n, "Total", "Margin"))
                .unwrap(),
            Fixed::from(6),
            "N: is leaf-only, exactly like unmarked"
        );
    }

    #[test]
    fn n_and_c_rules_together_cover_leaves_and_the_total() {
        // The idiom ADR-0041 recommends for a non-additive measure that needs both:
        // one N: statement (leaves) and one C: statement (the total). Each fires in
        // its own scope, and they never collide (a member is a leaf XOR a
        // consolidation), so both coordinates are correct simultaneously.
        let reg = build(
            ratio_cube(),
            "N:['Measure':'Margin'] = value['Measure':'Sales'] / value['Measure':'Cost'];\n\
             C:['Measure':'Margin'] = value['Measure':'Sales'] / value['Measure':'Cost'];",
        );
        let engine = CalcEngine::new(&reg);
        // Leaves: the N: rule -> per-leaf ratios (2.0, 4.0).
        assert_eq!(
            engine.value(0, &coord(&reg, "North", "Margin")).unwrap(),
            Fixed::from(2)
        );
        assert_eq!(
            engine.value(0, &coord(&reg, "South", "Margin")).unwrap(),
            Fixed::from(4)
        );
        // Total: the C: rule -> the recomputed ratio of totals (2.9091), not the
        // sum (6.0).
        assert_eq!(
            engine.value(0, &coord(&reg, "Total", "Margin")).unwrap(),
            "2.9091".parse::<Fixed>().unwrap()
        );
    }

    #[test]
    fn precedence_first_matching_area_wins_with_mixed_markers() {
        // Precedence is first-matching-area in source order (ADR-0007), and the
        // marker only changes WHICH coordinates an area matches, never how ties
        // break. Here two areas can both match the consolidated Total Margin: a C:
        // rule and an explicit `['Region':'Total']` rule. Source order decides.
        //
        // Case A: the C: rule is written FIRST, so it wins at the Total.
        let reg_a = build(
            ratio_cube(),
            "C:['Measure':'Margin'] = value['Measure':'Sales'] / value['Measure':'Cost'];\n\
             ['Region':'Total', 'Measure':'Margin'] = 42;",
        );
        assert_eq!(
            CalcEngine::new(&reg_a)
                .value(0, &coord(&reg_a, "Total", "Margin"))
                .unwrap(),
            "2.9091".parse::<Fixed>().unwrap(),
            "the earlier C: rule wins at the Total"
        );

        // Case B: swap the order — the explicit Total rule is FIRST, so it wins and
        // the Total is the constant 42, proving the marker did not special-case
        // precedence (still pure first-match).
        let reg_b = build(
            ratio_cube(),
            "['Region':'Total', 'Measure':'Margin'] = 42;\n\
             C:['Measure':'Margin'] = value['Measure':'Sales'] / value['Measure':'Cost'];",
        );
        assert_eq!(
            CalcEngine::new(&reg_b)
                .value(0, &coord(&reg_b, "Total", "Margin"))
                .unwrap(),
            Fixed::from(42),
            "the earlier explicit rule wins at the Total"
        );
    }

    /// A what-if overlay over one cube ordinal, for the sandbox tests.
    struct TestOverlay {
        ordinal: u32,
        cells: std::collections::BTreeMap<Vec<u32>, Fixed>,
        scope: u64,
    }
    impl SandboxOverlay for TestOverlay {
        fn leaf(&self, ordinal: u32, coord: &[u32]) -> Option<Fixed> {
            if ordinal == self.ordinal {
                self.cells.get(coord).copied()
            } else {
                None
            }
        }
        fn scope_id(&self) -> u64 {
            self.scope
        }
    }

    #[test]
    fn sandbox_overlay_overrides_leaf_and_recomputes_rules_and_rollups() {
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

        // Base values (no overlay).
        let base = CalcEngine::new(&reg);
        assert_eq!(
            base.value(0, &coord(&reg, "Total", "Sales")).unwrap(),
            Fixed::from(300)
        );
        assert_eq!(
            base.value(0, &coord(&reg, "North", "Margin")).unwrap(),
            Fixed::from(40)
        );
        assert_eq!(
            base.value(0, &coord(&reg, "Total", "Margin")).unwrap(),
            Fixed::from(90)
        );

        // Overlay: what-if North/Sales = 500.
        let mut cells = std::collections::BTreeMap::new();
        cells.insert(coord(&reg, "North", "Sales"), Fixed::from(500));
        let overlay = TestOverlay {
            ordinal: 0,
            cells,
            scope: 42,
        };
        let sb = CalcEngine::with_overlay(&reg, &overlay);
        // The stored leaf reads the override...
        assert_eq!(
            sb.value(0, &coord(&reg, "North", "Sales")).unwrap(),
            Fixed::from(500)
        );
        // ...the rule recomputes over it (500 - 60 = 440)...
        assert_eq!(
            sb.value(0, &coord(&reg, "North", "Margin")).unwrap(),
            Fixed::from(440)
        );
        // ...the Sales consolidation rolls it up (500 + 200 = 700)...
        assert_eq!(
            sb.value(0, &coord(&reg, "Total", "Sales")).unwrap(),
            Fixed::from(700)
        );
        // ...and the Margin consolidation rolls the recomputed leaves (440 + 50).
        assert_eq!(
            sb.value(0, &coord(&reg, "Total", "Margin")).unwrap(),
            Fixed::from(490)
        );

        // A fresh base engine is unaffected by the overlay (base data untouched).
        let base2 = CalcEngine::new(&reg);
        assert_eq!(
            base2.value(0, &coord(&reg, "Total", "Sales")).unwrap(),
            Fixed::from(300)
        );
        assert_eq!(
            base2.value(0, &coord(&reg, "Total", "Margin")).unwrap(),
            Fixed::from(90)
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
        // The Computing marker is dropped on failure, so a repeat of the same
        // read reports the same error again (never a spurious Cycle).
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
    fn string_value_enforces_the_element_deny_mask() {
        use epiphany_core::{CellResolver, ElementMask};
        // A cube with a string measure so string cells are writable.
        let mut region = Dimension::new("Region");
        let n = region.add_leaf("North");
        let s = region.add_leaf("South");
        let mut measure = Dimension::new("Measure");
        let note = measure.add_string("Note");
        let mut cube = Cube::new("Sales", vec![region, measure]).unwrap();
        cube.set_string(&[n, note], "north-note").unwrap();
        cube.set_string(&[s, note], "south-note").unwrap();
        let reg = build(cube, "");

        // Deny the South region element in the queried cube (ordinal 0).
        let counts = [reg.cube.dimension(0).len(), reg.cube.dimension(1).len()];
        let mask = ElementMask::from_denied(&counts, &[vec![s], vec![]]);
        let engine = CalcEngine::new(&reg).with_mask(Some(&mask), 0);
        let view = engine.view(0);
        // A permitted string cell reads through.
        assert_eq!(
            view.string_value(&[n, note]).unwrap().as_deref(),
            Some("north-note")
        );
        // A denied string cell must be refused, exactly like the numeric path --
        // not silently returned (the pre-fix bypass).
        assert_eq!(view.string_value(&[s, note]), Err(QueryError::AccessDenied));
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

    /// A registry that reports a compile failure for its cube, so reads must fail
    /// loud (never silently serve rule-less values). Models the API's
    /// `PinnedRegistry` state after a structural edit invalidates a rule.
    struct BrokenRules {
        cube: Cube,
        error: String,
    }
    impl EvalRegistry for BrokenRules {
        fn cube(&self, o: u32) -> Option<&Cube> {
            (o == 0).then_some(&self.cube)
        }
        fn compiled(&self, _o: u32) -> Option<&CompiledModel> {
            // No compiled model exists: the source failed to compile.
            None
        }
        fn ordinal(&self, name: &str) -> Option<u32> {
            (name == self.cube.name()).then_some(0)
        }
        fn compile_error(&self, o: u32) -> Option<&str> {
            (o == 0).then_some(self.error.as_str())
        }
    }

    #[test]
    fn a_cube_whose_rules_fail_to_compile_errors_on_read_not_stored_values() {
        let mut cube = sales_cube();
        let (n, s) = (
            cube.dimension(0).resolve("North").unwrap(),
            cube.dimension(0).resolve("South").unwrap(),
        );
        let sales = cube.dimension(1).resolve("Sales").unwrap();
        // Stored leaves exist: a silent rule-drop would serve THESE (wrong numbers).
        cube.set_leaf(&[n, sales], Fixed::from(100)).unwrap();
        cube.set_leaf(&[s, sales], Fixed::from(200)).unwrap();
        let reg = BrokenRules {
            cube,
            error: "unknown element 'Ghost' in dimension 'Measure'".to_string(),
        };
        let resolve = |region: &str, measure: &str| -> Vec<u32> {
            vec![
                reg.cube.dimension(0).resolve(region).unwrap(),
                reg.cube.dimension(1).resolve(measure).unwrap(),
            ]
        };
        let engine = CalcEngine::new(&reg);
        // A direct leaf read fails loud rather than returning the stored 100.
        match engine.value(0, &resolve("North", "Sales")) {
            Err(CalcError::RulesFailed { cube, error }) => {
                assert_eq!(cube, "Sales");
                assert!(
                    error.contains("Ghost"),
                    "carries the compile error: {error}"
                );
            }
            other => panic!("expected RulesFailed, got {other:?}"),
        }
        // A consolidation over the broken cube's leaves also fails loud (the failure
        // propagates through the rollup instead of silently summing stored leaves).
        assert!(matches!(
            engine.value(0, &resolve("Total", "Sales")),
            Err(CalcError::RulesFailed { .. })
        ));
    }

    #[test]
    fn a_carried_memo_gives_identical_results_and_accumulates() {
        // Build a rule-bearing cube so reads do real work worth memoizing.
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

        // Baseline: one engine (fresh memo) reading Total Margin.
        let total_margin = coord(&reg, "Total", "Margin");
        let baseline = CalcEngine::new(&reg).value(0, &total_margin).unwrap();
        assert_eq!(baseline, Fixed::from(90));

        // Now carry a memo across three per-cell engines (as the resolver does):
        // North Margin, then South Margin, then Total Margin. The carried memo must
        // start empty, grow across reads, and yield byte-identical results.
        let mut memo = CalcMemo::new();
        assert!(memo.is_empty());
        let reads = [
            (coord(&reg, "North", "Margin"), Fixed::from(40)),
            (coord(&reg, "South", "Margin"), Fixed::from(50)),
            (total_margin.clone(), Fixed::from(90)),
        ];
        for (c, expected) in &reads {
            let engine = CalcEngine::with_memo(&reg, memo);
            let got = engine.value(0, c).unwrap();
            assert_eq!(&got, expected, "carried-memo read matches");
            memo = engine.into_memo();
        }
        // The memo accumulated computed cells across the three reads (behavioral
        // proof of cross-cell reuse: a fresh-per-cell engine would leave it empty).
        assert!(
            memo.len() >= 3,
            "carried memo retains computed cells across reads, got {}",
            memo.len()
        );

        // Determinism: the carried-memo Total equals the fresh-memo baseline.
        let carried_total = CalcEngine::with_memo(&reg, memo)
            .value(0, &total_margin)
            .unwrap();
        assert_eq!(carried_total, baseline);
    }
}
