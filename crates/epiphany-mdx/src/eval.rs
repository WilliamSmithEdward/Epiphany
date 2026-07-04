//! Tree-walking evaluator for the MDX set sublanguage.
//!
//! [`evaluate`] resolves a [`SetExpr`] against a single borrowed [`Dimension`]
//! into an ordered, de-duplicated list of element indices. It is pure and reads
//! the dimension immutably, so it is safe to run over an MVCC read snapshot.
//!
//! Determinism: every ordering comes from a deterministic core primitive -
//! `iter_elements` is definition order, `children_of` is authored
//! (edge-declaration) rollup order, and `Order` uses a key sort with the input
//! position as a stable tie-break. The `Descendants` de-duplication uses a
//! `HashSet` purely for the visited skip-check; emission order is the
//! authored-order pre-order DFS, never the set's iteration order. The DFS is
//! iterative with an explicit work stack and a depth backstop, so a deep
//! consolidation chain returns a clean error instead of overflowing the stack.
//!
//! Crossjoin (`a * b`) is parsed but rejected here: a tuple set is not a valid
//! single-dimension member set. Tuple nesting is handled by the view layer
//! (Phase 3D), which crossjoins per-dimension subsets itself.

use std::collections::HashSet;
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
    /// A `Descendants` traversal exceeded the consolidation-depth backstop
    /// (a pathologically deep parent-child chain), stopped before it could
    /// exhaust the thread stack.
    TooDeep {
        /// The dimension being evaluated.
        dimension: String,
        /// The depth limit that was exceeded.
        limit: u32,
    },
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
            MdxEvalError::TooDeep { dimension, limit } => write!(
                f,
                "consolidation depth in dimension '{dimension}' exceeds the maximum of {limit}"
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
            children_of(dim, parent)
        }
        SetExpr::Descendants(r) => {
            let root = resolve_member(dim, r)?;
            descendants_of(dim, root)
        }
        SetExpr::Filter(set, pred) => {
            let base = eval_set(set, dim)?;
            let mut out = Vec::new();
            for element in base {
                // SQL-style filtering: a member is kept only when the predicate
                // is definitely true; both false and unknown (a missing
                // attribute) exclude it.
                if eval_predicate(dim, element, pred)? == Truth::True {
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

/// Resolve a member reference to an element index. A two-segment
/// `[Dim].[Member]` path validates its first segment against the dimension; a
/// bare `[Member]` resolves directly. Paths deeper than two segments (an
/// intermediate-ancestor scope like `[Region].[Total].[North]`) are not part of
/// the implemented subset and are rejected rather than silently resolving only
/// the last segment.
fn resolve_member(dim: &Dimension, r: &MemberRef) -> Result<u32, MdxEvalError> {
    check_dimension_qualifier(dim, r)?;
    let name = r.name();
    dim.resolve(name)
        .ok_or_else(|| MdxEvalError::UnknownMember {
            dimension: dim.name().to_string(),
            member: name.to_string(),
        })
}

/// Validate that a `.Members` reference names the dimension being evaluated,
/// applying the same first-segment / path-length rule as [`resolve_member`] so
/// the two spellings agree (`[Dim].Members` vs `[Dim].[Member]`).
fn dimension_ref(dim: &Dimension, r: &MemberRef) -> Result<(), MdxEvalError> {
    check_dimension_qualifier(dim, r)?;
    let name = r.name();
    if name != dim.name() {
        return Err(MdxEvalError::DimensionMismatch {
            expected: dim.name().to_string(),
            found: name.to_string(),
        });
    }
    Ok(())
}

/// Shared path shape / dimension-qualifier check for member and `.Members`
/// references. A two-segment path must be qualified by this dimension; a path
/// with more than two segments names an unsupported intermediate-ancestor scope
/// and is reported as a mismatch on the offending qualifier.
fn check_dimension_qualifier(dim: &Dimension, r: &MemberRef) -> Result<(), MdxEvalError> {
    if r.path.len() > 2 {
        return Err(MdxEvalError::DimensionMismatch {
            expected: dim.name().to_string(),
            found: r.path[..r.path.len() - 1].join("."),
        });
    }
    if r.path.len() == 2 && r.path[0] != dim.name() {
        return Err(MdxEvalError::DimensionMismatch {
            expected: dim.name().to_string(),
            found: r.path[0].clone(),
        });
    }
    Ok(())
}

/// The immediate children of `parent`, in the dimension's authored
/// (edge-declaration) rollup order. `children_of` on core reads the per-parent
/// edge list directly, so this avoids materializing and sorting the whole edge
/// set. The authored order is deterministic and matches MDX/TM1 `.Children`
/// hierarchy-order semantics.
fn children_of(dim: &Dimension, parent: u32) -> Result<Vec<u32>, MdxEvalError> {
    Ok(dedup(dim.children_of(parent)?))
}

/// Maximum consolidation-chain depth a single `Descendants` traversal will
/// follow before returning a clean evaluation error. A stack/loop backstop, not
/// a real modelling limit: authored hierarchies nest a handful of levels, far
/// below this. Deliberately a constant (not env-configurable), mirroring the
/// parser's [`MAX_PARSE_DEPTH`](crate::parser). Guards against a pathological
/// deep parent-child chain (buildable via the dimension-editing API / an ETL
/// import) whose recursion would otherwise abort the process.
const MAX_DESCENDANTS_DEPTH: u32 = 4096;

/// The member and all of its descendants, as an authored-order pre-order DFS,
/// de-duplicated by first visit (safe under alternate rollups).
///
/// Iterative with an explicit work stack (mirroring `Dimension::reaches`), so a
/// deep consolidation chain cannot overflow the thread stack. A depth budget
/// ([`MAX_DESCENDANTS_DEPTH`]) is a second backstop that returns a clean
/// [`MdxEvalError::TooDeep`] rather than allocating without bound.
fn descendants_of(dim: &Dimension, root: u32) -> Result<Vec<u32>, MdxEvalError> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    // Each frame carries the node and its depth from `root` (root is depth 0).
    let mut stack: Vec<(u32, u32)> = vec![(root, 0)];
    while let Some((node, depth)) = stack.pop() {
        if !seen.insert(node) {
            continue;
        }
        if depth > MAX_DESCENDANTS_DEPTH {
            return Err(MdxEvalError::TooDeep {
                dimension: dim.name().to_string(),
                limit: MAX_DESCENDANTS_DEPTH,
            });
        }
        out.push(node);
        // Push children in reverse so the authored order is emitted left-to-right
        // (LIFO stack), matching the previous recursive pre-order DFS.
        let children = dim.children_of(node)?;
        for &child in children.iter().rev() {
            stack.push((child, depth + 1));
        }
    }
    Ok(out)
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

/// Three-valued predicate result (SQL NULL semantics). A comparison touching a
/// missing attribute is `Unknown`, not `False`, so that `NOT`/`AND`/`OR`
/// propagate it correctly and the two spellings of "not equal"
/// (`x <> "y"` and `NOT x = "y"`) agree: both leave a missing-attribute member
/// out of a `Filter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Truth {
    True,
    False,
    Unknown,
}

impl Truth {
    fn from_bool(b: bool) -> Self {
        if b {
            Truth::True
        } else {
            Truth::False
        }
    }

    /// Kleene negation: `NOT Unknown` is `Unknown`.
    fn not(self) -> Self {
        match self {
            Truth::True => Truth::False,
            Truth::False => Truth::True,
            Truth::Unknown => Truth::Unknown,
        }
    }

    /// Kleene conjunction.
    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Truth::False, _) | (_, Truth::False) => Truth::False,
            (Truth::True, Truth::True) => Truth::True,
            _ => Truth::Unknown,
        }
    }

    /// Kleene disjunction.
    fn or(self, other: Self) -> Self {
        match (self, other) {
            (Truth::True, _) | (_, Truth::True) => Truth::True,
            (Truth::False, Truth::False) => Truth::False,
            _ => Truth::Unknown,
        }
    }
}

