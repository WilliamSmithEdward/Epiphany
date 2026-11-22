//! Evaluates a parsed query against one cube.

use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};

use super::parser::{Axis, Expr, Query};
use super::{AxisResult, MdxError, MemberResult, MemberType, QueryResult};
use crate::Scalar;
use crate::model::{Cube, Dimension, DimensionType, Element, ElementId, ElementType, Model, key};

mod sparse;

use sparse::Laid;

/// The most cells one query may return.
const MOST_CELLS: usize = 1_000_000;
/// The most tuples a set may hold while a query is worked out. A cross join multiplies, and
/// without a limit one short query could ask for more memory than there is.
const MOST_TUPLES: usize = 1_000_000;
/// The most steps one query, or one rule at one cell, may take to work out: every part of an
/// expression worked out is a step. Calculated members that each use the one before twice
/// double the work at every member, and a few dozen of them would otherwise never finish.
const MOST_STEPS: u64 = 100_000_000;
/// The most characters a text worked out may hold, which the same doubling would outgrow.
const MOST_TEXT: usize = 1_000_000;
/// The words that tell a function how to go about its work. Where one of them stands alone
/// it is that word, even in a model with a dimension of the same name; the dimension is
/// then written in brackets with its hierarchy, as `[Desc].[Desc]`.
const FLAGS: [&str; 13] = [
    "ALL",
    "ASC",
    "DESC",
    "BASC",
    "BDESC",
    "POST",
    "RECURSIVE",
    "SELF",
    "AFTER",
    "SELF_AND_AFTER",
    "LEAVES",
    "INCLUDEEMPTY",
    "EXCLUDEEMPTY",
];
/// How deeply calculated members may refer to one another.
const DEEPEST: u32 = 32;
/// How much of the thread's stack they may take in doing so, in bytes. A member worked out
/// inside another uses stack in proportion to how much its expression nests, and a build
/// without optimisation uses several times more; a thread that runs out ends the process.
const MOST_STACK: usize = 512 * 1024;

pub(super) type Evaluated<T> = Result<T, MdxError>;
/// Reads a cell of a cube, by the cube's position in the model.
pub(crate) type Read<'a> = &'a dyn Fn(usize, &[ElementId]) -> Evaluated<Scalar>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum Id {
    Element(ElementId),
    /// A calculated member, by its position in the query.
    Calculated(usize),
}

/// A member of the dimension at a position in the cube.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct Member {
    pub(super) position: usize,
    pub(super) id: Id,
}

/// Members of different dimensions.
pub(super) type Tuple = Vec<Member>;
/// One member per dimension of the cube: the cell an expression is evaluated at.
pub(super) type Context = Vec<Id>;

/// What an expression evaluates to.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Datum {
    Null,
    /// A rule's way of saying the cell keeps its stored or consolidated value.
    Stet,
    /// A rule's way of passing the cell on to the next rule.
    Continue,
    Number(f64),
    Text(String),
    Bool(bool),
    /// A dimension of the cube, by position.
    Dimension(usize),
    /// A level of a dimension: the dimension's position, and the level counted up from the
    /// leaves, which are level zero.
    Level(usize, u32),
    Member(Member),
    Tuple(Tuple),
    Set(Vec<Tuple>),
}

struct Calculated<'a> {
    position: usize,
    name: &'a str,
    expression: Expr,
    solve_order: f64,
    /// The members and tuples the expression names outright, where they are the same at
    /// every cell and so need finding only once.
    named: Option<Vec<Tuple>>,
}

pub(super) struct Evaluator<'a> {
    pub(super) model: &'a Model,
    /// The cube's position in the model.
    pub(super) cube_id: usize,
    pub(super) cube: &'a Cube,
    read: Read<'a>,
    /// Told which cells of the cube are about to be read, so that they can be added up together.
    expect: Expect<'a>,
    /// Shows the cells of the cube that may hold something, if it can.
    occupied: Occupied<'a>,
    calculated: Vec<Calculated<'a>>,
    sets: HashMap<String, Vec<Tuple>>,
    /// How many calculated members are being evaluated inside one another.
    depth: Cell<u32>,
    /// How many steps have been taken.
    steps: Cell<u64>,
    /// Where the stack stood when the outermost calculated member was begun.
    began: Cell<usize>,
}

/// Takes the cells of a cube that are about to be read.
pub(crate) type Expect<'a> = &'a dyn Fn(&[&[ElementId]]);
/// Shows every leaf cell of the cube that may hold something, unless there are more than a
/// number of them, or says that it cannot.
pub(crate) type Occupied<'a> = &'a dyn Fn(usize, &mut dyn FnMut(&[ElementId])) -> bool;

/// For work whose cells are read one at a time.
pub(super) fn unexpected(_: &[&[ElementId]]) {}

pub(super) fn fail<T>(message: impl Into<String>) -> Evaluated<T> {
    Err(MdxError::new(message))
}

/// A cell reader for work that must not depend on cell values: every cell is empty.
pub(super) fn no_cells(_: usize, _: &[ElementId]) -> Evaluated<Scalar> {
    Ok(Scalar::Number(0.0))
}

/// The elements a set over a one-dimension cube selects, reading every cell as empty.
pub(super) fn elements(model: &Model, cube: &Cube, set: &Expr) -> Evaluated<Vec<ElementId>> {
    let evaluator = Evaluator::new(model, usize::MAX, cube, &no_cells);
    let top = evaluator
        .dimension(0)
        .elements
        .iter()
        .position(|e| e.parents.is_empty());
    let context = vec![Id::Element(top.unwrap_or(0) as ElementId)];
    let element = |tuple: Tuple| match tuple.as_slice() {
        [
            Member {
                id: Id::Element(id),
                ..
            },
        ] => Ok(*id),
        _ => fail("a subset selects elements of its dimension"),
    };
    evaluator
        .set(set, &context)?
        .into_iter()
        .map(element)
        .collect()
}

