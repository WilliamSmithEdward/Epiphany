//! Tree-walking evaluator for the MDX set sublanguage.
//!
//! [`evaluate`] resolves a [`SetExpr`] against a single borrowed [`Dimension`]
//! into an ordered, de-duplicated list of element indices. It is pure and reads
//! the dimension immutably, so it is safe to run over an MVCC read snapshot.
//!
//! Determinism: every ordering comes from a deterministic core primitive -
//! `iter_elements` is definition order, `edges()` is sorted by `(parent,
//! child)`, and `Order` uses a key sort with the input position as a stable
//! tie-break. The `Descendants` de-duplication uses a `HashSet` purely for the
//! visited skip-check; emission order is the sorted-edge pre-order DFS, never
//! the set's iteration order.
//!
//! Crossjoin (`a * b`) is parsed but rejected here: a tuple set is not a valid
//! single-dimension member set. Tuple nesting is handled by the view layer
//! (Phase 3D), which crossjoins per-dimension subsets itself.

use std::collections::{BTreeMap, HashSet};
use std::fmt;

use epiphany_core::{AttributeValue, Dimension, Fixed, ModelError};

use crate::ast::{CmpOp, MemberRef, Operand, OrderDir, Predicate, SetExpr};

/// A failure while evaluating a set expression against a dimension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MdxEvalError {
    /// A member name did not resolve in the dimension.
    UnknownMember {
        /// The dimension being evaluated.
        dimension: String,
        /// The unresolved member name.
        member: String,
    },
    /// A `Filter` / `Order` referenced an attribute the dimension does not define.
    UnknownAttribute {
        /// The dimension being evaluated.
        dimension: String,
        /// The unknown attribute name.
        attribute: String,
    },
    /// A reference named a different dimension than the one being evaluated.
    DimensionMismatch {
        /// The dimension being evaluated.
        expected: String,
        /// The qualifier the expression actually named.
        found: String,
    },
    /// A comparison mixed incompatible operand types (e.g. text with numeric).
    TypeMismatch {
        /// A human-readable explanation.
        detail: String,
    },
    /// A crossjoin / tuple set appeared where a single-dimension set is required.
    TupleSetNotAllowed,
    /// An underlying core model error (e.g. an invalid numeric literal).
    Core(ModelError),
}

