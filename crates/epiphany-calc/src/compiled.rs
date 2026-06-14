//! The compiled, name-resolved form of a rule set.
//!
//! Compilation (see [`crate::compile`]) lowers the parsed [`crate::rules`] AST to
//! these types once per published model version: dimension/member/attribute/cube
//! names are resolved to indices, and the formula becomes a resolved expression
//! tree that the evaluator (Phase 4D) walks with zero string work and zero
//! re-parsing. The compiled form is immutable and never serialized (it is a
//! derived cache rebuilt from the rule source text).

use std::fmt;

use epiphany_core::{Cube, Fixed};

use crate::rules::{ArithOp, CmpOp, Span};

/// A compiled rule set, built once per engine `version`.
#[derive(Debug, Clone)]
pub struct CompiledModel {
    /// The engine commit version this was compiled against.
    pub version: u64,
    /// Compiled rules, in author (precedence) order; first matching rule wins.
    pub rules: Vec<CompiledRule>,
}

impl CompiledModel {
    /// The id of the first rule whose area matches `coord`, in author order, or
    /// `None` if no rule targets the coordinate.
    pub fn matching_rule(&self, cube: &Cube, coord: &[u32]) -> Option<RuleId> {
        self.rules
            .iter()
            .position(|r| r.area.matches(cube, coord))
            .map(RuleId)
    }
}

/// A stable rule identifier (its index in [`CompiledModel::rules`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RuleId(pub usize);

/// One compiled rule: a resolved target area and a resolved formula.
#[derive(Debug, Clone)]
pub struct CompiledRule {
    /// The cells this rule targets (per target-cube dimension).
    pub area: CompiledArea,
    /// The resolved formula.
    pub expr: CExpr,
    /// The source span of the rule statement (for explain / diagnostics).
    pub span: Span,
}

/// A resolved area: one membership predicate per target-cube dimension.
#[derive(Debug, Clone)]
pub struct CompiledArea {
    /// Index = dimension position in the target cube.
    pub per_dim: Vec<DimPredicate>,
}

impl CompiledArea {
    /// Whether `coord` (one element index per dimension) falls in this area.
    ///
    /// An unconstrained dimension (`Any`) matches only LEAF members, so a rule
    /// that leaves a dimension free computes leaf values and lets consolidations
    /// roll those up; overriding a consolidated cell requires explicitly naming
    /// the consolidated element (an `OneOf` set that includes it).
    pub fn matches(&self, cube: &Cube, coord: &[u32]) -> bool {
        if coord.len() != self.per_dim.len() {
            return false;
        }
        for (d, pred) in self.per_dim.iter().enumerate() {
            let idx = coord[d];
            let ok = match pred {
                DimPredicate::Any => cube
                    .dimension(d)
                    .element(idx)
                    .map(|e| e.kind.is_leaf())
                    .unwrap_or(false),
                DimPredicate::OneOf(set) => set.binary_search(&idx).is_ok(),
            };
            if !ok {
                return false;
            }
        }
        true
    }

    /// Whether two areas can select a common coordinate (used for the static
    /// dependency graph). Areas of different rank never intersect.
    pub fn intersects(&self, other: &CompiledArea) -> bool {
        self.per_dim.len() == other.per_dim.len()
            && self
                .per_dim
                .iter()
                .zip(&other.per_dim)
                .all(|(a, b)| a.intersects(b))
    }
}

/// A per-dimension membership predicate.
#[derive(Debug, Clone)]
pub enum DimPredicate {
    /// Any element of this dimension (the dimension was unconstrained).
    Any,
    /// One of a sorted set of element indices.
    OneOf(Vec<u32>),
}

impl DimPredicate {
    /// Whether two predicates share at least one element (conservative: `Any`
    /// intersects anything, used only by the static dependency graph).
    pub fn intersects(&self, other: &DimPredicate) -> bool {
        match (self, other) {
            (DimPredicate::Any, _) | (_, DimPredicate::Any) => true,
            (DimPredicate::OneOf(a), DimPredicate::OneOf(b)) => {
                // Both sorted: a linear merge-style membership check.
                let (mut i, mut j) = (0, 0);
                while i < a.len() && j < b.len() {
                    match a[i].cmp(&b[j]) {
                        std::cmp::Ordering::Less => i += 1,
                        std::cmp::Ordering::Greater => j += 1,
                        std::cmp::Ordering::Equal => return true,
                    }
                }
                false
            }
        }
    }
}

