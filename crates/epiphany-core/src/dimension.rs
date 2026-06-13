//! Dimensions, elements, and the consolidation hierarchy.

use std::collections::HashMap;

use crate::{Fixed, ModelError};

/// The kind of an element (the N/C/S typing).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElementKind {
    /// A numeric leaf (N): holds a numeric cell value and rolls up.
    Leaf,
    /// A string leaf (S): holds a text cell value and never aggregates.
    String,
    /// A consolidated element (C): computed by rolling up children.
    Consolidated,
}

impl ElementKind {
    /// Whether this element is a leaf (numeric or string), as opposed to a
    /// consolidated rollup.
    pub fn is_leaf(self) -> bool {
        matches!(self, ElementKind::Leaf | ElementKind::String)
    }
}

/// An element of a dimension.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Element {
    pub name: String,
    pub kind: ElementKind,
}

/// The kind of a dimension attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttributeKind {
    /// Free text.
    Text,
    /// An exact numeric value.
    Numeric,
    /// An alternate display name that also resolves to its element.
    Alias,
}

/// A value attached to an element via an attribute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttributeValue {
    Text(String),
    Numeric(Fixed),
}

/// An attribute definition: a named, typed column over a dimension's elements.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttributeDef {
    pub name: String,
    pub kind: AttributeKind,
}

/// A weighted parent->child consolidation edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Edge {
    child: u32,
    weight: i64,
}

/// A dimension: an ordered list of elements and their consolidation edges.
///
/// Supports alternate rollups: a child may roll up into more than one parent,
/// and a query element's leaf contributions accumulate (with weights) across
/// every path that reaches a given leaf.
#[derive(Clone, Debug)]
pub struct Dimension {
    name: String,
    elements: Vec<Element>,
    index_by_name: HashMap<String, u32>,
    /// `children[parent]` = the parent's weighted child edges.
    children: Vec<Vec<Edge>>,
    /// Attribute definitions, in declaration order.
    attributes: Vec<AttributeDef>,
    attr_index_by_name: HashMap<String, u32>,
    /// `attr_values[element]` maps an attribute index to that element's value.
    attr_values: Vec<HashMap<u32, AttributeValue>>,
    /// Reverse lookup from an alias value to its element index.
    alias_to_element: HashMap<String, u32>,
}

impl Dimension {
    /// Create an empty dimension.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            elements: Vec::new(),
            index_by_name: HashMap::new(),
            children: Vec::new(),
            attributes: Vec::new(),
            attr_index_by_name: HashMap::new(),
            attr_values: Vec::new(),
            alias_to_element: HashMap::new(),
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

