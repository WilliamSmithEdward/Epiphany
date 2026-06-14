//! Compile a parsed rule set to its resolved, index-addressed form.
//!
//! [`compile`] runs once per published model version: it resolves every
//! dimension, member, attribute, and cube name to an index against the model,
//! lowers each formula to a [`CExpr`] tree, and rejects parseable-but-deferred
//! constructs (string formulas, text functions, by-attribute overrides,
//! cross-cube dimension mapping) with [`CompileError::Unsupported`]. Evaluation
//! (Phase 4D) then walks the resolved tree with no string work.

use std::str::FromStr;

use epiphany_core::{AttributeValue, Cube, Dimension, Fixed};

use crate::compiled::{
    AddrSlot, CCell, CCond, CExpr, CompileError, CompiledArea, CompiledModel, CompiledRule,
    DimPredicate,
};
use crate::registry::CubeRegistry;
use crate::rules::{
    Area, BuiltinFunc, CellRef, CmpOp, Condition, Expr, FuncArg, FuncCall, Literal, MemberExpr,
    RuleDoc, SelectorKind, Span,
};

/// Compile `rules` against `target` (and any cross-cube cubes in `registry`).
///
/// `version` tags the result with the engine commit it was compiled for, so a
/// cache can key by it. The `registry` must contain `target`.
pub fn compile(
    target: &Cube,
    registry: &dyn CubeRegistry,
    rules: &RuleDoc,
    version: u64,
) -> Result<CompiledModel, CompileError> {
    let target_ordinal = registry
        .ordinal(target.name())
        .expect("registry must contain the target cube");
    let mut compiled = Vec::with_capacity(rules.rules.len());
    for rule in &rules.rules {
        let area = compile_area(target, &rule.area)?;
        let expr = lower_expr(&rule.formula, target, registry, target_ordinal, rule.span)?;
        compiled.push(CompiledRule {
            area,
            expr,
            span: rule.span,
        });
    }
    Ok(CompiledModel {
        version,
        rules: compiled,
    })
}

fn dim_position(cube: &Cube, name: &str) -> Option<usize> {
    cube.dimensions().iter().position(|d| d.name() == name)
}

fn compile_area(target: &Cube, area: &Area) -> Result<CompiledArea, CompileError> {
    let mut per_dim = vec![DimPredicate::Any; target.rank()];
    for sel in &area.selectors {
        let dim_pos =
            dim_position(target, &sel.dimension).ok_or_else(|| CompileError::UnknownDimension {
                cube: target.name().to_string(),
                dimension: sel.dimension.clone(),
                span: sel.span,
            })?;
        let set = resolve_selector(target.dimension(dim_pos), &sel.kind, sel.span)?;
        per_dim[dim_pos] = DimPredicate::OneOf(set);
    }
    Ok(CompiledArea { per_dim })
}

/// Resolve an area selector to a sorted, de-duplicated set of element indices.
fn resolve_selector(
    dim: &Dimension,
    kind: &SelectorKind,
    span: Span,
) -> Result<Vec<u32>, CompileError> {
    let unknown_member = |m: &str| CompileError::UnknownMember {
        dimension: dim.name().to_string(),
        member: m.to_string(),
        span,
    };
    let mut set: Vec<u32> = match kind {
        SelectorKind::Element(m) => vec![dim.resolve(m).ok_or_else(|| unknown_member(m))?],
        SelectorKind::All => (0..dim.len()).collect(),
        SelectorKind::Leaves => (0..dim.len())
            .filter(|&i| dim.element(i).map(|e| e.kind.is_leaf()).unwrap_or(false))
            .collect(),
        SelectorKind::Consolidated => (0..dim.len())
            .filter(|&i| dim.element(i).map(|e| !e.kind.is_leaf()).unwrap_or(false))
            .collect(),
        SelectorKind::Children(parent) => {
            let p = dim.resolve(parent).ok_or_else(|| unknown_member(parent))?;
            dim.edges()
                .into_iter()
                .filter(|&(parent_idx, _, _)| parent_idx == p)
                .map(|(_, child, _)| child)
                .collect()
        }
        SelectorKind::Descendants(root) => {
            let r = dim.resolve(root).ok_or_else(|| unknown_member(root))?;
            descendants(dim, r)
        }
        SelectorKind::AttrPredicate {
            attribute,
            op,
            value,
        } => resolve_attr_predicate(dim, attribute, *op, value, span)?,
    };
    set.sort_unstable();
    set.dedup();
    Ok(set)
}

