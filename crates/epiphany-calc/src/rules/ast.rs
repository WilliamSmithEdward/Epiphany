//! Abstract syntax for the rules language.
//!
//! A rules document is an ordered list of `area = formula ;` statements. An area
//! is a per-dimension selector tuple naming the target cells; a formula is an
//! arithmetic/conditional expression over cell references, literals, and a closed
//! function allow-list. This module is pure syntax plus a canonical [`Display`]
//! that round-trips back through [`crate::rules::parse`]. Spans are kept for
//! downstream compile diagnostics and so are excluded from equality (Display is
//! the canonical comparison).

use std::fmt;

use crate::rules::lexer::Span;

/// A parsed rules document; `rules` is in source (author) order.
#[derive(Debug, Clone)]
pub struct RuleDoc {
    /// The rule statements, in author order (index is precedence source order).
    pub rules: Vec<Rule>,
}

/// One `area = formula ;` statement.
#[derive(Debug, Clone)]
pub struct Rule {
    /// The cells this rule targets.
    pub area: Area,
    /// The value expression.
    pub formula: Expr,
    /// The whole-statement span (for diagnostics).
    pub span: Span,
}

/// A conjunction of per-dimension selectors. A dimension absent from the area is
/// unconstrained (the rule applies across all of its members).
#[derive(Debug, Clone)]
pub struct Area {
    /// One selector per constrained dimension (each dimension at most once).
    pub selectors: Vec<DimSelector>,
}

/// A selector constraining one dimension of an area.
#[derive(Debug, Clone)]
pub struct DimSelector {
    /// The dimension this selector constrains.
    pub dimension: String,
    /// How members of that dimension are chosen.
    pub kind: SelectorKind,
    /// The selector's span.
    pub span: Span,
}

/// How an area selects members of one dimension.
#[derive(Debug, Clone)]
pub enum SelectorKind {
    /// A single named element (the most specific).
    Element(String),
    /// Members whose attribute compares to a literal.
    AttrPredicate {
        /// The attribute name.
        attribute: String,
        /// The comparison operator.
        op: CmpOp,
        /// The right-hand literal.
        value: Literal,
    },
    /// The immediate children of an element.
    Children(String),
    /// An element and all of its descendants.
    Descendants(String),
    /// All numeric leaves of the dimension.
    Leaves,
    /// All consolidated elements of the dimension.
    Consolidated,
    /// Every element of the dimension.
    All,
}

/// A literal value (the right-hand side of an attribute predicate).
#[derive(Debug, Clone)]
pub enum Literal {
    /// A quoted string.
    Str(String),
    /// A numeric literal, kept verbatim.
    Number(String),
}

/// A value expression.
#[derive(Debug, Clone)]
pub enum Expr {
    /// A numeric literal (parsed to `Fixed` at compile time).
    Number(String),
    /// A string literal (string-cell formulas; deferred at eval for M4).
    Str(String),
    /// A cell reference.
    Cell(CellRef),
    /// Unary minus.
    Neg(Box<Expr>),
    /// A binary arithmetic operation.
    Bin {
        /// The operator.
        op: ArithOp,
        /// Left operand.
        left: Box<Expr>,
        /// Right operand.
        right: Box<Expr>,
    },
    /// `IF cond THEN expr [ELSE expr]`.
    If {
        /// The condition.
        cond: Box<Condition>,
        /// The value when the condition holds.
        then: Box<Expr>,
        /// The value otherwise (defaults to empty/zero when absent).
        otherwise: Option<Box<Expr>>,
    },
    /// A built-in function call.
    Func(FuncCall),
}

/// A binary arithmetic operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
}

/// A reference to a cell, relative to the current target unless fully qualified.
#[derive(Debug, Clone)]
pub struct CellRef {
    /// The cube name for a cross-cube reference; `None` is the current cube.
    pub cube: Option<String>,
    /// Per-dimension overrides of the current target coordinate.
    pub overrides: Vec<DimOverride>,
    /// Cross-cube dimension name mapping (`with (src -> dst)`).
    pub mapping: Vec<(String, String)>,
    /// The reference span.
    pub span: Span,
}

/// One dimension override within a cell reference.
#[derive(Debug, Clone)]
pub struct DimOverride {
    /// The dimension being overridden.
    pub dimension: String,
    /// The member to use for that dimension.
    pub member: MemberExpr,
}

/// How an overriding member is chosen.
#[derive(Debug, Clone)]
pub enum MemberExpr {
    /// A named element.
    Element(String),
    /// The element resolved by an attribute value (deferred at eval for M4).
    Attr(String),
}

/// A boolean condition (usable only inside `IF`).
#[derive(Debug, Clone)]
pub enum Condition {
    /// Logical conjunction.
    And(Box<Condition>, Box<Condition>),
    /// Logical disjunction.
    Or(Box<Condition>, Box<Condition>),
    /// Logical negation.
    Not(Box<Condition>),
    /// A comparison between two value expressions.
    Compare {
        /// Left operand.
        left: Expr,
        /// Comparison operator.
        op: CmpOp,
        /// Right operand.
        right: Expr,
    },
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

/// A built-in function call (closed allow-list).
#[derive(Debug, Clone)]
pub struct FuncCall {
    /// Which function.
    pub func: BuiltinFunc,
    /// The arguments.
    pub args: Vec<FuncArg>,
    /// The call span.
    pub span: Span,
}

/// The closed set of built-in functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinFunc {
    /// `Attr('Dim','Name')` - the current member's text attribute.
    Attr,
    /// `AttrNum('Dim','Name')` - the current member's numeric attribute.
    AttrNum,
    /// `IsLeaf('Dim')` - whether the current member is a leaf.
    IsLeaf,
    /// `ElementName('Dim')` - the current element's name.
    ElementName,
    /// `Undef()` - the explicit no-value sentinel.
    Undef,
}

