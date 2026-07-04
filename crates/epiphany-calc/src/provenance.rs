//! Calculation provenance ("explain").
//!
//! Given a cell, [`explain`] returns a [`CellTrace`]: the value, what produced it
//! (a stored leaf, a firing rule with its source span, or a consolidation), and
//! the input cells consulted (recursively, depth-bounded). It is a dedicated,
//! opt-in walk separate from the hot evaluation path, so normal reads carry no
//! tracing cost. The trace value always agrees with the evaluator, and inputs are
//! ordered deterministically.

use epiphany_core::{CellTrace, Cube, ElementMask, ExplainDepth, Fixed, TraceKind};

use crate::compiled::{AddrSlot, CCell, CCond, CExpr};
use crate::eval::{CalcEngine, CalcError, EvalRegistry, SandboxOverlay};
use crate::feeders::CoordWalk;

/// A safety cap on `ExplainDepth::Full` recursion (the per-query cycle guard in
/// the evaluator already prevents infinite loops; this bounds trace size).
const FULL_DEPTH_CAP: u32 = 32;

/// The largest number of contributing input nodes a single consolidation node
/// reports in a trace. A wide consolidation (a top-level total over thousands of
/// leaves) would otherwise materialize an input list as large as the dense leaf
/// space, dwarfing the trace itself; past this many non-zero contributions the
/// walk stops and the node's `inputs_truncated` flag is set. A fixed safety
/// backstop like the parser's `MAX_PARSE_DEPTH`, not an operational knob; the
/// node's reported value is always the exact, untruncated total (computed by the
/// evaluator, independent of this cap).
const MAX_EXPLAIN_INPUTS: usize = 64;

/// Explain the value at `coord` in cube `ordinal`, to the given depth.
pub fn explain(
    registry: &dyn EvalRegistry,
    ordinal: u32,
    coord: &[u32],
    depth: ExplainDepth,
) -> Result<CellTrace, CalcError> {
    explain_with(registry, ordinal, coord, depth, None, None)
}

/// Explain a value, optionally overlaying a sandbox's what-if leaves (ADR-0014)
/// so the provenance matches a sandboxed read rather than base, and optionally
/// applying the caller's element deny mask (ADR-0015) so explaining a denied cell
/// fails with `AccessDenied` rather than revealing its provenance.
pub fn explain_with(
    registry: &dyn EvalRegistry,
    ordinal: u32,
    coord: &[u32],
    depth: ExplainDepth,
    overlay: Option<&dyn SandboxOverlay>,
    mask: Option<&ElementMask>,
) -> Result<CellTrace, CalcError> {
    // `levels` is the recursion budget: a node expands its inputs while the
    // budget exceeds 1, so 1 = the cell alone, 2 = the cell plus one input level.
    let levels = match depth {
        ExplainDepth::Immediate => 2,
        ExplainDepth::Full => FULL_DEPTH_CAP,
        ExplainDepth::Levels(n) => n.saturating_add(1),
    };
    let engine = match overlay {
        Some(ov) => CalcEngine::with_overlay(registry, ov),
        None => CalcEngine::new(registry),
    }
    .with_mask(mask, ordinal);
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
            // The cells the evaluation actually consulted: condition cells (which
            // chose the branch) plus the cells of the *taken* branch only. This
            // matches what the evaluator read, so the trace neither hides the
            // deciding condition cells nor force-evaluates an untaken branch whose
            // own rule might error (which would fail the whole explain).
            let mut cells: Vec<&CCell> = Vec::new();
            collect_consulted_cells(engine, ordinal, coord, &rule.expr, &mut cells)?;
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
            // A rule lists exactly the cells it consulted (a bounded, structural
            // set), never the consolidation breadth cap, so it is never truncated.
            inputs_truncated: false,
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
            inputs_truncated: false,
        });
    }

    // A consolidation: its non-zero contributing leaves, each sub-traced. The
    // leaf space is walked streamingly ([`CoordWalk`]) in sorted order: only the
    // non-zero contributions are ever held, never the enumerated coordinates
    // (a top-level total's dense leaf product would dwarf the trace itself). The
    // reported input list is capped at [`MAX_EXPLAIN_INPUTS`] (CA2): a very wide
    // consolidation stops listing inputs past the cap and sets `inputs_truncated`,
    // so the trace stays bounded. The node's `value` above is the exact,
    // untruncated total (from the evaluator), unaffected by the cap.
    let mut inputs = Vec::new();
    let mut inputs_truncated = false;
    if remaining > 1 {
        let per_dim = contributing_leaf_dims(cube, coord)?;
        let mut walk = CoordWalk::new(&per_dim);
        let mut leaf = Vec::new();
        while walk.next_coord(&mut leaf) {
            let v = engine.value(ordinal, &leaf)?;
            if v != Fixed::ZERO {
                if inputs.len() == MAX_EXPLAIN_INPUTS {
                    // The cap is reached: stop expanding further contributions.
                    // Deterministic, because the walk is in sorted coordinate
                    // order, so the retained prefix is always the same one.
                    inputs_truncated = true;
                    break;
                }
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
        inputs_truncated,
    })
}

