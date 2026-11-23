//! What one user may do with the elements of the model's dimensions: see them, read their
//! cells, or write them too.
//!
//! Some elements are given a permit by name. An element without one of its own takes that
//! of the nearest elements above it that have one, the highest of them where it lies beneath
//! several; an element with none above it is bound by none, and its cube's role holds. A cell
//! is open to what the lowest of its elements allows. The model's own work, its rules and
//! the totals that add up the leaves beneath them, sees every element.

use std::collections::BTreeMap;

use crate::model::{Cube, DimensionId, ElementId, Model};

/// What may be done with an element: nothing, so that it is not there at all; read its cells;
/// or write them too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Permit {
    Hidden,
    Read,
    Write,
}

/// One user's permits, worked out for one model: for each dimension where some element is
/// given one, the permit of every element.
#[derive(Debug, Clone, Default)]
pub struct Access {
    dimensions: Vec<Option<Vec<Permit>>>,
}

impl Access {
    /// The permits given by name, dimension by dimension and element by element, as they
    /// fall on the elements of a model. A dimension or an element the model does not have is
    /// passed over: it may have been taken out since the permit was given.
    pub fn new(model: &Model, given: &BTreeMap<String, BTreeMap<String, Permit>>) -> Self {
        let mut dimensions = vec![None; model.dimensions().len()];
        for (dimension, permits) in given {
            let Some(id) = model.dimension_id(dimension) else {
                continue;
            };
            let dimension = &model.dimensions()[id as usize];
            let mut held: Vec<Option<Permit>> = vec![None; dimension.elements.len()];
            for (element, &permit) in permits {
                if let Some(element) = dimension.element_id(element) {
                    held[element as usize] = Some(permit);
                }
            }
            if held.iter().all(Option::is_none) {
                continue;
            }
            // A parent is a level above each of its children, so taken from the highest
            // level down, every element comes after those above it.
            let mut order: Vec<usize> = (0..held.len()).collect();
            order.sort_by_key(|&element| std::cmp::Reverse(dimension.elements[element].level));
            for element in order {
                if held[element].is_none() {
                    let parents = dimension.elements[element].parents.iter();
                    held[element] = parents.filter_map(|&parent| held[parent as usize]).max();
                }
            }
            let permits = held
                .into_iter()
                .map(|permit| permit.unwrap_or(Permit::Write));
            dimensions[id as usize] = Some(permits.collect());
        }
        Self { dimensions }
    }

    /// Whether any element's permit binds the user.
    pub fn binds(&self) -> bool {
        self.dimensions.iter().any(Option::is_some)
    }

    /// What the user may do with an element.
    pub fn of(&self, dimension: DimensionId, element: ElementId) -> Permit {
        match self.dimensions.get(dimension as usize) {
            Some(Some(permits)) => permits[element as usize],
            _ => Permit::Write,
        }
    }

    /// Whether the user may see an element.
    pub fn sees(&self, dimension: DimensionId, element: ElementId) -> bool {
        self.of(dimension, element) > Permit::Hidden
    }

    /// What the user may do with a cell of a cube: what the lowest of its elements allows.
    pub fn cell(&self, cube: &Cube, at: &[ElementId]) -> Permit {
        let permits = cube.dimensions.iter().zip(at);
        let permits = permits.map(|(&dimension, &element)| self.of(dimension, element));
        permits.min().unwrap_or(Permit::Write)
    }

    /// The first element beneath a cell, in the order of the cube's dimensions, whose leaf
    /// cells the user may not write: for a spread, which changes the leaves beneath it.
    pub fn unwritable_beneath(
        &self,
        model: &Model,
        cube: &Cube,
        at: &[ElementId],
    ) -> Option<(DimensionId, ElementId)> {
        cube.dimensions
            .iter()
            .zip(at)
            .find_map(|(&dimension, &element)| {
                let leaves =
                    model.dimensions()[dimension as usize].elements[element as usize].leaves();
                let unwritable = leaves.iter().map(|&(leaf, _)| leaf);
                let mut unwritable =
                    unwritable.filter(|&leaf| self.of(dimension, leaf) < Permit::Write);
                unwritable.next().map(|leaf| (dimension, leaf))
            })
    }
}
