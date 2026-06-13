//! Request/response DTOs for the cube and cell endpoints (clean modern JSON).
//!
//! Numeric cell values are decimal STRINGS, never JSON numbers (ADR-0008), so a
//! client never parses them as lossy IEEE-754 doubles.

use std::collections::BTreeMap;

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

/// A weighted consolidation edge.
#[derive(Debug, Serialize)]
pub struct EdgeDto {
    pub parent: String,
    pub child: String,
    pub weight: i64,
}

/// A dimension with its elements and hierarchy edges.
#[derive(Debug, Serialize)]
pub struct DimensionDto {
    pub name: String,
    pub elements: Vec<ElementDto>,
    pub edges: Vec<EdgeDto>,
}

/// A cube and its dimensions (the object browser and grid data source).
#[derive(Debug, Serialize)]
pub struct CubeDetailDto {
    pub name: String,
    pub dimensions: Vec<DimensionDto>,
}

/// One cell value. `value` is a decimal string (numeric) or text (string), or
/// null if unpopulated; `editable` is false for consolidated coordinates.
#[derive(Debug, Serialize)]
pub struct CellDto {
    pub coord: CoordMap,
    pub value: Option<String>,
    pub kind: &'static str,
    pub editable: bool,
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
