//! The query model seam: subsets and the set-evaluator trait.
//!
//! This module defines named selections ([`Subset`]) over a dimension and the
//! [`SetEvaluator`] trait by which dynamic (MDX) subsets are resolved. The trait
//! is *owned by core but implemented elsewhere* (epiphany-mdx), so core carries
//! no MDX dependency: a static subset resolves with no evaluator at all, and a
//! dynamic one delegates through the injected trait object. This mirrors how the
//! determinism `Clock`/`IdGen` are core-defined and injected at the edges.
//!
//! Views and cellsets build on this in Phase 3D.

use std::collections::HashSet;
use std::fmt;

use crate::{Cube, Dimension, ModelError};

/// Whether a saved object is shared with everyone or kept to its owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    /// Visible to all users.
    Public,
    /// Visible only to its owner (and administrators).
    Private,
}

impl Visibility {
    /// `true` for [`Visibility::Public`].
    pub fn is_public(self) -> bool {
        matches!(self, Visibility::Public)
    }
}

/// How a subset's members are determined.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubsetKind {
    /// An explicit, ordered list of member names (resolved at execution).
    Static {
        /// Member names in author order.
        members: Vec<String>,
    },
    /// An MDX set expression, evaluated at execution against the live dimension.
    Dynamic {
        /// The MDX set expression source.
        mdx: String,
    },
}

/// A named, ordered selection of elements from a single dimension.
///
/// Members are stored by name (not index) so a subset survives structural
/// change and round-trips through canonical text. Resolution to indices happens
/// at execution via [`resolve_subset`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subset {
    /// The subset name (unique within its dimension scope).
    pub name: String,
    /// The dimension this subset selects from.
    pub dimension: String,
    /// The owning user, if any (for visibility enforcement at the API layer).
    pub owner: Option<String>,
    /// Whether the subset is shared or private.
    pub visibility: Visibility,
    /// Static member list or dynamic MDX.
    pub kind: SubsetKind,
}

/// The seam by which dynamic (MDX) subsets are resolved.
///
/// Implemented in epiphany-mdx (`MdxEvaluator`) and injected at the composition
/// root, so epiphany-core never depends on the MDX crate. Evaluation reads the
/// cube/dimension immutably and resolves against the live (snapshot) dimension.
pub trait SetEvaluator {
    /// Evaluate an MDX set expression over `dim` (within `cube`) to an ordered,
    /// de-duplicated list of element indices.
    fn eval_set(&self, cube: &Cube, dim: &Dimension, mdx: &str) -> Result<Vec<u32>, QueryError>;
}

/// An evaluator that rejects dynamic subsets, letting the core query model and
/// its tests build and run with zero MDX dependency.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoSetEvaluator;

impl SetEvaluator for NoSetEvaluator {
    fn eval_set(&self, _cube: &Cube, _dim: &Dimension, _mdx: &str) -> Result<Vec<u32>, QueryError> {
        Err(QueryError::DynamicUnsupported)
    }
}

/// Errors from resolving subsets and executing views.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    /// A view referenced a subset that does not exist.
    UnknownSubset {
        /// The missing subset name.
        name: String,
    },
    /// A subset or axis named a dimension the cube does not have.
    UnknownDimension {
        /// The cube being queried.
        cube: String,
        /// The missing dimension name.
        dimension: String,
    },
    /// A static subset listed a member that does not resolve in its dimension.
    UnknownMember {
        /// The dimension being resolved.
        dimension: String,
        /// The unresolved member name.
        member: String,
    },
    /// A view's axes did not cover the cube's dimensions exactly once each.
    DimensionCoverage {
        /// A human-readable explanation.
        detail: String,
    },
    /// A dynamic subset was used with an evaluator that does not support MDX.
    DynamicUnsupported,
    /// A dynamic subset failed to parse or evaluate (message from the MDX layer).
    DynamicEval {
        /// The MDX parse/eval failure rendered as text.
        message: String,
    },
    /// An underlying core model error.
    Model(ModelError),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryError::UnknownSubset { name } => write!(f, "subset '{name}' not found"),
            QueryError::UnknownDimension { cube, dimension } => {
                write!(f, "dimension '{dimension}' not found in cube '{cube}'")
            }
            QueryError::UnknownMember { dimension, member } => {
                write!(f, "member '{member}' not found in dimension '{dimension}'")
            }
            QueryError::DimensionCoverage { detail } => write!(f, "{detail}"),
            QueryError::DynamicUnsupported => {
                write!(
                    f,
                    "dynamic (MDX) subsets are not supported by this evaluator"
                )
            }
            QueryError::DynamicEval { message } => write!(f, "{message}"),
            QueryError::Model(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for QueryError {}

impl From<ModelError> for QueryError {
    fn from(e: ModelError) -> Self {
        QueryError::Model(e)
    }
}

/// Find a dimension by name within a cube.
pub(crate) fn dimension_by_name<'a>(
    cube: &'a Cube,
    name: &str,
) -> Result<&'a Dimension, QueryError> {
    cube.dimensions()
        .iter()
        .find(|d| d.name() == name)
        .ok_or_else(|| QueryError::UnknownDimension {
            cube: cube.name().to_string(),
            dimension: name.to_string(),
        })
}

