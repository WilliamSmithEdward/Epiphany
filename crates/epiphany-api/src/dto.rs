//! Request/response DTOs for the cube and cell endpoints (clean modern JSON).
//!
//! Numeric cell values are decimal STRINGS, never JSON numbers (ADR-0008), so a
//! client never parses them as lossy IEEE-754 doubles.

use std::collections::BTreeMap;

use epiphany_core::TestCell;
use serde::{Deserialize, Serialize};

/// A cell coordinate: dimension name -> element name, one entry per dimension.
pub type CoordMap = BTreeMap<String, String>;

/// An element in a dimension.
#[derive(Debug, Serialize)]
pub struct ElementDto {
    pub name: String,
    /// `numeric`, `string`, or `consolidated`.
    pub kind: &'static str,
}

/// View-cache counters for the admin server overview (ADR-0028).
#[derive(Debug, Serialize)]
pub struct ViewCacheStatsDto {
    /// Whether the cache is on (`EPIPHANY_VIEW_CACHE_ENTRIES` > 0).
    pub enabled: bool,
    /// Cumulative cache hits since startup.
    pub hits: u64,
    /// Cumulative cache misses since startup.
    pub misses: u64,
    /// Resident entries across both pools.
    pub entries: usize,
}

/// The admin server overview: cross-cutting server stats. Extensible; today it
/// carries the view-cache counters.
#[derive(Debug, Serialize)]
pub struct OverviewDto {
    pub view_cache: ViewCacheStatsDto,
}

/// A weighted consolidation edge.
#[derive(Debug, Serialize)]
pub struct EdgeDto {
    pub parent: String,
    pub child: String,
    pub weight: i64,
}

/// One element's value for an attribute.
#[derive(Debug, Serialize)]
pub struct AttributeValueDto {
    pub element: String,
    pub value: String,
}

/// An attribute defined on a dimension, with the values set so far.
#[derive(Debug, Serialize)]
pub struct AttributeDto {
    pub name: String,
    /// `text`, `numeric`, or `alias`.
    pub kind: &'static str,
    pub values: Vec<AttributeValueDto>,
}

/// A dimension with its elements, hierarchy edges, and attributes.
#[derive(Debug, Serialize)]
pub struct DimensionDto {
    pub name: String,
    pub elements: Vec<ElementDto>,
    pub edges: Vec<EdgeDto>,
    #[serde(default)]
    pub attributes: Vec<AttributeDto>,
}

/// A cube and its dimensions (the object browser and grid data source).
#[derive(Debug, Serialize)]
pub struct CubeDetailDto {
    pub name: String,
    pub dimensions: Vec<DimensionDto>,
}

/// One cell value. `value` is a decimal string (numeric) or text (string), or
/// null if unpopulated; `editable` is false for consolidated coordinates;
/// `overlaid` is true when the value is a what-if override from the active
/// sandbox (ADR-0014), so the UI can mark it as uncommitted.
#[derive(Debug, Serialize)]
pub struct CellDto {
    pub coord: CoordMap,
    pub value: Option<String>,
    pub kind: &'static str,
    pub editable: bool,
    pub overlaid: bool,
}

/// Bulk cell read (for the pivot grid).
#[derive(Debug, Deserialize)]
pub struct ReadCellsRequest {
    pub coords: Vec<CoordMap>,
}

#[derive(Debug, Serialize)]
pub struct ReadCellsResponse {
    pub cells: Vec<CellDto>,
}

/// A single leaf write (the one-element batch case).
#[derive(Debug, Deserialize)]
pub struct WriteCellRequest {
    pub coord: CoordMap,
    pub value: String,
}

/// One write in a batch.
#[derive(Debug, Deserialize)]
pub struct BatchWriteItem {
    pub coord: CoordMap,
    pub value: String,
}