/// A resolved value expression (numeric for M4).
#[derive(Debug, Clone)]
pub enum CExpr {
    /// A numeric literal.
    Num(Fixed),
    /// A cell reference (resolved cube ordinal + per-dimension address).
    Cell(CCell),
    /// A numeric attribute lookup on the current member of a dimension.
    AttrNum {
        /// The target-cube dimension position.
        dim_pos: usize,
        /// The attribute index within that dimension.
        attr: u32,
    },
    /// The explicit no-value sentinel (evaluates to zero, marks "not populated").
    Undef,
    /// Unary minus.
    Neg(Box<CExpr>),
    /// A binary arithmetic operation.
    Bin {
        /// The operator.
        op: ArithOp,
        /// Left operand.
        left: Box<CExpr>,
        /// Right operand.
        right: Box<CExpr>,
    },
    /// A conditional.
    If {
        /// The condition.
        cond: Box<CCond>,
        /// The value when the condition holds.
        then: Box<CExpr>,
        /// The value otherwise (zero when absent).
        otherwise: Option<Box<CExpr>>,
    },
}

/// A resolved cell reference.
#[derive(Debug, Clone)]
pub struct CCell {
    /// The referenced cube's ordinal in the registry.
    pub cube: u32,
    /// One address slot per dimension of the referenced cube.
    pub addr: Vec<AddrSlot>,
}

/// How one dimension of a cell reference is addressed.
#[derive(Debug, Clone, Copy)]
pub enum AddrSlot {
    /// A fixed element index (an override).
    Pinned(u32),
    /// Copy the member from the current target coordinate at this dimension
    /// position (only valid for same-cube references).
    FromTarget(usize),
}

/// A resolved boolean condition (numeric comparisons for M4).
#[derive(Debug, Clone)]
pub enum CCond {
    /// Logical conjunction.
    And(Box<CCond>, Box<CCond>),
    /// Logical disjunction.
    Or(Box<CCond>, Box<CCond>),
    /// Logical negation.
    Not(Box<CCond>),
    /// A numeric comparison.
    Compare {
        /// Left operand.
        left: CExpr,
        /// Operator.
        op: CmpOp,
        /// Right operand.
        right: CExpr,
    },
}

/// A failure while compiling a rule set against a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileError {
    /// A referenced cube is not in the registry.
    UnknownCube { name: String, span: Span },
    /// A rule named a dimension the (target or referenced) cube lacks.
    UnknownDimension {
        cube: String,
        dimension: String,
        span: Span,
    },
    /// A rule named an element the dimension lacks.
    UnknownMember {
        dimension: String,
        member: String,
        span: Span,
    },
    /// A rule referenced an attribute the dimension does not define.
    UnknownAttribute {
        dimension: String,
        attribute: String,
        span: Span,
    },
    /// A numeric literal could not be parsed (e.g. more than four decimals).
    InvalidNumber { text: String, span: Span },
    /// A reference addressed a cube with the wrong number of dimensions.
    AddressRank {
        cube: String,
        expected: usize,
        got: usize,
        span: Span,
    },
    /// A parseable-but-deferred construct (string formula, text function,
    /// by-attribute override, cross-cube name mapping) is not supported in M4.
    Unsupported { feature: String, span: Span },
}

impl CompileError {
    /// The source span of this error.
    pub fn span(&self) -> Span {
        match self {
            CompileError::UnknownCube { span, .. }
            | CompileError::UnknownDimension { span, .. }
            | CompileError::UnknownMember { span, .. }
            | CompileError::UnknownAttribute { span, .. }
            | CompileError::InvalidNumber { span, .. }
            | CompileError::AddressRank { span, .. }
            | CompileError::Unsupported { span, .. } => *span,
        }
    }
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompileError::UnknownCube { name, .. } => write!(f, "unknown cube '{name}'"),
            CompileError::UnknownDimension {
                cube, dimension, ..
            } => write!(f, "unknown dimension '{dimension}' in cube '{cube}'"),
            CompileError::UnknownMember {
                dimension, member, ..
            } => write!(f, "unknown element '{member}' in dimension '{dimension}'"),
            CompileError::UnknownAttribute {
                dimension,
                attribute,
                ..
            } => write!(
                f,
                "unknown attribute '{attribute}' on dimension '{dimension}'"
            ),
            CompileError::InvalidNumber { text, .. } => write!(f, "invalid number '{text}'"),
            CompileError::AddressRank {
                cube,
                expected,
                got,
                ..
            } => write!(
                f,
                "reference to cube '{cube}' has {got} dimensions but the cube has {expected}"
            ),
            CompileError::Unsupported { feature, .. } => {
                write!(f, "{feature} is not supported yet")
            }
        }
    }
}

impl std::error::Error for CompileError {}
