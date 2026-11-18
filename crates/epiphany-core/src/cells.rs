//! The cells of one cube: stored leaf values, and consolidated values computed on read.

use std::fmt;

use rustc_hash::FxHashMap;

use crate::Value;
use crate::model::{Cube, Element, ElementId, ElementType, Model};

/// One element per dimension of a cube, in the cube's dimension order.
type Coordinate = Box<[ElementId]>;
/// The leaves of one element and the weight of each, ordered by id.
type Leaves<'a> = &'a [(ElementId, f64)];

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
    pub fn read(&self, model: &Model, cube: &Cube, at: &[ElementId]) -> Value {
        let elements = elements(model, cube, at);
        if is_text(elements.clone()) {
            return Value::Text(self.texts.get(at).cloned().unwrap_or_default());
        }
        if !elements.clone().any(is_consolidated) {
            return Value::Number(self.numbers.get(at).copied().unwrap_or(0.0));
        }
        let leaves: Vec<Leaves> = elements.map(|e| e.leaves.as_slice()).collect();
        let combinations = leaves
            .iter()
            .try_fold(1usize, |product, l| product.checked_mul(l.len()));
        Value::Number(match combinations {
            Some(0) => 0.0,
            Some(n) if n <= self.numbers.len() => self.sum_combinations(&leaves),
            _ => self.sum_stored(&leaves),
        })
    }

    /// Writes a leaf cell. Writing zero or an empty string clears it.
    pub fn write(
        &mut self,
        model: &Model,
        cube: &Cube,
        at: &[ElementId],
        value: Value,
    ) -> Result<(), CellError> {
        let elements = elements(model, cube, at);
        match (is_text(elements.clone()), value) {
            (true, Value::Text(text)) => store(&mut self.texts, at, text, String::is_empty),
            (false, Value::Number(number)) => {
                if !number.is_finite() {
                    return Err(CellError::NotFinite);
                }
                if elements.clone().any(is_consolidated) {
                    return Err(CellError::Consolidated);
                }
                store(&mut self.numbers, at, number, |n| *n == 0.0);
            }
            _ => return Err(CellError::WrongType),
        }
        Ok(())
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
            let mut dimension = leaves.len();
            loop {
                if dimension == 0 {
                    return total;
                }
                dimension -= 1;
                positions[dimension] += 1;
                if positions[dimension] == leaves[dimension].len() {
                    positions[dimension] = 0;
                }
                at[dimension] = leaves[dimension][positions[dimension]].0;
                if positions[dimension] != 0 {
                    break;
                }
            }
        }
    }

    /// Sums a consolidation by weighing every stored cell.
    fn sum_stored(&self, leaves: &[Leaves]) -> f64 {
        let weight = |at: &Coordinate| {
            at.iter().zip(leaves).try_fold(1.0, |weight, (id, l)| {
                let position = l.binary_search_by_key(id, |&(leaf, _)| leaf).ok()?;
                Some(weight * l[position].1)
            })
        };
        self.numbers
            .iter()
            .filter_map(|(at, value)| Some(weight(at)? * value))
            .sum()
    }
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
    /// A number was written to a cell with a consolidated element.
    Consolidated,
    /// A number was written to a text cell, or text to a numeric cell.
    WrongType,
    /// The number written was infinite or not a number.
    NotFinite,
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
            Self::Consolidated => write!(f, "a consolidated cell cannot be written"),
            Self::WrongType => write!(f, "the value has the wrong type for the cell"),
            Self::NotFinite => write!(f, "the number is not finite"),
        }
    }
}

impl std::error::Error for CellError {}
