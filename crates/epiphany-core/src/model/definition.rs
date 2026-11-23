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
    /// How the tiles are laid out.
    #[serde(default, skip_serializing_if = "DashboardLayout::is_grid")]
    pub layout: DashboardLayout,
    /// The tiles, in the order they are laid out; in a free layout, each over those before.
    #[serde(default)]
    pub tiles: Vec<TileDef>,
}

/// How a dashboard lays its tiles out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum DashboardLayout {
    /// On whole columns and rows, clear of one another, each as high up as there is room.
    #[default]
    Grid,
    /// Each where it was put, over another if it was put there.
    Free,
}

impl DashboardLayout {
    fn is_grid(&self) -> bool {
        *self == Self::Grid
    }
}

/// How many columns a dashboard's page is across.
pub const DASHBOARD_COLUMNS: f64 = 12.0;

/// One tile of a dashboard: a view of a cube and how it is shown, or a few lines of text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct TileDef {
    /// A title of its own; the view's name unless given.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// The cube of the view shown; none for a tile of text.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cube: String,
    /// A view saved with the cube.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub view: String,
    #[serde(default)]
    pub show: TileShow,
    /// Where the tile stands and how large it is. Without one it takes the next place
    /// there is, as wide as `width` says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place: Option<TilePlace>,
    /// How many of two columns the tile takes where it has no `place`: one or two.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub width: u8,
    /// A chart's series are the view's columns, rather than its rows.
    #[serde(default, skip_serializing_if = "is_false")]
    pub by_columns: bool,
    /// A chart writes each value on its mark.
    #[serde(default, skip_serializing_if = "is_false")]
    pub labels: bool,
    /// What a tile of text says: a line that begins `# ` is a heading, one that begins `- `
    /// an item of a list, and an empty line ends a paragraph.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
}

fn one() -> u8 {
    1
}

fn is_one(width: &u8) -> bool {
    *width == 1
}

/// Where a tile stands on its dashboard's page: in columns from the left, of the twelve
/// across, and in rows from the top, each of a fixed height. A free layout may place a tile
/// between them.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct TilePlace {
    pub x: f64,
    pub y: f64,
    /// Columns wide.
    pub w: f64,
    /// Rows high.
    pub h: f64,
}

impl TilePlace {
    /// Whether the tile is on the page: from its left edge to its right, and at or below
    /// its top, some width and height.
    pub fn on_page(&self) -> bool {
        [self.x, self.y, self.w, self.h]
            .iter()
            .all(|n| n.is_finite())
            && self.x >= 0.0
            && self.y >= 0.0
            && self.w > 0.0
            && self.h > 0.0
            && self.x + self.w <= DASHBOARD_COLUMNS + 1e-9
    }
}

/// How a tile shows its view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum TileShow {
    /// The first cell, large, as a key figure, with its change from the cell before it.
    Figure,
    #[default]
    Bars,
    Stacked,
    Lines,
    /// Lines with the ground beneath them filled.
    Area,
    /// Bars across, the largest first.
    Ranked,
    /// A ring of each value's share of their whole.
    Donut,
    Grid,
    /// The cells as squares, shaded from the lowest value to the highest.
    Heatmap,
    /// The tile's own text, no view.
    Text,
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
    /// For a string element of a cube's last dimension, the values its text cells may take:
    /// offered as a list where they are written, and any other refused. Text stored before
    /// is kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picklist: Option<PicklistDef>,
}

/// The values a text cell may take: given one by one, or the elements of a dimension, or of
/// one of its subsets, in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(untagged)]
pub enum PicklistDef {
    Values(Vec<String>),
    Members(MembersDef),
}

/// The elements of a dimension, or of one of its subsets, as a picklist offers them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct MembersDef {
    pub dimension: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subset: Option<String>,
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
    /// Checks of the values in the cube's cells, in the order they are made.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validations: Vec<ValidationDef>,
}

/// A check of the values in an area of a cube. A value written where it does not hold is
/// refused, or, where the check only warns, kept and marked as breaking it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ValidationDef {
    /// The cells checked: an area as a rule names one, such as `N: [Account].[Salaries]`;
    /// `N:`, `C:` or `S:` before it keeps to leaf, consolidated or text cells.
    pub cells: String,
    /// What holds wherever a value is acceptable, written as a rule's expression is and
    /// worked out at each cell checked, such as `[Account].[Salaries] >= 0`. True, any number
    /// but zero and any text but none hold; so does an expression that cannot be worked out.
    pub valid: String,
    /// What is said of a value where the check does not hold.
    pub message: String,
    /// Whether such a value is refused as it is written, rather than kept and warned of.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub refuse: bool,
}