fn descendants(dim: &Dimension, root: u32) -> Vec<u32> {
    let edges = dim.edges();
    let mut out = Vec::new();
    let mut stack = vec![root];
    let mut seen = std::collections::HashSet::new();
    while let Some(node) = stack.pop() {
        if !seen.insert(node) {
            continue;
        }
        out.push(node);
        // Push children (membership set; order is normalized by the caller's sort).
        for &(parent, child, _) in &edges {
            if parent == node {
                stack.push(child);
            }
        }
    }
    out
}

fn resolve_attr_predicate(
    dim: &Dimension,
    attribute: &str,
    op: CmpOp,
    value: &Literal,
    span: Span,
) -> Result<Vec<u32>, CompileError> {
    if dim.attribute_index(attribute).is_none() {
        return Err(CompileError::UnknownAttribute {
            dimension: dim.name().to_string(),
            attribute: attribute.to_string(),
            span,
        });
    }
    // A numeric literal is parsed once; a string literal compares as text.
    let numeric_rhs = match value {
        Literal::Number(n) => {
            Some(Fixed::from_str(n).map_err(|_| CompileError::InvalidNumber {
                text: n.clone(),
                span,
            })?)
        }
        Literal::Str(_) => None,
    };
    let mut set = Vec::new();
    for idx in 0..dim.len() {
        let matches = match (dim.attribute(idx, attribute), value, numeric_rhs) {
            (Some(AttributeValue::Text(s)), Literal::Str(v), _) => {
                cmp_apply(s.as_str(), v.as_str(), op)
            }
            (Some(AttributeValue::Numeric(f)), Literal::Number(_), Some(rhs)) => {
                cmp_apply(*f, rhs, op)
            }
            // A missing attribute or a type mismatch never matches.
            _ => false,
        };
        if matches {
            set.push(idx);
        }
    }
    Ok(set)
}

fn cmp_apply<T: Ord>(a: T, b: T, op: CmpOp) -> bool {
    match op {
        CmpOp::Eq => a == b,
        CmpOp::Ne => a != b,
        CmpOp::Lt => a < b,
        CmpOp::Le => a <= b,
        CmpOp::Gt => a > b,
        CmpOp::Ge => a >= b,
    }
}

fn lower_expr(
    expr: &Expr,
    target: &Cube,
    registry: &dyn CubeRegistry,
    target_ordinal: u32,
    rule_span: Span,
) -> Result<CExpr, CompileError> {
    match expr {
        Expr::Number(n) => {
            Fixed::from_str(n)
                .map(CExpr::Num)
                .map_err(|_| CompileError::InvalidNumber {
                    text: n.clone(),
                    span: rule_span,
                })
        }
        Expr::Str(_) => Err(CompileError::Unsupported {
            feature: "a string-valued rule formula".to_string(),
            span: rule_span,
        }),
        Expr::Cell(cell) => Ok(CExpr::Cell(lower_cell(
            cell,
            target,
            registry,
            target_ordinal,
        )?)),
        Expr::Neg(e) => Ok(CExpr::Neg(Box::new(lower_expr(
            e,
            target,
            registry,
            target_ordinal,
            rule_span,
        )?))),
        Expr::Bin { op, left, right } => Ok(CExpr::Bin {
            op: *op,
            left: Box::new(lower_expr(
                left,
                target,
                registry,
                target_ordinal,
                rule_span,
            )?),
            right: Box::new(lower_expr(
                right,
                target,
                registry,
                target_ordinal,
                rule_span,
            )?),
        }),
        Expr::If {
            cond,
            then,
            otherwise,
        } => Ok(CExpr::If {
            cond: Box::new(lower_cond(
                cond,
                target,
                registry,
                target_ordinal,
                rule_span,
            )?),
            then: Box::new(lower_expr(
                then,
                target,
                registry,
                target_ordinal,
                rule_span,
            )?),
            otherwise: match otherwise {
                Some(o) => Some(Box::new(lower_expr(
                    o,
                    target,
                    registry,
                    target_ordinal,
                    rule_span,
                )?)),
                None => None,
            },
        }),
        Expr::Func(call) => lower_func(call, target),
    }
}

