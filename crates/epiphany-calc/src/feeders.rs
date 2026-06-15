//! Sparse feeds and automatic feeder inference (ADR-0005).
//!
//! A feeder marks a rule-derived leaf coordinate as potentially populated, so a
//! consolidation can include it via the sparse union scan
//! ([`epiphany_core::Cube::consolidate_fed`]) instead of enumerating the dense
//! leaf space. [`infer_feeders`] derives feeders for the statically analyzable
//! rule shape (a leaf rule whose value comes from same-cube inputs): feed a
//! target leaf wherever an input leaf it reads is populated. This is a sound
//! over-approximation (it never under-feeds an analyzable rule); rules it cannot
//! analyze are reported so they can be manually fed or diagnosed (Phase 4F).
//! Determinism: the index is a sorted `BTreeSet`.

use std::collections::BTreeSet;

use epiphany_core::{Cube, Fixed};

use crate::compiled::{AddrSlot, CCell, CExpr, CompiledArea, CompiledModel, DimPredicate, RuleId};
use crate::eval::{CalcEngine, CalcError, EvalRegistry};
use crate::rules::ArithOp;

/// Cap on the cartesian expansion of one consolidated input. Beyond this the
/// input is conservatively assumed potent (feed the target) rather than fully
/// enumerated, bounding the cost of `input_potent` on a pathological hierarchy.
/// Over-feeding is a warning, never an under-feed, so the cap stays sound.
const INPUT_EXPANSION_CAP: usize = 4096;

/// Approximate bytes a fed cell costs (index slot plus the rule evaluation it
/// enables), used to estimate the waste of over-feeding (ROADMAP section 8).
const FED_CELL_BYTES: usize = 20;

/// A sparse set of fed (rule-derived) leaf coordinates, sorted for determinism.
#[derive(Debug, Clone, Default)]
pub struct FeederIndex {
    coords: BTreeSet<Box<[u32]>>,
}

impl FeederIndex {
    /// An empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark a coordinate fed.
    pub fn insert(&mut self, coord: &[u32]) {
        self.coords.insert(coord.to_vec().into_boxed_slice());
    }

    /// Whether a coordinate is fed.
    pub fn contains(&self, coord: &[u32]) -> bool {
        self.coords.contains(coord)
    }

    /// The fed coordinates, sorted, as a slice-friendly vector for
    /// [`epiphany_core::Cube::consolidate_fed`].
    pub fn coords(&self) -> Vec<Box<[u32]>> {
        self.coords.iter().cloned().collect()
    }

    /// The number of fed coordinates.
    pub fn len(&self) -> usize {
        self.coords.len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.coords.is_empty()
    }
}

/// A rule whose feeders could not be auto-inferred (it needs manual feeders or
/// only diagnostics), with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpaqueRule {
    /// The rule id.
    pub rule: RuleId,
    /// Why inference could not analyze it.
    pub reason: String,
}

/// The result of feeder inference: the fed set plus the rules it could not
/// analyze.
#[derive(Debug, Clone, Default)]
pub struct FeederInference {
    /// The inferred fed coordinates.
    pub index: FeederIndex,
    /// Rules inference could not analyze (manual feeders / diagnostics needed).
    pub opaque: Vec<OpaqueRule>,
}

/// One analyzable rule: its target leaf coordinates, the same-cube input cells
/// whose population drives the feed, and whether the rule has a base
/// contribution (a term that can be non-zero even when every same-cube input is
/// zero, so the whole target area must be fed).
struct Analyzable<'a> {
    targets: Vec<Vec<u32>>,
    inputs: Vec<&'a CCell>,
    base_potent: bool,
}