fn eval_predicate(dim: &Dimension, element: u32, pred: &Predicate) -> Result<Truth, MdxEvalError> {
    match pred {
        // Both sides are evaluated eagerly so that an ill-typed branch reports a
        // deterministic error regardless of the other branch's truth value.
        Predicate::And(l, r) => {
            let a = eval_predicate(dim, element, l)?;
            let b = eval_predicate(dim, element, r)?;
            Ok(a.and(b))
        }
        Predicate::Or(l, r) => {
            let a = eval_predicate(dim, element, l)?;
            let b = eval_predicate(dim, element, r)?;
            Ok(a.or(b))
        }
        Predicate::Not(p) => Ok(eval_predicate(dim, element, p)?.not()),
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

fn compare_vals(l: &Val, r: &Val, op: CmpOp) -> Result<Truth, MdxEvalError> {
    match (l, r) {
        // A missing attribute yields `Unknown` (SQL NULL semantics): the result
        // propagates through NOT/AND/OR unchanged, and a bare comparison on a
        // missing value leaves the member out of a `Filter`.
        (Val::Missing, _) | (_, Val::Missing) => Ok(Truth::Unknown),
        (Val::Text(a), Val::Text(b)) => Ok(Truth::from_bool(apply_op(a.as_str(), b.as_str(), op))),
        (Val::Num(a), Val::Num(b)) => Ok(Truth::from_bool(apply_op(a, b, op))),
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
    fn children_is_authored_order() {
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

    /// `.Children` follows the authored (edge-declaration) rollup order, not the
    /// element-creation index order. `Q` is created before its children, and its
    /// children are attached in a deliberately non-index order.
    #[test]
    fn children_preserve_authored_rollup_order() {
        let mut d = Dimension::new("Cal");
        let q = d.add_consolidated("Q"); // index 0
        let jan = d.add_leaf("Jan"); // 1
        let feb = d.add_leaf("Feb"); // 2
        let mar = d.add_leaf("Mar"); // 3
                                     // Attach out of index order: Mar, Jan, Feb.
        d.add_child(q, mar, 1).unwrap();
        d.add_child(q, jan, 1).unwrap();
        d.add_child(q, feb, 1).unwrap();
        assert_eq!(
            eval_names("[Cal].[Q].Children", &d),
            vec!["Mar", "Jan", "Feb"],
            "children must follow authored rollup order, not element index"
        );
        assert_eq!(
            eval_names("Descendants([Cal].[Q])", &d),
            vec!["Q", "Mar", "Jan", "Feb"],
            "descendants pre-order must follow authored rollup order"
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

    /// A pathologically deep parent-child chain must return a clean `TooDeep`
    /// error instead of overflowing the thread stack (the recursion guard).
    #[test]
    fn descendants_on_deep_chain_errors_cleanly() {
        let mut d = Dimension::new("Chain");
        // A linear consolidation chain far deeper than MAX_DESCENDANTS_DEPTH.
        let depth = (MAX_DESCENDANTS_DEPTH as usize) + 10;
        let mut prev = d.add_consolidated("n0");
        for i in 1..depth {
            let node = if i + 1 == depth {
                d.add_leaf(format!("n{i}"))
            } else {
                d.add_consolidated(format!("n{i}"))
            };
            d.add_child(prev, node, 1).unwrap();
            prev = node;
        }
        let expr = parse("Descendants([Chain].[n0])").unwrap();
        let err = evaluate(&expr, &d).unwrap_err();
        assert!(
            matches!(err, MdxEvalError::TooDeep { .. }),
            "deep chain must error cleanly, got {err:?}"
        );
    }

    /// A chain right at the depth limit still evaluates successfully (the guard
    /// only trips past the backstop, so legitimate hierarchies are unaffected).
    #[test]
    fn descendants_within_depth_budget_succeeds() {
        let mut d = Dimension::new("Chain");
        let depth = 200usize; // well within MAX_DESCENDANTS_DEPTH
        let mut prev = d.add_consolidated("n0");
        for i in 1..depth {
            let node = if i + 1 == depth {
                d.add_leaf(format!("n{i}"))
            } else {
                d.add_consolidated(format!("n{i}"))
            };
            d.add_child(prev, node, 1).unwrap();
            prev = node;
        }
        let expr = parse("Descendants([Chain].[n0])").unwrap();
        let got = evaluate(&expr, &d).unwrap();
        assert_eq!(got.len(), depth, "every node on the chain is visited once");
    }

    /// `<>` and `NOT =` must agree on members whose attribute is missing:
    /// consolidations have no `Code`, so both spellings must EXCLUDE them
    /// (SQL three-valued logic: a comparison on a missing value is unknown).
    #[test]
    fn ne_and_not_eq_agree_on_missing_attribute() {
        let d = region();
        let ne = eval_names(
            "Filter([Region].Members, Properties(\"Code\") <> \"N\")",
            &d,
        );
        let not_eq = eval_names(
            "Filter([Region].Members, NOT Properties(\"Code\") = \"N\")",
            &d,
        );
        // South and East have a Code that is not "N"; North's is "N";
        // Total/Coastal/All have no Code, so both forms drop them.
        assert_eq!(ne, vec!["South", "East"]);
        assert_eq!(
            ne, not_eq,
            "`<>` and `NOT =` must agree on missing-attribute members"
        );
    }

    /// A path deeper than `[Dim].[Member]` names an unsupported
    /// intermediate-ancestor scope and is rejected rather than silently
    /// resolving only the last segment.
    #[test]
    fn deep_member_path_is_rejected() {
        let d = region();
        let err = evaluate(&parse("[Region].[Total].[North]").unwrap(), &d).unwrap_err();
        assert!(
            matches!(err, MdxEvalError::DimensionMismatch { .. }),
            "a 3-segment member path must be rejected, got {err:?}"
        );
    }

    /// `dimension_ref` (`.Members`) applies the same first-segment rule as a
    /// member reference: a wrong dimension qualifier is a mismatch.
    #[test]
    fn members_wrong_dimension_qualifier_is_rejected() {
        let d = region();
        let err = evaluate(&parse("[Bogus].[Region].Members").unwrap(), &d).unwrap_err();
        assert!(
            matches!(err, MdxEvalError::DimensionMismatch { .. }),
            "a wrong .Members qualifier must be rejected, got {err:?}"
        );
    }
}
