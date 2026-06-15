//! Error types for the core model.

use std::fmt;

/// Errors from building or querying the multidimensional model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelError {
    /// An element name was not found in the dimension.
    ElementNotFound { dimension: String, element: String },
    /// An element index was out of range for the dimension.
    ElementIndexOutOfRange {
        dimension: String,
        index: u32,
        len: u32,
    },
    /// A coordinate had the wrong number of components for the cube.
    RankMismatch { expected: usize, got: usize },
    /// Attempted to write a cell whose coordinate includes a non-leaf element.
    WriteToNonLeaf { dimension: String, element: String },
    /// Adding a consolidation edge would create a cycle.
    CycleDetected {
        dimension: String,
        parent: String,
        child: String,
    },
    /// A consolidation edge referenced a parent that is not a consolidated element.
    ParentNotConsolidated { dimension: String, element: String },
    /// Fixed-point arithmetic overflowed the representable range.
    Overflow,
    /// A cube must have at least one dimension.
    EmptyCube,
    /// A numeric value could not be parsed from text.
    InvalidNumber { text: String },
    /// An attribute name was not defined on the dimension.
    AttributeNotFound {
        dimension: String,
        attribute: String,
    },
    /// An attribute value did not match the attribute's declared kind.
    AttributeTypeMismatch {
        dimension: String,
        attribute: String,
    },
    /// An attribute was re-declared with a different kind.
    AttributeKindConflict {
        dimension: String,
        attribute: String,
    },
    /// Two elements were given the same alias within one dimension.
    AliasConflict { dimension: String, alias: String },
    /// Attempted to write a numeric value to a string-typed leaf.
    CellTypeMismatch { dimension: String, element: String },
    /// Attempted to write a string value to a coordinate with no string element.
    StringCellRequiresStringElement { cube: String },
    /// A schema change named a dimension the cube does not have.
    UnknownDimension { cube: String, dimension: String },
    /// A new cube declared the same dimension name more than once.
    DuplicateDimension { dimension: String },
    /// A schema change re-declared an existing element with a different kind.
    ElementKindConflict { dimension: String, element: String },
    /// A schema change re-declared an existing edge with a different weight.
    EdgeWeightConflict {
        dimension: String,
        parent: String,
        child: String,
    },
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModelError::ElementNotFound { dimension, element } => {
                write!(f, "element '{element}' not found in dimension '{dimension}'")
            }
            ModelError::ElementIndexOutOfRange {
                dimension,
                index,
                len,
            } => write!(
                f,
                "element index {index} out of range for dimension '{dimension}' (len {len})"
            ),
            ModelError::RankMismatch { expected, got } => write!(
                f,
                "coordinate has {got} components but the cube has {expected} dimensions"
            ),
            ModelError::WriteToNonLeaf { dimension, element } => write!(
                f,
                "cannot write to non-leaf element '{element}' in dimension '{dimension}'"
            ),
            ModelError::CycleDetected {
                dimension,
                parent,
                child,
            } => write!(
                f,
                "adding '{child}' under '{parent}' would create a cycle in dimension '{dimension}'"
            ),
            ModelError::ParentNotConsolidated { dimension, element } => write!(
                f,
                "element '{element}' in dimension '{dimension}' is not consolidated and cannot have children"
            ),
            ModelError::Overflow => write!(f, "fixed-point arithmetic overflow"),
            ModelError::EmptyCube => write!(f, "a cube must have at least one dimension"),
            ModelError::InvalidNumber { text } => write!(f, "invalid number: '{text}'"),
            ModelError::AttributeNotFound {
                dimension,
                attribute,
            } => write!(
                f,
                "attribute '{attribute}' is not defined on dimension '{dimension}'"
            ),
            ModelError::AttributeTypeMismatch {
                dimension,
                attribute,
            } => write!(
                f,
                "value for attribute '{attribute}' on dimension '{dimension}' does not match its kind"
            ),
            ModelError::AttributeKindConflict {
                dimension,
                attribute,
            } => write!(
                f,
                "attribute '{attribute}' already exists on dimension '{dimension}' with a different kind"
            ),
            ModelError::AliasConflict { dimension, alias } => write!(
                f,
                "alias '{alias}' is assigned to more than one element in dimension '{dimension}'"
            ),
            ModelError::CellTypeMismatch { dimension, element } => write!(
                f,
                "cannot write a numeric value to string-typed element '{element}' in dimension '{dimension}'"
            ),
            ModelError::StringCellRequiresStringElement { cube } => write!(
                f,
                "a string cell in cube '{cube}' must address a string element"
            ),
            ModelError::UnknownDimension { cube, dimension } => write!(
                f,
                "cube '{cube}' has no dimension '{dimension}'"
            ),
            ModelError::DuplicateDimension { dimension } => write!(
                f,
                "dimension '{dimension}' is declared more than once"
            ),
            ModelError::ElementKindConflict { dimension, element } => write!(
                f,
                "element '{element}' already exists in dimension '{dimension}' with a different kind"
            ),
            ModelError::EdgeWeightConflict {
                dimension,
                parent,
                child,
            } => write!(
                f,
                "edge '{parent}' -> '{child}' already exists in dimension '{dimension}' with a different weight"
            ),
        }
    }
}

impl std::error::Error for ModelError {}