    /// Iterate the dimension's elements in definition order.
    pub fn iter_elements(&self) -> impl Iterator<Item = &Element> + '_ {
        self.elements.iter()
    }

    /// All consolidation edges as `(parent, child, weight)`, sorted canonically
    /// by `(parent, child)` for deterministic, diff-friendly output.
    pub fn edges(&self) -> Vec<(u32, u32, i64)> {
        let mut out = Vec::new();
        for (parent, edges) in self.children.iter().enumerate() {
            for edge in edges {
                out.push((parent as u32, edge.child, edge.weight));
            }
        }
        out.sort_by_key(|&(parent, child, _)| (parent, child));
        out
    }

    /// Define an attribute (idempotent by name; returns its index).
    pub fn add_attribute(&mut self, name: impl Into<String>, kind: AttributeKind) -> u32 {
        let name = name.into();
        if let Some(&existing) = self.attr_index_by_name.get(&name) {
            return existing;
        }
        let index = self.attributes.len() as u32;
        self.attr_index_by_name.insert(name.clone(), index);
        self.attributes.push(AttributeDef { name, kind });
        index
    }

    /// The attribute definitions, in declaration order.
    pub fn attribute_defs(&self) -> &[AttributeDef] {
        &self.attributes
    }

    /// Find an attribute's index by name.
    pub fn attribute_index(&self, name: &str) -> Option<u32> {
        self.attr_index_by_name.get(name).copied()
    }

    /// Set an element's value for a defined attribute.
    ///
    /// The value type must match the attribute kind (Numeric takes a numeric
    /// value; Text and Alias take text). An Alias value also becomes resolvable
    /// via `resolve`, and must be unique within the dimension.
    pub fn set_attribute(
        &mut self,
        element: u32,
        attribute: &str,
        value: AttributeValue,
    ) -> Result<(), ModelError> {
        self.element(element)?;
        let attr_index =
            self.attribute_index(attribute)
                .ok_or_else(|| ModelError::AttributeNotFound {
                    dimension: self.name.clone(),
                    attribute: attribute.to_string(),
                })?;
        let kind = self.attributes[attr_index as usize].kind;
        let type_ok = matches!(
            (kind, &value),
            (AttributeKind::Numeric, AttributeValue::Numeric(_))
                | (AttributeKind::Text, AttributeValue::Text(_))
                | (AttributeKind::Alias, AttributeValue::Text(_))
        );
        if !type_ok {
            return Err(ModelError::AttributeTypeMismatch {
                dimension: self.name.clone(),
                attribute: attribute.to_string(),
            });
        }
        if kind == AttributeKind::Alias {
            if let AttributeValue::Text(alias) = &value {
                if let Some(&owner) = self.alias_to_element.get(alias) {
                    if owner != element {
                        return Err(ModelError::AliasConflict {
                            dimension: self.name.clone(),
                            alias: alias.clone(),
                        });
                    }
                }
                self.alias_to_element.insert(alias.clone(), element);
            }
        }
        self.attr_values[element as usize].insert(attr_index, value);
        Ok(())
    }

    /// Read an element's value for an attribute, if set.
    pub fn attribute(&self, element: u32, attribute: &str) -> Option<&AttributeValue> {
        let attr_index = self.attribute_index(attribute)?;
        self.attr_values
            .get(element as usize)
            .and_then(|values| values.get(&attr_index))
    }

    /// All set attribute values as `(element, attribute, value)`, sorted
    /// canonically by `(element, attribute)` for deterministic output.
    pub fn attribute_values(&self) -> Vec<(u32, u32, AttributeValue)> {
        let mut out = Vec::new();
        for (element, values) in self.attr_values.iter().enumerate() {
            for (&attr_index, value) in values {
                out.push((element as u32, attr_index, value.clone()));
            }
        }
        out.sort_by_key(|&(element, attr_index, _)| (element, attr_index));
        out
    }

    /// Resolve a name to an element index, by element name first, then by alias.
    pub fn resolve(&self, name: &str) -> Option<u32> {
        self.index_of(name)
            .or_else(|| self.alias_to_element.get(name).copied())
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
        self.attr_values.push(HashMap::new());
        index
    }

    /// Add a numeric leaf element (or return the existing index for that name).
    pub fn add_leaf(&mut self, name: impl Into<String>) -> u32 {
        self.add_element(name, ElementKind::Leaf)
    }

    /// Add a string leaf element (or return the existing index for that name).
    pub fn add_string(&mut self, name: impl Into<String>) -> u32 {
        self.add_element(name, ElementKind::String)
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
            // String leaves hold text, not numbers, so they never contribute to
            // a numeric rollup.
            ElementKind::String => {}
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
        // Total = A + B ; Big = Total + A  ->  A contributes weight 2.
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

    #[test]
    fn attributes_store_and_read() {
        let mut d = Dimension::new("Region");
        let north = d.add_leaf("North");
        d.add_attribute("Code", AttributeKind::Text);
        d.add_attribute("Population", AttributeKind::Numeric);
        d.set_attribute(north, "Code", AttributeValue::Text("N".into()))
            .unwrap();
        d.set_attribute(
            north,
            "Population",
            AttributeValue::Numeric(Fixed::from(1000)),
        )
        .unwrap();
        assert_eq!(
            d.attribute(north, "Code"),
            Some(&AttributeValue::Text("N".into()))
        );
        assert_eq!(
            d.attribute(north, "Population"),
            Some(&AttributeValue::Numeric(Fixed::from(1000)))
        );
        assert_eq!(d.attribute(north, "Missing"), None);
    }

    #[test]
    fn alias_resolves_to_element() {
        let mut d = Dimension::new("Region");
        let na = d.add_leaf("NA");
        d.add_attribute("Alias", AttributeKind::Alias);
        d.set_attribute(na, "Alias", AttributeValue::Text("North America".into()))
            .unwrap();
        assert_eq!(d.resolve("NA"), Some(na));
        assert_eq!(d.resolve("North America"), Some(na));
        assert_eq!(d.resolve("Nowhere"), None);
    }

    #[test]
    fn attribute_type_mismatch_is_rejected() {
        let mut d = Dimension::new("D");
        let a = d.add_leaf("A");
        d.add_attribute("Population", AttributeKind::Numeric);
        assert!(matches!(
            d.set_attribute(a, "Population", AttributeValue::Text("x".into())),
            Err(ModelError::AttributeTypeMismatch { .. })
        ));
    }

    #[test]
    fn duplicate_alias_is_rejected() {
        let mut d = Dimension::new("D");
        let a = d.add_leaf("A");
        let b = d.add_leaf("B");
        d.add_attribute("Alias", AttributeKind::Alias);
        d.set_attribute(a, "Alias", AttributeValue::Text("X".into()))
            .unwrap();
        assert!(matches!(
            d.set_attribute(b, "Alias", AttributeValue::Text("X".into())),
            Err(ModelError::AliasConflict { .. })
        ));
    }

    #[test]
    fn string_leaf_is_a_leaf_but_does_not_aggregate() {
        let mut d = Dimension::new("Measure");
        let sales = d.add_leaf("Sales");
        let comment = d.add_string("Comment");
        let total = d.add_consolidated("Total");
        d.add_child(total, sales, 1).unwrap();
        d.add_child(total, comment, 1).unwrap(); // allowed; contributes nothing
        assert_eq!(d.element(comment).unwrap().kind, ElementKind::String);
        assert!(d.element(comment).unwrap().kind.is_leaf());
        // Total expands only to the numeric leaf.
        assert_eq!(d.leaf_weights(total).unwrap(), vec![(sales, 1)]);
    }
}
