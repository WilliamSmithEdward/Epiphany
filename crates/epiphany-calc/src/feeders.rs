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

use epiphany_core::Cube;

use crate::compiled::{AddrSlot, CCell, CExpr, CompiledModel, CompiledRule, DimPredicate, RuleId};

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

/// Infer feeders for a cube's compiled rules.
///
/// `target_ordinal` is the cube's own ordinal (so same-cube inputs, which drive
/// the feed set, are recognized). Cross-cube and consolidated inputs do not
/// localize the feed set, so they are ignored for localization; a rule with no
/// analyzable same-cube input is reported opaque.
pub fn infer_feeders(cube: &Cube, model: &CompiledModel, target_ordinal: u32) -> FeederInference {
    let mut result = FeederInference::default();
    for (i, rule) in model.rules.iter().enumerate() {
        let id = RuleId(i);
        if targets_consolidated(cube, rule) {
            // A consolidation-override targets a consolidated cell, not a leaf, so
            // it needs no leaf feeder (its value is computed at the coord).
            continue;
        }
        match infer_rule(cube, rule, target_ordinal, &mut result.index) {
            Ok(()) => {}
            Err(reason) => result.opaque.push(OpaqueRule { rule: id, reason }),
        }
    }
    result
}

/// Whether any pinned dimension of the area names a consolidated element (making
/// the rule a consolidation override rather than a leaf rule).
fn targets_consolidated(cube: &Cube, rule: &CompiledRule) -> bool {
    rule.area.per_dim.iter().enumerate().any(|(d, pred)| {
        matches!(pred, DimPredicate::OneOf(set)
            if set.iter().any(|&i| cube
                .dimension(d)
                .element(i)
                .map(|e| !e.kind.is_leaf())
                .unwrap_or(false)))
    })
}

fn collect_cells<'a>(expr: &'a CExpr, out: &mut Vec<&'a CCell>) {
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

/// Infer feeders for one analyzable leaf rule, or return why it is opaque.
fn infer_rule(
    cube: &Cube,
    rule: &CompiledRule,
    target_ordinal: u32,
    index: &mut FeederIndex,
) -> Result<(), String> {
    let mut cells = Vec::new();
    collect_cells(&rule.expr, &mut cells);
    let same_cube: Vec<&CCell> = cells
        .iter()
        .copied()
        .filter(|c| c.cube == target_ordinal)
        .collect();
    if same_cube.is_empty() {
        return Err("no same-cube input to localize feeders".to_string());
    }

    // The target member for a pinned-input dimension must be a single leaf, so we
    // can map a populated input back to one target leaf.
    let single_target = |d: usize| -> Option<u32> {
        match &rule.area.per_dim[d] {
            DimPredicate::OneOf(set) if set.len() == 1 => Some(set[0]),
            _ => None,
        }
    };

    for cell in &same_cube {
        // Validate the analyzable shape first: every pinned-input dim needs a
        // single-leaf target member.
        for (d, slot) in cell.addr.iter().enumerate() {
            if matches!(slot, AddrSlot::Pinned(_)) && single_target(d).is_none() {
                return Err(format!(
                    "input pins dimension {d} but the target area for it is not a single member"
                ));
            }
        }
        // Walk the populated leaves; a cell that matches the input pattern feeds
        // the corresponding target leaf.
        for (pop, _) in cube.cell_entries() {
            if !input_matches(cube, cell, &rule.area.per_dim, &pop) {
                continue;
            }
            let target: Vec<u32> = cell
                .addr
                .iter()
                .enumerate()
                .map(|(d, slot)| match slot {
                    AddrSlot::FromTarget(_) => pop[d],
                    AddrSlot::Pinned(_) => single_target(d).expect("validated above"),
                })
                .collect();
            if rule.area.matches(cube, &target) {
                index.insert(&target);
            }
        }
    }
    Ok(())
}

/// Whether a populated leaf `pop` could be the input addressed by `cell` for some
/// target in the rule's area: pinned dims must match the pin, and copied
/// (FromTarget) dims must be a leaf the area admits.
fn input_matches(cube: &Cube, cell: &CCell, area: &[DimPredicate], pop: &[u32]) -> bool {
    if cell.addr.len() != pop.len() {
        return false;
    }
    for (d, slot) in cell.addr.iter().enumerate() {
        match slot {
            AddrSlot::Pinned(pin) => {
                if pop[d] != *pin {
                    return false;
                }
            }
            AddrSlot::FromTarget(_) => {
                let is_leaf = cube
                    .dimension(d)
                    .element(pop[d])
                    .map(|e| e.kind.is_leaf())
                    .unwrap_or(false);
                let admitted = match &area[d] {
                    DimPredicate::Any => is_leaf,
                    DimPredicate::OneOf(set) => set.binary_search(&pop[d]).is_ok(),
                };
                if !admitted {
                    return false;
                }
            }
        }
    }
    true
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
    fn rule_with_no_same_cube_input_is_opaque() {
        let cube = sales_cube();
        // A constant rule has no input to localize feeders.
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse("['Measure':'Margin'] = 5;").unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&cube, &model, 0);
        assert_eq!(inf.opaque.len(), 1);
        assert!(inf.index.is_empty());
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
}
