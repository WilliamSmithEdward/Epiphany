//! The cells of one cube: stored leaf values, and consolidated values computed on read.

use std::fmt;

use rustc_hash::FxHashMap;

use crate::Scalar;
use crate::model::{Cube, Element, ElementId, ElementType, Model};

/// One element per dimension of a cube, in the cube's dimension order.
pub(crate) type Coordinate = Box<[ElementId]>;
/// Values not yet stored, by the coordinate they will be written to.
pub(crate) type Writes = FxHashMap<Coordinate, Scalar>;
/// The leaves of one element and the weight of each, ordered by id.
pub(crate) type Leaves<'a> = &'a [(ElementId, f64)];

/// The stored cells of one cube. Zero and empty cells are not stored.
///
/// A cell is a text cell when its element in the last dimension is a string element, and a
/// numeric cell otherwise. Coordinates must come from [`coordinate`] on the same model.
#[derive(Debug, Clone, Default)]
pub struct Cells {
    numbers: FxHashMap<Coordinate, f64>,
    texts: FxHashMap<Coordinate, String>,
}

impl Cells {
    pub fn read(&self, model: &Model, cube: &Cube, at: &[ElementId]) -> Scalar {
        let elements = elements(model, cube, at);
        if is_text(elements.clone()) {
            return Scalar::Text(self.texts.get(at).cloned().unwrap_or_default());
        }
        if !elements.clone().any(is_consolidated) {
            return Scalar::Number(self.numbers.get(at).copied().unwrap_or(0.0));
        }
        let leaves: Vec<Leaves> = elements.map(|e| e.leaves.as_slice()).collect();
        let combinations = leaves
            .iter()
            .try_fold(1usize, |product, l| product.checked_mul(l.len()));
        Scalar::Number(match combinations {
            Some(0) => 0.0,
            Some(n) if n <= self.numbers.len() => self.sum_combinations(&leaves),
            _ => self.sum_stored(&leaves),
        })
    }

    /// Reads a cell as it would be once the pending writes were stored.
    pub(crate) fn read_with(
        &self,
        model: &Model,
        cube: &Cube,
        at: &[ElementId],
        pending: &Writes,
    ) -> Scalar {
        if let Some(value) = pending.get(at) {
            return value.clone();
        }
        let stored = match self.read(model, cube, at) {
            Scalar::Number(stored) => stored,
            text => return text,
        };
        let leaves: Vec<Leaves> = elements(model, cube, at)
            .map(|e| e.leaves.as_slice())
            .collect();
        let change = |(leaf, value): (&Coordinate, &Scalar)| {
            let Scalar::Number(number) = value else {
                return None;
            };
            let before = self.numbers.get(leaf).copied().unwrap_or(0.0);
            Some(weight(&leaves, leaf)? * (number - before))
        };
        Scalar::Number(stored + pending.iter().filter_map(change).sum::<f64>())
    }

    /// Every stored cell and its value.
    pub(crate) fn entries(&self) -> impl Iterator<Item = (&Coordinate, Scalar)> {
        let numbers = self
            .numbers
            .iter()
            .map(|(at, &number)| (at, Scalar::Number(number)));
        let texts = self
            .texts
            .iter()
            .map(|(at, text)| (at, Scalar::Text(text.clone())));
        numbers.chain(texts)
    }

    /// Writes a leaf cell. Writing zero or an empty string clears it.
    pub fn write(
        &mut self,
        model: &Model,
        cube: &Cube,
        at: &[ElementId],
        value: Scalar,
    ) -> Result<(), CellError> {
        check(model, cube, at, &value)?;
        self.set(at, value);
        Ok(())
    }

    /// Stores a value that has passed [`check`].
    pub(crate) fn set(&mut self, at: &[ElementId], value: Scalar) {
        match value {
            Scalar::Number(number) => store(&mut self.numbers, at, number, |n| *n == 0.0),
            Scalar::Text(text) => store(&mut self.texts, at, text, String::is_empty),
        }
    }

    /// Sums a consolidation by looking up every combination of its leaves.
    fn sum_combinations(&self, leaves: &[Leaves]) -> f64 {
        let mut positions = vec![0; leaves.len()];
        let mut at: Vec<ElementId> = leaves.iter().map(|l| l[0].0).collect();
        let mut total = 0.0;
        loop {
            if let Some(value) = self.numbers.get(at.as_slice()) {
                let weights = positions.iter().zip(leaves).map(|(&p, l)| l[p].1);
                total += weights.product::<f64>() * value;
            }
            if !advance(leaves, &mut positions, &mut at) {
                return total;
            }
        }
    }

    /// The stored leaf numbers as they would be once the pending writes were stored.
    pub(crate) fn numbers_with<'a>(
        &'a self,
        pending: Option<&'a Writes>,
    ) -> impl Iterator<Item = (&'a [ElementId], f64)> {
        let replaced =
            move |at: &Coordinate| pending.is_some_and(|pending| pending.contains_key(at));
        let stored = self.numbers.iter().filter(move |(at, _)| !replaced(at));
        let written = pending
            .into_iter()
            .flatten()
            .filter_map(|(at, value)| match value {
                Scalar::Number(number) if *number != 0.0 => Some((&**at, *number)),
                _ => None,
            });
        stored.map(|(at, &number)| (&**at, number)).chain(written)
    }

    /// One leaf number as it would be once the pending writes were stored.
    pub(crate) fn number_with(&self, at: &[ElementId], pending: Option<&Writes>) -> f64 {
        match pending.and_then(|pending| pending.get(at)) {
            Some(Scalar::Number(number)) => *number,
            Some(Scalar::Text(_)) => 0.0,
            None => self.numbers.get(at).copied().unwrap_or(0.0),
        }
    }

    /// Sums a consolidation by weighing every stored cell.
    fn sum_stored(&self, leaves: &[Leaves]) -> f64 {
        self.numbers
            .iter()
            .filter_map(|(at, value)| Some(weight(leaves, at)? * value))
            .sum()
    }
}

