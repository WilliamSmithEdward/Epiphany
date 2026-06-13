//! Dimensions, elements, and the consolidation hierarchy.

use std::collections::HashMap;

use crate::ModelError;

/// The kind of an element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElementKind {
    /// A leaf element that can hold data.
    Leaf,
    /// A consolidated element computed by rolling up children.
    Consolidated,
}

/// An element of a dimension.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Element {
    pub name: String,
    pub kind: ElementKind,
}

/// A weighted parent→child consolidation edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Edge {
    child: u32,
    weight: i64,
}

/// A dimension: an ordered list of elements and their consolidation edges.
///
/// Supports alternate rollups — a child may roll up into more than one parent,
/// and a query element's leaf contributions accumulate (with weights) across
/// every path that reaches a given leaf.
#[derive(Clone, Debug)]
pub struct Dimension {
    name: String,
    elements: Vec<Element>,
    index_by_name: HashMap<String, u32>,
    /// `children[parent]` = the parent's weighted child edges.
    children: Vec<Vec<Edge>>,
}

impl Dimension {
    /// Create an empty dimension.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            elements: Vec::new(),
            index_by_name: HashMap::new(),
            children: Vec::new(),
        }
    }

    /// The dimension name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Number of elements.
    pub fn len(&self) -> u32 {
        self.elements.len() as u32
    }

    /// `true` if the dimension has no elements.
    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }

    /// Look up an element by index.
    pub fn element(&self, index: u32) -> Result<&Element, ModelError> {
        self.elements
            .get(index as usize)
            .ok_or(ModelError::ElementIndexOutOfRange {
                dimension: self.name.clone(),
                index,
                len: self.len(),
            })
    }

    /// Find an element's index by name.
    pub fn index_of(&self, name: &str) -> Option<u32> {
        self.index_by_name.get(name).copied()
    }

    fn add_element(&mut self, name: impl Into<String>, kind: ElementKind) -> u32 {
        let name = name.into();
        if let Some(&existing) = self.index_by_name.get(&name) {
            return existing; // idempotent: re-adding a name returns its index
        }
        let index = self.elements.len() as u32;
        self.index_by_name.insert(name.clone(), index);
        self.elements.push(Element { name, kind });
        self.children.push(Vec::new());
        index
    }

    /// Add a leaf element (or return the existing index for that name).
    pub fn add_leaf(&mut self, name: impl Into<String>) -> u32 {
        self.add_element(name, ElementKind::Leaf)
    }

    /// Add a consolidated element (or return the existing index for that name).
    pub fn add_consolidated(&mut self, name: impl Into<String>) -> u32 {
        self.add_element(name, ElementKind::Consolidated)
    }

    /// Add a weighted child edge under a consolidated parent.
    ///
    /// Rejects out-of-range indices, non-consolidated parents, and edges that
    /// would introduce a cycle (the hierarchy is kept a DAG).
    pub fn add_child(&mut self, parent: u32, child: u32, weight: i64) -> Result<(), ModelError> {
        self.element(child)?;
        let parent_kind = self.element(parent)?.kind;
        if parent_kind != ElementKind::Consolidated {
            return Err(ModelError::ParentNotConsolidated {
                dimension: self.name.clone(),
                element: self.elements[parent as usize].name.clone(),
            });
        }
        if child == parent || self.reaches(child, parent) {
            return Err(ModelError::CycleDetected {
                dimension: self.name.clone(),
                parent: self.elements[parent as usize].name.clone(),
                child: self.elements[child as usize].name.clone(),
            });
        }
        self.children[parent as usize].push(Edge { child, weight });
        Ok(())
    }

    /// Does `from` reach `to` by following child edges?
    fn reaches(&self, from: u32, to: u32) -> bool {
        let mut stack = vec![from];
        let mut seen = vec![false; self.elements.len()];
        while let Some(node) = stack.pop() {
            if node == to {
                return true;
            }
            if seen[node as usize] {
                continue;
            }
            seen[node as usize] = true;
            for edge in &self.children[node as usize] {
                stack.push(edge.child);
            }
        }
        false
    }

    /// Expand an element into its leaf descendants with accumulated weights.
    ///
    /// A leaf expands to itself with weight 1. The result is sorted by leaf
    /// index (deterministic) and excludes leaves whose net weight is zero.
    pub fn leaf_weights(&self, element: u32) -> Result<Vec<(u32, i64)>, ModelError> {
        self.element(element)?;
        let mut acc: HashMap<u32, i64> = HashMap::new();
        self.accumulate_leaves(element, 1, &mut acc);
        let mut out: Vec<(u32, i64)> = acc.into_iter().filter(|&(_, w)| w != 0).collect();
        out.sort_by_key(|&(leaf, _)| leaf);
        Ok(out)
    }

    fn accumulate_leaves(&self, element: u32, weight: i64, acc: &mut HashMap<u32, i64>) {
        match self.elements[element as usize].kind {
            ElementKind::Leaf => {
                *acc.entry(element).or_insert(0) += weight;
            }
            ElementKind::Consolidated => {
                for edge in &self.children[element as usize] {
                    let path_weight = weight.saturating_mul(edge.weight);
                    self.accumulate_leaves(edge.child, path_weight, acc);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_expands_to_itself() {
        let mut d = Dimension::new("D");
        let a = d.add_leaf("A");
        assert_eq!(d.leaf_weights(a).unwrap(), vec![(a, 1)]);
    }

    #[test]
    fn consolidated_expands_to_weighted_leaves() {
        let mut d = Dimension::new("Version");
        let actual = d.add_leaf("Actual");
        let budget = d.add_leaf("Budget");
        let variance = d.add_consolidated("Variance");
        d.add_child(variance, actual, 1).unwrap();
        d.add_child(variance, budget, -1).unwrap();
        assert_eq!(
            d.leaf_weights(variance).unwrap(),
            vec![(actual, 1), (budget, -1)]
        );
    }

    #[test]
    fn alternate_paths_accumulate_weight() {
        // Total = A + B ; Big = Total + A  →  A contributes weight 2.
        let mut d = Dimension::new("D");
        let a = d.add_leaf("A");
        let b = d.add_leaf("B");
        let total = d.add_consolidated("Total");
        let big = d.add_consolidated("Big");
        d.add_child(total, a, 1).unwrap();
        d.add_child(total, b, 1).unwrap();
        d.add_child(big, total, 1).unwrap();
        d.add_child(big, a, 1).unwrap();
        assert_eq!(d.leaf_weights(big).unwrap(), vec![(a, 2), (b, 1)]);
    }

    #[test]
    fn non_consolidated_parent_is_rejected() {
        let mut d = Dimension::new("D");
        let a = d.add_leaf("A");
        let b = d.add_leaf("B");
        assert!(matches!(
            d.add_child(a, b, 1).unwrap_err(),
            ModelError::ParentNotConsolidated { .. }
        ));
    }
}
