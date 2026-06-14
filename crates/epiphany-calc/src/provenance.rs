//! Calculation provenance ("explain").
//!
//! Given a cell, [`explain`] returns a [`CellTrace`]: the value, what produced it
//! (a stored leaf, a firing rule with its source span, or a consolidation), and
//! the input cells consulted (recursively, depth-bounded). It is a dedicated,
//! opt-in walk separate from the hot evaluation path, so normal reads carry no
//! tracing cost. The trace value always agrees with the evaluator, and inputs are
//! ordered deterministically.

use epiphany_core::{CellTrace, Cube, ExplainDepth, Fixed, TraceKind};

use crate::compiled::AddrSlot;
use crate::eval::{CalcEngine, CalcError, EvalRegistry};
use crate::feeders::collect_cells;

/// A safety cap on `ExplainDepth::Full` recursion (the per-query cycle guard in
/// the evaluator already prevents infinite loops; this bounds trace size).
const FULL_DEPTH_CAP: u32 = 32;

/// Explain the value at `coord` in cube `ordinal`, to the given depth.
pub fn explain(
    registry: &dyn EvalRegistry,
    ordinal: u32,
    coord: &[u32],
    depth: ExplainDepth,
) -> Result<CellTrace, CalcError> {
    // `levels` is the recursion budget: a node expands its inputs while the
    // budget exceeds 1, so 1 = the cell alone, 2 = the cell plus one input level.
    let levels = match depth {
        ExplainDepth::Immediate => 2,
        ExplainDepth::Full => FULL_DEPTH_CAP,
        ExplainDepth::Levels(n) => n.saturating_add(1),
    };
    let engine = CalcEngine::new(registry);
    explain_node(&engine, registry, ordinal, coord, levels)
}

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

fn explain_node(
    engine: &CalcEngine,
    registry: &dyn EvalRegistry,
    ordinal: u32,
    coord: &[u32],
    remaining: u32,
) -> Result<CellTrace, CalcError> {
    let cube = registry
        .cube(ordinal)
        .ok_or(CalcError::UnknownCube(ordinal))?;
    let compiled = registry.compiled(ordinal);
    let value = engine.value(ordinal, coord)?;
    let names = coord_names(cube, coord);

    // A firing rule (a rule-derived leaf, or an explicit consolidation override).
    if let Some(rid) = compiled.and_then(|cm| cm.matching_rule(cube, coord)) {
        let rule = &compiled.expect("compiled present").rules[rid.0];
        let mut inputs = Vec::new();
        if remaining > 1 {
            let mut cells = Vec::new();
            collect_cells(&rule.expr, &mut cells);
            for cell in cells {
                let abs: Vec<u32> = cell
                    .addr
                    .iter()
                    .map(|slot| match slot {
                        AddrSlot::Pinned(idx) => *idx,
                        AddrSlot::FromTarget(pos) => coord[*pos],
                    })
                    .collect();
                inputs.push(explain_node(
                    engine,
                    registry,
                    cell.cube,
                    &abs,
                    remaining - 1,
                )?);
            }
        }
        return Ok(CellTrace {
            cube: cube.name().to_string(),
            coord: names,
            value,
            kind: TraceKind::Rule {
                rule: rid.0,
                span: (rule.span.start, rule.span.end),
            },
            inputs,
        });
    }

    let all_leaf = coord.iter().enumerate().all(|(d, &i)| {
        cube.dimension(d)
            .element(i)
            .map(|e| e.kind.is_leaf())
            .unwrap_or(false)
    });
    if all_leaf {
        return Ok(CellTrace {
            cube: cube.name().to_string(),
            coord: names,
            value,
            kind: TraceKind::Stored,
            inputs: vec![],
        });
    }

    // A consolidation: its non-zero contributing leaves, each sub-traced.
    let mut inputs = Vec::new();
    if remaining > 1 {
        for leaf in contributing_leaves(cube, coord)? {
            let v = engine.value(ordinal, &leaf)?;
            if v != Fixed::ZERO {
                inputs.push(explain_node(
                    engine,
                    registry,
                    ordinal,
                    &leaf,
                    remaining - 1,
                )?);
            }
        }
    }
    Ok(CellTrace {
        cube: cube.name().to_string(),
        coord: names,
        value,
        kind: TraceKind::Consolidation {
            contributions: inputs.len(),
        },
        inputs,
    })
}

