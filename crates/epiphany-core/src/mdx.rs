//! MDX queries: text is lexed, parsed into a syntax tree, then evaluated against a cube.

mod eval;
mod lexer;
mod parser;
mod rules;

use std::collections::BTreeMap;
use std::fmt;

use serde::Serialize;

use crate::Scalar;
use crate::model::{Cube, DimensionId, ElementId, Model, key};

pub(crate) use parser::Query;
pub use rules::{Feeder, Rule};
pub(crate) use rules::{
    Outcome, compile_feeder, compile_rule, evaluate_rule, feeder_targets, matches,
};

/// Parses the text of a query.
pub(crate) fn parse(text: &str) -> Result<Query, MdxError> {
    parser::Parser::new(lexer::lex(text)?).query()
}

/// The elements an MDX set over one dimension selects. The set is worked out from the
/// dimension alone: every cell it might read is empty.
pub(crate) fn subset(
    model: &Model,
    dimension: DimensionId,
    text: &str,
) -> Result<Vec<ElementId>, MdxError> {
    let set = parser::Parser::new(lexer::lex(text)?).set()?;
    let alone = Cube {
        name: String::new(),
        dimensions: vec![dimension],
        views: Vec::new(),
        rules: Vec::new(),
        feeders: Vec::new(),
        sparse: false,
    };
    eval::elements(model, &alone, &set)
}

/// Checks that a view's query can be answered by its cube, by running it with every cell
/// empty.
pub(crate) fn check_view(model: &Model, cube_id: usize, text: &str) -> Result<(), MdxError> {
    let query = parse(text)?;
    let cube = &model.cubes()[cube_id];
    if key(&query.cube) != key(&cube.name) {
        return Err(MdxError::new(format!(
            "the view selects from '{}'",
            query.cube
        )));
    }
    query
        .evaluate(model, cube_id, &eval::no_cells, &|_| ())
        .map(drop)
}

/// Why a query could not be parsed or evaluated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdxError {
    pub message: String,
    /// Where in the query text the problem was found, counted in characters from zero.
    pub position: Option<usize>,
}

impl MdxError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            position: None,
        }
    }
}

impl fmt::Display for MdxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.position {
            Some(position) => write!(f, "{} at character {position}", self.message),
            None => self.message.fmt(f),
        }
    }
}

impl std::error::Error for MdxError {}

/// The answer to a query.
///
/// `cells` holds one value per combination of a tuple from each axis, the first axis varying
/// fastest. With columns and rows that is row by row: the cell of row `r` and column `c` is at
/// `r * columns + c`. With columns only there is one cell per column, and with no axes a
/// single cell. A cell with no value, such as a division by zero, is null.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct QueryResult {
    pub cube: String,
    /// The columns axis, then the rows axis, then any further axes the query has.
    pub axes: Vec<AxisResult>,
    /// The members the `WHERE` clause narrows the query to. A dimension that is neither here
    /// nor on an axis stands at its first top element.
    pub slicer: Vec<MemberResult>,
    pub cells: Vec<Option<Scalar>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AxisResult {
    /// Each tuple holds one member per dimension on the axis.
    pub tuples: Vec<Vec<MemberResult>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MemberResult {
    pub dimension: String,
    pub name: String,
    pub unique_name: String,
    #[serde(rename = "type")]
    pub kind: MemberType,
    /// Zero for a leaf or a calculated member, otherwise one more than the highest child level.
    pub level: u32,
    /// The attributes asked for with `PROPERTIES`, where the member has a value for them.
    pub properties: BTreeMap<String, Scalar>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum MemberType {
    Numeric,
    #[serde(rename = "string")]
    Text,
    Consolidated,
    Calculated,
}