/// Whether `expr` has a *localizable* base contribution: a term that can be
/// non-zero when every cell it reads is zero, so the rule is non-zero across its
/// whole target area regardless of which inputs are populated (a non-zero
/// constant, an attribute, or such a term inside a conditional branch). Such a
/// rule feeds its entire area. A conservative-but-sound static analysis: it may
/// answer `true` when the real value happens to be zero (a harmless over-feed),
/// but never `false` when a constant/attribute term can be non-zero.
///
/// Cells are deliberately *not* base contributions: a same-cube cell is zeroed by
/// definition, and a cross-cube cell is a non-localizable input handled by the
/// opaque path (a cross-cube-only rule is reported, not fed), per ADR-0005.
fn base_potent(expr: &CExpr) -> bool {
    match expr {
        CExpr::Num(n) => *n != Fixed::ZERO,
        // An attribute value can be non-zero on its own.
        CExpr::AttrNum { .. } => true,
        CExpr::Cell(_) => false,
        CExpr::Undef => false,
        CExpr::Neg(e) => base_potent(e),
        CExpr::Bin { op, left, right } => match op {
            // A sum/difference is non-zero if either side can be.
            ArithOp::Add | ArithOp::Sub => base_potent(left) || base_potent(right),
            // A product is non-zero only if both sides can be.
            ArithOp::Mul => base_potent(left) && base_potent(right),
            // A quotient is non-zero only if the numerator can be.
            ArithOp::Div => base_potent(left),
        },
        // The value is one of the branches; the condition does not contribute a
        // magnitude. A missing else branch is zero.
        CExpr::If {
            cond: _,
            then,
            otherwise,
        } => base_potent(then) || otherwise.as_ref().is_some_and(|o| base_potent(o)),
    }
}