/// Moves to the next combination of one leaf per dimension, the last dimension turning
/// fastest. `positions` says which leaf each dimension is at and `at` is the combination.
/// Returns false once every combination has been visited.
pub(crate) fn advance(leaves: &[Leaves], positions: &mut [usize], at: &mut [ElementId]) -> bool {
    for dimension in (0..leaves.len()).rev() {
        positions[dimension] += 1;
        if positions[dimension] == leaves[dimension].len() {
            positions[dimension] = 0;
        }
        at[dimension] = leaves[dimension][positions[dimension]].0;
        if positions[dimension] != 0 {
            return true;
        }
    }
    false
}

/// The weight a leaf cell carries in a consolidation, if it is part of it.
pub(crate) fn weight(leaves: &[Leaves], at: &[ElementId]) -> Option<f64> {
    at.iter().zip(leaves).try_fold(1.0, |weight, (id, l)| {
        let position = l.binary_search_by_key(id, |&(leaf, _)| leaf).ok()?;
        Some(weight * l[position].1)
    })
}

/// Resolves one element name or alias per dimension of the cube into a coordinate.
pub fn coordinate<S: AsRef<str>>(
    model: &Model,
    cube: &Cube,
    names: &[S],
) -> Result<Vec<ElementId>, CellError> {
    if names.len() != cube.dimensions.len() {
        return Err(CellError::Arity {
            expected: cube.dimensions.len(),
            found: names.len(),
        });
    }
    let resolve = |(&dimension, name): (&u32, &S)| {
        let dimension = &model.dimensions()[dimension as usize];
        let found = dimension.element_id(name.as_ref());
        found.ok_or_else(|| CellError::UnknownElement {
            dimension: dimension.name.clone(),
            element: name.as_ref().to_owned(),
        })
    };
    cube.dimensions.iter().zip(names).map(resolve).collect()
}

/// Whether a value may be written to a cell.
pub fn check(
    model: &Model,
    cube: &Cube,
    at: &[ElementId],
    value: &Scalar,
) -> Result<(), CellError> {
    let elements = elements(model, cube, at);
    match (is_text(elements.clone()), value) {
        (true, Scalar::Text(_)) => Ok(()),
        (false, Scalar::Number(number)) if !number.is_finite() => Err(CellError::NotFinite),
        (false, Scalar::Number(_)) if elements.clone().any(is_consolidated) => {
            Err(CellError::Consolidated)
        }
        (false, Scalar::Number(_)) => Ok(()),
        _ => Err(CellError::WrongType),
    }
}

/// The elements of a coordinate, in dimension order.
fn elements<'a>(
    model: &'a Model,
    cube: &'a Cube,
    at: &'a [ElementId],
) -> impl DoubleEndedIterator<Item = &'a Element> + Clone {
    assert_eq!(at.len(), cube.dimensions.len(), "one element per dimension");
    let element =
        move |(&d, &e): (&u32, &ElementId)| &model.dimensions()[d as usize].elements[e as usize];
    cube.dimensions.iter().zip(at).map(element)
}

/// Whether the elements address a text cell: the last one is a string element.
fn is_text<'a>(mut elements: impl DoubleEndedIterator<Item = &'a Element>) -> bool {
    elements
        .next_back()
        .is_some_and(|e| e.kind == ElementType::Text)
}

fn is_consolidated(element: &Element) -> bool {
    element.kind == ElementType::Consolidated
}

fn store<V>(
    cells: &mut FxHashMap<Coordinate, V>,
    at: &[ElementId],
    value: V,
    is_empty: impl Fn(&V) -> bool,
) {
    if is_empty(&value) {
        cells.remove(at);
    } else {
        cells.insert(at.into(), value);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellError {
    /// The number of names differs from the number of dimensions.
    Arity {
        expected: usize,
        found: usize,
    },
    UnknownElement {
        dimension: String,
        element: String,
    },
    UnknownCube {
        cube: String,
    },
    /// A number was written to a cell with a consolidated element.
    Consolidated,
    /// A number was written to a text cell, or text to a numeric cell.
    WrongType,
    /// A value was written to a cell that a rule calculates.
    Calculated,
    /// The number written was infinite or not a number.
    NotFinite,
    /// A value could not be shared out among the leaves of a cell; the text says why.
    Spread(String),
}

impl CellError {
    /// A stable identifier for the kind of error.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Arity { .. } => "wrong_arity",
            Self::UnknownElement { .. } => "unknown_element",
            Self::UnknownCube { .. } => "unknown_cube",
            Self::Consolidated => "consolidated_cell",
            Self::WrongType => "wrong_type",
            Self::Calculated => "calculated_cell",
            Self::NotFinite => "not_finite",
            Self::Spread(_) => "spread_failed",
        }
    }
}

impl fmt::Display for CellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Arity { expected, found } => {
                write!(f, "expected {expected} elements, found {found}")
            }
            Self::UnknownElement { dimension, element } => {
                write!(f, "unknown element '{element}' in dimension '{dimension}'")
            }
            Self::UnknownCube { cube } => write!(f, "unknown cube '{cube}'"),
            Self::Consolidated => write!(f, "a consolidated cell cannot be written"),
            Self::WrongType => write!(f, "the value has the wrong type for the cell"),
            Self::Calculated => write!(f, "the cell is calculated by a rule and cannot be written"),
            Self::NotFinite => write!(f, "the number is not finite"),
            Self::Spread(reason) => reason.fmt(f),
        }
    }
}

impl std::error::Error for CellError {}
