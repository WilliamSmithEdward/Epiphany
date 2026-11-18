//! Evaluates a parsed query against one cube.

use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};

use super::parser::{Axis, Expr, Query};
use super::{AxisResult, MdxError, MemberResult, MemberType, QueryResult};
use crate::Value;
use crate::model::{Cube, Dimension, Element, ElementId, ElementType, Model, key};

/// The most cells one query may return.
const MOST_CELLS: usize = 1_000_000;
/// How deeply calculated members may refer to one another.
const DEEPEST: u32 = 32;

type Evaluated<T> = Result<T, MdxError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Id {
    Element(ElementId),
    /// A calculated member, by its position in the query.
    Calculated(usize),
}

/// A member of the dimension at a position in the cube.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Member {
    position: usize,
    id: Id,
}

/// Members of different dimensions.
type Tuple = Vec<Member>;
/// One member per dimension of the cube: the cell an expression is evaluated at.
type Context = Vec<Id>;

/// What an expression evaluates to.
#[derive(Debug, Clone, PartialEq)]
enum Datum {
    Null,
    Number(f64),
    Text(String),
    Bool(bool),
    /// A dimension of the cube, by position.
    Dimension(usize),
    Member(Member),
    Tuple(Tuple),
    Set(Vec<Tuple>),
}

struct Calculated<'a> {
    position: usize,
    name: &'a str,
    expression: &'a Expr,
    solve_order: f64,
}

struct Evaluator<'a> {
    model: &'a Model,
    cube: &'a Cube,
    read: &'a dyn Fn(&[ElementId]) -> Value,
    calculated: Vec<Calculated<'a>>,
    sets: HashMap<String, Vec<Tuple>>,
    /// How many calculated members are being evaluated inside one another.
    depth: Cell<u32>,
}

fn fail<T>(message: impl Into<String>) -> Evaluated<T> {
    Err(MdxError::new(message))
}

impl Query {
    /// Runs the query against a cube, reading stored and consolidated cells through `read`.
    pub(crate) fn evaluate(
        &self,
        model: &Model,
        cube: &Cube,
        read: &dyn Fn(&[ElementId]) -> Value,
    ) -> Evaluated<QueryResult> {
        let mut evaluator = Evaluator {
            model,
            cube,
            read,
            calculated: Vec::new(),
            sets: HashMap::new(),
            depth: Cell::new(0),
        };
        for member in &self.members {
            let [dimension, .., name] = member.name.as_slice() else {
                return fail("a calculated member is named [Dimension].[Name]");
            };
            evaluator.calculated.push(Calculated {
                position: evaluator.position(dimension)?,
                name,
                expression: &member.expression,
                solve_order: member.solve_order,
            });
        }

        // Every dimension starts at its first top element; the slicer then narrows some.
        let top = |dimension: &Dimension| {
            let top = dimension.elements.iter().position(|e| e.parents.is_empty());
            Id::Element(top.unwrap_or(0) as ElementId)
        };
        let mut context: Context = evaluator.dimensions().map(top).collect();
        for (name, expression) in &self.sets {
            let set = evaluator.set(expression, &context)?;
            evaluator.sets.insert(key(name), set);
        }
        let mut used = HashSet::new();
        let mut claim = |tuple: &Tuple| match tuple.iter().all(|m| used.insert(m.position)) {
            true => Ok(()),
            false => fail("a dimension is used on more than one axis or in the slicer"),
        };
        let mut slicer = Tuple::new();
        if let Some(expression) = &self.slicer {
            slicer = match evaluator.evaluate(expression, &context)? {
                Datum::Member(member) => vec![member],
                Datum::Tuple(tuple) => tuple,
                _ => return fail("WHERE takes a member or a tuple"),
            };
            claim(&slicer)?;
            context = with(&context, &slicer);
        }

        let mut axes: Vec<&Axis> = self.axes.iter().collect();
        axes.sort_by_key(|axis| axis.ordinal);
        if axes
            .iter()
            .enumerate()
            .any(|(index, axis)| axis.ordinal != index)
        {
            return fail("axes are numbered from COLUMNS without gaps or repeats");
        }
        let mut sets = Vec::new();
        for axis in &axes {
            let set = evaluator.set(&axis.set, &context)?;
            if let Some(tuple) = set.first() {
                claim(tuple)?;
            }
            sets.push(set);
        }

        // One cell per combination of a tuple from each axis, the first axis varying fastest.
        let sizes: Vec<usize> = sets.iter().map(Vec::len).collect();
        let places = Places::new(&sizes)?;
        let mut cells = Vec::with_capacity(places.count);
        for index in 0..places.count {
            let mut context = context.clone();
            for (set, place) in sets.iter().zip(places.of(index)) {
                context = with(&context, &set[place]);
            }
            cells.push(evaluator.cell(&context)?);
        }

        // NON EMPTY drops the tuples of an axis whose cells are all empty.
        let mut filled: Vec<Vec<bool>> = sizes.iter().map(|&size| vec![false; size]).collect();
        for (index, cell) in cells.iter().enumerate() {
            for (filled, place) in filled.iter_mut().zip(places.of(index)) {
                filled[place] |= !is_empty(cell);
            }
        }
        let kept: Vec<Vec<usize>> = axes
            .iter()
            .zip(&filled)
            .map(|(axis, filled)| {
                let keep = |place: &usize| !axis.non_empty || filled[*place];
                (0..filled.len()).filter(keep).collect()
            })
            .collect();

        let value = |cell: &Datum| match cell {
            Datum::Number(number) => Some(Value::Number(*number)),
            Datum::Text(text) => Some(Value::Text(text.clone())),
            _ => None,
        };
        let kept_places = Places::new(&kept.iter().map(Vec::len).collect::<Vec<_>>())?;
        let kept_cells = (0..kept_places.count).map(|index| {
            let original = kept.iter().zip(kept_places.of(index)).zip(&places.strides);
            let original = original.map(|((kept, place), stride)| kept[place] * stride);
            value(&cells[original.sum::<usize>()])
        });
        let axes = axes
            .iter()
            .zip(&sets)
            .zip(&kept)
            .map(|((axis, set), kept)| {
                let tuple = |&index: &usize| {
                    let members = set[index].iter();
                    members
                        .map(|&member| evaluator.describe(member, &axis.properties))
                        .collect()
                };
                AxisResult {
                    tuples: kept.iter().map(tuple).collect(),
                }
            });
        Ok(QueryResult {
            cube: cube.name.clone(),
            cells: kept_cells.collect(),
            axes: axes.collect(),
            slicer: slicer
                .iter()
                .map(|&member| evaluator.describe(member, &[]))
                .collect(),
        })
    }
}