fn lower_func(call: &FuncCall, target: &Cube) -> Result<CExpr, CompileError> {
    match call.func {
        BuiltinFunc::Undef => Ok(CExpr::Undef),
        BuiltinFunc::AttrNum => {
            let (dim_name, attr_name) = two_string_args(call)?;
            let dim_pos =
                dim_position(target, &dim_name).ok_or_else(|| CompileError::UnknownDimension {
                    cube: target.name().to_string(),
                    dimension: dim_name.clone(),
                    span: call.span,
                })?;
            let attr = target
                .dimension(dim_pos)
                .attribute_index(&attr_name)
                .ok_or(CompileError::UnknownAttribute {
                    dimension: dim_name,
                    attribute: attr_name,
                    span: call.span,
                })?;
            Ok(CExpr::AttrNum { dim_pos, attr })
        }
        // Text/boolean functions are numeric-only-deferred for M4.
        BuiltinFunc::Attr | BuiltinFunc::IsLeaf | BuiltinFunc::ElementName => {
            Err(CompileError::Unsupported {
                feature: format!("the {}() function", call.func.name()),
                span: call.span,
            })
        }
    }
}

fn two_string_args(call: &FuncCall) -> Result<(String, String), CompileError> {
    if let [FuncArg::Str(a), FuncArg::Str(b)] = call.args.as_slice() {
        Ok((a.clone(), b.clone()))
    } else {
        Err(CompileError::Unsupported {
            feature: format!(
                "{}(dimension, attribute) with these arguments",
                call.func.name()
            ),
            span: call.span,
        })
    }
}

fn lower_cell(
    cell: &CellRef,
    target: &Cube,
    registry: &dyn CubeRegistry,
    target_ordinal: u32,
) -> Result<CCell, CompileError> {
    if !cell.mapping.is_empty() {
        return Err(CompileError::Unsupported {
            feature: "cross-cube dimension mapping".to_string(),
            span: cell.span,
        });
    }
    let (ordinal, ref_cube) = match &cell.cube {
        None => (target_ordinal, target),
        Some(name) => {
            let ord = registry
                .ordinal(name)
                .ok_or_else(|| CompileError::UnknownCube {
                    name: name.clone(),
                    span: cell.span,
                })?;
            let cube = registry
                .cube(ord)
                .ok_or_else(|| CompileError::UnknownCube {
                    name: name.clone(),
                    span: cell.span,
                })?;
            (ord, cube)
        }
    };

    // Every override must name a dimension of the referenced cube.
    for ov in &cell.overrides {
        if dim_position(ref_cube, &ov.dimension).is_none() {
            return Err(CompileError::UnknownDimension {
                cube: ref_cube.name().to_string(),
                dimension: ov.dimension.clone(),
                span: cell.span,
            });
        }
    }

    let mut addr = Vec::with_capacity(ref_cube.rank());
    for rd in 0..ref_cube.rank() {
        let ref_dim = ref_cube.dimension(rd);
        match cell
            .overrides
            .iter()
            .find(|o| o.dimension == ref_dim.name())
        {
            Some(ov) => match &ov.member {
                MemberExpr::Element(m) => {
                    let idx = ref_dim
                        .resolve(m)
                        .ok_or_else(|| CompileError::UnknownMember {
                            dimension: ref_dim.name().to_string(),
                            member: m.clone(),
                            span: cell.span,
                        })?;
                    addr.push(AddrSlot::Pinned(idx));
                }
                MemberExpr::Attr(_) => {
                    return Err(CompileError::Unsupported {
                        feature: "a by-attribute member override".to_string(),
                        span: cell.span,
                    })
                }
            },
            None => {
                // Default an un-overridden dimension to the target's member of
                // the same name (the relative-reference convention). A referenced
                // dimension with no same-named target dimension must be addressed
                // explicitly (cross-cube mapping is deferred).
                match dim_position(target, ref_dim.name()) {
                    Some(tpos) => addr.push(AddrSlot::FromTarget(tpos)),
                    None => {
                        return Err(CompileError::UnknownDimension {
                            cube: ref_cube.name().to_string(),
                            dimension: ref_dim.name().to_string(),
                            span: cell.span,
                        })
                    }
                }
            }
        }
    }
    Ok(CCell {
        cube: ordinal,
        addr,
    })
}