impl BuiltinFunc {
    /// The canonical spelling.
    pub fn name(self) -> &'static str {
        match self {
            BuiltinFunc::Attr => "Attr",
            BuiltinFunc::AttrNum => "AttrNum",
            BuiltinFunc::IsLeaf => "IsLeaf",
            BuiltinFunc::ElementName => "ElementName",
            BuiltinFunc::Undef => "Undef",
        }
    }

    /// Resolve a bare word (case-insensitive) to a built-in, if any.
    pub fn from_word(word: &str) -> Option<BuiltinFunc> {
        match word.to_ascii_lowercase().as_str() {
            "attr" => Some(BuiltinFunc::Attr),
            "attrnum" => Some(BuiltinFunc::AttrNum),
            "isleaf" => Some(BuiltinFunc::IsLeaf),
            "elementname" => Some(BuiltinFunc::ElementName),
            "undef" => Some(BuiltinFunc::Undef),
            _ => None,
        }
    }
}

/// An argument to a built-in function.
#[derive(Debug, Clone)]
pub enum FuncArg {
    /// A quoted string argument (e.g. a dimension/attribute name).
    Str(String),
    /// A value expression argument.
    Expr(Expr),
}

/// Quote a name or string for canonical output (`'` doubled).
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

impl fmt::Display for ArithOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ArithOp::Add => "+",
            ArithOp::Sub => "-",
            ArithOp::Mul => "*",
            ArithOp::Div => "/",
        })
    }
}

impl fmt::Display for CmpOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CmpOp::Eq => "=",
            CmpOp::Ne => "<>",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        })
    }
}

impl fmt::Display for Literal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Literal::Str(s) => f.write_str(&quote(s)),
            Literal::Number(n) => f.write_str(n),
        }
    }
}

impl fmt::Display for SelectorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SelectorKind::Element(m) => f.write_str(&quote(m)),
            SelectorKind::AttrPredicate {
                attribute,
                op,
                value,
            } => write!(f, "@{} {op} {value}", quote(attribute)),
            SelectorKind::Children(m) => write!(f, "{{children of {}}}", quote(m)),
            SelectorKind::Descendants(m) => write!(f, "{{descendants of {}}}", quote(m)),
            SelectorKind::Leaves => f.write_str("{leaves}"),
            SelectorKind::Consolidated => f.write_str("{consolidated}"),
            SelectorKind::All => f.write_str("{all}"),
        }
    }
}

impl fmt::Display for Area {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self
            .selectors
            .iter()
            .map(|s| format!("{}: {}", quote(&s.dimension), s.kind))
            .collect();
        write!(f, "[{}]", parts.join(", "))
    }
}

impl fmt::Display for MemberExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemberExpr::Element(m) => f.write_str(&quote(m)),
            MemberExpr::Attr(a) => write!(f, "!{}", quote(a)),
        }
    }
}

impl fmt::Display for CellRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.cube {
            Some(cube) => write!(f, "{}!", quote(cube))?,
            None => f.write_str("value")?,
        }
        if !self.overrides.is_empty() {
            let parts: Vec<String> = self
                .overrides
                .iter()
                .map(|o| format!("{}: {}", quote(&o.dimension), o.member))
                .collect();
            write!(f, "[{}]", parts.join(", "))?;
        }
        if !self.mapping.is_empty() {
            let parts: Vec<String> = self
                .mapping
                .iter()
                .map(|(src, dst)| format!("{} -> {}", quote(src), quote(dst)))
                .collect();
            write!(f, " with ({})", parts.join(", "))?;
        }
        Ok(())
    }
}

impl fmt::Display for FuncCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self.args.iter().map(|a| a.to_string()).collect();
        write!(f, "{}({})", self.func.name(), parts.join(", "))
    }
}

impl fmt::Display for FuncArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FuncArg::Str(s) => f.write_str(&quote(s)),
            FuncArg::Expr(e) => write!(f, "{e}"),
        }
    }
}

impl fmt::Display for Condition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // No grouping parentheses: conditions follow the precedence grammar
        // (NOT > AND > OR), and the parser produces precedence-shaped trees, so
        // paren-free output round-trips. Arithmetic operands carry their own
        // parens via Expr's Display.
        match self {
            Condition::And(a, b) => write!(f, "{a} AND {b}"),
            Condition::Or(a, b) => write!(f, "{a} OR {b}"),
            Condition::Not(c) => write!(f, "NOT {c}"),
            Condition::Compare { left, op, right } => write!(f, "{left} {op} {right}"),
        }
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Number(n) => f.write_str(n),
            Expr::Str(s) => f.write_str(&quote(s)),
            Expr::Cell(r) => write!(f, "{r}"),
            Expr::Neg(e) => write!(f, "-{e}"),
            Expr::Bin { op, left, right } => write!(f, "({left} {op} {right})"),
            Expr::If {
                cond,
                then,
                otherwise,
            } => match otherwise {
                Some(o) => write!(f, "IF {cond} THEN {then} ELSE {o}"),
                None => write!(f, "IF {cond} THEN {then}"),
            },
            Expr::Func(c) => write!(f, "{c}"),
        }
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} = {};", self.area, self.formula)
    }
}

impl fmt::Display for RuleDoc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self.rules.iter().map(|r| r.to_string()).collect();
        f.write_str(&parts.join("\n"))
    }
}