/// Infer feeders for a cube's compiled rules (ADR-0005).
///
/// For each rule that targets leaves, every target leaf is fed when at least one
/// of the rule's same-cube inputs is *potentially non-zero* at that target: a
/// stored leaf, a leaf fed by another rule, or a consolidated input that rolls up
/// any such leaf. This is computed to a **fixpoint** seeded by the stored leaves,
/// so a rule reading another rule's derived output is fed too (chained rules).
/// It is a sound over-approximation: it never under-feeds an analyzable rule, and
/// at worst over-feeds a target whose inputs turn out to be zero (a warning).
///
/// A rule with a base contribution (a non-zero constant or attribute term, see
/// [`base_potent`]) feeds its whole target area, since it is non-zero everywhere
/// regardless of input population. `target_ordinal` is the cube's own ordinal;
/// cross-cube inputs cannot be localized from this cube's data, so a rule whose
/// only inputs are cross-cube is reported opaque rather than guessed at. A rule
/// whose area selects no leaf coordinates is a pure consolidation override and
/// needs no leaf feeder.
pub fn infer_feeders(cube: &Cube, model: &CompiledModel, target_ordinal: u32) -> FeederInference {
    let mut result = FeederInference::default();

    // Classify rules: skip pure overrides (no leaf targets), report opaque rules
    // (no same-cube input to localize), and keep the rest as analyzable.
    let mut analyzable: Vec<Analyzable> = Vec::new();
    for (i, rule) in model.rules.iter().enumerate() {
        let targets = area_leaf_coords(cube, &rule.area);
        if targets.is_empty() {
            continue; // pure consolidation override: value computed at the coord
        }
        let mut cells = Vec::new();
        collect_cells(&rule.expr, &mut cells);
        let inputs: Vec<&CCell> = cells
            .iter()
            .copied()
            .filter(|c| c.cube == target_ordinal)
            .collect();
        let bp = base_potent(&rule.expr);
        if bp || !inputs.is_empty() {
            // Analyzable: a base term feeds the whole area, and/or same-cube inputs
            // localize the feed via the fixpoint.
            analyzable.push(Analyzable {
                targets,
                inputs,
                base_potent: bp,
            });
        } else if !cells.is_empty() {
            // The only inputs are cross-cube (or otherwise not same-cube): the feed
            // cannot be localized from this cube. Report it rather than guess
            // (ADR-0005), so it can be manually fed or diagnosed.
            result.opaque.push(OpaqueRule {
                rule: RuleId(i),
                reason: "only cross-cube inputs; feeders cannot be localized".to_string(),
            });
        }
        // Otherwise the rule has no cells and no base term: identically zero over
        // its area, so there is nothing to feed and nothing to report.
    }

    // Fixpoint: a leaf is "potent" (potentially non-zero) if it is stored or has
    // been fed. Feed a target whose input is potent; the newly fed target is then
    // itself potent, so a later iteration can feed a rule that reads it. Each
    // round only adds feeders, and there are finitely many target leaves, so this
    // terminates.
    let mut potent: BTreeSet<Vec<u32>> = cube.cell_entries().map(|(coord, _)| coord).collect();
    loop {
        let mut changed = false;
        for a in &analyzable {
            for target in &a.targets {
                if result.index.contains(target) {
                    continue;
                }
                // A base-potent rule is non-zero across its whole area, so feed
                // every target; otherwise feed where a same-cube input is potent.
                let feed = a.base_potent
                    || a.inputs
                        .iter()
                        .any(|cell| input_potent(cube, cell, target, &potent));
                if feed {
                    result.index.insert(target);
                    potent.insert(target.clone());
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    result
}

/// Feeder validation diagnostics (Phase 4F).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeederDiagnostics {
    /// Rule-target leaves with a non-zero rule value that are NOT fed: a silent
    /// wrong-zero in rollups. This is the hard error condition.
    pub under_fed: Vec<Vec<u32>>,
    /// Fed coordinates whose rule value is zero: wasted scan/RAM (a warning).
    pub over_fed: Vec<Vec<u32>>,
    /// The number of fed coordinates.
    pub fed_cell_count: usize,
    /// An estimate of the RAM/scan cost of the over-fed cells.
    pub estimated_over_fed_bytes: usize,
}

impl FeederDiagnostics {
    /// Whether the model is correctly fed (no under-feed). Over-feed is a warning,
    /// not a correctness failure.
    pub fn is_clean(&self) -> bool {
        self.under_fed.is_empty()
    }
}

/// Validate a feeder index against the true (densely-evaluated) rule values for a
/// cube, reporting under-feed (an error) and over-feed (a warning). This is an
/// explicit on-demand operation, never on the read path.
///
/// Determinism: candidate target leaves and fed coordinates are checked in sorted
/// order, so the lists are byte-identical run to run.
pub fn validate_feeders(
    registry: &dyn EvalRegistry,
    ordinal: u32,
    index: &FeederIndex,
) -> Result<FeederDiagnostics, CalcError> {
    let cube = registry
        .cube(ordinal)
        .ok_or(CalcError::UnknownCube(ordinal))?;
    let model = match registry.compiled(ordinal) {
        Some(m) => m,
        None => return Ok(FeederDiagnostics::default()),
    };
    let engine = CalcEngine::new(registry);

    // Under-feed: every leaf a rule targets with a non-zero value must be fed.
    // `area_leaf_coords` returns only leaf targets, so a pure consolidation
    // override (no leaf targets) contributes none and a mixed-target rule still
    // has its leaf targets checked (no override skip can hide an under-feed).
    let mut under = BTreeSet::new();
    for rule in &model.rules {
        for target in area_leaf_coords(cube, &rule.area) {
            if engine.value(ordinal, &target)? != Fixed::ZERO && !index.contains(&target) {
                under.insert(target);
            }
        }
    }

    // Over-feed: a fed coordinate whose rule value is zero is wasted.
    let mut over = BTreeSet::new();
    for fed in index.coords() {
        if engine.value(ordinal, &fed)? == Fixed::ZERO {
            over.insert(fed.to_vec());
        }
    }

    Ok(FeederDiagnostics {
        under_fed: under.into_iter().collect(),
        estimated_over_fed_bytes: over.len() * FED_CELL_BYTES,
        over_fed: over.into_iter().collect(),
        fed_cell_count: index.len(),
    })
}

/// Dense enumeration of the LEAF coordinates a rule's area selects (the cartesian
/// product of each dimension's admitted leaf members). Bounded by the area size.
fn area_leaf_coords(cube: &Cube, area: &CompiledArea) -> Vec<Vec<u32>> {
    let mut per_dim: Vec<Vec<u32>> = Vec::with_capacity(cube.rank());
    for d in 0..cube.rank() {
        let is_leaf = |i: u32| {
            cube.dimension(d)
                .element(i)
                .map(|e| e.kind.is_leaf())
                .unwrap_or(false)
        };
        let leaves: Vec<u32> = match &area.per_dim[d] {
            DimPredicate::Any => (0..cube.dimension(d).len())
                .filter(|&i| is_leaf(i))
                .collect(),
            DimPredicate::OneOf(set) => set.iter().copied().filter(|&i| is_leaf(i)).collect(),
        };
        per_dim.push(leaves);
    }
    let total: usize = per_dim.iter().map(|v| v.len()).product();
    if total == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(total);
    for n in 0..total {
        let mut rem = n;
        let mut coord = vec![0u32; cube.rank()];
        for d in 0..cube.rank() {
            let len = per_dim[d].len();
            coord[d] = per_dim[d][rem % len];
            rem /= len;
        }
        out.push(coord);
    }
    out
}

pub(crate) fn collect_cells<'a>(expr: &'a CExpr, out: &mut Vec<&'a CCell>) {
    match expr {
        CExpr::Cell(c) => out.push(c),
        CExpr::Neg(e) => collect_cells(e, out),
        CExpr::Bin { left, right, .. } => {
            collect_cells(left, out);
            collect_cells(right, out);
        }
        CExpr::If {
            cond: _,
            then,
            otherwise,
        } => {
            collect_cells(then, out);
            if let Some(o) = otherwise {
                collect_cells(o, out);
            }
        }
        CExpr::Num(_) | CExpr::AttrNum { .. } | CExpr::Undef => {}
    }
}

/// Whether the input addressed by `cell` for target leaf `target` is potentially
/// non-zero: at least one leaf it resolves to is in `potent`. Each dimension
/// resolves to a leaf set: a copied (`FromTarget`) dim to the target's leaf, a
/// pinned leaf to itself, and a pinned consolidated element to the leaves it rolls
/// up (so a rule reading a consolidated input is fed when any contributing leaf
/// is potent). An input that resolves to no leaves (an empty consolidation) is
/// not potent, which is correct: its value is zero.
fn input_potent(cube: &Cube, cell: &CCell, target: &[u32], potent: &BTreeSet<Vec<u32>>) -> bool {
    if cell.addr.len() != target.len() {
        return false;
    }
    let mut per_dim: Vec<Vec<u32>> = Vec::with_capacity(cell.addr.len());
    for (d, slot) in cell.addr.iter().enumerate() {
        let element = match slot {
            AddrSlot::Pinned(pin) => *pin,
            // `cell.addr` is in the referenced cube's dimension order; for a
            // same-cube reference (the only kind that uses `FromTarget`) that
            // matches the target's order, so the copied member is `target[d]`.
            AddrSlot::FromTarget(_) => target[d],
        };
        // Expand to the contributing leaves under `element` (a leaf yields itself;
        // a consolidation yields its non-zero-weight leaves, deterministically).
        let leaves: Vec<u32> = match cube.dimension(d).leaf_weights(element) {
            Ok(lw) => lw.into_iter().map(|(leaf, _)| leaf).collect(),
            Err(_) => return false,
        };
        if leaves.is_empty() {
            return false;
        }
        per_dim.push(leaves);
    }
    // Bound the work on a pathological hierarchy: a huge consolidated input is
    // conservatively treated as potent (a sound over-feed) rather than enumerated.
    let total: usize = per_dim.iter().map(|v| v.len()).product();
    if total > INPUT_EXPANSION_CAP {
        return true;
    }
    cartesian_any(&per_dim, |coord| potent.contains(coord))
}

/// Whether any coordinate in the cartesian product of `per_dim` satisfies `pred`.
/// Deterministic (it walks the product in index order) and bounded by the product
/// size, which is small in practice (most dimensions resolve to a single leaf).
fn cartesian_any(per_dim: &[Vec<u32>], mut pred: impl FnMut(&[u32]) -> bool) -> bool {
    let total: usize = per_dim.iter().map(|v| v.len()).product();
    if total == 0 {
        return false;
    }
    let mut coord = vec![0u32; per_dim.len()];
    for n in 0..total {
        let mut rem = n;
        for (d, dim) in per_dim.iter().enumerate() {
            coord[d] = dim[rem % dim.len()];
            rem /= dim.len();
        }
        if pred(&coord) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::compile;
    use crate::eval::{CalcEngine, EvalRegistry};
    use crate::registry::SingleCube;
    use crate::rules::parse;
    use epiphany_core::{Cube, Dimension, Fixed};

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

    #[test]
    fn infers_feeders_for_a_leaf_rule() {
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
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse("['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];")
                .unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&cube, &model, 0);
        assert!(inf.opaque.is_empty(), "the rule is analyzable");
        let margin = cube.dimension(1).resolve("Margin").unwrap();
        // North and South have a populated input -> both Margin leaves fed.
        assert!(inf.index.contains(&[n, margin]));
        assert!(inf.index.contains(&[s, margin]));
        assert_eq!(inf.index.len(), 2);
    }

    #[test]
    fn sparse_fed_consolidation_equals_dense() {
        // With complete inferred feeders, the sparse union scan equals the dense
        // consolidate_with for the rule-derived rollup (no under-feed).
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
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse("['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];")
                .unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&cube, &model, 0);
        let reg = OneCube { cube, model };
        let engine = CalcEngine::new(&reg);
        let total = reg.cube.dimension(0).resolve("Total").unwrap();
        let margin = reg.cube.dimension(1).resolve("Margin").unwrap();
        let coord = [total, margin];
        // Dense (the always-correct path the evaluator uses).
        let dense = engine.value(0, &coord).unwrap();
        // Sparse union scan over the inferred feeders.
        let fed = inf.index.coords();
        let sparse = reg
            .cube
            .consolidate_fed::<epiphany_core::QueryError, _>(&coord, &fed, |lc| {
                Ok(engine.value(0, lc)?)
            })
            .unwrap();
        assert_eq!(sparse, dense);
        assert_eq!(dense, Fixed::from(90));
    }

    #[test]
    fn constant_rule_is_base_potent_and_feeds_its_whole_area() {
        let cube = sales_cube();
        // A non-zero constant is non-zero across the whole target area, so every
        // target leaf is fed (and the rule is not opaque: it is fully analyzed).
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse("['Measure':'Margin'] = 5;").unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&cube, &model, 0);
        assert!(inf.opaque.is_empty());
        let margin = cube.dimension(1).resolve("Margin").unwrap();
        let (n, s) = (
            cube.dimension(0).resolve("North").unwrap(),
            cube.dimension(0).resolve("South").unwrap(),
        );
        assert!(inf.index.contains(&[n, margin]));
        assert!(inf.index.contains(&[s, margin]));
    }

    #[test]
    fn additive_constant_feeds_targets_with_empty_inputs() {
        // Margin = Sales + 5 is non-zero even where Sales is empty (it is 5), so
        // both regions must be fed -- the base-potent case the per-input fixpoint
        // alone would under-feed.
        let mut cube = sales_cube();
        let n = cube.dimension(0).resolve("North").unwrap();
        let sales = cube.dimension(1).resolve("Sales").unwrap();
        cube.set_leaf(&[n, sales], Fixed::from(100)).unwrap();
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse("['Measure':'Margin'] = value['Measure':'Sales'] + 5;").unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&cube, &model, 0);
        let margin = cube.dimension(1).resolve("Margin").unwrap();
        let s = cube.dimension(0).resolve("South").unwrap();
        assert!(inf.index.contains(&[n, margin]));
        assert!(
            inf.index.contains(&[s, margin]),
            "South is fed even though its Sales input is empty (Margin = 5 there)"
        );
    }

    #[test]
    fn conditional_with_constant_branch_is_base_potent() {
        // The else branch is a non-zero constant, so the rule can be non-zero
        // anywhere: every target is fed regardless of the input population.
        let mut cube = sales_cube();
        let n = cube.dimension(0).resolve("North").unwrap();
        let sales = cube.dimension(1).resolve("Sales").unwrap();
        cube.set_leaf(&[n, sales], Fixed::from(100)).unwrap();
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse(
                "['Measure':'Margin'] = if value['Measure':'Sales'] > 100 \
                 then value['Measure':'Sales'] else 50;",
            )
            .unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&cube, &model, 0);
        let margin = cube.dimension(1).resolve("Margin").unwrap();
        let s = cube.dimension(0).resolve("South").unwrap();
        assert!(inf.index.contains(&[n, margin]));
        assert!(inf.index.contains(&[s, margin]));
    }

    /// Build the Margin model populated for the given regions, returning the
    /// registry and inferred feeders.
    fn margin_model(populate_south: bool) -> (OneCube, FeederInference) {
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
        if populate_south {
            cube.set_leaf(&[s, sales], Fixed::from(200)).unwrap();
            cube.set_leaf(&[s, cost], Fixed::from(150)).unwrap();
        }
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse("['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];")
                .unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&cube, &model, 0);
        (OneCube { cube, model }, inf)
    }

    #[test]
    fn validate_clean_model_has_no_under_or_over_feed() {
        let (reg, inf) = margin_model(true);
        let diag = validate_feeders(&reg, 0, &inf.index).unwrap();
        assert!(diag.is_clean(), "no under-feed");
        assert!(diag.over_fed.is_empty(), "no over-feed");
        assert_eq!(diag.fed_cell_count, 2);
    }

    #[test]
    fn missing_feeders_are_reported_under_fed() {
        let (reg, _inf) = margin_model(true);
        // An empty index under-feeds both non-zero Margin leaves.
        let diag = validate_feeders(&reg, 0, &FeederIndex::new()).unwrap();
        assert!(!diag.is_clean());
        assert_eq!(diag.under_fed.len(), 2);
    }

    #[test]
    fn fed_but_zero_is_reported_over_fed() {
        // South unpopulated: its Margin is zero, so feeding it is over-feed.
        let (reg, inf) = margin_model(false);
        let s = reg.cube.dimension(0).resolve("South").unwrap();
        let margin = reg.cube.dimension(1).resolve("Margin").unwrap();
        let mut idx = inf.index.clone();
        idx.insert(&[s, margin]);
        let diag = validate_feeders(&reg, 0, &idx).unwrap();
        assert_eq!(diag.over_fed, vec![vec![s, margin]]);
        assert!(diag.estimated_over_fed_bytes > 0);
        assert!(diag.is_clean(), "over-feed is not an under-feed");
    }

    #[test]
    fn consolidation_override_needs_no_feeder() {
        let cube = sales_cube();
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse("['Region':'Total', 'Measure':'Sales'] = 1000;").unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&cube, &model, 0);
        assert!(inf.index.is_empty());
        assert!(
            inf.opaque.is_empty(),
            "an override is not opaque, just feeder-less"
        );
    }

    /// Like `sales_cube` but with an extra leaf measure `Net`, for chained rules.
    fn chain_cube() -> Cube {
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
        measure.add_leaf("Net");
        Cube::new("Sales", vec![region, measure]).unwrap()
    }

    #[test]
    fn chained_rules_feed_the_downstream_target() {
        let mut cube = chain_cube();
        let n = cube.dimension(0).resolve("North").unwrap();
        let (sales, cost) = (
            cube.dimension(1).resolve("Sales").unwrap(),
            cube.dimension(1).resolve("Cost").unwrap(),
        );
        cube.set_leaf(&[n, sales], Fixed::from(100)).unwrap();
        cube.set_leaf(&[n, cost], Fixed::from(60)).unwrap();
        // Rule A derives Margin; Rule B reads the derived Margin to derive Net.
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse(
                "['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];\n\
                 ['Measure':'Net'] = value['Measure':'Margin'];",
            )
            .unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&cube, &model, 0);
        let (margin, net) = (
            cube.dimension(1).resolve("Margin").unwrap(),
            cube.dimension(1).resolve("Net").unwrap(),
        );
        let s = cube.dimension(0).resolve("South").unwrap();
        // The fixpoint feeds Net[North], which reads the rule-derived Margin[North]
        // -- the chained dependency the stored-leaf-only inference missed.
        assert!(inf.index.contains(&[n, margin]));
        assert!(inf.index.contains(&[n, net]));
        // South has no stored inputs, so nothing is fed there (still tight).
        assert!(!inf.index.contains(&[s, margin]));
        assert!(!inf.index.contains(&[s, net]));
    }

    #[test]
    fn consolidated_input_feeds_via_its_leaves() {
        let mut cube = chain_cube();
        let n = cube.dimension(0).resolve("North").unwrap();
        let (sales, cost) = (
            cube.dimension(1).resolve("Sales").unwrap(),
            cube.dimension(1).resolve("Cost").unwrap(),
        );
        cube.set_leaf(&[n, sales], Fixed::from(100)).unwrap();
        cube.set_leaf(&[n, cost], Fixed::from(60)).unwrap();
        // Net reads the Region:Total rollup of the rule-derived Margin: a
        // consolidated input. The total is non-zero (Margin[North] != 0), so both
        // Net leaves are fed -- the consolidated-input case the old inference missed.
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse(
                "['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];\n\
                 ['Measure':'Net'] = value['Region':'Total', 'Measure':'Margin'];",
            )
            .unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&cube, &model, 0);
        let net = cube.dimension(1).resolve("Net").unwrap();
        let s = cube.dimension(0).resolve("South").unwrap();
        assert!(inf.index.contains(&[n, net]));
        assert!(inf.index.contains(&[s, net]));
    }

    #[test]
    fn multi_element_target_with_pinned_input_feeds_all_targets() {
        let mut cube = chain_cube();
        let n = cube.dimension(0).resolve("North").unwrap();
        let cost = cube.dimension(1).resolve("Cost").unwrap();
        cube.set_leaf(&[n, cost], Fixed::from(60)).unwrap();
        // Target is every leaf measure; the input pins one measure. The old
        // inference rejected a pinned input on a multi-member target dim as opaque;
        // now every target leaf whose pinned input is populated is fed.
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse("['Measure':{leaves}] = value['Measure':'Cost'];").unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&cube, &model, 0);
        assert!(
            inf.opaque.is_empty(),
            "a pinned input on a multi-leaf target is now analyzable"
        );
        let s = cube.dimension(0).resolve("South").unwrap();
        let leaves =
            ["Sales", "Cost", "Margin", "Net"].map(|m| cube.dimension(1).resolve(m).unwrap());
        for m in leaves {
            assert!(
                inf.index.contains(&[n, m]),
                "North feeds every measure leaf"
            );
            assert!(!inf.index.contains(&[s, m]), "South has no populated input");
        }
    }

    #[test]
    fn mixed_target_override_rule_under_feed_is_detected() {
        let mut cube = sales_cube();
        let n = cube.dimension(0).resolve("North").unwrap();
        let (sales, cost) = (
            cube.dimension(1).resolve("Sales").unwrap(),
            cube.dimension(1).resolve("Cost").unwrap(),
        );
        cube.set_leaf(&[n, sales], Fixed::from(100)).unwrap();
        cube.set_leaf(&[n, cost], Fixed::from(60)).unwrap();
        // The Region target spans both leaves and the Total consolidation. The old
        // classifier treated this as a pure override and skipped validation, hiding
        // an under-feed of the leaf target [North, Margin] (value 40).
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse(
                "['Region':{descendants of 'Total'}, 'Measure':'Margin'] = \
                 value['Measure':'Sales'] - value['Measure':'Cost'];",
            )
            .unwrap(),
            1,
        )
        .unwrap();
        let reg = OneCube { cube, model };
        let diag = validate_feeders(&reg, 0, &FeederIndex::new()).unwrap();
        let n = reg.cube.dimension(0).resolve("North").unwrap();
        let margin = reg.cube.dimension(1).resolve("Margin").unwrap();
        assert!(
            diag.under_fed.contains(&vec![n, margin]),
            "the mixed-target rule's leaf under-feed is no longer silently skipped"
        );
    }
}