/// The leaf coordinates that contribute to a consolidated coordinate, in sorted
/// order (the cartesian product of each dimension's weighted leaves).
fn contributing_leaves(cube: &Cube, coord: &[u32]) -> Result<Vec<Vec<u32>>, CalcError> {
    let mut per_dim: Vec<Vec<u32>> = Vec::with_capacity(cube.rank());
    for (d, &idx) in coord.iter().enumerate() {
        let mut leaves: Vec<u32> = cube
            .dimension(d)
            .leaf_weights(idx)
            .map_err(CalcError::Model)?
            .into_iter()
            .map(|(leaf, _)| leaf)
            .collect();
        leaves.sort_unstable();
        per_dim.push(leaves);
    }
    let total: usize = per_dim.iter().map(|v| v.len()).product();
    if total == 0 {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(total);
    for n in 0..total {
        let mut rem = n;
        let mut c = vec![0u32; cube.rank()];
        for d in 0..cube.rank() {
            let len = per_dim[d].len();
            c[d] = per_dim[d][rem % len];
            rem /= len;
        }
        out.push(c);
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::compile;
    use crate::compiled::CompiledModel;
    use crate::registry::SingleCube;
    use crate::rules::parse;
    use epiphany_core::{Cube, Dimension};

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

    fn margin_reg() -> OneCube {
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
        OneCube { cube, model }
    }

    #[test]
    fn explains_a_rule_derived_leaf() {
        let reg = margin_reg();
        let n = reg.cube.dimension(0).resolve("North").unwrap();
        let margin = reg.cube.dimension(1).resolve("Margin").unwrap();
        let trace = explain(&reg, 0, &[n, margin], ExplainDepth::Full).unwrap();
        assert_eq!(trace.value, Fixed::from(40));
        assert!(matches!(trace.kind, TraceKind::Rule { .. }));
        assert_eq!(trace.coord, vec!["North", "Margin"]);
        // Inputs are the stored Sales and Cost.
        assert_eq!(trace.inputs.len(), 2);
        assert!(trace
            .inputs
            .iter()
            .all(|i| matches!(i.kind, TraceKind::Stored)));
        let input_values: Vec<i64> = trace.inputs.iter().map(|i| i.value.to_scaled()).collect();
        assert!(input_values.contains(&Fixed::from(100).to_scaled()));
        assert!(input_values.contains(&Fixed::from(60).to_scaled()));
    }

    #[test]
    fn explains_a_consolidation_of_rule_leaves() {
        let reg = margin_reg();
        let total = reg.cube.dimension(0).resolve("Total").unwrap();
        let margin = reg.cube.dimension(1).resolve("Margin").unwrap();
        let trace = explain(&reg, 0, &[total, margin], ExplainDepth::Full).unwrap();
        assert_eq!(trace.value, Fixed::from(90));
        match trace.kind {
            TraceKind::Consolidation { contributions } => assert_eq!(contributions, 2),
            other => panic!("expected a consolidation, got {other:?}"),
        }
        // Each contributing leaf is itself a rule-derived Margin.
        assert_eq!(trace.inputs.len(), 2);
        assert!(trace
            .inputs
            .iter()
            .all(|i| matches!(i.kind, TraceKind::Rule { .. })));
        // The trace total agrees with the engine value.
        assert_eq!(
            trace.value,
            CalcEngine::new(&reg).value(0, &[total, margin]).unwrap()
        );
    }

    #[test]
    fn immediate_depth_omits_grandchildren() {
        let reg = margin_reg();
        let total = reg.cube.dimension(0).resolve("Total").unwrap();
        let margin = reg.cube.dimension(1).resolve("Margin").unwrap();
        let trace = explain(&reg, 0, &[total, margin], ExplainDepth::Immediate).unwrap();
        // One level of inputs (the Margin leaves), but their inputs are omitted.
        assert_eq!(trace.inputs.len(), 2);
        assert!(trace.inputs.iter().all(|i| i.inputs.is_empty()));
    }

    #[test]
    fn explain_is_deterministic() {
        let reg = margin_reg();
        let total = reg.cube.dimension(0).resolve("Total").unwrap();
        let margin = reg.cube.dimension(1).resolve("Margin").unwrap();
        let a = explain(&reg, 0, &[total, margin], ExplainDepth::Full).unwrap();
        let b = explain(&reg, 0, &[total, margin], ExplainDepth::Full).unwrap();
        assert_eq!(a, b);
    }
}
