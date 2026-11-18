//! The model definition: the JSON shape of dimensions and cubes, before validation.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Value;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ModelDef {
    #[serde(default)]
    pub dimensions: Vec<DimensionDef>,
    #[serde(default)]
    pub cubes: Vec<CubeDef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct DimensionDef {
    pub name: String,
    #[serde(rename = "type", default)]
    pub kind: DimensionType,
    pub elements: Vec<ElementDef>,
    /// Declared attributes and their types. Elements may only carry attributes declared here.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, AttributeType>,
    /// Reusable selections of this dimension's elements, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub subsets: BTreeMap<String, SubsetDef>,
}

/// A selection of a dimension's elements.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum SubsetDef {
    /// The elements, by name or alias, in the order given.
    Elements(Vec<String>),
    /// An MDX set over the dimension, such as `TM1FilterByLevel([Period].Members, 0)`. It is
    /// worked out from the dimension alone, so it cannot depend on cell values.
    Mdx(String),
}

/// A saved query against a cube.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ViewDef {
    /// The MDX query, which must select from the cube the view belongs to.
    pub mdx: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum DimensionType {
    #[default]
    Standard,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ElementDef {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: ElementType,
    /// Weighted children, in order. Present on consolidated elements only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ChildDef>,
    /// Attribute values, keyed by attribute name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum ElementType {
    Numeric,
    #[serde(rename = "string")]
    Text,
    Consolidated,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ChildDef {
    pub name: String,
    #[serde(default = "unit_weight")]
    pub weight: f64,
}

fn unit_weight() -> f64 {
    1.0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum AttributeType {
    #[serde(rename = "string")]
    Text,
    Numeric,
    /// A text attribute whose values are alternative names for their elements.
    Alias,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct CubeDef {
    pub name: String,
    pub dimensions: Vec<String>,
    /// Saved queries against this cube, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub views: BTreeMap<String, ViewDef>,
}