/// How the cells of a query are numbered: by one place on each axis, the first axis varying
/// fastest.
struct Places {
    sizes: Vec<usize>,
    /// How far apart in the numbering two neighbours on each axis are.
    strides: Vec<usize>,
    count: usize,
}

impl Places {
    fn new(sizes: &[usize]) -> Evaluated<Self> {
        let mut strides = Vec::with_capacity(sizes.len());
        let mut count = 1usize;
        for &size in sizes {
            strides.push(count);
            count = count.saturating_mul(size);
        }
        match count > MOST_CELLS {
            true => fail(format!("the query has more than {MOST_CELLS} cells")),
            false => Ok(Self {
                sizes: sizes.to_vec(),
                strides,
                count,
            }),
        }
    }

    /// The place on each axis of the cell with a number.
    fn of(&self, index: usize) -> impl Iterator<Item = usize> {
        let axes = self.sizes.iter().zip(&self.strides);
        axes.map(move |(size, stride)| index / stride % size)
    }
}

/// A context with the members of a tuple put in their places.
fn with(context: &Context, tuple: &Tuple) -> Context {
    let mut context = context.clone();
    for member in tuple {
        context[member.position] = member.id;
    }
    context
}

fn is_empty(datum: &Datum) -> bool {
    match datum {
        Datum::Null => true,
        Datum::Number(number) => *number == 0.0,
        Datum::Text(text) => text.is_empty(),
        _ => false,
    }
}

/// The single word an argument consists of, in upper case, as flags such as `DESC` are.
fn flag(argument: Option<&Expr>) -> Option<String> {
    match argument {
        Some(Expr::Name(parts)) if parts.len() == 1 => Some(parts[0].to_uppercase()),
        _ => None,
    }
}

/// Orders values for sorting: nothing first, then numbers, then text.
fn compare(left: &Datum, right: &Datum) -> Ordering {
    let rank = |datum: &Datum| match datum {
        Datum::Number(_) => 1,
        Datum::Text(_) => 2,
        _ => 0,
    };
    match (left, right) {
        (Datum::Number(left), Datum::Number(right)) => left.total_cmp(right),
        (Datum::Text(left), Datum::Text(right)) => left.cmp(right),
        _ => rank(left).cmp(&rank(right)),
    }
}

impl<'a> Evaluator<'a> {
    fn dimensions(&self) -> impl Iterator<Item = &'a Dimension> + use<'a> {
        let model = self.model;
        let dimensions = self.cube.dimensions.iter();
        dimensions.map(move |&id| &model.dimensions()[id as usize])
    }