fn lower_cond(
    cond: &Condition,
    target: &Cube,
    registry: &dyn CubeRegistry,
    target_ordinal: u32,
    rule_span: Span,
) -> Result<CCond, CompileError> {
    match cond {
        Condition::And(a, b) => Ok(CCond::And(
            Box::new(lower_cond(a, target, registry, target_ordinal, rule_span)?),
            Box::new(lower_cond(b, target, registry, target_ordinal, rule_span)?),
        )),
        Condition::Or(a, b) => Ok(CCond::Or(
            Box::new(lower_cond(a, target, registry, target_ordinal, rule_span)?),
            Box::new(lower_cond(b, target, registry, target_ordinal, rule_span)?),
        )),
        Condition::Not(c) => Ok(CCond::Not(Box::new(lower_cond(
            c,
            target,
            registry,
            target_ordinal,
            rule_span,
        )?))),
        Condition::Compare { left, op, right } => Ok(CCond::Compare {
            left: lower_expr(left, target, registry, target_ordinal, rule_span)?,
            op: *op,
            right: lower_expr(right, target, registry, target_ordinal, rule_span)?,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiled::CExpr;
    use crate::registry::{SingleCube, VecRegistry};
    use crate::rules::parse;
    use epiphany_core::{Cube, Dimension};

    /// Sales: Region(North,South,Total) x Measure(Sales,Cost,Margin).
    fn sales() -> Cube {
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

    fn compile_one(cube: &Cube, src: &str) -> Result<CompiledModel, CompileError> {
        let reg = SingleCube::new(cube);
        compile(cube, &reg, &parse(src).unwrap(), 1)
    }

    #[test]
    fn area_and_formula_resolve() {
        let cube = sales();
        let m = compile_one(
            &cube,
            "['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];",
        )
        .unwrap();
        assert_eq!(m.rules.len(), 1);
        let area = &m.rules[0].area;
        // Region unconstrained -> Any; Measure -> OneOf([Margin index = 2]).
        assert!(matches!(area.per_dim[0], DimPredicate::Any));
        assert!(matches!(&area.per_dim[1], DimPredicate::OneOf(s) if s == &vec![2]));
        // Formula: Bin(Sub, Cell, Cell).
        assert!(matches!(
            m.rules[0].expr,
            CExpr::Bin {
                op: crate::rules::ArithOp::Sub,
                ..
            }
        ));
    }

    #[test]
    fn any_matches_leaves_only_not_consolidations() {
        let cube = sales();
        let m = compile_one(&cube, "['Measure':'Margin'] = 1;").unwrap();
        let area = &m.rules[0].area;
        let north = cube.dimension(0).resolve("North").unwrap();
        let total = cube.dimension(0).resolve("Total").unwrap();
        let margin = cube.dimension(1).resolve("Margin").unwrap();
        // Region=Any matches a leaf (North) but not a consolidation (Total).
        assert!(area.matches(&cube, &[north, margin]));
        assert!(!area.matches(&cube, &[total, margin]));
    }

    #[test]
    fn explicit_consolidation_override_matches() {
        let cube = sales();
        let m = compile_one(&cube, "['Region':'Total', 'Measure':'Margin'] = 99;").unwrap();
        let total = cube.dimension(0).resolve("Total").unwrap();
        let margin = cube.dimension(1).resolve("Margin").unwrap();
        assert!(m.rules[0].area.matches(&cube, &[total, margin]));
    }

    #[test]
    fn area_selector_families() {
        let cube = sales();
        let m = compile_one(
            &cube,
            "['Region':{leaves}, 'Measure':{children of 'X'}] = 1;",
        );
        // children of a missing element -> UnknownMember.
        assert!(matches!(m, Err(CompileError::UnknownMember { .. })));
        let ok = compile_one(&cube, "['Region':{leaves}] = 1;").unwrap();
        // {leaves} -> North, South (indices 0,1), not Total.
        assert!(matches!(&ok.rules[0].area.per_dim[0], DimPredicate::OneOf(s) if s == &vec![0, 1]));
    }

    #[test]
    fn cross_cube_reference_compiles_to_ordinal_and_slots() {
        let sales = sales();
        let mut fx = Dimension::new("Pair");
        fx.add_leaf("USD");
        let fx_cube = Cube::new("FX", vec![fx]).unwrap();
        let reg = VecRegistry::new(vec![sales.clone(), fx_cube]);
        let doc = parse("['Measure':'Sales'] = 'FX'!['Pair':'USD'];").unwrap();
        let m = compile(&sales, &reg, &doc, 1).unwrap();
        match &m.rules[0].expr {
            CExpr::Cell(c) => {
                assert_eq!(c.cube, 1, "FX ordinal");
                assert!(matches!(c.addr.as_slice(), [AddrSlot::Pinned(_)]));
            }
            other => panic!("expected a cross-cube cell, got {other:?}"),
        }
    }

    #[test]
    fn same_cube_ref_defaults_unoverridden_dims_to_target() {
        let cube = sales();
        // value['Measure':'Sales'] -> Region copied from target, Measure pinned.
        let m = compile_one(&cube, "['Measure':'Margin'] = value['Measure':'Sales'];").unwrap();
        match &m.rules[0].expr {
            CExpr::Cell(c) => {
                assert!(matches!(c.addr[0], AddrSlot::FromTarget(0)));
                assert!(matches!(c.addr[1], AddrSlot::Pinned(_)));
            }
            other => panic!("expected a cell, got {other:?}"),
        }
    }

    #[test]
    fn compile_error_table() {
        let cube = sales();
        // Unknown dimension in the area.
        assert!(matches!(
            compile_one(&cube, "['Nope':'x'] = 1;"),
            Err(CompileError::UnknownDimension { .. })
        ));
        // Unknown member.
        assert!(matches!(
            compile_one(&cube, "['Measure':'Ghost'] = 1;"),
            Err(CompileError::UnknownMember { .. })
        ));
        // Invalid number (too many decimals).
        assert!(matches!(
            compile_one(&cube, "['Measure':'Sales'] = 1.23456;"),
            Err(CompileError::InvalidNumber { .. })
        ));
        // Deferred constructs -> Unsupported.
        assert!(matches!(
            compile_one(&cube, "['Measure':'Sales'] = 'hello';"),
            Err(CompileError::Unsupported { .. })
        ));
        assert!(matches!(
            compile_one(&cube, "['Measure':'Sales'] = value['Region': !'code'];"),
            Err(CompileError::Unsupported { .. })
        ));
        assert!(matches!(
            compile_one(&cube, "['Measure':'Sales'] = Attr('Region','Code');"),
            Err(CompileError::Unsupported { .. })
        ));
    }

    #[test]
    fn compile_is_deterministic() {
        let cube = sales();
        let src = "['Measure':'Margin'] = IF value['Measure':'Sales'] > 0 THEN value['Measure':'Sales'] - value['Measure':'Cost'] ELSE 0;";
        let a = format!("{:?}", compile_one(&cube, src).unwrap());
        let b = format!("{:?}", compile_one(&cube, src).unwrap());
        assert_eq!(a, b);
    }
}
