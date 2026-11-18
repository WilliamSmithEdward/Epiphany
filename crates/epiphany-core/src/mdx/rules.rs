//! Rules and feeders: statements written in the query language and attached to a cube.
//!
//! A rule says how the cells of an area get their value:
//!
//! ```text
//! [Account].[Margin] = [Account].[Revenue] - [Account].[Cost]
//! N: ([Account].[Price], [Version].[Budget]) = LookupCube("Rates", [Currency].[USD]) * 2
//! ```
//!
//! The area is a member or a tuple; a dimension it does not name may be at any element. `N:`
//! limits the rule to leaf cells, `C:` to consolidated cells and `S:` to text cells; without
//! one it covers every numeric cell of the area. The first rule that covers a cell decides
//! it, unless that rule answers `STET` (keep the stored or consolidated value) or `CONTINUE`
//! (ask the next rule).
//!
//! A feeder says which calculated leaf cells may hold a value, so that a consolidation over
//! them need not visit every cell:
//!
//! ```text
//! [Account].[Revenue] => [Account].[Margin]
//! ```
//!
//! Wherever a cell of the source area holds something, the matching target cell is fed.

use super::eval::{Context, Datum, Evaluated, Evaluator, Id, Read, fail, no_cells};
use super::lexer::lex;
use super::parser::{Expr, Parser};
use crate::Scalar;
use crate::model::{ElementId, ElementType, Model};

/// The element each dimension of a cube must be at, where a statement names one.
pub(crate) type Area = Vec<Option<ElementId>>;

#[derive(Debug)]
pub struct Rule {
    pub(crate) area: Area,
    pub(crate) scope: Scope,
    expression: Expr,
}

/// The kind of cell a rule covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    /// Every numeric cell.
    Numeric,
    /// Numeric leaf cells.
    Leaf,
    /// Consolidated cells.
    Consolidated,
    /// Text cells.
    Text,
}

#[derive(Debug)]
pub struct Feeder {
    pub(crate) source: Area,
    targets: Vec<Expr>,
    /// The cubes the targets are in, by position in the model.
    pub(crate) feeds: Vec<usize>,
}

/// What a rule says about a cell.
pub(crate) enum Outcome {
    Value(Scalar),
    /// The cell keeps its stored or consolidated value.
    Stet,
    /// The next rule decides.
    Continue,
}

impl Rule {
    /// Whether the rule covers a cell, given whether the cell is text or consolidated.
    pub(crate) fn covers(&self, at: &[ElementId], text: bool, consolidated: bool) -> bool {
        let fits = match self.scope {
            Scope::Text => text,
            Scope::Numeric => !text,
            Scope::Leaf => !text && !consolidated,
            Scope::Consolidated => !text && consolidated,
        };
        fits && matches(&self.area, at)
    }

    /// Whether the rule can decide leaf cells.
    pub(crate) fn decides_leaves(&self) -> bool {
        matches!(self.scope, Scope::Numeric | Scope::Leaf)
    }
}

/// Whether a cell lies in an area.
pub(crate) fn matches(area: &Area, at: &[ElementId]) -> bool {
    let fits =
        |(wanted, element): (&Option<ElementId>, &ElementId)| wanted.is_none_or(|w| w == *element);
    area.iter().zip(at).all(fits)
}

fn area(evaluator: &Evaluator, expression: &Expr) -> Evaluated<Area> {
    let tuple = match evaluator.evaluate(expression, &evaluator.top())? {
        Datum::Member(member) => vec![member],
        Datum::Tuple(tuple) => tuple,
        _ => return fail("an area is a member or a tuple"),
    };
    let mut area = vec![None; evaluator.cube.dimensions.len()];
    for member in tuple {
        let Id::Element(id) = member.id else {
            return fail("an area names elements");
        };
        area[member.position] = Some(id);
    }
    Ok(area)
}

pub(crate) fn compile_rule(model: &Model, cube_id: usize, text: &str) -> Evaluated<Rule> {
    let (scope, area_expression, expression) = Parser::new(lex(text)?).rule()?;
    let evaluator = Evaluator::new(model, cube_id, &model.cubes()[cube_id], &no_cells);
    Ok(Rule {
        area: area(&evaluator, &area_expression)?,
        scope: match scope {
            None => Scope::Numeric,
            Some('N') => Scope::Leaf,
            Some('C') => Scope::Consolidated,
            Some(_) => Scope::Text,
        },
        expression: evaluator.bind(&expression)?,
    })
}

pub(crate) fn compile_feeder(model: &Model, cube_id: usize, text: &str) -> Evaluated<Feeder> {
    let (source, targets) = Parser::new(lex(text)?).feeder()?;
    let evaluator = Evaluator::new(model, cube_id, &model.cubes()[cube_id], &no_cells);
    let targets: Vec<Expr> = targets
        .iter()
        .map(|target| evaluator.bind(target))
        .collect::<Evaluated<_>>()?;
    let feeds = targets
        .iter()
        .map(|target| Ok(evaluator.target(target, &evaluator.top())?.0));
    Ok(Feeder {
        source: area(&evaluator, &source)?,
        feeds: feeds.collect::<Evaluated<_>>()?,
        targets,
    })
}

fn context(at: &[ElementId]) -> Context {
    at.iter().map(|&id| Id::Element(id)).collect()
}

/// What a rule says about one cell it covers.
pub(crate) fn evaluate_rule(
    model: &Model,
    cube_id: usize,
    rule: &Rule,
    at: &[ElementId],
    read: Read,
) -> Evaluated<Outcome> {
    let cube = &model.cubes()[cube_id];
    let evaluator = Evaluator::new(model, cube_id, cube, read);
    let context = context(at);
    let value = evaluator.evaluate(&rule.expression, &context)?;
    let last =
        &model.dimensions()[*cube.dimensions.last().expect("cubes have dimensions") as usize];
    let text = last.elements[*at.last().expect("cubes have dimensions") as usize].kind
        == ElementType::Text;
    Ok(match (evaluator.scalar(value, &context)?, text) {
        (Datum::Stet, _) => Outcome::Stet,
        (Datum::Continue, _) => Outcome::Continue,
        (Datum::Text(text), true) => Outcome::Value(Scalar::Text(text)),
        (Datum::Number(number), false) => Outcome::Value(Scalar::Number(number)),
        // A value that cannot be computed, such as a division by zero, is an empty cell.
        (Datum::Null, false) => Outcome::Value(Scalar::Number(0.0)),
        (Datum::Null, true) => Outcome::Value(Scalar::Text(String::new())),
        (Datum::Bool(truth), false) => Outcome::Value(Scalar::Number(f64::from(u8::from(truth)))),
        (_, true) => return fail("the rule gives a number for a text cell"),
        (_, false) => return fail("the rule gives text for a numeric cell"),
    })
}

/// The cells a feeder feeds from one cell of its source area: for each target, its cube and
/// the element it has in each dimension, which may be a consolidation standing for its leaves.
pub(crate) fn feeder_targets(
    model: &Model,
    cube_id: usize,
    feeder: &Feeder,
    at: &[ElementId],
) -> Evaluated<Vec<(usize, Vec<ElementId>)>> {
    let evaluator = Evaluator::new(model, cube_id, &model.cubes()[cube_id], &no_cells);
    let context = context(at);
    let target = |target: &Expr| evaluator.target(target, &context);
    feeder.targets.iter().map(target).collect()
}
