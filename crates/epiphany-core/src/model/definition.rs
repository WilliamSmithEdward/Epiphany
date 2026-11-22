//! The model definition: the JSON shape of dimensions and cubes, before validation.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Scalar;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ModelDef {
    #[serde(default)]
    pub dimensions: Vec<DimensionDef>,
    #[serde(default)]
    pub cubes: Vec<CubeDef>,
    /// Pages of saved views, each shown as a chart, a figure or a grid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dashboards: Vec<DashboardDef>,
}

/// A page of tiles, each showing a view saved with a cube.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct DashboardDef {
    pub name: String,
    /// What the dashboard is for, shown beneath its name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Dimensions the page offers a choice of member for. Every tile whose cube has one
    /// stands at the member chosen, in place of the one its view names, unless the view
    /// places the dimension on an axis.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<String>,
    /// The tiles, in the order they are laid out.
    #[serde(default)]
    pub tiles: Vec<TileDef>,
}

/// One tile of a dashboard: a view of a cube, and how it is shown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct TileDef {
    /// A title of its own; the view's name unless given.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    pub cube: String,
    /// A view saved with the cube.
    pub view: String,
    #[serde(default)]
    pub show: TileShow,
    /// How many columns of the page the tile takes: one or two.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub width: u8,
}

fn one() -> u8 {
    1
}

fn is_one(width: &u8) -> bool {
    *width == 1
}

/// How a tile shows its view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum TileShow {
    /// The first cell, large, as a key figure.
    Figure,
    #[default]
    Bars,
    Stacked,
    Lines,
    Grid,
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
    /// Names for the dimension's levels, from the leaves up: the first names level zero. A
    /// query may then say `[Period].[Quarter]` for the level, unless an element has that
    /// name, and `Ytd`, `Qtd`, `Mtd` and `Wtd` look for the levels named Year, Quarter,
    /// Month and Week.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub levels: Vec<String>,
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
    /// The sizes the grid of the view was given, kept with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry: Option<Geometry>,
    /// How the view shows its numbers; as the reader's locale writes them unless given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<NumberFormat>,
    /// How the view draws the eye to some numbers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlights: Option<Highlights>,
}

/// How a view draws the eye to numbers: bars, shades, and colours by rule. Totals take no
/// bar or shade, being out of scale with what they add up.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct Highlights {
    /// A bar beneath each number, as long as its share of the largest in its column.
    #[serde(default, skip_serializing_if = "is_false")]
    pub bars: bool,
    /// Each number shaded from the lowest in its column to the highest.
    #[serde(default, skip_serializing_if = "is_false")]
    pub shades: bool,
    /// Colours for the numbers that meet a test; the first rule a number meets decides.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<HighlightRule>,
}

/// A colour for the numbers that meet a test against a value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct HighlightRule {
    pub test: Comparison,
    pub value: f64,
    pub tone: Tone,
}

/// How a number is compared with a rule's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum Comparison {
    Above,
    AtLeast,
    Below,
    AtMost,
    Equals,
}

/// What a highlight says of a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum Tone {
    Good,
    Bad,
    Warn,
    Accent,
}

/// How numbers are shown. Only the showing changes: values are read, written and copied in
/// full.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct NumberFormat {
    /// Digits after the decimal sign, always shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimals: Option<u8>,
    /// The unit numbers are shown in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<Scale>,
    /// Negative numbers in parentheses, as accounts show them, rather than after a minus.
    #[serde(default, skip_serializing_if = "is_false")]
    pub parentheses: bool,
    /// Zeros shown as a dash.
    #[serde(default, skip_serializing_if = "is_false")]
    pub dash_zeros: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

/// A unit numbers are shown in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum Scale {
    Thousands,
    Millions,
    Billions,
}

/// The sizes a grid was given by hand: the height of its rows and the widths of columns,
/// each column known by a key of the client's choosing. Sizes not given are fitted to the
/// content.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct Geometry {
    /// The height of every row, in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_height: Option<u16>,
    /// The width of each column, in pixels, by key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub widths: BTreeMap<String, u16>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum DimensionType {
    #[default]
    Standard,
    /// The dimension that holds a cube's periods, which `Ytd` and its kin run along when a
    /// query does not say which member they are to end at.
    Time,
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
    pub attributes: BTreeMap<String, Scalar>,
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
    /// Statements that calculate cells, each `[N: | C: | S:] area = expression`. The area is a
    /// member or a tuple, such as `[Account].[Margin]`; the expression is written as in MDX.
    /// The first rule that covers a cell decides it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<String>,
    /// Statements that say which calculated leaf cells may hold a value, each
    /// `source => target, ...`, such as `[Account].[Revenue] => [Account].[Margin]`. A cube
    /// that is fed consolidates only its stored and fed cells.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feeders: Vec<String>,
}
