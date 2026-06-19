//! Abstract syntax for the supported MDX set sublanguage.
//!
//! A [`SetExpr`] evaluates, over a single dimension, to an ordered and
//! de-duplicated list of element indices; over a cube (for `Crossjoin` / `*`)
//! to an ordered list of member tuples. Evaluation lives in `eval` (Phase 3B);
//! this module is pure syntax plus a canonical [`Display`] that round-trips back
//! through [`crate::parse`].

use std::fmt;

/// A parsed set expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetExpr {
    /// `{ a, b, c }` - the listed sub-sets in order (de-duplicated at eval).
    Set(Vec<SetExpr>),
    /// A bare member reference (`[Region].[North]`); a singleton set.
    Member(MemberRef),
    /// `<ref>.Members` - every element of the named dimension, definition order.
    Members(MemberRef),
    /// `<ref>.Children` - the member's immediate children, edge order.
    Children(MemberRef),
    /// `<ref>.Descendants` or `Descendants(<ref>)` - pre-order DFS, de-duped.
    Descendants(MemberRef),
    /// `Filter(set, predicate)` - members for which the predicate holds.
    Filter(Box<SetExpr>, Predicate),
    /// `Order(set, "Attr", dir)` - stable sort by an attribute key.
    Order(Box<SetExpr>, String, OrderDir),
    /// `Crossjoin(a, b)` or `a * b` - the cartesian product, `a`-major.
    Crossjoin(Box<SetExpr>, Box<SetExpr>),
}

/// A dotted member path: `[Region].[North]` becomes `["Region", "North"]`; a
/// bare `North` becomes `["North"]`. The evaluator resolves the path against a
/// specific dimension (which it already knows), so a leading dimension segment
/// is validated rather than required.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberRef {
    /// The path segments, outermost first.
    pub path: Vec<String>,
}

impl MemberRef {
    /// Construct a member reference from its path segments.
    pub fn new(path: Vec<String>) -> Self {
        Self { path }
    }

    /// The member name (the last path segment), or `""` for an empty path.
    pub fn name(&self) -> &str {
        self.path.last().map(String::as_str).unwrap_or("")
    }
}

/// Sort direction for `Order`. The `B` forms break hierarchy (a flat sort);
/// the plain forms preserve hierarchy as a stable tie-break (Phase 3B).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderDir {
    /// Ascending, hierarchy-preserving.
    Asc,
    /// Descending, hierarchy-preserving.
    Desc,
    /// Ascending, hierarchy-breaking (flat).
    BAsc,
    /// Descending, hierarchy-breaking (flat).
    BDesc,
}

/// A `Filter` predicate over the current member's attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Predicate {
    /// Logical conjunction.
    And(Box<Predicate>, Box<Predicate>),
    /// Logical disjunction.
    Or(Box<Predicate>, Box<Predicate>),
    /// Logical negation.
    Not(Box<Predicate>),
    /// A binary comparison between two operands.
    Compare {
        /// Left operand.
        left: Operand,
        /// Comparison operator.
        op: CmpOp,
        /// Right operand.
        right: Operand,
    },
}

/// An operand within a [`Predicate`] comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operand {
    /// The current member's value for an attribute, via `.Properties("Attr")`.
    Property(String),
    /// A quoted string literal.
    Str(String),
    /// A numeric literal, kept verbatim (parsed to `Fixed` at eval time).
    Number(String),
}

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    /// `=`
    Eq,
    /// `<>`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

/// Escape a name for canonical bracketed output (`]` becomes `]]`).
fn bracket(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

/// Escape a string literal for canonical double-quoted output.
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

impl fmt::Display for MemberRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self.path.iter().map(|p| bracket(p)).collect();
        write!(f, "{}", parts.join("."))
    }
}

impl fmt::Display for OrderDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            OrderDir::Asc => "ASC",
            OrderDir::Desc => "DESC",
            OrderDir::BAsc => "BASC",
            OrderDir::BDesc => "BDESC",
        };
        f.write_str(s)
    }
}

impl fmt::Display for CmpOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            CmpOp::Eq => "=",
            CmpOp::Ne => "<>",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        };
        f.write_str(s)
    }
}

impl fmt::Display for Operand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operand::Property(attr) => write!(f, "Properties({})", quote(attr)),
            Operand::Str(s) => f.write_str(&quote(s)),
            Operand::Number(n) => f.write_str(n),
        }
    }
}

impl fmt::Display for Predicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Predicate::And(l, r) => write!(f, "({l} AND {r})"),
            Predicate::Or(l, r) => write!(f, "({l} OR {r})"),
            Predicate::Not(p) => write!(f, "(NOT {p})"),
            Predicate::Compare { left, op, right } => write!(f, "{left} {op} {right}"),
        }
    }
}

impl fmt::Display for SetExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SetExpr::Set(items) => {
                let parts: Vec<String> = items.iter().map(|i| i.to_string()).collect();
                write!(f, "{{{}}}", parts.join(", "))
            }
            SetExpr::Member(r) => write!(f, "{r}"),
            SetExpr::Members(r) => write!(f, "{r}.Members"),
            SetExpr::Children(r) => write!(f, "{r}.Children"),
            SetExpr::Descendants(r) => write!(f, "Descendants({r})"),
            SetExpr::Filter(s, p) => write!(f, "Filter({s}, {p})"),
            SetExpr::Order(s, attr, dir) => write!(f, "Order({s}, {}, {dir})", bracket(attr)),
            SetExpr::Crossjoin(a, b) => write!(f, "Crossjoin({a}, {b})"),
        }
    }
}

/// Which axis a set is bound to in a `SELECT`. The pivot UI emits `COLUMNS`/`ROWS`;
/// `ON <n>` (0 = columns, 1 = rows, ...) is the general ordinal form, canonicalized
/// so `ON 0`/`ON COLUMNS` and `ON 1`/`ON ROWS` compare equal (used for duplicate
/// detection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxisName {
    /// `ON COLUMNS` (axis 0).
    Columns,
    /// `ON ROWS` (axis 1).
    Rows,
    /// `ON <n>` for n >= 2.
    Ordinal(u32),
}

/// A parsed full MDX `SELECT` query: per-axis sets, the cube from `FROM`, and the
/// optional `WHERE ( ... )` slicer. The set on each axis may be a `Crossjoin` of
/// per-dimension sets (the view layer expands those into tuples).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    /// Each axis in source order: the axis it is bound to and its set expression.
    pub axes: Vec<(AxisName, SetExpr)>,
    /// The cube named in `FROM` (unbracketed).
    pub cube: String,
    /// The `WHERE ( m, ... )` slicer members; empty when there is no `WHERE`.
    pub slicer: Vec<MemberRef>,
}

impl fmt::Display for AxisName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AxisName::Columns => f.write_str("COLUMNS"),
            AxisName::Rows => f.write_str("ROWS"),
            AxisName::Ordinal(n) => write!(f, "{n}"),
        }
    }
}

impl fmt::Display for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let axes: Vec<String> = self
            .axes
            .iter()
            .map(|(axis, set)| format!("  {set} ON {axis}"))
            .collect();
        write!(
            f,
            "SELECT\n{}\nFROM {}",
            axes.join(",\n"),
            bracket(&self.cube)
        )?;
        if !self.slicer.is_empty() {
            let members: Vec<String> = self.slicer.iter().map(|m| m.to_string()).collect();
            write!(f, "\nWHERE ( {} )", members.join(", "))?;
        }
        Ok(())
    }
}