/// Collect the cells a rule expression *actually consulted* at `target`, for a
/// faithful provenance trace: every cell of a non-conditional term, and for an
/// `IF`, the condition's cells plus only the taken branch's cells (evaluating the
/// condition through the engine, so the branch choice matches evaluation exactly).
/// Deterministic: cells are appended in source order.
fn collect_consulted_cells<'a>(
    engine: &CalcEngine,
    ordinal: u32,
    target: &[u32],
    expr: &'a CExpr,
    out: &mut Vec<&'a CCell>,
) -> Result<(), CalcError> {
    match expr {
        CExpr::Cell(c) => out.push(c),
        CExpr::Neg(e) => collect_consulted_cells(engine, ordinal, target, e, out)?,
        CExpr::Bin { left, right, .. } => {
            collect_consulted_cells(engine, ordinal, target, left, out)?;
            collect_consulted_cells(engine, ordinal, target, right, out)?;
        }
        CExpr::If {
            cond,
            then,
            otherwise,
        } => {
            // The condition cells decided the branch, so they are consulted inputs.
            collect_condition_cells(cond, out);
            // Follow only the branch the evaluator took.
            if engine.eval_condition(cond, ordinal, target)? {
                collect_consulted_cells(engine, ordinal, target, then, out)?;
            } else if let Some(o) = otherwise {
                collect_consulted_cells(engine, ordinal, target, o, out)?;
            }
        }
        CExpr::Num(_) | CExpr::AttrNum { .. } | CExpr::Undef => {}
    }
    Ok(())
}

/// Append the cells referenced anywhere in a condition (both comparison operands
/// and all sub-conditions), in source order. Does not evaluate: a condition's
/// cells are all "consulted" for provenance regardless of short-circuiting.
fn collect_condition_cells<'a>(cond: &'a CCond, out: &mut Vec<&'a CCell>) {
    match cond {
        CCond::And(a, b) | CCond::Or(a, b) => {
            collect_condition_cells(a, out);
            collect_condition_cells(b, out);
        }
        CCond::Not(c) => collect_condition_cells(c, out),
        CCond::Compare { left, right, .. } => {
            collect_expr_cells(left, out);
            collect_expr_cells(right, out);
        }
    }
}

/// Append every cell referenced in an expression (all branches), in source order.
/// Used for condition operands, where every referenced cell is a consulted input.
fn collect_expr_cells<'a>(expr: &'a CExpr, out: &mut Vec<&'a CCell>) {
    match expr {
        CExpr::Cell(c) => out.push(c),
        CExpr::Neg(e) => collect_expr_cells(e, out),
        CExpr::Bin { left, right, .. } => {
            collect_expr_cells(left, out);
            collect_expr_cells(right, out);
        }
        CExpr::If {
            cond,
            then,
            otherwise,
        } => {
            collect_condition_cells(cond, out);
            collect_expr_cells(then, out);
            if let Some(o) = otherwise {
                collect_expr_cells(o, out);
            }
        }
        CExpr::Num(_) | CExpr::AttrNum { .. } | CExpr::Undef => {}
    }
}