impl Query {
    /// Runs the query against a cube, reading stored and consolidated cells through `read`,
    /// after saying through `expect` which cells of the cube the axes will ask for.
    /// `standing` names members, `(dimension, member)`, that the query stands at in place of
    /// those its WHERE names for their dimensions, or beside them; where an axis places one
    /// of their dimensions, the axis decides it.
    pub(crate) fn evaluate(
        &self,
        model: &Model,
        cube_id: usize,
        read: Read,
        expect: Expect,
        occupied: Occupied,
        standing: &[(&str, &str)],
    ) -> Evaluated<QueryResult> {
        let cube = &model.cubes()[cube_id];
        let mut evaluator = Evaluator::new(model, cube_id, cube, read);
        evaluator.expect = expect;
        evaluator.occupied = occupied;
        for member in &self.members {
            let [dimension, .., name] = member.name.as_slice() else {
                return fail("a calculated member is named [Dimension].[Name]");
            };
            evaluator.calculated.push(Calculated {
                position: evaluator.position(dimension)?,
                name,
                expression: member.expression.clone(),
                solve_order: member.solve_order,
                named: None,
            });
        }

        // Every dimension starts at its first top element; the slicer then narrows some.
        let mut context = evaluator.top();
        // The names in an expression are settled once, just before it is first worked out,
        // and not again at every cell or tuple it is worked out for.
        for (name, expression) in &self.sets {
            let set = evaluator.set(&evaluator.settled(expression), &context)?;
            evaluator.sets.insert(key(name), set);
        }
        // Those of the calculated members wait until every member and set is known.
        for index in 0..evaluator.calculated.len() {
            let settled = evaluator.settled(&evaluator.calculated[index].expression);
            if fixed(&settled) {
                let mut named = Vec::new();
                evaluator.named(&settled, &context, &mut named);
                evaluator.calculated[index].named = Some(named);
            }
            evaluator.calculated[index].expression = settled;
        }
        let mut used = HashSet::new();
        let mut claim = |tuple: &Tuple| match tuple.iter().all(|m| used.insert(m.position)) {
            true => Ok(()),
            false => fail("a dimension is used on more than one axis or in the slicer"),
        };
        let mut slicer = Tuple::new();
        if let Some(expression) = &self.slicer {
            slicer = match evaluator.evaluate(&evaluator.settled(expression), &context)? {
                Datum::Member(member) => vec![member],
                Datum::Tuple(tuple) => tuple,
                _ => return fail("WHERE takes a member or a tuple"),
            };
            claim(&slicer)?;
        }
        // The members to stand at take the slicer's place for their dimensions. They claim no
        // dimension: an axis that places one puts its own members in every cell.
        for &(dimension, name) in standing {
            let named = Expr::Name(vec![dimension.to_owned(), name.to_owned()]);
            let Datum::Member(member) = evaluator.evaluate(&evaluator.settled(&named), &context)?
            else {
                return fail(format!(
                    "[{dimension}].[{name}] is not a member to stand at"
                ));
            };
            match slicer
                .iter_mut()
                .find(|other| other.position == member.position)
            {
                Some(other) => *other = member,
                None => slicer.push(member),
            }
        }
        context = with(&context, &slicer);

        let mut axes: Vec<&Axis> = self.axes.iter().collect();
        axes.sort_by_key(|axis| axis.ordinal);
        if axes
            .iter()
            .enumerate()
            .any(|(index, axis)| axis.ordinal != index)
        {
            return fail("axes are numbered from COLUMNS without gaps or repeats");
        }
        let mut laid = Vec::new();
        let mut placed = HashSet::new();
        for axis in &axes {
            let axis = evaluator.lay(&evaluator.settled(&axis.set), axis.non_empty, &context)?;
            let first: Option<Tuple> = match &axis {
                Laid::Out(set) => set.first().cloned(),
                Laid::Crossed(sets, _) => Some(sets.iter().map(|set| set[0][0]).collect()),
            };
            if let Some(tuple) = first {
                claim(&tuple)?;
                placed.extend(tuple.iter().map(|member| member.position));
            }
            laid.push(axis);
        }
        // A cross join left unwritten is now cut down to its tuples that may hold something,
        // which takes knowing what the other axes hold.
        let mut sets = Vec::new();
        for index in 0..laid.len() {
            let others = laid.iter().enumerate().filter(|&(other, _)| other != index);
            let beside: Vec<&Vec<Tuple>> = others
                .flat_map(|(_, other)| match other {
                    Laid::Out(set) => std::slice::from_ref(set),
                    Laid::Crossed(sets, _) => sets.as_slice(),
                })
                .collect();
            sets.push(evaluator.cut(&laid[index], &beside, &context)?);
        }

        // One cell per combination of a tuple from each axis, the first axis varying fastest.
        let sizes: Vec<usize> = sets.iter().map(Vec::len).collect();
        let places = Places::new(&sizes)?;
        let contexts: Vec<Vec<Id>> = (0..places.count)
            .map(|index| {
                // One copy of the slicer's cell, with each axis's tuple put into it.
                let mut cell = context.clone();
                for (set, place) in sets.iter().zip(places.of(index)) {
                    for member in &set[place] {
                        cell[member.position] = member.id;
                    }
                }
                cell
            })
            .collect();
        let totals: Vec<Vec<ElementId>> = contexts
            .iter()
            .flat_map(|context| evaluator.ahead(context))
            .collect();
        expect(&totals.iter().map(Vec::as_slice).collect::<Vec<_>>());
        let cells = contexts
            .iter()
            .map(|context| evaluator.cell(context))
            .collect::<Evaluated<Vec<Datum>>>()?;

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
            Datum::Number(number) => Some(Scalar::Number(*number)),
            Datum::Text(text) => Some(Scalar::Text(text.clone())),
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
                // A member stands in many tuples of a long axis, and is described once.
                let mut described: HashMap<Member, MemberResult> = HashMap::new();
                let tuple = |&index: &usize| {
                    let members = set[index].iter();
                    let describe = |&member: &Member| {
                        let known = described.entry(member);
                        known
                            .or_insert_with(|| evaluator.describe(member, &axis.properties))
                            .clone()
                    };
                    members.map(describe).collect()
                };
                AxisResult {
                    tuples: kept.iter().map(tuple).collect(),
                }
            });
        Ok(QueryResult {
            cube: cube.name.clone(),
            cells: kept_cells.collect(),
            axes: axes.collect(),
            // A member stood at whose dimension an axis places is not where the cells are.
            slicer: slicer
                .iter()
                .filter(|member| !placed.contains(&member.position))
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

/// A cell moved along the tuples of a set: each tuple is put into the one copy of the cell,
/// in the place of the tuple before it, so that a set of any length costs one copy.
struct Along {
    cell: Context,
    /// What stood in the places the last tuple took.
    before: Vec<(usize, Id)>,
}

impl Along {
    fn new(context: &Context) -> Self {
        Self {
            cell: context.clone(),
            before: Vec::new(),
        }
    }

    /// The cell with this tuple in it and the last one out of it again.
    fn at(&mut self, tuple: &Tuple) -> &Context {
        // Last put in, first put back: right even of a tuple with two of one dimension.
        while let Some((position, id)) = self.before.pop() {
            self.cell[position] = id;
        }
        for member in tuple {
            self.before
                .push((member.position, self.cell[member.position]));
            self.cell[member.position] = member.id;
        }
        &self.cell
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
    pub(super) fn new(model: &'a Model, cube_id: usize, cube: &'a Cube, read: Read<'a>) -> Self {
        Self {
            model,
            cube_id,
            cube,
            read,
            expect: &unexpected,
            occupied: &|_, _| false,
            calculated: Vec::new(),
            sets: HashMap::new(),
            depth: Cell::new(0),
            steps: Cell::new(0),
            began: Cell::new(0),
        }
    }

    /// The cell every dimension starts at: its first top element.
    pub(super) fn top(&self) -> Context {
        let top = |dimension: &Dimension| {
            let top = dimension.elements.iter().position(|e| e.parents.is_empty());
            Id::Element(top.unwrap_or(0) as ElementId)
        };
        self.dimensions().map(top).collect()
    }

    /// A set in the order of its hierarchies: each member before what is beneath it, or
    /// after it if `post`.
    fn hierarchized(&self, set: Vec<Tuple>, post: bool) -> Vec<Tuple> {
        let mut orders: HashMap<usize, Vec<usize>> = HashMap::new();
        let mut place = |member: &Member| match member.id {
            Id::Element(id) => {
                let order = orders.entry(member.position);
                order.or_insert_with(|| self.hierarchy_order(member.position, post))[id as usize]
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
        order.into_iter().map(|index| set[index].clone()).collect()
    }

    /// An expression with its names settled, or as it stands if one of them means nothing:
    /// that one is then left to fail where it is used, if it ever is.
    fn settled(&self, expression: &Expr) -> Expr {
        self.bind(expression).unwrap_or_else(|_| expression.clone())
    }

    /// A copy of an expression with every name that can be settled now replaced by what it
    /// names, so that evaluating it again and again does no lookups by name. It also reports
    /// names that mean nothing. Names inside `LookupCube` belong to another cube and are left.
    pub(super) fn bind(&self, expression: &Expr) -> Evaluated<Expr> {
        let all = |items: &[Expr]| {
            items
                .iter()
                .map(|item| self.bind(item))
                .collect::<Evaluated<Vec<_>>>()
        };
        let one = |item: &Expr| self.bind(item).map(Box::new);
        Ok(match expression {
            Expr::Name(parts) => match (parts.as_slice(), self.resolve(parts)) {
                ([word], _) if ["STET", "CONTINUE"].contains(&&*word.to_uppercase()) => {
                    Expr::Call(word.to_uppercase(), Vec::new())
                }
                ([word], _) if FLAGS.contains(&&*word.to_uppercase()) => expression.clone(),
                (
                    _,
                    Ok(Datum::Member(Member {
                        position,
                        id: Id::Element(id),
                    })),
                ) => Expr::Member(position, id),
                (
                    _,
                    Ok(Datum::Member(Member {
                        position,
                        id: Id::Calculated(index),
                    })),
                ) => Expr::Calculated(position, index),
                (_, Ok(Datum::Dimension(position))) => Expr::Dimension(position),
                // A single word that names nothing may be a flag such as DESC.
                ([_], Err(_)) => expression.clone(),
                (_, Err(error)) => return Err(error),
                (_, Ok(_)) => expression.clone(),
            },
            Expr::Call(function, arguments) if function == "LOOKUPCUBE" => {
                self.lookup(arguments, &self.top())?;
                expression.clone()
            }
            Expr::Call(function, arguments) => Expr::Call(function.clone(), all(arguments)?),
            Expr::Method(target, method, arguments) => {
                Expr::Method(one(target)?, method.clone(), all(arguments)?)
            }
            Expr::Set(items) => Expr::Set(all(items)?),
            Expr::Tuple(items) => Expr::Tuple(all(items)?),
            Expr::Not(operand) => Expr::Not(one(operand)?),
            Expr::Negate(operand) => Expr::Negate(one(operand)?),
            Expr::Binary(left, operator, right) => Expr::Binary(one(left)?, operator, one(right)?),
            Expr::Number(_)
            | Expr::Text(_)
            | Expr::Member(..)
            | Expr::Calculated(..)
            | Expr::Dimension(_) => expression.clone(),
        })
    }

    /// The cell `LookupCube("Cube", member, ...)` refers to. Each dimension the other cube
    /// shares with this one starts at this cell's element; its other dimensions start at their
    /// top. The members then move the dimensions they belong to: a name is looked up in the
    /// other cube, and anything else is worked out here and carried across.
    pub(super) fn lookup(
        &self,
        arguments: &[Expr],
        context: &Context,
    ) -> Evaluated<(usize, Vec<ElementId>)> {
        let [Expr::Text(name), members @ ..] = arguments else {
            return fail("LookupCube takes a cube name in quotes, then members");
        };
        let unknown = || MdxError::new(format!("unknown cube '{name}'"));
        let cube_id = self.model.cube_id(name).ok_or_else(unknown)?;
        let cube = &self.model.cubes()[cube_id];
        let other = Evaluator::new(self.model, cube_id, cube, self.read);
        let shared = |dimension: u32| self.cube.dimensions.iter().position(|&d| d == dimension);
        let element = |id: Id| match id {
            Id::Element(id) => Ok(id),
            Id::Calculated(_) => fail("LookupCube cannot carry a calculated member across"),
        };
        let mut at = Vec::with_capacity(cube.dimensions.len());
        for (&dimension, top) in cube.dimensions.iter().zip(other.top()) {
            at.push(element(
                shared(dimension).map_or(top, |position| context[position]),
            )?);
        }
        for member in members {
            let named = match member {
                Expr::Name(parts) => other.resolve(parts).ok(),
                _ => None,
            };
            let (dimension, id) = match named {
                Some(Datum::Member(member)) => {
                    (cube.dimensions[member.position], element(member.id)?)
                }
                _ => {
                    let member = self.member(member, context)?;
                    (self.cube.dimensions[member.position], element(member.id)?)
                }
            };
            let position = cube.dimensions.iter().position(|&d| d == dimension);
            let position = position.ok_or_else(|| {
                let dimension = &self.model.dimensions()[dimension as usize].name;
                MdxError::new(format!(
                    "cube '{}' has no dimension '{dimension}'",
                    cube.name
                ))
            })?;
            at[position] = id;
        }
        Ok((cube_id, at))
    }

    /// The cell an expression points at: a member or tuple of this cube, or a `LookupCube`.
    pub(super) fn target(
        &self,
        expression: &Expr,
        context: &Context,
    ) -> Evaluated<(usize, Vec<ElementId>)> {
        if let Expr::Call(function, arguments) = expression
            && function == "LOOKUPCUBE"
        {
            return self.lookup(arguments, context);
        }
        let tuple = match self.evaluate(expression, context)? {
            Datum::Member(member) => vec![member],
            Datum::Tuple(tuple) => tuple,
            _ => return fail("expected a member, a tuple or a LookupCube"),
        };
        let elements = with(context, &tuple).into_iter().map(|id| match id {
            Id::Element(id) => Ok(id),
            Id::Calculated(_) => fail("expected elements, found a calculated member"),
        });
        Ok((self.cube_id, elements.collect::<Evaluated<_>>()?))
    }

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
            None => match self.dimension(position).level(name) {
                Some(level) => Ok(Datum::Level(position, level)),
                None => fail(format!("dimension '{first}' has no member '{name}'")),
            },
        }
    }

    pub(super) fn evaluate(&self, expression: &Expr, context: &Context) -> Evaluated<Datum> {
        self.steps.set(self.steps.get() + 1);
        if self.steps.get() > MOST_STEPS {
            return fail(format!(
                "more than {MOST_STEPS} steps are needed to work this out"
            ));
        }
        Ok(match expression {
            Expr::Number(number) => Datum::Number(*number),
            Expr::Text(text) => Datum::Text(text.clone()),
            Expr::Name(parts) => self.resolve(parts)?,
            &Expr::Member(position, id) => Datum::Member(Member {
                position,
                id: Id::Element(id),
            }),
            &Expr::Calculated(position, index) => Datum::Member(Member {
                position,
                id: Id::Calculated(index),
            }),
            &Expr::Dimension(position) => Datum::Dimension(position),
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
                    let member = match self.evaluate(item, context)? {
                        // A place with no member in it, such as the member before the
                        // first, leaves the tuple without a cell.
                        Datum::Null => return Ok(Datum::Null),
                        Datum::Member(member) => member,
                        _ => return fail("expected a member"),
                    };
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
        if operator == "XOR" {
            let one = self.truth(left, context)? != self.truth(right, context)?;
            return Ok(Datum::Bool(one));
        }
        let (left, right) = (
            self.evaluate(left, context)?,
            self.evaluate(right, context)?,
        );
        match (operator, &left, &right) {
            // The same member, tuple, level or dimension, or nothing on both sides: what
            // the two are, where `=` compares the values of their cells.
            ("IS", ..) => return Ok(Datum::Bool(left == right)),
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
            return bounded_text(format!("{left}{right}"));
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
        let measure = |context: &Context| match arguments.get(1) {
            Some(expression) => {
                let value = self.evaluate(expression, context)?;
                self.scalar(value, context)
            }
            None => self.cell(context),
        };
        let all = |argument: Option<&Expr>| match flag(argument).as_deref() {
            None => Ok(false),
            Some("ALL") => Ok(true),
            Some(other) => fail(format!("unknown {function} flag {other}")),
        };
        let whole = |argument: &Expr| Ok(self.number(argument, context)?.max(0.0) as usize);
        match function {
            "SUM" | "AGGREGATE" | "AVG" | "MIN" | "MAX" | "MEDIAN" | "VAR" | "VARIANCE"
            | "VARP" | "VARIANCEP" | "STDEV" | "STDDEV" | "STDEVP" | "STDDEVP" => {
                count(1..=2)?;
                // As everywhere in the engine, a cell holding zero is an empty cell, so the
                // average, the least and greatest, the middle and the spread are taken over
                // the cells that hold something.
                let mut values = Vec::new();
                let set = self.set(&arguments[0], context)?;
                self.foresee(arguments.get(1), context, &set);
                let mut along = Along::new(context);
                for tuple in set {
                    let value = measure(along.at(&tuple))?;
                    if !is_empty(&value) {
                        values.push(to_number(&value)?);
                    }
                }
                let total: f64 = values.iter().sum();
                let count = values.len() as f64;
                let mean = total / count;
                // Of a sample the squares are shared among one fewer than there are, which
                // one value alone cannot be; of a population, among all of them.
                let spread = |sample: bool| {
                    let squares: f64 = values.iter().map(|value| (value - mean).powi(2)).sum();
                    let among = count - f64::from(u8::from(sample));
                    (among > 0.0).then(|| squares / among)
                };
                let number = |value: Option<f64>| value.map_or(Datum::Null, Datum::Number);
                Ok(match (function, values.is_empty()) {
                    ("SUM" | "AGGREGATE", _) => Datum::Number(total),
                    (_, true) => Datum::Null,
                    ("AVG", _) => Datum::Number(mean),
                    ("MIN", _) => Datum::Number(values.into_iter().fold(f64::INFINITY, f64::min)),
                    ("MAX", _) => {
                        Datum::Number(values.into_iter().fold(f64::NEG_INFINITY, f64::max))
                    }
                    ("MEDIAN", _) => {
                        values.sort_by(f64::total_cmp);
                        let middle = values.len() / 2;
                        Datum::Number(match values.len() % 2 {
                            0 => (values[middle - 1] + values[middle]) / 2.0,
                            _ => values[middle],
                        })
                    }
                    ("VAR" | "VARIANCE", _) => number(spread(true)),
                    ("VARP" | "VARIANCEP", _) => number(spread(false)),
                    ("STDEV" | "STDDEV", _) => number(spread(true).map(f64::sqrt)),
                    _ => number(spread(false).map(f64::sqrt)),
                })
            }
            "COUNT" => {
                count(1..=2)?;
                let set = self.set(&arguments[0], context)?;
                let counted = match flag(arguments.get(1)).as_deref() {
                    None | Some("INCLUDEEMPTY") => set.len(),
                    Some("EXCLUDEEMPTY") => {
                        self.foresee(None, context, &set);
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
                let set = self.set(&arguments[0], context)?;
                if arguments.len() == 3 {
                    self.foresee(arguments.get(2), context, &set);
                }
                let mut along = Along::new(context);
                for tuple in set {
                    let value = match arguments.get(2) {
                        Some(expression) => {
                            let context = along.at(&tuple);
                            let value = self.evaluate(expression, context)?;
                            self.scalar(value, context)?
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
            "TOPSUM" | "BOTTOMSUM" | "TOPPERCENT" | "BOTTOMPERCENT" => {
                count(3..=3)?;
                let set = self.set(&arguments[0], context)?;
                let wanted = self.number(&arguments[1], context)?;
                self.foresee(arguments.get(2), context, &set);
                let mut keyed = Vec::new();
                let mut along = Along::new(context);
                for tuple in set {
                    let context = along.at(&tuple);
                    let value = self.evaluate(&arguments[2], context)?;
                    let value = match self.scalar(value, context)? {
                        Datum::Null => 0.0,
                        value => to_number(&value)?,
                    };
                    keyed.push((value, tuple));
                }
                keyed.sort_by(|(left, _), (right, _)| match function.starts_with("TOP") {
                    true => right.total_cmp(left),
                    false => left.total_cmp(right),
                });
                // From the greatest down, or the least up, as many as it takes for their
                // values to come to the amount: a sum, or a percentage of the whole.
                let whole: f64 = keyed.iter().map(|(value, _)| value).sum();
                let amount = match function.ends_with("PERCENT") {
                    true => whole * wanted / 100.0,
                    false => wanted,
                };
                // An amount below zero, a share of a set that comes to a loss say, is
                // reached from above.
                let there = |reached: f64| match amount >= 0.0 {
                    true => reached >= amount,
                    false => reached <= amount,
                };
                let mut reached = 0.0;
                let mut kept = Vec::new();
                for (value, tuple) in keyed {
                    if there(reached) {
                        break;
                    }
                    reached += value;
                    kept.push(tuple);
                }
                Ok(Datum::Set(kept))
            }
            "GENERATE" => {
                count(2..=3)?;
                let mut generated = Vec::new();
                for tuple in self.set(&arguments[0], context)? {
                    generated.extend(self.set(&arguments[1], &with(context, &tuple))?);
                    bounded(Some(generated.len()))?;
                }
                Ok(Datum::Set(match all(arguments.get(2))? {
                    true => generated,
                    false => distinct(generated),
                }))
            }
            "STET" => Ok(Datum::Stet),
            "CONTINUE" => Ok(Datum::Continue),
            "LOOKUPCUBE" => {
                let (cube, at) = self.lookup(arguments, context)?;
                Ok(match (self.read)(cube, &at)? {
                    Scalar::Number(number) => Datum::Number(number),
                    Scalar::Text(text) => Datum::Text(text),
                })
            }
            "TM1SUBSETTOSET" => {
                // A third argument says whether the subset is public or private; here
                // every subset is public, so it is read past.
                count(2..=3)?;
                let dimension = self.evaluate(&arguments[0], context)?;
                let name = self.evaluate(&arguments[1], context)?;
                let (Datum::Dimension(position), Datum::Text(name)) = (dimension, name) else {
                    return fail("TM1SUBSETTOSET takes a dimension and a subset name in quotes");
                };
                let dimension = self.dimension(position);
                let subset = dimension.subset(&name).ok_or_else(|| {
                    let dimension = &dimension.name;
                    MdxError::new(format!("dimension '{dimension}' has no subset '{name}'"))
                })?;
                let member = |&id: &ElementId| {
                    vec![Member {
                        position,
                        id: Id::Element(id),
                    }]
                };
                Ok(Datum::Set(subset.elements.iter().map(member).collect()))
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
            "TM1SORT" | "TM1SORTBYINDEX" => {
                count(1..=2)?;
                let mut set = self.set(&arguments[0], context)?;
                // By name, or by the place of each member in its dimension.
                let place = |tuple: &Tuple| match (function, tuple[0].id) {
                    ("TM1SORT", _) => (0, key(self.name(tuple[0]))),
                    (_, Id::Element(id)) => (id as usize, String::new()),
                    (_, Id::Calculated(index)) => (usize::MAX, index.to_string()),
                };
                match flag(arguments.get(1)).as_deref() {
                    None | Some("ASC") => set.sort_by_key(place),
                    Some("DESC") => set.sort_by_key(|tuple| std::cmp::Reverse(place(tuple))),
                    Some(other) => return fail(format!("unknown {function} flag {other}")),
                }
                Ok(Datum::Set(set))
            }
            "TM1MEMBER" => {
                count(2..=2)?;
                let tuple = match self.evaluate(&arguments[0], context)? {
                    Datum::Member(member) => vec![member],
                    Datum::Tuple(tuple) => tuple,
                    _ => return fail("TM1MEMBER takes a tuple and a place in it, from zero"),
                };
                let place = whole(&arguments[1])?;
                Ok(tuple
                    .get(place)
                    .map_or(Datum::Null, |&member| Datum::Member(member)))
            }
            "TM1TUPLESIZE" => {
                count(1..=1)?;
                Ok(Datum::Number(
                    match self.evaluate(&arguments[0], context)? {
                        Datum::Member(_) => 1.0,
                        Datum::Tuple(tuple) => tuple.len() as f64,
                        _ => 0.0,
                    },
                ))
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
                let set = self.set(&arguments[0], context)?;
                self.foresee(arguments.get(1), context, &set);
                let mut along = Along::new(context);
                for tuple in set {
                    if self.truth(&arguments[1], along.at(&tuple))? {
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
                // A long cross join is first cut down to what may hold something.
                let laid = self.lay(&arguments[0], true, context)?;
                let beside: &[&Vec<Tuple>] = match arguments.len() {
                    1 => &[],
                    _ => &[&across],
                };
                let set = self.cut(&laid, beside, context)?;
                self.non_empty(set, &across, context).map(Datum::Set)
            }
            "NONEMPTYCROSSJOIN" => {
                count(2..=usize::MAX)?;
                let crossed = Expr::Call("CROSSJOIN".to_owned(), arguments.to_vec());
                let set = self.cut(&self.lay(&crossed, true, context)?, &[], context)?;
                self.non_empty(set, &[Tuple::new()], context)
                    .map(Datum::Set)
            }
            "ADDCALCULATEDMEMBERS" | "STRIPCALCULATEDMEMBERS" => {
                count(1..=1)?;
                let mut set = self.set(&arguments[0], context)?;
                let calculated = |member: &Member| matches!(member.id, Id::Calculated(_));
                if function == "STRIPCALCULATEDMEMBERS" {
                    set.retain(|tuple| !tuple.iter().any(calculated));
                    return Ok(Datum::Set(set));
                }
                // After the set, the calculated members of its dimension that it lacks.
                let position = match set.first().map(Vec::as_slice) {
                    Some(&[member]) => member.position,
                    Some(_) => return fail("ADDCALCULATEDMEMBERS takes a set of one dimension"),
                    None => return Ok(Datum::Set(set)),
                };
                for (index, member) in self.calculated.iter().enumerate() {
                    let added = vec![Member {
                        position,
                        id: Id::Calculated(index),
                    }];
                    if member.position == position && !set.contains(&added) {
                        set.push(added);
                    }
                }
                Ok(Datum::Set(set))
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
                let set = self.set(&arguments[0], context)?;
                Ok(Datum::Set(self.hierarchized(set, post)))
            }
            "DESCENDANTS" => {
                count(1..=3)?;
                let member = self.member(&arguments[0], context)?;
                // How far down: a number of steps, or as far as a level.
                let (mut distance, mut level) = (None, None);
                match arguments
                    .get(1)
                    .map(|argument| self.evaluate(argument, context))
                {
                    Some(Ok(Datum::Level(position, to))) => {
                        self.of_level(member, position)?;
                        level = Some(to);
                    }
                    Some(steps) => {
                        distance = Some(to_number(&self.scalar(steps?, context)?)? as usize);
                    }
                    None => {}
                }
                let which = flag(arguments.get(2)).unwrap_or_else(|| "SELF".to_owned());
                if !["SELF", "AFTER", "SELF_AND_AFTER", "LEAVES"].contains(&which.as_str()) {
                    return fail(format!("unknown DESCENDANTS flag {which}"));
                }
                // Without a distance or a level, the member and everything below it. Levels
                // count up from the leaves, so what comes after a level has a lower number.
                let wanted = |depth: usize, element: &Element| {
                    let (below, at) = match (distance, level) {
                        (Some(distance), _) => (depth > distance, depth == distance),
                        (_, Some(level)) => (element.level < level, element.level == level),
                        _ => (true, true),
                    };
                    match which.as_str() {
                        "LEAVES" => element.children.is_empty(),
                        "AFTER" => below,
                        "SELF_AND_AFTER" => below || at,
                        _ => at,
                    }
                };
                let (id, _) = self.element(member)?;
                let dimension = self.dimension(member.position);
                let mut found = Vec::new();
                let mut seen = HashSet::new();
                let mut pending = vec![(id, 0)];
                // Parents before children, children in their declared order.
                while let Some((id, depth)) = pending.pop() {
                    let element = &dimension.elements[id as usize];
                    if wanted(depth, element) && seen.insert(id) {
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
                // Up to a level, or up a number of steps.
                match self.evaluate(&arguments[1], context)? {
                    Datum::Level(position, level) => {
                        self.of_level(member, position)?;
                        member = self.ancestor_at(member, level)?.ok_or_else(|| {
                            let name = self.name(member);
                            MdxError::new(format!(
                                "member '{name}' has no ancestor at level {level}"
                            ))
                        })?;
                    }
                    steps => {
                        for _ in 0..to_number(&self.scalar(steps, context)?)? as usize {
                            member = self.parent(member)?;
                        }
                    }
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
            "CASE" => {
                // The value beside the first condition that holds; failing them all, the
                // value left over at the end, or nothing.
                let (pairs, otherwise) = arguments.as_chunks::<2>();
                for [condition, value] in pairs {
                    if self.truth(condition, context)? {
                        return self.evaluate(value, context);
                    }
                }
                match otherwise {
                    [otherwise] => self.evaluate(otherwise, context),
                    _ => Ok(Datum::Null),
                }
            }
            "NULL" => {
                count(0..=0)?;
                Ok(Datum::Null)
            }
            "ISANCESTOR" => {
                count(2..=2)?;
                let above = self.member(&arguments[0], context)?;
                let below = self.member(&arguments[1], context)?;
                Ok(Datum::Bool(
                    above.position == below.position && self.beneath(below, above),
                ))
            }
            "EXTRACT" => {
                count(2..=usize::MAX)?;
                let set = self.set(&arguments[0], context)?;
                let mut positions = Vec::new();
                for argument in &arguments[1..] {
                    match self.evaluate(argument, context)? {
                        Datum::Dimension(position) if !positions.contains(&position) => {
                            positions.push(position);
                        }
                        Datum::Dimension(_) => return fail("EXTRACT takes each dimension once"),
                        _ => return fail("EXTRACT takes a set and then dimensions"),
                    }
                }
                // What each tuple holds of those dimensions, in the order they are given,
                // each such tuple once.
                let mut found = Vec::new();
                for tuple in set {
                    let of =
                        |&position: &usize| tuple.iter().find(|member| member.position == position);
                    let kept: Option<Tuple> = positions
                        .iter()
                        .map(of)
                        .map(Option::<&Member>::copied)
                        .collect();
                    match kept {
                        Some(kept) => found.push(kept),
                        None => return fail("EXTRACT takes dimensions of the set"),
                    }
                }
                Ok(Datum::Set(distinct(found)))
            }
            "ISEMPTY" => {
                count(1..=1)?;
                let value = self.evaluate(&arguments[0], context)?;
                Ok(Datum::Bool(is_empty(&self.scalar(value, context)?)))
            }
            "RANK" => {
                count(2..=3)?;
                let tuple = match self.evaluate(&arguments[0], context)? {
                    Datum::Member(member) => vec![member],
                    Datum::Tuple(tuple) => tuple,
                    _ => return fail("RANK takes a member or a tuple, then a set"),
                };
                let set = self.set(&arguments[1], context)?;
                let Some(expression) = arguments.get(2) else {
                    // Without an expression the rank is the tuple's place in the set.
                    let place = set.iter().position(|other| *other == tuple);
                    return Ok(Datum::Number(place.map_or(0.0, |place| place as f64 + 1.0)));
                };
                if !set.contains(&tuple) {
                    return Ok(Datum::Number(0.0));
                }
                self.foresee(Some(expression), context, &set);
                let along = std::cell::RefCell::new(Along::new(context));
                let value = |tuple: &Tuple| {
                    let mut along = along.borrow_mut();
                    let context = along.at(tuple);
                    let value = self.evaluate(expression, context)?;
                    to_number(&self.scalar(value, context)?)
                };
                // One more than the number of tuples with a greater value, so equals share.
                let own = value(&tuple)?;
                let mut rank = 1.0;
                for other in &set {
                    rank += f64::from(value(other)? > own);
                }
                Ok(Datum::Number(rank))
            }
            "STRTOMEMBER" | "STRTOTUPLE" | "STRTOSET" => {
                count(1..=1)?;
                let text = self.text(&arguments[0], context)?;
                let parsed = super::parser::Parser::new(super::lexer::lex(&text)?).set()?;
                match (function, self.evaluate(&parsed, context)?) {
                    ("STRTOMEMBER", member @ Datum::Member(_)) => Ok(member),
                    ("STRTOTUPLE", Datum::Member(member)) => Ok(Datum::Tuple(vec![member])),
                    ("STRTOTUPLE", tuple @ Datum::Tuple(_)) => Ok(tuple),
                    ("STRTOSET", _) => self.set(&parsed, context).map(Datum::Set),
                    _ => fail(format!("{function}: '{text}' is not one")),
                }
            }
            "ABS" | "INT" | "FIX" | "SQR" | "SQRT" | "EXP" | "LOG" | "LOG10" | "SGN" => {
                count(1..=1)?;
                let number = self.number(&arguments[0], context)?;
                let result = match function {
                    "ABS" => number.abs(),
                    "INT" => number.floor(),
                    "FIX" => number.trunc(),
                    "SQR" | "SQRT" => number.sqrt(),
                    "EXP" => number.exp(),
                    "LOG" => number.ln(),
                    "LOG10" => number.log10(),
                    _ => match number {
                        n if n > 0.0 => 1.0,
                        n if n < 0.0 => -1.0,
                        _ => 0.0,
                    },
                };
                // Like a division by zero, a root or logarithm that does not exist is no value.
                Ok(match result.is_finite() {
                    true => Datum::Number(result),
                    false => Datum::Null,
                })
            }
            "ROUND" => {
                count(1..=2)?;
                let number = self.number(&arguments[0], context)?;
                let digits = match arguments.get(1) {
                    Some(digits) => self.number(digits, context)?.trunc(),
                    None => 0.0,
                };
                // Halves round away from zero, as a person rounds them.
                let scale = 10f64.powf(digits);
                Ok(Datum::Number((number * scale).round() / scale))
            }
            "MOD" | "POWER" => {
                count(2..=2)?;
                let left = self.number(&arguments[0], context)?;
                let right = self.number(&arguments[1], context)?;
                let result = match function {
                    "MOD" => left - right * (left / right).floor(),
                    _ => left.powf(right),
                };
                Ok(match result.is_finite() {
                    true => Datum::Number(result),
                    false => Datum::Null,
                })
            }
            "LEN" | "UCASE" | "LCASE" | "TRIM" | "VAL" => {
                count(1..=1)?;
                let text = self.text(&arguments[0], context)?;
                Ok(match function {
                    "LEN" => Datum::Number(text.chars().count() as f64),
                    "UCASE" => Datum::Text(text.to_uppercase()),
                    "LCASE" => Datum::Text(text.to_lowercase()),
                    "TRIM" => Datum::Text(text.trim().to_owned()),
                    // The number a text begins with, or zero if it begins with none.
                    _ => Datum::Number(leading_number(text.trim())),
                })
            }
            "STR" | "CSTR" => {
                count(1..=1)?;
                self.text(&arguments[0], context).map(Datum::Text)
            }
            "LEFT" | "RIGHT" => {
                count(2..=2)?;
                let text: Vec<char> = self.text(&arguments[0], context)?.chars().collect();
                let wanted = whole(&arguments[1])?.min(text.len());
                let kept = match function {
                    "LEFT" => &text[..wanted],
                    _ => &text[text.len() - wanted..],
                };
                Ok(Datum::Text(kept.iter().collect()))
            }
            "MID" => {
                count(2..=3)?;
                let text = self.text(&arguments[0], context)?;
                // Characters are counted from one.
                let skipped = whole(&arguments[1])?.saturating_sub(1);
                let wanted = match arguments.get(2) {
                    Some(length) => whole(length)?,
                    None => usize::MAX,
                };
                Ok(Datum::Text(
                    text.chars().skip(skipped).take(wanted).collect(),
                ))
            }
            "INSTR" => {
                count(2..=2)?;
                let text = self.text(&arguments[0], context)?;
                let sought = self.text(&arguments[1], context)?;
                // The place of the first character of the match, counted from one, or zero.
                let place = text.find(&sought).map(|at| text[..at].chars().count() + 1);
                Ok(Datum::Number(place.unwrap_or(0) as f64))
            }
            "REPLACE" => {
                count(3..=3)?;
                let text = self.text(&arguments[0], context)?;
                let sought = self.text(&arguments[1], context)?;
                let with = self.text(&arguments[2], context)?;
                bounded_text(match sought.is_empty() {
                    true => text,
                    false => text.replace(&sought, &with),
                })
            }
            "COUSIN" => {
                count(2..=2)?;
                let member = self.member(&arguments[0], context)?;
                let other = self.member(&arguments[1], context)?;
                // The member's own ancestor at the level of the one given, then the same
                // way down from the one given.
                let level = self.element(other)?.1.level;
                let cousin = match self.ancestor_at(member, level)? {
                    Some(ancestor) => self.cousin(member, ancestor, other)?,
                    None => None,
                };
                Ok(cousin.map_or(Datum::Null, Datum::Member))
            }
            "PARALLELPERIOD" => {
                count(1..=3)?;
                let (position, level) = self.level(&arguments[0], context)?;
                let back = match arguments.get(1) {
                    Some(back) => self.number(back, context)?,
                    None => 1.0,
                };
                let member = self.member_of(arguments.get(2), position, context)?;
                self.of_level(member, position)?;
                // The member in the same place under the ancestor that many places back.
                let mut parallel = None;
                if let Some(ancestor) = self.ancestor_at(member, level)?
                    && let Some(earlier) = self.shift(ancestor, -back)?
                {
                    parallel = self.cousin(member, ancestor, earlier)?;
                }
                Ok(parallel.map_or(Datum::Null, Datum::Member))
            }
            "PERIODSTODATE" => {
                count(1..=2)?;
                let (position, level) = self.level(&arguments[0], context)?;
                let member = self.member_of(arguments.get(1), position, context)?;
                self.of_level(member, position)?;
                self.periods_to_date(member, level).map(Datum::Set)
            }
            "YTD" | "QTD" | "MTD" | "WTD" => {
                count(0..=1)?;
                // To the member given, or to the current member of the cube's time dimension.
                let member = match arguments.first() {
                    Some(argument) => self.member(argument, context)?,
                    None => self.now(function, context)?,
                };
                let wanted = match function {
                    "YTD" => "Year",
                    "QTD" => "Quarter",
                    "MTD" => "Month",
                    _ => "Week",
                };
                let dimension = self.dimension(member.position);
                let Some(level) = dimension.level(wanted) else {
                    return fail(format!(
                        "{function} needs a level named '{wanted}' in dimension '{}'",
                        dimension.name
                    ));
                };
                self.periods_to_date(member, level).map(Datum::Set)
            }
            "LASTPERIODS" => {
                count(1..=2)?;
                let periods = self.number(&arguments[0], context)?.trunc();
                let member = match arguments.get(1) {
                    Some(argument) => self.member(argument, context)?,
                    None => self.now(function, context)?,
                };
                // That many members of the level ending at the member, or for a negative
                // number beginning at it; fewer where the level ends first.
                let mut found = Vec::new();
                let mut next = (periods != 0.0).then_some(member);
                while let Some(period) = next {
                    found.push(vec![period]);
                    next = match found.len() < periods.abs() as usize {
                        true => self.shift(period, -periods.signum())?,
                        false => None,
                    };
                }
                if periods > 0.0 {
                    found.reverse();
                }
                Ok(Datum::Set(found))
            }
            "OPENINGPERIOD" | "CLOSINGPERIOD" => {
                count(1..=2)?;
                let (position, level) = self.level(&arguments[0], context)?;
                let mut member = self.member_of(arguments.get(1), position, context)?;
                self.of_level(member, position)?;
                // Down by first children, or by last, as far as the level.
                loop {
                    let (_, element) = self.element(member)?;
                    let child = match function {
                        "OPENINGPERIOD" => element.children.first(),
                        _ => element.children.last(),
                    };
                    match (element.level.cmp(&level), child) {
                        (Ordering::Equal, _) => return Ok(Datum::Member(member)),
                        (Ordering::Greater, Some(&(child, _))) => member.id = Id::Element(child),
                        _ => return Ok(Datum::Null),
                    }
                }
            }
            "DRILLDOWNMEMBER" | "TM1DRILLDOWNMEMBER" => {
                count(2..=3)?;
                let recursive = match flag(arguments.get(2)).as_deref() {
                    None => false,
                    Some("RECURSIVE") => true,
                    Some(other) => return fail(format!("unknown {function} flag {other}")),
                };
                let set = self.set(&arguments[0], context)?;
                // Every member, where TM1's function is told ALL; else those of the second set.
                let all = function == "TM1DRILLDOWNMEMBER"
                    && flag(arguments.get(1)).as_deref() == Some("ALL");
                let (position, members) = match all {
                    true => (None, HashSet::new()),
                    false => self.chosen(self.set(&arguments[1], context)?),
                };
                let drilled = |member: Member, _: &Element| all || members.contains(&member);
                self.drill_down(set, position, recursive, &drilled)
                    .map(Datum::Set)
            }
            "DRILLDOWNLEVEL" => {
                count(1..=2)?;
                let set = self.set(&arguments[0], context)?;
                let first = |tuple: &Tuple| tuple.first().copied();
                let opened = match arguments.get(1) {
                    // The members of the level given.
                    Some(level) => {
                        let (position, level) = self.level(level, context)?;
                        let at = |_: Member, element: &Element| element.level == level;
                        self.drill_down(set, Some(position), false, &at)
                    }
                    // The members furthest beneath the top of their dimension.
                    None => {
                        let depths = set
                            .iter()
                            .filter_map(first)
                            .map(|member| self.depth(member));
                        let deepest = depths.max().unwrap_or(0);
                        let at = |member: Member, _: &Element| self.depth(member) == deepest;
                        self.drill_down(set, None, false, &at)
                    }
                };
                opened.map(Datum::Set)
            }
            "DRILLUPMEMBER" => {
                count(2..=2)?;
                let set = self.set(&arguments[0], context)?;
                let (position, members) = self.chosen(self.set(&arguments[1], context)?);
                Ok(Datum::Set(self.drill_up(set, position, &members)))
            }
            "TOGGLEDRILLSTATE" => {
                count(2..=2)?;
                let set = self.set(&arguments[0], context)?;
                let (position, members) = self.chosen(self.set(&arguments[1], context)?);
                let of = |tuple: &Tuple| {
                    let member = tuple
                        .iter()
                        .find(|member| Some(member.position) == position);
                    member.copied()
                };
                // A chosen member is open if what follows it stands beneath it: that one is
                // closed, and any other opened.
                let mut open = HashSet::new();
                for pair in set.windows(2) {
                    if let (Some(member), Some(next)) = (of(&pair[0]), of(&pair[1]))
                        && members.contains(&member)
                        && self.beneath(next, member)
                    {
                        open.insert(member);
                    }
                }
                let set = self.drill_up(set, position, &open);
                let shut = |member: Member, _: &Element| {
                    members.contains(&member) && !open.contains(&member)
                };
                self.drill_down(set, position, false, &shut).map(Datum::Set)
            }
            _ => fail(format!("unknown function {function}")),
        }
    }

    /// The tuples of a set whose cell holds something, here or at any of the tuples across.
    fn non_empty(
        &self,
        set: Vec<Tuple>,
        across: &[Tuple],
        context: &Context,
    ) -> Evaluated<Vec<Tuple>> {
        for other in across {
            self.foresee(None, &with(context, other), &set);
        }
        let mut kept = Vec::new();
        let mut along = Along::new(context);
        for tuple in set {
            let context = along.at(&tuple);
            // Across nothing, which is the usual case, the cell is the tuple's own.
            let mut cells = across.iter().map(|other| match other.is_empty() {
                true => self.cell(context),
                false => self.cell(&with(context, other)),
            });
            if cells.try_fold(false, |any, cell| Ok(any || !is_empty(&cell?)))? {
                kept.push(tuple);
            }
        }
        Ok(kept)
    }

    /// A set without the tuples that follow one of some members and stand beneath it, the
    /// member looked at being the tuple's member of the dimension at `position`.
    fn drill_up(
        &self,
        set: Vec<Tuple>,
        position: Option<usize>,
        members: &HashSet<Member>,
    ) -> Vec<Tuple> {
        let mut closed: Option<Member> = None;
        let mut kept = Vec::new();
        for tuple in set {
            let member = tuple
                .iter()
                .find(|member| Some(member.position) == position)
                .copied();
            let beneath = match (closed, member) {
                (Some(closed), Some(member)) => self.beneath(member, closed),
                _ => false,
            };
            if beneath {
                continue;
            }
            closed = member.filter(|member| members.contains(member));
            kept.push(tuple);
        }
        kept
    }

    /// Whether a member stands beneath another of its dimension, by any of its parents.
    fn beneath(&self, member: Member, other: Member) -> bool {
        let (Id::Element(id), Id::Element(other)) = (member.id, other.id) else {
            return false;
        };
        let elements = &self.dimension(member.position).elements;
        let mut waiting = elements[id as usize].parents.to_vec();
        while let Some(next) = waiting.pop() {
            if next == other {
                return true;
            }
            waiting.extend(&elements[next as usize].parents);
        }
        false
    }

    /// The members of a member's own level beneath its ancestor at a level, in the order of
    /// the hierarchy, as far as the member itself. None if it has no ancestor there.
    fn periods_to_date(&self, member: Member, level: u32) -> Evaluated<Vec<Tuple>> {
        let (id, element) = self.element(member)?;
        let mut periods = Vec::new();
        if let Some(ancestor) = self.ancestor_at(member, level)? {
            let elements = &self.dimension(member.position).elements;
            let mut waiting = vec![self.element(ancestor)?.0];
            while let Some(next) = waiting.pop() {
                let other = &elements[next as usize];
                if other.level == element.level && !periods.contains(&next) {
                    periods.push(next);
                }
                if next == id {
                    break;
                }
                if other.level > element.level {
                    waiting.extend(other.children.iter().rev().map(|&(child, _)| child));
                }
            }
        }
        let one = |id: ElementId| {
            vec![Member {
                position: member.position,
                id: Id::Element(id),
            }]
        };
        Ok(periods.into_iter().map(one).collect())
    }

    /// A level belongs to one dimension and means nothing to a member of another.
    fn of_level(&self, member: Member, position: usize) -> Evaluated<()> {
        match member.position == position {
            true => Ok(()),
            false => fail(format!(
                "the level is one of dimension '{}', and the member belongs to '{}'",
                self.dimension(position).name,
                self.dimension(member.position).name
            )),
        }
    }

    /// A level given as an argument: the position of its dimension, and the level.
    fn level(&self, expression: &Expr, context: &Context) -> Evaluated<(usize, u32)> {
        match self.evaluate(expression, context)? {
            Datum::Level(position, level) => Ok((position, level)),
            _ => fail("expected a level, such as [Period].Levels(1) or [Period].[Q1].Level"),
        }
    }

    /// A member given as an argument, or without one the current member of a dimension.
    fn member_of(
        &self,
        expression: Option<&Expr>,
        position: usize,
        context: &Context,
    ) -> Evaluated<Member> {
        match expression {
            Some(expression) => self.member(expression, context),
            None => Ok(Member {
                position,
                id: context[position],
            }),
        }
    }

    /// The member some places after this one, or before it if the number is negative, among
    /// those of its level in the dimension's order. Nothing past either end.
    fn shift(&self, member: Member, by: f64) -> Evaluated<Option<Member>> {
        let (id, element) = self.element(member)?;
        let elements = &self.dimension(member.position).elements;
        // From the member towards the end asked for, counting the elements of its level:
        // the next one is found without looking at the rest of the dimension.
        let mut at = id as usize;
        let mut left = by.trunc().abs() as usize;
        while left > 0 {
            at = match by > 0.0 {
                true => at + 1,
                false => at.wrapping_sub(1),
            };
            match elements.get(at) {
                Some(other) => left -= usize::from(other.level == element.level),
                None => return Ok(None),
            }
        }
        Ok(Some(Member {
            position: member.position,
            id: Id::Element(at as ElementId),
        }))
    }

    /// The member itself or the ancestor of it that is at a level, going up by first parents.
    /// Nothing if that way up passes the level by or ends beneath it.
    fn ancestor_at(&self, mut member: Member, level: u32) -> Evaluated<Option<Member>> {
        loop {
            let (_, element) = self.element(member)?;
            if element.level == level {
                return Ok(Some(member));
            }
            if element.level > level || element.parents.is_empty() {
                return Ok(None);
            }
            member = self.parent(member)?;
        }
    }

    /// The member reached from `other` by the way that leads from `ancestor` down to
    /// `member`: the same child at every step. Nothing if `other` has no such child.
    fn cousin(&self, member: Member, ancestor: Member, other: Member) -> Evaluated<Option<Member>> {
        let mut steps = Vec::new();
        let mut at = member;
        while at != ancestor {
            let parent = self.parent(at)?;
            let children = &self.element(parent)?.1.children;
            steps.push(
                children
                    .iter()
                    .position(|&(id, _)| Id::Element(id) == at.id),
            );
            at = parent;
        }
        let mut found = other;
        for step in steps.into_iter().rev() {
            let children = &self.element(found)?.1.children;
            let Some(&(id, _)) = step.and_then(|step| children.get(step)) else {
                return Ok(None);
            };
            found = Member {
                position: found.position,
                id: Id::Element(id),
            };
        }
        Ok(Some(found))
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
        // The member some places before or after this one among those of its level, in the
        // dimension's order; nothing past either end.
        let shifted = |by: f64| -> Evaluated<Datum> {
            let found = self.shift(member()?, by)?;
            Ok(found.map_or(Datum::Null, Datum::Member))
        };
        let distance = || match arguments {
            [distance] => self.number(distance, context),
            _ => fail(format!(".{method} takes one distance")),
        };
        match (method, &target) {
            ("PREVMEMBER", _) => shifted(-1.0),
            ("NEXTMEMBER", _) => shifted(1.0),
            ("LAG", _) => shifted(-distance()?),
            ("LEAD", _) => shifted(distance()?),
            ("LEVEL", _) => {
                let member = member()?;
                Ok(Datum::Level(member.position, self.element(member)?.1.level))
            }
            ("LEVELS", &Datum::Dimension(position)) => {
                let [level] = arguments else {
                    return fail(".LEVELS takes one level, by number or by name");
                };
                // By the name the dimension gives it, or by its number from the leaves up.
                match self.evaluate(level, context)? {
                    Datum::Text(name) => match self.dimension(position).level(&name) {
                        Some(level) => Ok(Datum::Level(position, level)),
                        None => {
                            let dimension = &self.dimension(position).name;
                            fail(format!("dimension '{dimension}' has no level '{name}'"))
                        }
                    },
                    number => {
                        let number = to_number(&self.scalar(number, context)?)?;
                        Ok(Datum::Level(position, number.max(0.0) as u32))
                    }
                }
            }
            ("NAME", &Datum::Level(position, level)) => {
                let named = self.dimension(position).levels.get(level as usize);
                Ok(Datum::Text(
                    named.cloned().unwrap_or_else(|| level.to_string()),
                ))
            }
            ("LEVELS", _) => fail(".LEVELS applies to a dimension"),
            // A dimension has one hierarchy here, which is the dimension itself.
            ("DIMENSION" | "HIERARCHY", _) => match target {
                Datum::Dimension(position) | Datum::Level(position, _) => {
                    Ok(Datum::Dimension(position))
                }
                Datum::Member(member) => Ok(Datum::Dimension(member.position)),
                _ => fail(format!(
                    ".{method} applies to a member, a level or a dimension"
                )),
            },
            ("NAME", &Datum::Dimension(position)) => {
                Ok(Datum::Text(self.dimension(position).name.clone()))
            }
            ("UNIQUENAME", &Datum::Dimension(position)) => {
                Ok(Datum::Text(format!("[{}]", self.dimension(position).name)))
            }
            ("ORDINAL", &Datum::Level(_, level)) => Ok(Datum::Number(f64::from(level))),
            ("MEMBERS", &Datum::Level(position, level)) => {
                let elements = self.dimension(position).elements.iter().enumerate();
                let at = elements.filter(|(_, element)| element.level == level);
                Ok(Datum::Set(
                    at.map(|(id, _)| vec![one(position, id as ElementId)])
                        .collect(),
                ))
            }
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
                let siblings = self.siblings(member)?.into_iter();
                Ok(Datum::Set(
                    siblings.map(|id| vec![one(member.position, id)]).collect(),
                ))
            }
            ("FIRSTSIBLING" | "LASTSIBLING", _) => {
                let member = member()?;
                let siblings = self.siblings(member)?;
                // A member is among its own siblings, so there is always one.
                let found = match method {
                    "FIRSTSIBLING" => siblings.first(),
                    _ => siblings.last(),
                };
                Ok(found.map_or(Datum::Null, |&id| Datum::Member(one(member.position, id))))
            }
            ("DEFAULTMEMBER", &Datum::Dimension(position)) => Ok(Datum::Member(Member {
                position,
                id: self.top()[position],
            })),
            ("DEFAULTMEMBER", _) => fail(".DEFAULTMEMBER applies to a dimension"),
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
                Some(Scalar::Number(number)) => Datum::Number(*number),
                Some(Scalar::Text(text)) => Datum::Text(text.clone()),
                None => Datum::Null,
            },
            (_, None) => Datum::Null,
        })
    }

    /// A member and its siblings: the children of its first parent, or for a top element the
    /// tops of the dimension.
    fn siblings(&self, member: Member) -> Evaluated<Vec<ElementId>> {
        let (_, element) = self.element(member)?;
        let elements = &self.dimension(member.position).elements;
        Ok(match element.parents.first() {
            Some(&parent) => {
                let children = elements[parent as usize].children.iter();
                children.map(|&(id, _)| id).collect()
            }
            None => {
                let tops = elements.iter().enumerate();
                let tops = tops.filter(|(_, element)| element.parents.is_empty());
                tops.map(|(id, _)| id as ElementId).collect()
            }
        })
    }

    /// The current member of the cube's dimension of type `time`, for a function that is
    /// given no member to start from.
    fn now(&self, function: &str, context: &Context) -> Evaluated<Member> {
        let time = |&position: &usize| self.dimension(position).kind == DimensionType::Time;
        match (0..context.len()).find(time) {
            Some(position) => Ok(Member {
                position,
                id: context[position],
            }),
            None => fail(format!(
                "{function} needs a member, or a dimension of type 'time' in the cube"
            )),
        }
    }

    /// A set with the children of some of its tuples put in after them, each child standing
    /// in its parent's place in the tuple. The member looked at is the tuple's member of the
    /// dimension at `position`, or without one its first. `drilled` says whether a member is
    /// to be opened; the children put in are asked too if `recursive`.
    fn drill_down(
        &self,
        set: Vec<Tuple>,
        position: Option<usize>,
        recursive: bool,
        drilled: &dyn Fn(Member, &Element) -> bool,
    ) -> Evaluated<Vec<Tuple>> {
        let mut found = Vec::new();
        let mut waiting: Vec<(Tuple, bool)> = set.into_iter().rev().map(|t| (t, true)).collect();
        while let Some((tuple, asked)) = waiting.pop() {
            let place = match position {
                Some(position) => tuple.iter().position(|member| member.position == position),
                None => (!tuple.is_empty()).then_some(0),
            };
            if asked
                && let Some(place) = place
                && let Ok((_, element)) = self.element(tuple[place])
                && drilled(tuple[place], element)
            {
                for &(child, _) in element.children.iter().rev() {
                    let mut below = tuple.clone();
                    below[place].id = Id::Element(child);
                    waiting.push((below, recursive));
                }
            }
            found.push(tuple);
            bounded(Some(found.len()))?;
        }
        Ok(found)
    }

    /// The members of the tuples of a set, and the position of their dimension: what a
    /// drilling function is told to open or to close.
    fn chosen(&self, set: Vec<Tuple>) -> (Option<usize>, HashSet<Member>) {
        let members: HashSet<Member> = set.into_iter().flatten().collect();
        (members.iter().next().map(|member| member.position), members)
    }

    /// How many steps a member is beneath the top of its dimension, going up by first parents.
    fn depth(&self, mut member: Member) -> usize {
        let mut depth = 0;
        while let Ok(parent) = self.parent(member) {
            member = parent;
            depth += 1;
        }
        depth
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
        self.foresee(arguments.get(1), context, &set);
        let along = std::cell::RefCell::new(Along::new(context));
        let value = |tuple: &Tuple| {
            let mut along = along.borrow_mut();
            let context = along.at(tuple);
            let value = self.evaluate(&arguments[1], context)?;
            self.scalar(value, context)
        };
        let turned = |ordering: Ordering| match descending {
            true => ordering.reverse(),
            false => ordering,
        };
        // By value alone the hierarchy has no say: each tuple has one value, and tuples of
        // one value stand in the order of their members, as they would by the longer way.
        if !hierarchical {
            let mut keyed = Vec::with_capacity(set.len());
            for tuple in set {
                keyed.push((value(&tuple)?, tuple));
            }
            keyed.sort_by(|(left, one), (right, other)| {
                let members = || one.iter().map(|m| m.id).cmp(other.iter().map(|m| m.id));
                turned(compare(left, right)).then_with(members)
            });
            return Ok(keyed.into_iter().map(|(_, tuple)| tuple).collect());
        }

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
            // Equal values keep members apart by identity, so a parent's children stay together.
            turned(compare(&left.0, &right.0)).then_with(|| left.1.cmp(&right.1))
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

    /// Says which cells evaluating an expression at each tuple of a set is about to read: the
    /// cell of every member or tuple the expression names outright, or with no expression the
    /// tuple's own cell. Cells it cannot foresee are simply read when their turn comes.
    fn foresee(&self, expression: Option<&Expr>, context: &Context, set: &[Tuple]) {
        let mut named = Vec::new();
        match expression {
            Some(expression) => self.named(expression, context, &mut named),
            None => named.push(Tuple::new()),
        }
        // The cell at each tuple with each named member in it, given member by member so
        // that one which is no total is turned down without being written out.
        let mut along = Along::new(context);
        let mut cells: Vec<Vec<ElementId>> = Vec::new();
        for tuple in set {
            let context = along.at(tuple);
            let at = |named: &Tuple| {
                let placed = |(position, id): (usize, &Id)| {
                    let member = named.iter().find(|member| member.position == position);
                    member.map_or(*id, |member| member.id)
                };
                self.total_of(context.iter().enumerate().map(placed))
            };
            cells.extend(named.iter().filter_map(at));
        }
        (self.expect)(&cells.iter().map(Vec::as_slice).collect::<Vec<_>>());
    }

    /// The coordinate of the cell at a context, if it is a cell of the cube that is added
    /// up: every member an element, and one of them at least a consolidation. Those are the
    /// cells worth saying are about to be read; a leaf is read as quickly unannounced.
    fn total_at(&self, context: &Context) -> Option<Vec<ElementId>> {
        self.total_of(context.iter().copied())
    }

    /// The same of a cell given member by member, in the cube's order, so that a cell which
    /// is no total costs nothing to turn down.
    fn total_of(&self, ids: impl Iterator<Item = Id> + Clone) -> Option<Vec<ElementId>> {
        let consolidated = |(position, id): (usize, Id)| match id {
            Id::Element(id) => {
                let element = &self.dimension(position).elements[id as usize];
                element.kind == ElementType::Consolidated
            }
            Id::Calculated(_) => false,
        };
        if !ids.clone().enumerate().any(consolidated) {
            return None;
        }
        let element = |id: Id| match id {
            Id::Element(element) => Some(element),
            Id::Calculated(_) => None,
        };
        ids.map(element).collect()
    }

    /// The totals that reading the cell at a context will ask for: the cell itself, or where
    /// a calculated member stands there, the cells its expression names outright.
    fn ahead(&self, context: &Context) -> Vec<Vec<ElementId>> {
        let Some(calculated) = self.calculated_at(context) else {
            return self.total_at(context).into_iter().collect();
        };
        // The cell at the context with the members of a tuple put in their places.
        let at = |named: &Tuple| {
            let placed = |(position, id): (usize, &Id)| {
                let member = named.iter().find(|member| member.position == position);
                member.map_or(*id, |member| member.id)
            };
            self.total_of(context.iter().enumerate().map(placed))
        };
        match &calculated.named {
            Some(named) => named.iter().filter_map(at).collect(),
            None => {
                let mut named = Vec::new();
                self.named(&calculated.expression, context, &mut named);
                named.iter().filter_map(at).collect()
            }
        }
    }

    /// The members and tuples an expression names outright, through arithmetic, comparisons
    /// and the functions that choose between values. A value that is named and then not
    /// chosen is a cell read for nothing, which costs less than reading totals one by one.
    fn named(&self, expression: &Expr, context: &Context, found: &mut Vec<Tuple>) {
        match expression {
            Expr::Binary(left, _, right) => {
                self.named(left, context, found);
                self.named(right, context, found);
            }
            Expr::Not(inner) | Expr::Negate(inner) => self.named(inner, context, found),
            Expr::Call(function, arguments) if CHOOSING.contains(&function.as_str()) => {
                for argument in arguments {
                    self.named(argument, context, found);
                }
            }
            Expr::Name(_) | Expr::Member(..) | Expr::Calculated(..) | Expr::Tuple(_) => {
                match self.evaluate(expression, context) {
                    Ok(Datum::Member(member)) => found.push(vec![member]),
                    Ok(Datum::Tuple(tuple)) => found.push(tuple),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// The calculated member that decides the cell at a context, if one stands there. Of
    /// several, the one with the highest solve order is calculated last, over the results of
    /// the others; among equals, the first in the cube's order.
    fn calculated_at(&self, context: &Context) -> Option<&Calculated<'a>> {
        let calculated = context.iter().filter_map(|id| match id {
            Id::Calculated(index) => Some(&self.calculated[*index]),
            Id::Element(_) => None,
        });
        calculated.reduce(
            |chosen, other| match other.solve_order > chosen.solve_order {
                true => other,
                false => chosen,
            },
        )
    }

    /// The value of the cell at a context. A calculated member there is evaluated first.
    fn cell(&self, context: &Context) -> Evaluated<Datum> {
        let Some(calculated) = self.calculated_at(context) else {
            let element = |id: &Id| match id {
                Id::Element(id) => *id,
                Id::Calculated(_) => unreachable!("checked above"),
            };
            // Where the cell is, written where it is cheapest: here, for a cube of few
            // enough dimensions, since this is done for every cell of every grid.
            let mut few = [0; 16];
            let many: Vec<ElementId>;
            let at = match context.len() <= few.len() {
                true => {
                    few.iter_mut()
                        .zip(context)
                        .for_each(|(slot, id)| *slot = element(id));
                    &few[..context.len()]
                }
                false => {
                    many = context.iter().map(element).collect();
                    &many
                }
            };
            return Ok(match (self.read)(self.cube_id, at)? {
                Scalar::Number(number) => Datum::Number(number),
                Scalar::Text(text) => Datum::Text(text),
            });
        };
        // How far the stack has moved since the first of them is told by where a local of
        // this call stands.
        let here = std::ptr::from_ref(&calculated).addr();
        if self.depth.get() == 0 {
            self.began.set(here);
        }
        if self.depth.get() == DEEPEST || self.began.get().abs_diff(here) > MOST_STACK {
            let name = calculated.name;
            return fail(format!("calculated member '{name}' refers to itself"));
        }
        self.depth.set(self.depth.get() + 1);
        let value = self.evaluate(&calculated.expression, context);
        let value = value.and_then(|value| self.scalar(value, context));
        self.depth.set(self.depth.get() - 1);
        value
    }

    /// Reduces a value to a number, text, truth or nothing: a member or tuple gives its cell.
    pub(super) fn scalar(&self, datum: Datum, context: &Context) -> Evaluated<Datum> {
        match datum {
            Datum::Member(member) => {
                // The one member put in its place, without a tuple made to hold it.
                let mut context = context.clone();
                context[member.position] = member.id;
                self.cell(&context)
            }
            Datum::Tuple(tuple) => self.cell(&with(context, &tuple)),
            Datum::Set(_) | Datum::Dimension(_) | Datum::Level(..) => {
                fail("expected a value, found a set")
            }
            scalar => Ok(scalar),
        }
    }

    fn number(&self, expression: &Expr, context: &Context) -> Evaluated<f64> {
        let value = self.evaluate(expression, context)?;
        to_number(&self.scalar(value, context)?)
    }

    /// An expression's value as text; a number is written as briefly as it can be.
    fn text(&self, expression: &Expr, context: &Context) -> Evaluated<String> {
        let value = self.evaluate(expression, context)?;
        match self.scalar(value, context)? {
            Datum::Text(text) => Ok(text),
            Datum::Number(number) => Ok(number.to_string()),
            Datum::Null => Ok(String::new()),
            _ => fail("expected text"),
        }
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

/// The number at the start of a text, or zero if there is none there.
fn leading_number(text: &str) -> f64 {
    let ends = (1..=text.len())
        .rev()
        .filter(|&end| text.is_char_boundary(end));
    let mut numbers = ends.filter_map(|end| text[..end].parse::<f64>().ok());
    numbers.find(|number| number.is_finite()).unwrap_or(0.0)
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

/// The functions that choose between the values given them, through which the cells an
/// expression names are looked for.
const CHOOSING: [&str; 4] = ["IIF", "CASE", "COALESCEEMPTY", "ISEMPTY"];

/// Whether what an expression names outright is the same at every cell: its names are all
/// plain ones, none worked out from the cell it stands at as `CurrentMember` is.
fn fixed(expression: &Expr) -> bool {
    let plain = |item: &Expr| {
        matches!(
            item,
            Expr::Name(_) | Expr::Member(..) | Expr::Calculated(..)
        )
    };
    match expression {
        Expr::Binary(left, _, right) => fixed(left) && fixed(right),
        Expr::Not(inner) | Expr::Negate(inner) => fixed(inner),
        Expr::Call(function, arguments) if CHOOSING.contains(&function.as_str()) => {
            arguments.iter().all(fixed)
        }
        Expr::Tuple(items) => items.iter().all(plain),
        _ => true,
    }
}

/// Text, unless it is longer than text may be.
fn bounded_text(text: String) -> Evaluated<Datum> {
    match text.len() <= MOST_TEXT {
        true => Ok(Datum::Text(text)),
        false => fail(format!("a text has more than {MOST_TEXT} characters")),
    }
}

/// Refuses a set of more tuples than a set may hold, or of more than can be counted.
fn bounded(tuples: Option<usize>) -> Evaluated<()> {
    match tuples.is_some_and(|tuples| tuples <= MOST_TUPLES) {
        true => Ok(()),
        false => fail(format!("a set has more than {MOST_TUPLES} tuples")),
    }
}

/// Whether text matches a pattern in which `*` stands for any run of characters and `?` for
/// any one.
/// Each `*` first stands for nothing; when what follows it fails, the last one takes one
/// more character and the rest is tried again. No earlier `*` need ever be gone back to, so
/// a pattern of many stars costs no more than its length times the text's.
fn wildcard(pattern: &[char], text: &[char]) -> bool {
    let (mut at, mut read) = (0, 0);
    // The place after the last star, and how much of the text it had taken.
    let mut star: Option<(usize, usize)> = None;
    while read < text.len() {
        match (pattern.get(at), star) {
            (Some('*'), _) => {
                star = Some((at + 1, read));
                at += 1;
            }
            (Some(&p), _) if p == '?' || p == text[read] => {
                at += 1;
                read += 1;
            }
            (_, Some((after, taken))) => {
                star = Some((after, taken + 1));
                at = after;
                read = taken + 1;
            }
            _ => return false,
        }
    }
    pattern[at..].iter().all(|&p| p == '*')
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
    bounded(left.len().checked_mul(right.len()))?;
    let mut joined = Vec::with_capacity(left.len() * right.len());
    for left in left {
        for right in right {
            joined.push([left.as_slice(), right].concat());
        }
    }
    Ok(joined)
}
