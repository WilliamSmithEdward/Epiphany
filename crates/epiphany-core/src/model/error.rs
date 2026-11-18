//! Why a model definition was rejected.

use std::fmt;

/// The first problem found in a model definition.
///
/// `what` names the kind of thing at fault ("element", "child", "dimension", ...) and `within`
/// names where it was found ("dimension 'Account'", "cube 'General Ledger'", "the model").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelError {
    EmptyName {
        what: &'static str,
        within: String,
    },
    Duplicate {
        what: &'static str,
        within: String,
        name: String,
    },
    Unknown {
        what: &'static str,
        within: String,
        name: String,
    },
    /// A consolidated element has no children.
    NoChildren {
        dimension: String,
        element: String,
    },
    /// An element that is not consolidated has children.
    ChildrenOnLeaf {
        dimension: String,
        element: String,
    },
    /// An element is its own descendant.
    Cycle {
        dimension: String,
        element: String,
    },
    /// An attribute value does not match the attribute's declared type.
    AttributeType {
        dimension: String,
        element: String,
        attribute: String,
    },
    /// An alias is also the name or alias of a different element.
    AliasCollision {
        dimension: String,
        alias: String,
    },
    /// The MDX of a subset or a view is wrong.
    Mdx {
        within: String,
        error: String,
    },
    /// A cube has no dimensions.
    NoDimensions {
        cube: String,
    },
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyName { what, within } => write!(f, "empty {what} name in {within}"),
            Self::Duplicate { what, within, name } => {
                write!(f, "duplicate {what} '{name}' in {within}")
            }
            Self::Unknown { what, within, name } => {
                write!(f, "unknown {what} '{name}' in {within}")
            }
            Self::NoChildren { dimension, element } => write!(
                f,
                "consolidated element '{element}' in dimension '{dimension}' has no children"
            ),
            Self::ChildrenOnLeaf { dimension, element } => write!(
                f,
                "element '{element}' in dimension '{dimension}' has children but is not consolidated"
            ),
            Self::Cycle { dimension, element } => write!(
                f,
                "element '{element}' in dimension '{dimension}' is its own descendant"
            ),
            Self::AttributeType {
                dimension,
                element,
                attribute,
            } => write!(
                f,
                "attribute '{attribute}' of element '{element}' in dimension '{dimension}' has the wrong type"
            ),
            Self::AliasCollision { dimension, alias } => write!(
                f,
                "alias '{alias}' in dimension '{dimension}' is already the name or alias of another element"
            ),
            Self::Mdx { within, error } => write!(f, "invalid MDX in {within}: {error}"),
            Self::NoDimensions { cube } => write!(f, "cube '{cube}' has no dimensions"),
        }
    }
}

impl std::error::Error for ModelError {}
