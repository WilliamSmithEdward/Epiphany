//! MDX queries: text is lexed, parsed into a syntax tree, then evaluated against a cube.

mod eval;
mod lexer;
mod parser;
mod rules;

use std::cell::Cell;
use std::collections::BTreeMap;
use std::fmt;

use serde::Serialize;

use crate::Scalar;
use crate::access::Access;
use crate::model::{DimensionId, ElementId, Model, key};

pub(crate) use eval::Read;

/// Finds a subset the one asking a query keeps for themself, by its dimension's name and its
/// own: the names of its members.
pub type Own<'a> = &'a dyn Fn(&str, &str) -> Option<Vec<String>>;

/// Who asks a query, as far as answering it goes.
#[derive(Clone, Copy)]
pub struct Asker<'a> {
    /// Which cubes, by name, they may read. The query's own cube and any it looks cells up in
    /// must be among them; what the model's rules read is the model's business.
    pub may_read: &'a dyn Fn(&str) -> bool,
    /// Members, `(dimension, member)`, to stand at in place of those the query's WHERE
    /// names for their dimensions, or beside them; an axis that places one of their
    /// dimensions decides it.
    pub standing: &'a [(&'a str, &'a str)],
    /// The subsets they keep for themselves, which `TM1SubsetToSet` with "private" finds:
    /// by dimension and name, the names of a subset's members. None where nobody asks, as
    /// for the model's views, which cannot take anyone's.
    pub own: Option<Own<'a>>,
    /// The elements they may see, where some are hidden from them: those are not there for
    /// the query at all, though the totals they add up to still count them.
    pub access: Option<&'a Access>,
}

impl Asker<'static> {
    /// Anyone at all: who may read every cube, stands nowhere, keeps no subsets of their own
    /// and sees every element.
    pub const ANYONE: Self = Self {
        may_read: &every,
        standing: &[],
        own: Some(&nobodys),
        access: None,
    };
}

fn every(_: &str) -> bool {
    true
}

fn nobodys(_: &str, _: &str) -> Option<Vec<String>> {
    None
}
pub(crate) use parser::{Expr, Query};
pub use rules::{Feeder, Rule};
pub(crate) use rules::{
    Outcome, compile_feeder, compile_rule, evaluate_rule, feeder_targets, holds, matches,
};

/// Parses the text of a query.
pub(crate) fn parse(text: &str) -> Result<Query, MdxError> {
    parser::Parser::new(lexer::lex(text)?).query()
}

/// The elements an MDX set over one dimension selects, worked out from the dimension alone
/// with every cell read as empty; and the set itself where it reads any cube's cells, through
/// `LookupCube`, to be worked out again, as the cells then stand, whenever it is used.
pub(crate) fn subset(
    model: &Model,
    dimension: DimensionId,
    text: &str,
) -> Result<(Vec<ElementId>, Option<Expr>), MdxError> {
    let set = parser::Parser::new(lexer::lex(text)?).set()?;
    let read = Cell::new(false);
    let noting = |_: usize, _: &[ElementId]| {
        read.set(true);
        Ok(Scalar::Number(0.0))
    };
    let elements = eval::elements(model, dimension, &set, &noting)?;
    Ok((elements, read.get().then_some(set)))
}

/// The elements a set over one dimension selects now, reading cells through `read`.
pub(crate) fn subset_now(
    model: &Model,
    dimension: DimensionId,
    set: &Expr,
    read: Read,
) -> Result<Vec<ElementId>, MdxError> {
    eval::elements(model, dimension, set, read)
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
    // Nobody asks the model's own views.
    let nobody = Asker {
        own: None,
        ..Asker::ANYONE
    };
    let empty = |_: usize, _: &mut dyn FnMut(&[ElementId])| false;
    query
        .evaluate(model, cube_id, &eval::no_cells, &|_| (), &empty, &nobody)
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
    /// The cells written out, one for each of `cells`, where a calculated member that
    /// decides them has a `FORMAT_STRING`: as its pattern says, the way `Format` writes a
    /// number or a date, and null for the others. Left out where no calculated member of the
    /// query has a format.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub formatted: Vec<Option<String>>,
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