impl fmt::Display for MdxEvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MdxEvalError::UnknownMember { dimension, member } => {
                write!(f, "member '{member}' not found in dimension '{dimension}'")
            }
            MdxEvalError::UnknownAttribute {
                dimension,
                attribute,
            } => write!(
                f,
                "attribute '{attribute}' is not defined on dimension '{dimension}'"
            ),
            MdxEvalError::DimensionMismatch { expected, found } => write!(
                f,
                "expression refers to '{found}' but the set is over dimension '{expected}'"
            ),
            MdxEvalError::TypeMismatch { detail } => write!(f, "type mismatch: {detail}"),
            MdxEvalError::TupleSetNotAllowed => write!(
                f,
                "a crossjoin (tuple) set cannot be used where a single-dimension member set is required"
            ),
            MdxEvalError::Core(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for MdxEvalError {}

impl From<ModelError> for MdxEvalError {
    fn from(e: ModelError) -> Self {
        MdxEvalError::Core(e)
    }
}

/// Evaluate a set expression over a single dimension into an ordered,
/// de-duplicated list of element indices.
pub fn evaluate(expr: &SetExpr, dim: &Dimension) -> Result<Vec<u32>, MdxEvalError> {
    Ok(dedup(eval_set(expr, dim)?))
}

fn eval_set(expr: &SetExpr, dim: &Dimension) -> Result<Vec<u32>, MdxEvalError> {
    match expr {
        SetExpr::Set(items) => {
            let mut out = Vec::new();
            for item in items {
                out.extend(eval_set(item, dim)?);
            }
            Ok(dedup(out))
        }
        SetExpr::Member(r) => Ok(vec![resolve_member(dim, r)?]),
        SetExpr::Members(r) => {
            dimension_ref(dim, r)?;
            Ok((0..dim.len()).collect())
        }
        SetExpr::Children(r) => {
            let parent = resolve_member(dim, r)?;
            Ok(children_of(dim, parent))
        }
        SetExpr::Descendants(r) => {
            let root = resolve_member(dim, r)?;
            Ok(descendants_of(dim, root))
        }
        SetExpr::Filter(set, pred) => {
            let base = eval_set(set, dim)?;
            let mut out = Vec::new();
            for element in base {
                if eval_predicate(dim, element, pred)? {
                    out.push(element);
                }
            }
            Ok(out)
        }
        SetExpr::Order(set, attr, dir) => {
            let base = eval_set(set, dim)?;
            order_set(dim, base, attr, *dir)
        }
        SetExpr::Crossjoin(_, _) => Err(MdxEvalError::TupleSetNotAllowed),
    }
}

/// Resolve a member reference to an element index, validating the dimension
/// qualifier (the first segment of a `[Dim].[Member]` path) if present.
fn resolve_member(dim: &Dimension, r: &MemberRef) -> Result<u32, MdxEvalError> {
    if r.path.len() >= 2 && r.path[0] != dim.name() {
        return Err(MdxEvalError::DimensionMismatch {
            expected: dim.name().to_string(),
            found: r.path[0].clone(),
        });
    }
    let name = r.name();
    dim.resolve(name)
        .ok_or_else(|| MdxEvalError::UnknownMember {
            dimension: dim.name().to_string(),
            member: name.to_string(),
        })
}

/// Validate that a `.Members` reference names the dimension being evaluated.
fn dimension_ref(dim: &Dimension, r: &MemberRef) -> Result<(), MdxEvalError> {
    let named = r.name();
    if named != dim.name() {
        return Err(MdxEvalError::DimensionMismatch {
            expected: dim.name().to_string(),
            found: named.to_string(),
        });
    }
    Ok(())
}

/// The immediate children of `parent`, in edge-sorted (child-index) order.
fn children_of(dim: &Dimension, parent: u32) -> Vec<u32> {
    let kids: Vec<u32> = dim
        .edges()
        .into_iter()
        .filter(|&(p, _, _)| p == parent)
        .map(|(_, child, _)| child)
        .collect();
    dedup(kids)
}

/// The member and all of its descendants, as a sorted-edge pre-order DFS,
/// de-duplicated by first visit (safe under alternate rollups).
fn descendants_of(dim: &Dimension, root: u32) -> Vec<u32> {
    let mut adjacency: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for (parent, child, _) in dim.edges() {
        adjacency.entry(parent).or_default().push(child);
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    collect_descendants(&adjacency, root, &mut seen, &mut out);
    out
}

fn collect_descendants(
    adjacency: &BTreeMap<u32, Vec<u32>>,
    node: u32,
    seen: &mut HashSet<u32>,
    out: &mut Vec<u32>,
) {
    if !seen.insert(node) {
        return;
    }
    out.push(node);
    if let Some(children) = adjacency.get(&node) {
        for &child in children {
            collect_descendants(adjacency, child, seen, out);
        }
    }
}

/// A total-ordered sort key. Missing values sort before any present value; a
/// numeric value sorts before any text value (both cases are deterministic).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum SortKey {
    Missing,
    Num(Fixed),
    Text(String),
}

fn sort_key(dim: &Dimension, element: u32, attr: &str) -> SortKey {
    match dim.attribute(element, attr) {
        Some(AttributeValue::Text(s)) => SortKey::Text(s.clone()),
        Some(AttributeValue::Numeric(f)) => SortKey::Num(*f),
        None => SortKey::Missing,
    }
}

/// Stable sort by an attribute key. The `B`-prefixed (hierarchy-breaking) and
/// plain directions are treated alike here: our subsets are flat member lists,
/// so both produce a flat key sort with the input order as the tie-break.
fn order_set(
    dim: &Dimension,
    items: Vec<u32>,
    attr: &str,
    dir: OrderDir,
) -> Result<Vec<u32>, MdxEvalError> {
    if dim.attribute_index(attr).is_none() {
        return Err(MdxEvalError::UnknownAttribute {
            dimension: dim.name().to_string(),
            attribute: attr.to_string(),
        });
    }
    let ascending = matches!(dir, OrderDir::Asc | OrderDir::BAsc);
    let mut keyed: Vec<(SortKey, usize, u32)> = items
        .iter()
        .enumerate()
        .map(|(i, &element)| (sort_key(dim, element, attr), i, element))
        .collect();
    keyed.sort_by(|a, b| {
        let ord = if ascending {
            a.0.cmp(&b.0)
        } else {
            b.0.cmp(&a.0)
        };
        ord.then(a.1.cmp(&b.1))
    });
    Ok(keyed.into_iter().map(|(_, _, element)| element).collect())
}

/// A resolved operand value for predicate comparison.
enum Val {
    Missing,
    Text(String),
    Num(Fixed),
}

fn eval_predicate(dim: &Dimension, element: u32, pred: &Predicate) -> Result<bool, MdxEvalError> {
    match pred {
        // Both sides are evaluated eagerly so that an ill-typed branch reports a
        // deterministic error regardless of the other branch's truth value.
        Predicate::And(l, r) => {
            let a = eval_predicate(dim, element, l)?;
            let b = eval_predicate(dim, element, r)?;
            Ok(a && b)
        }
        Predicate::Or(l, r) => {
            let a = eval_predicate(dim, element, l)?;
            let b = eval_predicate(dim, element, r)?;
            Ok(a || b)
        }
        Predicate::Not(p) => Ok(!eval_predicate(dim, element, p)?),
        Predicate::Compare { left, op, right } => {
            let l = eval_operand(dim, element, left)?;
            let r = eval_operand(dim, element, right)?;
            compare_vals(&l, &r, *op)
        }
    }
}

fn eval_operand(dim: &Dimension, element: u32, operand: &Operand) -> Result<Val, MdxEvalError> {
    match operand {
        Operand::Property(attr) => {
            if dim.attribute_index(attr).is_none() {
                return Err(MdxEvalError::UnknownAttribute {
                    dimension: dim.name().to_string(),
                    attribute: attr.clone(),
                });
            }
            Ok(match dim.attribute(element, attr) {
                Some(AttributeValue::Text(s)) => Val::Text(s.clone()),
                Some(AttributeValue::Numeric(f)) => Val::Num(*f),
                None => Val::Missing,
            })
        }
        Operand::Str(s) => Ok(Val::Text(s.clone())),
        Operand::Number(n) => Ok(Val::Num(n.parse::<Fixed>()?)),
    }
}

fn compare_vals(l: &Val, r: &Val, op: CmpOp) -> Result<bool, MdxEvalError> {
    match (l, r) {
        // A missing attribute makes the comparison false (the member is filtered
        // out), as with SQL NULL semantics.
        (Val::Missing, _) | (_, Val::Missing) => Ok(false),
        (Val::Text(a), Val::Text(b)) => Ok(apply_op(a.as_str(), b.as_str(), op)),
        (Val::Num(a), Val::Num(b)) => Ok(apply_op(a, b, op)),
        _ => Err(MdxEvalError::TypeMismatch {
            detail: "cannot compare a text value with a numeric value".to_string(),
        }),
    }
}

fn apply_op<T: Ord>(a: T, b: T, op: CmpOp) -> bool {
    match op {
        CmpOp::Eq => a == b,
        CmpOp::Ne => a != b,
        CmpOp::Lt => a < b,
        CmpOp::Le => a <= b,
        CmpOp::Gt => a > b,
        CmpOp::Ge => a >= b,
    }
}

/// Drop later duplicates, preserving first-occurrence order.
fn dedup(values: Vec<u32>) -> Vec<u32> {
    let mut seen = HashSet::new();
    values.into_iter().filter(|v| seen.insert(*v)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;
    use epiphany_core::AttributeKind;

    /// Region: North/South/East leaves; Total = N+S+E; Coastal = N+E;
    /// All = Total + Coastal. Attributes: Code (text), Pop (numeric).
    fn region() -> Dimension {
        let mut d = Dimension::new("Region");
        let north = d.add_leaf("North");
        let south = d.add_leaf("South");
        let east = d.add_leaf("East");
        let total = d.add_consolidated("Total");
        let coastal = d.add_consolidated("Coastal");
        let all = d.add_consolidated("All");
        d.add_child(total, north, 1).unwrap();
        d.add_child(total, south, 1).unwrap();
        d.add_child(total, east, 1).unwrap();
        d.add_child(coastal, north, 1).unwrap();
        d.add_child(coastal, east, 1).unwrap();
        d.add_child(all, total, 1).unwrap();
        d.add_child(all, coastal, 1).unwrap();
        d.add_attribute("Code", AttributeKind::Text);
        d.add_attribute("Pop", AttributeKind::Numeric);
        d.set_attribute(north, "Code", AttributeValue::Text("N".into()))
            .unwrap();
        d.set_attribute(south, "Code", AttributeValue::Text("S".into()))
            .unwrap();
        d.set_attribute(east, "Code", AttributeValue::Text("E".into()))
            .unwrap();
        d.set_attribute(north, "Pop", AttributeValue::Numeric(Fixed::from(300)))
            .unwrap();
        d.set_attribute(south, "Pop", AttributeValue::Numeric(Fixed::from(100)))
            .unwrap();
        d.set_attribute(east, "Pop", AttributeValue::Numeric(Fixed::from(200)))
            .unwrap();
        d
    }

    fn names(dim: &Dimension, indices: &[u32]) -> Vec<String> {
        indices
            .iter()
            .map(|&i| dim.element(i).unwrap().name.clone())
            .collect()
    }

    fn eval_names(src: &str, dim: &Dimension) -> Vec<String> {
        let expr = parse(src).unwrap();
        names(dim, &evaluate(&expr, dim).unwrap())
    }

    #[test]
    fn members_is_definition_order() {
        assert_eq!(
            eval_names("[Region].Members", &region()),
            vec!["North", "South", "East", "Total", "Coastal", "All"]
        );
    }

    #[test]
    fn children_is_edge_sorted() {
        let d = region();
        assert_eq!(
            eval_names("[Region].[Total].Children", &d),
            vec!["North", "South", "East"]
        );
        assert_eq!(
            eval_names("[Region].[Coastal].Children", &d),
            vec!["North", "East"]
        );
    }

    #[test]
    fn descendants_includes_self_and_dedups_alternate_rollups() {
        let d = region();
        // Total's subtree, self first, then children in edge order.
        assert_eq!(
            eval_names("Descendants([Region].[Total])", &d),
            vec!["Total", "North", "South", "East"]
        );
        // All reaches North via both Total and Coastal; it appears exactly once,
        // at its first (Total path) visit.
        assert_eq!(
            eval_names("[Region].[All].Descendants", &d),
            vec!["All", "Total", "North", "South", "East", "Coastal"]
        );
    }

    #[test]
    fn set_literal_concatenates_and_dedups() {
        let d = region();
        assert_eq!(
            eval_names("{[Region].[North], [Region].[North], [Region].[South]}", &d),
            vec!["North", "South"]
        );
    }

    #[test]
    fn filter_on_text_attribute() {
        let d = region();
        assert_eq!(
            eval_names("Filter([Region].Members, Properties(\"Code\") = \"N\")", &d),
            vec!["North"]
        );
    }

    #[test]
    fn filter_on_numeric_attribute_with_and() {
        let d = region();
        assert_eq!(
            eval_names(
                "Filter([Region].Members, Properties(\"Pop\") >= 200 AND Properties(\"Pop\") < 300)",
                &d
            ),
            vec!["East"]
        );
    }

    #[test]
    fn order_by_numeric_attribute_ascending_and_descending() {
        let d = region();
        assert_eq!(
            eval_names("Order([Region].[Total].Children, \"Pop\", ASC)", &d),
            vec!["South", "East", "North"]
        );
        assert_eq!(
            eval_names("Order([Region].[Total].Children, \"Pop\", DESC)", &d),
            vec!["North", "East", "South"]
        );
    }

    #[test]
    fn order_is_stable_on_missing_keys() {
        let d = region();
        // Consolidations have no Pop; missing keys sort first (ASC) and keep
        // their input order as the tie-break.
        let got = eval_names(
            "Order({[Region].[Total], [Region].[North]}, \"Pop\", ASC)",
            &d,
        );
        assert_eq!(got, vec!["Total", "North"]);
    }

    #[test]
    fn unknown_member_is_reported() {
        let d = region();
        let err = evaluate(&parse("[Region].[Nowhere]").unwrap(), &d).unwrap_err();
        assert_eq!(
            err,
            MdxEvalError::UnknownMember {
                dimension: "Region".into(),
                member: "Nowhere".into()
            }
        );
    }

    #[test]
    fn dimension_mismatch_is_reported() {
        let d = region();
        let err = evaluate(&parse("[Other].[North]").unwrap(), &d).unwrap_err();
        assert_eq!(
            err,
            MdxEvalError::DimensionMismatch {
                expected: "Region".into(),
                found: "Other".into()
            }
        );
    }

    #[test]
    fn unknown_attribute_is_reported() {
        let d = region();
        let err = evaluate(
            &parse("Filter([Region].Members, Properties(\"Nope\") = \"x\")").unwrap(),
            &d,
        )
        .unwrap_err();
        assert!(matches!(err, MdxEvalError::UnknownAttribute { .. }));
    }

    #[test]
    fn type_mismatch_is_reported() {
        let d = region();
        // Code is text; comparing it to a number is a type error where present.
        let err = evaluate(
            &parse("Filter([Region].Members, Properties(\"Code\") > 5)").unwrap(),
            &d,
        )
        .unwrap_err();
        assert!(matches!(err, MdxEvalError::TypeMismatch { .. }));
    }

    #[test]
    fn crossjoin_in_a_subset_is_rejected() {
        let d = region();
        let err = evaluate(&parse("[Region].Members * [Region].Members").unwrap(), &d).unwrap_err();
        assert_eq!(err, MdxEvalError::TupleSetNotAllowed);
    }

    #[test]
    fn evaluation_is_deterministic_and_descendants_unique() {
        let d = region();
        let expr = parse("[Region].[All].Descendants").unwrap();
        let a = evaluate(&expr, &d).unwrap();
        let b = evaluate(&expr, &d).unwrap();
        assert_eq!(a, b, "same expression must evaluate identically");
        let unique: HashSet<u32> = a.iter().copied().collect();
        assert_eq!(unique.len(), a.len(), "descendants must not repeat");
    }
}