/// A transactional, all-or-nothing batch write. With `base_version` set, the
/// commit is rejected (409) if the cube moved on (optimistic concurrency).
#[derive(Debug, Deserialize)]
pub struct BatchWriteRequest {
    pub writes: Vec<BatchWriteItem>,
    #[serde(default)]
    pub base_version: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct BatchWriteResponse {
    pub applied: usize,
    pub version: u64,
}

/// Spread a value entered at a (possibly consolidated) coordinate across its
/// contributing leaves (ADR-0029). `method` is one of `equal`, `proportional`,
/// `repeat`, or `clear`.
#[derive(Debug, Deserialize)]
pub struct SpreadRequest {
    pub target: CoordMap,
    pub value: String,
    pub method: String,
}

// ---- model testing (shared by flow tests and rule tests) ----

/// One assertion in a test: a coordinate and its expected value.
#[derive(Debug, Serialize, Deserialize)]
pub struct TestCellDto {
    pub coord: CoordMap,
    pub value: String,
}

/// Convert a test-cell DTO into the core [`TestCell`].
pub(crate) fn to_cell(c: TestCellDto) -> TestCell {
    TestCell {
        coord: c.coord,
        value: c.value,
    }
}

/// Convert a core [`TestCell`] into its DTO.
pub(crate) fn from_cell(c: &TestCell) -> TestCellDto {
    TestCellDto {
        coord: c.coord.clone(),
        value: c.value.clone(),
    }
}

/// One failed assertion in a test run: where, expected, and actual.
#[derive(Debug, Serialize)]
pub struct FailureDto {
    pub coord: CoordMap,
    pub expected: String,
    pub actual: String,
}

/// One test's outcome: its name, whether it passed, and any failures.
#[derive(Debug, Serialize)]
pub struct TestOutcomeDto {
    pub name: String,
    pub passed: bool,
    pub failures: Vec<FailureDto>,
}

/// A test run report: overall pass/fail plus each test's outcome. Shared by the
/// flow-test and rule-test run endpoints (they map their respective engine
/// outcomes into this one shape).
#[derive(Debug, Serialize)]
pub struct TestReportDto {
    pub all_passed: bool,
    pub outcomes: Vec<TestOutcomeDto>,
}

// ---- subsets ----

/// A subset definition request (create, replace, or preview). For preview the
/// `name` is optional; for create it is required (replace takes it from the URL).
#[derive(Debug, Deserialize)]
pub struct SubsetBody {
    #[serde(default)]
    pub name: Option<String>,
    /// `public` (default) or `private`.
    #[serde(default)]
    pub visibility: Option<String>,
    /// `static` or `dynamic`.
    pub kind: String,
    /// Member names (static subsets).
    #[serde(default)]
    pub members: Vec<String>,
    /// MDX set expression (dynamic subsets).
    #[serde(default)]
    pub mdx: Option<String>,
}

/// A subset as returned to clients.
#[derive(Debug, Serialize)]
pub struct SubsetDto {
    pub name: String,
    pub dimension: String,
    pub owner: Option<String>,
    pub visibility: &'static str,
    pub kind: &'static str,
    pub members: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mdx: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SubsetListResponse {
    pub subsets: Vec<SubsetDto>,
}

/// One resolved member of a subset/dimension.
#[derive(Debug, Serialize)]
pub struct MemberDto {
    pub name: String,
    pub kind: &'static str,
}

#[derive(Debug, Serialize)]
pub struct MembersResponse {
    pub members: Vec<MemberDto>,
}

/// Request body for resolving an MDX set expression to members.
#[derive(Debug, Deserialize)]
pub struct MdxPreviewRequest {
    pub mdx: String,
}

// ---- views and cellsets ----

/// One axis placement in a view request.
#[derive(Debug, Deserialize)]
pub struct AxisSpecBody {
    pub dimension: String,
    /// `subset` or `members`.
    #[serde(rename = "type")]
    pub spec_type: String,
    #[serde(default)]
    pub subset: Option<String>,
    #[serde(default)]
    pub members: Vec<String>,
}

/// One context (slicer) assignment.
#[derive(Debug, Deserialize, Serialize)]
pub struct ContextEntryDto {
    pub dimension: String,
    pub member: String,
}

/// A view definition request (create, replace, or ad-hoc execute).
#[derive(Debug, Deserialize)]
pub struct ViewBody {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default)]
    pub suppress_zeros: bool,
    #[serde(default)]
    pub rows: Vec<AxisSpecBody>,
    #[serde(default)]
    pub columns: Vec<AxisSpecBody>,
    #[serde(default)]
    pub context: Vec<ContextEntryDto>,
}

/// One axis placement as returned to clients.
#[derive(Debug, Serialize)]
pub struct AxisSpecDto {
    pub dimension: String,
    #[serde(rename = "type")]
    pub spec_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subset: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
}

/// A view as returned to clients.
#[derive(Debug, Serialize)]
pub struct ViewDto {
    pub name: String,
    pub cube: String,
    pub owner: Option<String>,
    pub visibility: &'static str,
    pub suppress_zeros: bool,
    pub rows: Vec<AxisSpecDto>,
    pub columns: Vec<AxisSpecDto>,
    pub context: Vec<ContextEntryDto>,
}

#[derive(Debug, Serialize)]
pub struct ViewListResponse {
    pub views: Vec<ViewDto>,
}

/// One member of an axis tuple in a cellset.
#[derive(Debug, Serialize)]
pub struct AxisMemberDto {
    pub dimension: String,
    pub name: String,
    pub kind: &'static str,
}

/// One cell value in a cellset (row-major; `ordinal` is its flat index).
/// `overlaid` is true when the value is a what-if override from the active
/// sandbox (ADR-0014).
#[derive(Debug, Serialize)]
pub struct CellsetCellDto {
    pub value: Option<String>,
    pub kind: &'static str,
    pub editable: bool,
    pub ordinal: usize,
    pub overlaid: bool,
}

/// How many tuples zero-suppression removed.
#[derive(Debug, Serialize)]
pub struct SuppressedDto {
    pub row_tuples: usize,
    pub column_tuples: usize,
}

/// An executed view: nested axis tuples plus a row-major value matrix.
#[derive(Debug, Serialize)]
pub struct CellsetDto {
    pub row_dimensions: Vec<String>,
    pub column_dimensions: Vec<String>,
    pub row_tuples: Vec<Vec<AxisMemberDto>>,
    pub column_tuples: Vec<Vec<AxisMemberDto>>,
    pub context: Vec<ContextEntryDto>,
    pub cells: Vec<CellsetCellDto>,
    pub version: u64,
    pub suppressed: SuppressedDto,
}
