//! The MDX implementation of core's [`SetEvaluator`] seam.
//!
//! [`MdxEvaluator`] bridges this crate's pure [`parse`](crate::parse) +
//! [`evaluate`](crate::evaluate) front end to the `epiphany_core::SetEvaluator`
//! trait. The server constructs one and injects it wherever dynamic subsets must
//! resolve; core itself never names this type, keeping the dependency edge
//! one-way (mdx -> core).
//!
//! Parse and evaluation failures are flattened into
//! `QueryError::DynamicEval { message }`; callers that need the precise parse
//! span (the editor preview path) call [`parse`](crate::parse) directly instead.

use epiphany_core::{Cube, Dimension, QueryError, SetEvaluator};

/// Resolves dynamic (MDX) subsets via the crate's parser and evaluator.
#[derive(Clone, Copy, Debug, Default)]
pub struct MdxEvaluator;

impl MdxEvaluator {
    /// Construct an evaluator.
    pub fn new() -> Self {
        MdxEvaluator
    }
}

impl SetEvaluator for MdxEvaluator {
    fn eval_set(&self, _cube: &Cube, dim: &Dimension, mdx: &str) -> Result<Vec<u32>, QueryError> {
        let expr = crate::parse(mdx).map_err(|e| QueryError::DynamicEval {
            message: e.to_string(),
        })?;
        crate::evaluate(&expr, dim).map_err(|e| QueryError::DynamicEval {
            message: e.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epiphany_core::{resolve_subset, Cube, Dimension, Subset, SubsetKind, Visibility};

    fn region_cube() -> Cube {
        let mut region = Dimension::new("Region");
        let north = region.add_leaf("North");
        let south = region.add_leaf("South");
        let total = region.add_consolidated("Total");
        region.add_child(total, north, 1).unwrap();
        region.add_child(total, south, 1).unwrap();
        Cube::new("Sales", vec![region]).unwrap()
    }

    fn subset(kind: SubsetKind) -> Subset {
        Subset {
            name: "S".into(),
            dimension: "Region".into(),
            owner: None,
            visibility: Visibility::Public,
            kind,
        }
    }

    #[test]
    fn dynamic_subset_matches_the_equivalent_static_list() {
        let cube = region_cube();
        let dynamic = subset(SubsetKind::Dynamic {
            mdx: "[Region].[Total].Children".into(),
        });
        let static_list = subset(SubsetKind::Static {
            members: vec!["North".into(), "South".into()],
        });
        let from_mdx = resolve_subset(&cube, &dynamic, &MdxEvaluator).unwrap();
        let from_static = resolve_subset(&cube, &static_list, &MdxEvaluator).unwrap();
        assert_eq!(from_mdx, from_static);
        assert_eq!(from_mdx, vec![0, 1]);
    }

    #[test]
    fn a_parse_error_becomes_a_dynamic_eval_error() {
        let cube = region_cube();
        let bad = subset(SubsetKind::Dynamic {
            mdx: "{[Region].[North]".into(),
        });
        assert!(matches!(
            resolve_subset(&cube, &bad, &MdxEvaluator),
            Err(QueryError::DynamicEval { .. })
        ));
    }
}
