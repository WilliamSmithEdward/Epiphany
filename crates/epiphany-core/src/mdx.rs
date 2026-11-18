//! MDX queries: text is lexed, parsed into a syntax tree, then evaluated against a cube.

mod eval;
mod lexer;
mod parser;

use std::collections::BTreeMap;
use std::fmt;

use serde::Serialize;

use crate::Value;

pub(crate) use parser::Query;

/// Parses the text of a query.
pub(crate) fn parse(text: &str) -> Result<Query, MdxError> {
    parser::Parser::new(lexer::lex(text)?).query()
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
/// With columns and rows, `cells` holds one value per pair of tuples, row by row: the cell of
/// row `r` and column `c` is at `r * columns + c`. With columns only there is one cell per
/// column, and with no axes a single cell. A cell with no value, such as a division by zero,
/// is null.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct QueryResult {
    pub cube: String,
    /// The columns axis, then the rows axis, as far as the query has them.
    pub axes: Vec<AxisResult>,
    pub cells: Vec<Option<Value>>,
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
    pub properties: BTreeMap<String, Value>,
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