    fn dimension(&self, position: usize) -> &'a Dimension {
        &self.model.dimensions()[self.cube.dimensions[position] as usize]
    }

    /// The position in the cube of a dimension, by name.
    fn position(&self, name: &str) -> Evaluated<usize> {
        let found = self.dimensions().position(|d| key(&d.name) == key(name));
        found.ok_or_else(|| MdxError::new(format!("the cube has no dimension '{name}'")))
    }

    /// The element behind a member, which a calculated member does not have.
    fn element(&self, member: Member) -> Evaluated<(ElementId, &'a Element)> {
        match member.id {
            Id::Element(id) => Ok((id, &self.dimension(member.position).elements[id as usize])),
            Id::Calculated(_) => fail("a calculated member has no place in the hierarchy"),
        }
    }

    fn name(&self, member: Member) -> &'a str {
        match member.id {
            Id::Element(id) => &self.dimension(member.position).elements[id as usize].name,
            Id::Calculated(index) => self.calculated[index].name,
        }
    }

    fn unique_name(&self, member: Member) -> String {
        let dimension = &self.dimension(member.position).name;
        format!("[{dimension}].[{}]", self.name(member))
    }

    /// What a dotted name refers to: a named set, a dimension, or a member.
    fn resolve(&self, parts: &[String]) -> Evaluated<Datum> {
        let [first, rest @ ..] = parts else {
            return fail("expected a name");
        };
        if let (Some(set), true) = (self.sets.get(&key(first)), rest.is_empty()) {
            return Ok(Datum::Set(set.clone()));
        }
        let position = self.position(first)?;
        // Only the last part matters, since names are unique within a dimension. Parts
        // before it name the dimension's one hierarchy or the member's ancestors.
        let Some(name) = rest.last() else {
            return Ok(Datum::Dimension(position));
        };
        let calculated = self.calculated.iter().position(|calculated| {
            calculated.position == position && key(calculated.name) == key(name)
        });
        let element = || self.dimension(position).element_id(name).map(Id::Element);
        match calculated.map(Id::Calculated).or_else(element) {
            Some(id) => Ok(Datum::Member(Member { position, id })),
            // `[Dimension].[Dimension]` is the hierarchy, which is the dimension itself.
            None if rest.len() == 1 && key(name) == key(first) => Ok(Datum::Dimension(position)),
            None => fail(format!("dimension '{first}' has no member '{name}'")),
        }
    }

    fn evaluate(&self, expression: &Expr, context: &Context) -> Evaluated<Datum> {
        Ok(match expression {
            Expr::Number(number) => Datum::Number(*number),
            Expr::Text(text) => Datum::Text(text.clone()),
            Expr::Name(parts) => self.resolve(parts)?,
            Expr::Set(items) => {
                let mut set = Vec::new();
                for item in items {
                    set.extend(self.set(item, context)?);
                }
                if set
                    .windows(2)
                    .any(|pair| !same_dimensions(&pair[0], &pair[1]))
                {
                    return fail("the tuples of a set must have the same dimensions");
                }
                Datum::Set(set)
            }
            Expr::Tuple(items) => {
                let mut tuple = Tuple::new();
                for item in items {
                    let member = self.member(item, context)?;
                    if tuple.iter().any(|other| other.position == member.position) {
                        return fail("a tuple has one member per dimension");
                    }
                    tuple.push(member);
                }
                Datum::Tuple(tuple)
            }
            Expr::Call(function, arguments) => self.call(function, arguments, context)?,
            Expr::Method(target, method, arguments) => {
                self.method(target, method, arguments, context)?
            }
            Expr::Not(operand) => Datum::Bool(!self.truth(operand, context)?),
            Expr::Negate(operand) => Datum::Number(-self.number(operand, context)?),
            Expr::Binary(left, operator, right) => self.binary(left, operator, right, context)?,
        })
    }

    fn binary(
        &self,
        left: &Expr,
        operator: &str,
        right: &Expr,
        context: &Context,
    ) -> Evaluated<Datum> {
        // The right side of AND and OR is only evaluated when the left does not settle it.
        if operator == "AND" {
            let both = self.truth(left, context)? && self.truth(right, context)?;
            return Ok(Datum::Bool(both));
        }
        if operator == "OR" {
            let either = self.truth(left, context)? || self.truth(right, context)?;
            return Ok(Datum::Bool(either));
        }
        let (left, right) = (
            self.evaluate(left, context)?,
            self.evaluate(right, context)?,
        );
        match (operator, &left, &right) {
            ("*", Datum::Set(left), Datum::Set(right)) => {
                return cross_join(left, right).map(Datum::Set);
            }
            ("+", Datum::Set(left), Datum::Set(right)) => {
                return union(left.clone(), right.clone(), false).map(Datum::Set);
            }
            // Every element from one member to the other, in the order they are declared.
            (":", &Datum::Member(first), &Datum::Member(last)) => {
                let (first_id, _) = self.element(first)?;
                let (last_id, _) = self.element(last)?;
                if first.position != last.position {
                    return fail("a range runs between two members of one dimension");
                }
                let ids = first_id.min(last_id)..=first_id.max(last_id);
                let member = |id| {
                    vec![Member {
                        position: first.position,
                        id: Id::Element(id),
                    }]
                };
                return Ok(Datum::Set(ids.map(member).collect()));
            }
            (":", ..) => return fail("a range runs between two members"),
            _ => {}
        }
        let (left, right) = (self.scalar(left, context)?, self.scalar(right, context)?);
        if let ("+", Datum::Text(left), Datum::Text(right)) = (operator, &left, &right) {
            return Ok(Datum::Text(format!("{left}{right}")));
        }
        let ordering = || compare(&left, &right);
        let equal = || match (&left, &right) {
            (Datum::Text(left), Datum::Text(right)) => left == right,
            _ => to_number(&left).ok() == to_number(&right).ok(),
        };
        let arithmetic = |apply: fn(f64, f64) -> f64| -> Evaluated<Datum> {
            let result = apply(to_number(&left)?, to_number(&right)?);
            Ok(match result.is_finite() {
                true => Datum::Number(result),
                false => Datum::Null,
            })
        };
        match operator {
            "+" => arithmetic(|a, b| a + b),
            "-" => arithmetic(|a, b| a - b),
            "*" => arithmetic(|a, b| a * b),
            "/" => arithmetic(|a, b| a / b),
            "=" => Ok(Datum::Bool(equal())),
            "<>" => Ok(Datum::Bool(!equal())),
            "<" => Ok(Datum::Bool(ordering() == Ordering::Less)),
            "<=" => Ok(Datum::Bool(ordering() != Ordering::Greater)),
            ">" => Ok(Datum::Bool(ordering() == Ordering::Greater)),
            ">=" => Ok(Datum::Bool(ordering() != Ordering::Less)),
            _ => fail(format!("unknown operator '{operator}'")),
        }
    }

    fn call(&self, function: &str, arguments: &[Expr], context: &Context) -> Evaluated<Datum> {
        let count = |allowed: std::ops::RangeInclusive<usize>| {
            let (least, most) = (*allowed.start(), *allowed.end());
            match allowed.contains(&arguments.len()) {
                true => Ok(()),
                false if most == usize::MAX => {
                    fail(format!("{function} takes at least {least} arguments"))
                }
                false => fail(format!("{function} takes {least} to {most} arguments")),
            }
        };
        // The value an aggregate takes from a tuple: the given expression there, or its cell.
        let measure = |tuple: &Tuple| {
            let context = with(context, tuple);
            match arguments.get(1) {
                Some(expression) => {
                    let value = self.evaluate(expression, &context)?;
                    self.scalar(value, &context)
                }
                None => self.cell(&context),
            }
        };
        let all = |argument: Option<&Expr>| match flag(argument).as_deref() {
            None => Ok(false),
            Some("ALL") => Ok(true),
            Some(other) => fail(format!("unknown {function} flag {other}")),
        };
        let whole = |argument: &Expr| Ok(self.number(argument, context)?.max(0.0) as usize);
        match function {
            "SUM" | "AGGREGATE" | "AVG" | "MIN" | "MAX" => {
                count(1..=2)?;
                // As everywhere in the engine, a cell holding zero is an empty cell, so the
                // average, least and greatest are taken over the cells that hold something.
                let mut values = Vec::new();
                for tuple in self.set(&arguments[0], context)? {
                    let value = measure(&tuple)?;
                    if !is_empty(&value) {
                        values.push(to_number(&value)?);
                    }
                }
                let total: f64 = values.iter().sum();
                Ok(match (function, values.is_empty()) {
                    ("SUM" | "AGGREGATE", _) => Datum::Number(total),
                    (_, true) => Datum::Null,
                    ("AVG", _) => Datum::Number(total / values.len() as f64),
                    ("MIN", _) => Datum::Number(values.into_iter().fold(f64::INFINITY, f64::min)),
                    _ => Datum::Number(values.into_iter().fold(f64::NEG_INFINITY, f64::max)),
                })
            }
            "COUNT" => {
                count(1..=2)?;
                let set = self.set(&arguments[0], context)?;
                let counted = match flag(arguments.get(1)).as_deref() {
                    None | Some("INCLUDEEMPTY") => set.len(),
                    Some("EXCLUDEEMPTY") => {
                        let mut filled = 0;
                        for tuple in &set {
                            filled += usize::from(!is_empty(&self.cell(&with(context, tuple))?));
                        }
                        filled
                    }
                    Some(other) => return fail(format!("unknown COUNT flag {other}")),
                };
                Ok(Datum::Number(counted as f64))
            }
            "UNION" => {
                count(2..=3)?;
                let (left, right) = (
                    self.set(&arguments[0], context)?,
                    self.set(&arguments[1], context)?,
                );
                union(left, right, all(arguments.get(2))?).map(Datum::Set)
            }
            "EXCEPT" | "INTERSECT" => {
                count(2..=2)?;
                let left = distinct(self.set(&arguments[0], context)?);
                let right: HashSet<Tuple> = self.set(&arguments[1], context)?.into_iter().collect();
                let wanted = function == "INTERSECT";
                let kept = left
                    .into_iter()
                    .filter(|tuple| right.contains(tuple) == wanted);
                Ok(Datum::Set(kept.collect()))
            }
            "DISTINCT" => {
                count(1..=1)?;
                Ok(Datum::Set(distinct(self.set(&arguments[0], context)?)))
            }
            "HEAD" | "TAIL" => {
                count(1..=2)?;
                let mut set = self.set(&arguments[0], context)?;
                let wanted = match arguments.get(1) {
                    Some(argument) => whole(argument)?,
                    None => 1,
                };
                let dropped = set.len().saturating_sub(wanted);
                match function {
                    "HEAD" => set.truncate(wanted),
                    _ => drop(set.drain(..dropped)),
                }
                Ok(Datum::Set(set))
            }
            "SUBSET" => {
                count(2..=3)?;
                let set = self.set(&arguments[0], context)?;
                let wanted = match arguments.get(2) {
                    Some(argument) => whole(argument)?,
                    None => usize::MAX,
                };
                let kept = set.into_iter().skip(whole(&arguments[1])?).take(wanted);
                Ok(Datum::Set(kept.collect()))
            }
            "TOPCOUNT" | "BOTTOMCOUNT" => {
                count(2..=3)?;
                let mut keyed = Vec::new();
                for tuple in self.set(&arguments[0], context)? {
                    let value = match arguments.get(2) {
                        Some(expression) => {
                            let context = with(context, &tuple);
                            let value = self.evaluate(expression, &context)?;
                            self.scalar(value, &context)?
                        }
                        None => Datum::Null,
                    };
                    keyed.push((value, tuple));
                }
                // Without an expression the set keeps its order and is only cut short.
                keyed.sort_by(|(left, _), (right, _)| match function {
                    "TOPCOUNT" => compare(right, left),
                    _ => compare(left, right),
                });
                let kept = keyed.into_iter().take(whole(&arguments[1])?);
                Ok(Datum::Set(kept.map(|(_, tuple)| tuple).collect()))
            }
            "GENERATE" => {
                count(2..=3)?;
                let mut generated = Vec::new();
                for tuple in self.set(&arguments[0], context)? {
                    generated.extend(self.set(&arguments[1], &with(context, &tuple))?);
                }
                Ok(Datum::Set(match all(arguments.get(2))? {
                    true => generated,
                    false => distinct(generated),
                }))
            }
            "TM1SUBSETALL" => {
                count(1..=1)?;
                self.method(&arguments[0], "MEMBERS", &[], context)
            }
            "TM1FILTERBYLEVEL" => {
                count(2..=usize::MAX)?;
                let mut levels = Vec::new();
                for argument in &arguments[1..] {
                    levels.push(self.number(argument, context)?);
                }
                let level = |tuple: &Tuple| self.element(tuple[0]).map(|(_, e)| f64::from(e.level));
                let mut kept = Vec::new();
                for tuple in self.set(&arguments[0], context)? {
                    if levels.contains(&level(&tuple)?) {
                        kept.push(tuple);
                    }
                }
                Ok(Datum::Set(kept))
            }
            "TM1FILTERBYPATTERN" => {
                count(2..=2)?;
                let Datum::Text(pattern) = self.evaluate(&arguments[1], context)? else {
                    return fail("TM1FILTERBYPATTERN takes a pattern in quotes");
                };
                let pattern: Vec<char> = key(&pattern).chars().collect();
                let matches = |tuple: &Tuple| {
                    let name: Vec<char> = key(self.name(tuple[0])).chars().collect();
                    wildcard(&pattern, &name)
                };
                let set = self.set(&arguments[0], context)?;
                Ok(Datum::Set(set.into_iter().filter(matches).collect()))
            }
            "TM1SORT" => {
                count(1..=2)?;
                let mut set = self.set(&arguments[0], context)?;
                let name = |tuple: &Tuple| key(self.name(tuple[0]));
                match flag(arguments.get(1)).as_deref() {
                    None | Some("ASC") => set.sort_by_key(name),
                    Some("DESC") => set.sort_by_key(|tuple| std::cmp::Reverse(name(tuple))),
                    Some(other) => return fail(format!("unknown TM1SORT flag {other}")),
                }
                Ok(Datum::Set(set))
            }
            "ISLEAF" => {
                count(1..=1)?;
                let member = self.member(&arguments[0], context)?;
                let leaf = self
                    .element(member)
                    .is_ok_and(|(_, e)| e.children.is_empty());
                Ok(Datum::Bool(leaf))
            }
            "COALESCEEMPTY" => {
                count(1..=usize::MAX)?;
                for argument in arguments {
                    let value = self.evaluate(argument, context)?;
                    let value = self.scalar(value, context)?;
                    if !is_empty(&value) {
                        return Ok(value);
                    }
                }
                Ok(Datum::Null)
            }
            "CROSSJOIN" => {
                count(2..=usize::MAX)?;
                let mut joined = self.set(&arguments[0], context)?;
                for argument in &arguments[1..] {
                    joined = cross_join(&joined, &self.set(argument, context)?)?;
                }
                Ok(Datum::Set(joined))
            }
            "FILTER" => {
                count(2..=2)?;
                let mut kept = Vec::new();
                for tuple in self.set(&arguments[0], context)? {
                    if self.truth(&arguments[1], &with(context, &tuple))? {
                        kept.push(tuple);
                    }
                }
                Ok(Datum::Set(kept))
            }
            "NONEMPTY" => {
                count(1..=2)?;
                let across = match arguments.get(1) {
                    Some(argument) => self.set(argument, context)?,
                    None => vec![Tuple::new()],
                };
                let mut kept = Vec::new();
                for tuple in self.set(&arguments[0], context)? {
                    let context = with(context, &tuple);
                    let mut cells = across.iter().map(|other| self.cell(&with(&context, other)));
                    if cells.try_fold(false, |any, cell| Ok(any || !is_empty(&cell?)))? {
                        kept.push(tuple);
                    }
                }
                Ok(Datum::Set(kept))
            }
            "ORDER" => {
                count(2..=3)?;
                self.order(arguments, context).map(Datum::Set)
            }
            "HIERARCHIZE" => {
                count(1..=2)?;
                let post = match flag(arguments.get(1)).as_deref() {
                    None => false,
                    Some("POST") => true,
                    Some(other) => return fail(format!("unknown HIERARCHIZE flag {other}")),
                };
                let mut set = self.set(&arguments[0], context)?;
                let mut orders: HashMap<usize, Vec<usize>> = HashMap::new();
                let mut place = |member: &Member| match member.id {
                    Id::Element(id) => {
                        let order = orders.entry(member.position);
                        order.or_insert_with(|| self.hierarchy_order(member.position, post))
                            [id as usize]
                    }
                    // Calculated members follow the hierarchy, in the order they were defined.
                    Id::Calculated(index) => usize::MAX - self.calculated.len() + index,
                };
                let places: Vec<Vec<usize>> = set
                    .iter()
                    .map(|tuple| tuple.iter().map(&mut place).collect())
                    .collect();
                let mut order: Vec<usize> = (0..set.len()).collect();
                order.sort_by(|&a, &b| places[a].cmp(&places[b]));
                set = order.into_iter().map(|index| set[index].clone()).collect();
                Ok(Datum::Set(set))
            }
            "DESCENDANTS" => {
                count(1..=3)?;
                let member = self.member(&arguments[0], context)?;
                let distance = match arguments.get(1) {
                    Some(argument) => Some(self.number(argument, context)? as usize),
                    None => None,
                };
                let which = flag(arguments.get(2)).unwrap_or_else(|| "SELF".to_owned());
                if !["SELF", "AFTER", "SELF_AND_AFTER", "LEAVES"].contains(&which.as_str()) {
                    return fail(format!("unknown DESCENDANTS flag {which}"));
                }
                // Without a distance, the member and everything below it.
                let wanted = |depth: usize, leaf: bool| match (which.as_str(), distance) {
                    ("LEAVES", _) => leaf,
                    (_, None) => true,
                    ("AFTER", Some(distance)) => depth > distance,
                    ("SELF_AND_AFTER", Some(distance)) => depth >= distance,
                    (_, Some(distance)) => depth == distance,
                };
                let (id, _) = self.element(member)?;
                let dimension = self.dimension(member.position);
                let mut found = Vec::new();
                let mut seen = HashSet::new();
                let mut pending = vec![(id, 0)];
                // Parents before children, children in their declared order.
                while let Some((id, depth)) = pending.pop() {
                    let element = &dimension.elements[id as usize];
                    if wanted(depth, element.children.is_empty()) && seen.insert(id) {
                        found.push(vec![Member {
                            position: member.position,
                            id: Id::Element(id),
                        }]);
                    }
                    let children = element.children.iter().rev();
                    pending.extend(children.map(|&(child, _)| (child, depth + 1)));
                }
                Ok(Datum::Set(found))
            }
            "ANCESTOR" | "ANCESTORS" => {
                count(2..=2)?;
                let mut member = self.member(&arguments[0], context)?;
                for _ in 0..self.number(&arguments[1], context)? as usize {
                    member = self.parent(member)?;
                }
                Ok(match function {
                    "ANCESTOR" => Datum::Member(member),
                    _ => Datum::Set(vec![vec![member]]),
                })
            }
            "IIF" => {
                count(3..=3)?;
                let chosen = match self.truth(&arguments[0], context)? {
                    true => &arguments[1],
                    false => &arguments[2],
                };
                self.evaluate(chosen, context)
            }
            "ISEMPTY" => {
                count(1..=1)?;
                let value = self.evaluate(&arguments[0], context)?;
                Ok(Datum::Bool(is_empty(&self.scalar(value, context)?)))
            }
            _ => fail(format!("unknown function {function}")),
        }
    }

    fn method(
        &self,
        target: &Expr,
        method: &str,
        arguments: &[Expr],
        context: &Context,
    ) -> Evaluated<Datum> {
        let target = self.evaluate(target, context)?;
        let found = match &target {
            Datum::Member(member) => Some(*member),
            _ => None,
        };
        let member =
            || found.ok_or_else(|| MdxError::new(format!(".{method} applies to a member")));
        let one = |position: usize, id: ElementId| Member {
            position,
            id: Id::Element(id),
        };
        let child = |first: bool| {
            let member = member()?;
            let (_, element) = self.element(member)?;
            let child = match first {
                true => element.children.first(),
                false => element.children.last(),
            };
            let child = child.map(|&(id, _)| one(member.position, id));
            child.map(Datum::Member).ok_or_else(|| {
                MdxError::new(format!("member '{}' has no children", self.name(member)))
            })
        };
        match (method, &target) {
            ("MEMBERS", &Datum::Dimension(position)) => {
                let count = self.dimension(position).elements.len() as ElementId;
                Ok(Datum::Set(
                    (0..count).map(|id| vec![one(position, id)]).collect(),
                ))
            }
            ("ALLMEMBERS", &Datum::Dimension(position)) => {
                let count = self.dimension(position).elements.len() as ElementId;
                let elements = (0..count).map(|id| vec![one(position, id)]);
                let calculated = self.calculated.iter().enumerate();
                let calculated = calculated.filter(|(_, member)| member.position == position);
                let calculated = calculated.map(|(index, _)| {
                    vec![Member {
                        position,
                        id: Id::Calculated(index),
                    }]
                });
                Ok(Datum::Set(elements.chain(calculated).collect()))
            }
            ("ALLMEMBERS", _) => fail(".ALLMEMBERS applies to a dimension"),
            ("COUNT", Datum::Set(set)) => Ok(Datum::Number(set.len() as f64)),
            ("ITEM", Datum::Set(_) | Datum::Tuple(_)) => {
                let [index] = arguments else {
                    return fail(".Item takes one position, counted from zero");
                };
                let index = self.number(index, context)? as usize;
                let item = match &target {
                    Datum::Set(set) => set.get(index).cloned().map(Datum::Tuple),
                    Datum::Tuple(tuple) => tuple.get(index).copied().map(Datum::Member),
                    _ => unreachable!("matched above"),
                };
                item.ok_or_else(|| MdxError::new(format!(".Item({index}) is past the end")))
            }
            ("SIBLINGS", _) => {
                let member = member()?;
                let (_, element) = self.element(member)?;
                let elements = &self.dimension(member.position).elements;
                // The children of the first parent, or for a top element the other tops.
                let siblings: Vec<ElementId> = match element.parents.first() {
                    Some(&parent) => {
                        let children = elements[parent as usize].children.iter();
                        children.map(|&(id, _)| id).collect()
                    }
                    None => {
                        let tops = elements.iter().enumerate();
                        let tops = tops.filter(|(_, element)| element.parents.is_empty());
                        tops.map(|(id, _)| id as ElementId).collect()
                    }
                };
                let siblings = siblings.into_iter();
                Ok(Datum::Set(
                    siblings.map(|id| vec![one(member.position, id)]).collect(),
                ))
            }
            ("CURRENTMEMBER", &Datum::Dimension(position)) => Ok(Datum::Member(Member {
                position,
                id: context[position],
            })),
            ("MEMBERS" | "CURRENTMEMBER", _) => fail(format!(".{method} applies to a dimension")),
            ("CHILDREN", _) => {
                let member = member()?;
                let (_, element) = self.element(member)?;
                let children = element.children.iter();
                Ok(Datum::Set(
                    children
                        .map(|&(id, _)| vec![one(member.position, id)])
                        .collect(),
                ))
            }
            ("PARENT", _) => Ok(Datum::Member(self.parent(member()?)?)),
            ("FIRSTCHILD", _) => child(true),
            ("LASTCHILD", _) => child(false),
            ("NAME", _) => Ok(Datum::Text(self.name(member()?).to_owned())),
            ("UNIQUENAME", _) => Ok(Datum::Text(self.unique_name(member()?))),
            ("VALUE", _) => self.scalar(target, context),
            ("PROPERTIES", _) => {
                let [name] = arguments else {
                    return fail(".Properties takes the name of one property");
                };
                let Datum::Text(name) = self.evaluate(name, context)? else {
                    return fail(".Properties takes the name of a property in quotes");
                };
                self.property(member()?, &name)
            }
            _ => fail(format!("unknown method .{method}")),
        }
    }

    /// A built-in property of a member, or failing that its attribute of that name.
    fn property(&self, member: Member, name: &str) -> Evaluated<Datum> {
        let element = self.element(member).ok();
        let first = |ids: &[ElementId]| {
            let parent = ids.first().map(|&id| Member {
                position: member.position,
                id: Id::Element(id),
            });
            parent.map_or(Datum::Null, |parent| Datum::Text(self.unique_name(parent)))
        };
        Ok(match (name.to_uppercase().as_str(), element) {
            ("MEMBER_NAME", _) => Datum::Text(self.name(member).to_owned()),
            ("MEMBER_UNIQUE_NAME", _) => Datum::Text(self.unique_name(member)),
            ("LEVEL_NUMBER", element) => {
                Datum::Number(element.map_or(0.0, |(_, element)| f64::from(element.level)))
            }
            ("CHILDREN_CARDINALITY", element) => {
                Datum::Number(element.map_or(0.0, |(_, element)| element.children.len() as f64))
            }
            ("PARENT_UNIQUE_NAME", Some((_, element))) => first(&element.parents),
            (_, Some((id, _))) => match self.dimension(member.position).attribute_value(id, name) {
                Some(Value::Number(number)) => Datum::Number(*number),
                Some(Value::Text(text)) => Datum::Text(text.clone()),
                None => Datum::Null,
            },
            (_, None) => Datum::Null,
        })
    }

    fn parent(&self, member: Member) -> Evaluated<Member> {
        let (_, element) = self.element(member)?;
        let parent = element.parents.first().ok_or_else(|| {
            MdxError::new(format!("member '{}' has no parent", self.name(member)))
        })?;
        Ok(Member {
            position: member.position,
            id: Id::Element(*parent),
        })
    }

    /// `Order(set, expression [, ASC | DESC | BASC | BDESC])`. The first two keep each member
    /// under its parent and order siblings by value; the last two order by value alone.
    fn order(&self, arguments: &[Expr], context: &Context) -> Evaluated<Vec<Tuple>> {
        let set = self.set(&arguments[0], context)?;
        let (descending, hierarchical) = match flag(arguments.get(2)).as_deref() {
            None | Some("ASC") => (false, true),
            Some("DESC") => (true, true),
            Some("BASC") => (false, false),
            Some("BDESC") => (true, false),
            Some(other) => return fail(format!("unknown ORDER flag {other}")),
        };
        let value = |tuple: &Tuple| {
            let context = with(context, tuple);
            let value = self.evaluate(&arguments[1], &context)?;
            self.scalar(value, &context)
        };
        // For each member of a tuple, the path from its top ancestor down to it, each step
        // with the value the tuple would have with that ancestor in the member's place.
        let path = |tuple: &Tuple, index: usize| -> Evaluated<Vec<(Datum, Id)>> {
            let mut steps = vec![tuple[index]];
            while let (true, Ok(parent)) = (hierarchical, self.parent(steps[steps.len() - 1])) {
                steps.push(parent);
            }
            let step = |&member: &Member| {
                let mut tuple = tuple.clone();
                tuple[index] = member;
                Ok((value(&tuple)?, member.id))
            };
            steps.iter().rev().map(step).collect()
        };
        let mut keyed = Vec::with_capacity(set.len());
        for tuple in set {
            let paths: Vec<_> = (0..tuple.len())
                .map(|index| path(&tuple, index))
                .collect::<Evaluated<_>>()?;
            keyed.push((paths, tuple));
        }
        let by_value = |left: &(Datum, Id), right: &(Datum, Id)| {
            let ordering = compare(&left.0, &right.0);
            let ordering = match descending {
                true => ordering.reverse(),
                false => ordering,
            };
            // Equal values keep members apart by identity, so a parent's children stay together.
            ordering.then_with(|| left.1.cmp(&right.1))
        };
        let by_path = |left: &Vec<(Datum, Id)>, right: &Vec<(Datum, Id)>| {
            let steps = left
                .iter()
                .zip(right)
                .map(|(left, right)| by_value(left, right));
            let differing = steps.into_iter().find(|ordering| ordering.is_ne());
            differing.unwrap_or_else(|| left.len().cmp(&right.len()))
        };
        keyed.sort_by(|(left, _), (right, _)| {
            let paths = left
                .iter()
                .zip(right)
                .map(|(left, right)| by_path(left, right));
            paths
                .into_iter()
                .find(|ordering| ordering.is_ne())
                .unwrap_or(Ordering::Equal)
        });
        Ok(keyed.into_iter().map(|(_, tuple)| tuple).collect())
    }

    /// The place of each element of a dimension when its tree is walked from the top: parents
    /// before children, or after them when `post` is set. Indexed by element id.
    fn hierarchy_order(&self, position: usize, post: bool) -> Vec<usize> {
        let elements = &self.dimension(position).elements;
        // Each element is placed where it is first met; `None` marks one being walked.
        let mut places: Vec<Option<usize>> = vec![Some(usize::MAX); elements.len()];
        let mut next = 0;
        fn walk(
            elements: &[Element],
            id: usize,
            post: bool,
            places: &mut [Option<usize>],
            next: &mut usize,
        ) {
            if places[id] != Some(usize::MAX) {
                return;
            }
            places[id] = None;
            if !post {
                places[id] = Some(*next);
                *next += 1;
            }
            for &(child, _) in &elements[id].children {
                walk(elements, child as usize, post, places, next);
            }
            if post {
                places[id] = Some(*next);
                *next += 1;
            }
        }
        for (id, element) in elements.iter().enumerate() {
            if element.parents.is_empty() {
                walk(elements, id, post, &mut places, &mut next);
            }
        }
        places
            .into_iter()
            .map(|place| place.unwrap_or(usize::MAX))
            .collect()
    }

    /// The value of the cell at a context. A calculated member there is evaluated first.
    fn cell(&self, context: &Context) -> Evaluated<Datum> {
        // Of several calculated members, the one with the highest solve order is calculated
        // last, over the results of the others; among equals, the first in the cube's order.
        let calculated = context.iter().filter_map(|id| match id {
            Id::Calculated(index) => Some(&self.calculated[*index]),
            Id::Element(_) => None,
        });
        let calculated =
            calculated.reduce(
                |chosen, other| match other.solve_order > chosen.solve_order {
                    true => other,
                    false => chosen,
                },
            );
        let Some(calculated) = calculated else {
            let at: Vec<ElementId> = context
                .iter()
                .map(|id| match id {
                    Id::Element(id) => *id,
                    Id::Calculated(_) => unreachable!("checked above"),
                })
                .collect();
            return Ok(match (self.read)(&at) {
                Value::Number(number) => Datum::Number(number),
                Value::Text(text) => Datum::Text(text),
            });
        };
        if self.depth.get() == DEEPEST {
            let name = calculated.name;
            return fail(format!("calculated member '{name}' refers to itself"));
        }
        self.depth.set(self.depth.get() + 1);
        let value = self.evaluate(calculated.expression, context);
        let value = value.and_then(|value| self.scalar(value, context));
        self.depth.set(self.depth.get() - 1);
        value
    }

    /// Reduces a value to a number, text, truth or nothing: a member or tuple gives its cell.
    fn scalar(&self, datum: Datum, context: &Context) -> Evaluated<Datum> {
        match datum {
            Datum::Member(member) => self.cell(&with(context, &vec![member])),
            Datum::Tuple(tuple) => self.cell(&with(context, &tuple)),
            Datum::Set(_) | Datum::Dimension(_) => fail("expected a value, found a set"),
            scalar => Ok(scalar),
        }
    }

    fn number(&self, expression: &Expr, context: &Context) -> Evaluated<f64> {
        let value = self.evaluate(expression, context)?;
        to_number(&self.scalar(value, context)?)
    }

    fn truth(&self, expression: &Expr, context: &Context) -> Evaluated<bool> {
        let value = self.evaluate(expression, context)?;
        match self.scalar(value, context)? {
            Datum::Bool(truth) => Ok(truth),
            _ => fail("expected a condition"),
        }
    }

    fn member(&self, expression: &Expr, context: &Context) -> Evaluated<Member> {
        match self.evaluate(expression, context)? {
            Datum::Member(member) => Ok(member),
            _ => fail("expected a member"),
        }
    }

    fn set(&self, expression: &Expr, context: &Context) -> Evaluated<Vec<Tuple>> {
        match self.evaluate(expression, context)? {
            Datum::Set(set) => Ok(set),
            Datum::Tuple(tuple) => Ok(vec![tuple]),
            Datum::Member(member) => Ok(vec![vec![member]]),
            Datum::Dimension(position) => {
                let name = &self.dimension(position).name;
                fail(format!(
                    "expected a set; for every member of '{name}' use .Members"
                ))
            }
            _ => fail("expected a set"),
        }
    }

    /// A member as reported on an axis, with the attributes asked for.
    fn describe(&self, member: Member, properties: &[Vec<String>]) -> MemberResult {
        let dimension = self.dimension(member.position);
        let element = self.element(member).ok();
        let mut values = BTreeMap::new();
        for property in properties {
            let [dimensions @ .., name] = property.as_slice() else {
                continue;
            };
            let applies = dimensions.iter().all(|d| key(d) == key(&dimension.name));
            let value = element.and_then(|(id, _)| dimension.attribute_value(id, name));
            if let (true, Some(value)) = (applies, value) {
                values.insert(name.clone(), value.clone());
            }
        }
        MemberResult {
            dimension: dimension.name.clone(),
            name: self.name(member).to_owned(),
            unique_name: self.unique_name(member),
            kind: match element.map(|(_, element)| element.kind) {
                Some(ElementType::Numeric) => MemberType::Numeric,
                Some(ElementType::Text) => MemberType::Text,
                Some(ElementType::Consolidated) => MemberType::Consolidated,
                None => MemberType::Calculated,
            },
            level: element.map_or(0, |(_, element)| element.level),
            properties: values,
        }
    }
}