/// Resolve a subset to an ordered, de-duplicated list of element indices.
///
/// Static subsets resolve member names in author order (first occurrence wins);
/// dynamic subsets delegate to `eval`. The result is deterministic.
pub fn resolve_subset(
    cube: &Cube,
    subset: &Subset,
    eval: &dyn SetEvaluator,
) -> Result<Vec<u32>, QueryError> {
    let dim = dimension_by_name(cube, &subset.dimension)?;
    match &subset.kind {
        SubsetKind::Static { members } => {
            let mut out = Vec::new();
            let mut seen = HashSet::new();
            for name in members {
                let idx = dim.resolve(name).ok_or_else(|| QueryError::UnknownMember {
                    dimension: dim.name().to_string(),
                    member: name.clone(),
                })?;
                if seen.insert(idx) {
                    out.push(idx);
                }
            }
            Ok(out)
        }
        SubsetKind::Dynamic { mdx } => eval.eval_set(cube, dim, mdx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Dimension;

    fn region_cube() -> Cube {
        let mut region = Dimension::new("Region");
        let north = region.add_leaf("North");
        let south = region.add_leaf("South");
        let total = region.add_consolidated("Total");
        region.add_child(total, north, 1).unwrap();
        region.add_child(total, south, 1).unwrap();
        Cube::new("Sales", vec![region]).unwrap()
    }

    fn static_subset(members: &[&str]) -> Subset {
        Subset {
            name: "S".into(),
            dimension: "Region".into(),
            owner: None,
            visibility: Visibility::Public,
            kind: SubsetKind::Static {
                members: members.iter().map(|s| s.to_string()).collect(),
            },
        }
    }

    #[test]
    fn static_subset_preserves_author_order_and_dedups() {
        let cube = region_cube();
        let subset = static_subset(&["South", "North", "South", "Total"]);
        let resolved = resolve_subset(&cube, &subset, &NoSetEvaluator).unwrap();
        // Author order, first occurrence wins (South=1, North=0, Total=2).
        assert_eq!(resolved, vec![1, 0, 2]);
    }

    #[test]
    fn dynamic_subset_is_rejected_without_an_evaluator() {
        let cube = region_cube();
        let subset = Subset {
            name: "D".into(),
            dimension: "Region".into(),
            owner: None,
            visibility: Visibility::Public,
            kind: SubsetKind::Dynamic {
                mdx: "[Region].Members".into(),
            },
        };
        assert_eq!(
            resolve_subset(&cube, &subset, &NoSetEvaluator),
            Err(QueryError::DynamicUnsupported)
        );
    }

    #[test]
    fn unknown_dimension_is_reported() {
        let cube = region_cube();
        let subset = Subset {
            name: "S".into(),
            dimension: "Nope".into(),
            owner: None,
            visibility: Visibility::Public,
            kind: SubsetKind::Static {
                members: vec!["North".into()],
            },
        };
        assert!(matches!(
            resolve_subset(&cube, &subset, &NoSetEvaluator),
            Err(QueryError::UnknownDimension { .. })
        ));
    }

    #[test]
    fn unknown_member_is_reported() {
        let cube = region_cube();
        let subset = static_subset(&["North", "Atlantis"]);
        assert_eq!(
            resolve_subset(&cube, &subset, &NoSetEvaluator),
            Err(QueryError::UnknownMember {
                dimension: "Region".into(),
                member: "Atlantis".into()
            })
        );
    }

    #[test]
    fn resolution_is_deterministic() {
        let cube = region_cube();
        let subset = static_subset(&["Total", "North", "South"]);
        let a = resolve_subset(&cube, &subset, &NoSetEvaluator).unwrap();
        let b = resolve_subset(&cube, &subset, &NoSetEvaluator).unwrap();
        assert_eq!(a, b);
    }
}