/// The per-dimension sorted leaves contributing to a consolidated coordinate.
/// The contributing leaf coordinates are their cartesian product; walking it
/// with [`CoordWalk`] yields them in sorted (lexicographic) order without ever
/// materializing the product, whose size for a top-level total is the full
/// dense leaf space (multi-GB of coordinates on a production cube, the pre-fix
/// explain OOM) while these lists are only its per-dimension factors.
fn contributing_leaf_dims(cube: &Cube, coord: &[u32]) -> Result<Vec<Vec<u32>>, CalcError> {
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
    Ok(per_dim)
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
        // Each contributing leaf is itself a rule-derived Margin, and the
        // contributions stream out in sorted coordinate order.
        assert_eq!(trace.inputs.len(), 2);
        assert!(trace
            .inputs
            .iter()
            .all(|i| matches!(i.kind, TraceKind::Rule { .. })));
        assert_eq!(trace.inputs[0].coord, vec!["North", "Margin"]);
        assert_eq!(trace.inputs[1].coord, vec!["South", "Margin"]);
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
    fn explain_includes_condition_cells_and_follows_only_the_taken_branch() {
        // Condition reads Cost; taken branch (Sales > 0 path via Cost < 100) reads
        // Sales; the untaken ELSE divides by Cost, which is non-zero here but the
        // untaken branch also references a cell the trace must NOT evaluate. The
        // trace should list the condition's cell (Cost) and the taken branch's cell
        // (Sales), and must not fail on the untaken branch.
        let mut cube = sales_cube();
        let n = cube.dimension(0).resolve("North").unwrap();
        let (sales, cost) = (
            cube.dimension(1).resolve("Sales").unwrap(),
            cube.dimension(1).resolve("Cost").unwrap(),
        );
        cube.set_leaf(&[n, sales], Fixed::from(100)).unwrap();
        cube.set_leaf(&[n, cost], Fixed::from(60)).unwrap();
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse(
                "['Measure':'Margin'] = IF value['Measure':'Cost'] < 100 \
                 THEN value['Measure':'Sales'] ELSE value['Measure':'Sales'];",
            )
            .unwrap(),
            1,
        )
        .unwrap();
        let reg = OneCube { cube, model };
        let margin = reg.cube.dimension(1).resolve("Margin").unwrap();
        let trace = explain(&reg, 0, &[n, margin], ExplainDepth::Full).unwrap();
        assert_eq!(trace.value, Fixed::from(100));
        // Inputs: Cost (from the condition) and Sales (the taken branch). The ELSE
        // branch's Sales is not double-listed because it was not taken.
        let coords: Vec<&str> = trace.inputs.iter().map(|i| i.coord[1].as_str()).collect();
        assert!(coords.contains(&"Cost"), "condition cell Cost is traced");
        assert!(
            coords.contains(&"Sales"),
            "taken-branch cell Sales is traced"
        );
        assert_eq!(trace.inputs.len(), 2, "condition + taken branch only");
    }

    #[test]
    fn explain_tolerates_an_erroring_untaken_branch() {
        // The taken branch is fine; the untaken branch would divide by a zero cell.
        // Pre-fix, force-evaluating both branches turned explain into a DivByZero
        // error; now only the taken branch is followed, so explain succeeds.
        let mut cube = sales_cube();
        let n = cube.dimension(0).resolve("North").unwrap();
        let (sales, cost) = (
            cube.dimension(1).resolve("Sales").unwrap(),
            cube.dimension(1).resolve("Cost").unwrap(),
        );
        cube.set_leaf(&[n, sales], Fixed::from(100)).unwrap();
        // Cost left at zero -> the ELSE branch's Sales/Cost would be DivByZero.
        let _ = cost;
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse(
                "['Measure':'Margin'] = IF value['Measure':'Sales'] > 0 \
                 THEN value['Measure':'Sales'] \
                 ELSE value['Measure':'Sales'] / value['Measure':'Cost'];",
            )
            .unwrap(),
            1,
        )
        .unwrap();
        let reg = OneCube { cube, model };
        let margin = reg.cube.dimension(1).resolve("Margin").unwrap();
        let trace = explain(&reg, 0, &[n, margin], ExplainDepth::Full).unwrap();
        assert_eq!(trace.value, Fixed::from(100));
        // Only the condition cell + taken-branch cell (both Sales) are traced.
        assert!(trace.inputs.iter().all(|i| i.coord[1] == "Sales"));
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

    #[test]
    fn wide_consolidation_caps_and_flags_its_inputs() {
        // A Total over more leaves than the explain breadth cap (CA2). Each leaf
        // holds 1, so the exact total is the leaf count and every leaf is a
        // non-zero contribution. The trace must report the EXACT total but list at
        // most MAX_EXPLAIN_INPUTS inputs, with `inputs_truncated` set.
        let n = super::MAX_EXPLAIN_INPUTS + 10;
        let mut region = Dimension::new("Region");
        let leaves: Vec<u32> = (0..n as u32)
            .map(|i| region.add_leaf(format!("R{i}")))
            .collect();
        let total = region.add_consolidated("Total");
        for &leaf in &leaves {
            region.add_child(total, leaf, 1).unwrap();
        }
        let mut cube = Cube::new("Sales", vec![region]).unwrap();
        for &leaf in &leaves {
            cube.set_leaf(&[leaf], Fixed::from(1)).unwrap();
        }
        let model = compile(&cube, &SingleCube::new(&cube), &parse("").unwrap(), 1).unwrap();
        let reg = OneCube { cube, model };

        let trace = explain(&reg, 0, &[total], ExplainDepth::Full).unwrap();
        // The value is the exact untruncated total (all n leaves), NOT the cap.
        assert_eq!(trace.value, Fixed::from(n as i32));
        // The listed inputs are capped and the truncation flag is set.
        assert_eq!(trace.inputs.len(), super::MAX_EXPLAIN_INPUTS);
        assert!(
            trace.inputs_truncated,
            "a wide consolidation flags truncation"
        );
        match trace.kind {
            TraceKind::Consolidation { contributions } => {
                assert_eq!(
                    contributions,
                    super::MAX_EXPLAIN_INPUTS,
                    "count matches listed inputs"
                );
            }
            other => panic!("expected a consolidation, got {other:?}"),
        }
        // Deterministic: the same capped prefix every time (sorted-order walk).
        let again = explain(&reg, 0, &[total], ExplainDepth::Full).unwrap();
        assert_eq!(trace, again);
    }

    #[test]
    fn narrow_consolidation_is_not_flagged_truncated() {
        // A small consolidation lists all its inputs and leaves the flag clear.
        let reg = margin_reg();
        let total = reg.cube.dimension(0).resolve("Total").unwrap();
        let margin = reg.cube.dimension(1).resolve("Margin").unwrap();
        let trace = explain(&reg, 0, &[total, margin], ExplainDepth::Full).unwrap();
        assert_eq!(trace.inputs.len(), 2);
        assert!(
            !trace.inputs_truncated,
            "a fully-listed node is not truncated"
        );
    }
}