fn to_number(datum: &Datum) -> Evaluated<f64> {
    match datum {
        Datum::Number(number) => Ok(*number),
        Datum::Null => Ok(0.0),
        Datum::Text(text) => fail(format!("'{text}' is not a number")),
        _ => fail("expected a number"),
    }
}

fn same_dimensions(left: &Tuple, right: &Tuple) -> bool {
    let positions = |tuple: &Tuple| tuple.iter().map(|m| m.position).collect::<Vec<_>>();
    positions(left) == positions(right)
}

/// A set without its repeated tuples, each kept where it first appears.
fn distinct(set: Vec<Tuple>) -> Vec<Tuple> {
    let mut seen = HashSet::new();
    set.into_iter()
        .filter(|tuple| seen.insert(tuple.clone()))
        .collect()
}

/// The left set followed by the right, without repeats unless they are asked for.
fn union(mut left: Vec<Tuple>, right: Vec<Tuple>, repeats: bool) -> Evaluated<Vec<Tuple>> {
    if let (Some(left), Some(right)) = (left.first(), right.first())
        && !same_dimensions(left, right)
    {
        return fail("the sets of a union must have the same dimensions");
    }
    left.extend(right);
    Ok(match repeats {
        true => left,
        false => distinct(left),
    })
}

/// Whether text matches a pattern in which `*` stands for any run of characters and `?` for
/// any one.
fn wildcard(pattern: &[char], text: &[char]) -> bool {
    match (pattern, text) {
        ([], []) => true,
        (['*', rest @ ..], _) => (0..=text.len()).any(|skipped| wildcard(rest, &text[skipped..])),
        ([p, rest @ ..], [c, text @ ..]) => (*p == '?' || p == c) && wildcard(rest, text),
        _ => false,
    }
}

/// Every tuple of the left set joined with every tuple of the right.
fn cross_join(left: &[Tuple], right: &[Tuple]) -> Evaluated<Vec<Tuple>> {
    let shared = |left: &Tuple, right: &Tuple| {
        left.iter()
            .any(|a| right.iter().any(|b| a.position == b.position))
    };
    if let (Some(left), Some(right)) = (left.first(), right.first())
        && shared(left, right)
    {
        return fail("the sets of a cross join must have different dimensions");
    }
    let mut joined = Vec::with_capacity(left.len() * right.len());
    for left in left {
        for right in right {
            joined.push([left.as_slice(), right].concat());
        }
    }
    Ok(joined)
}
