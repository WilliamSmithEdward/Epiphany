//! A model together with the cells of its cubes, addressed by name.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::Value;
use crate::cells::{CellError, Cells, check, coordinate};
use crate::model::{ElementId, Model, ModelDef, ModelError};

/// A cell addressed by cube name and one element name or alias per dimension.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct CellRef {
    pub cube: String,
    pub at: Vec<String>,
}

/// A value to write to a leaf cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct CellWrite {
    pub cube: String,
    pub at: Vec<String>,
    pub value: Value,
}

/// Why a batch of writes was rejected: the first failing write and its error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchError {
    pub index: usize,
    pub error: CellError,
}

impl fmt::Display for BatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "write {}: {}", self.index, self.error)
    }
}

impl std::error::Error for BatchError {}

#[derive(Debug)]
pub struct Database {
    definition: ModelDef,
    model: Model,
    /// The cells of each cube, in [`Model::cubes`] order.
    cells: Vec<Cells>,
}

impl Database {
    pub fn new(definition: ModelDef) -> Result<Self, ModelError> {
        let model = Model::compile(&definition)?;
        let cells = vec![Cells::default(); model.cubes().len()];
        Ok(Self {
            definition,
            model,
            cells,
        })
    }

    pub fn definition(&self) -> &ModelDef {
        &self.definition
    }

    pub fn model(&self) -> &Model {
        &self.model
    }

    pub fn read(&self, cell: &CellRef) -> Result<Value, CellError> {
        let (cube, at) = self.locate(&cell.cube, &cell.at)?;
        Ok(self.cells[cube].read(&self.model, &self.model.cubes()[cube], &at))
    }

    /// Writes every cell, or none of them if any write is invalid.
    pub fn write(&mut self, writes: &[CellWrite]) -> Result<(), BatchError> {
        let locate = |(index, write): (usize, &CellWrite)| {
            let located = self.locate(&write.cube, &write.at).and_then(|(cube, at)| {
                check(&self.model, &self.model.cubes()[cube], &at, &write.value)?;
                Ok((cube, at))
            });
            located.map_err(|error| BatchError { index, error })
        };
        let located: Vec<_> = writes
            .iter()
            .enumerate()
            .map(locate)
            .collect::<Result<_, _>>()?;
        for ((cube, at), write) in located.into_iter().zip(writes) {
            self.cells[cube].set(&at, write.value.clone());
        }
        Ok(())
    }

    /// Resolves a cube name and element names into a cube id and a coordinate.
    fn locate(&self, cube: &str, at: &[String]) -> Result<(usize, Vec<ElementId>), CellError> {
        let unknown = || CellError::UnknownCube {
            cube: cube.to_owned(),
        };
        let id = self.model.cube_id(cube).ok_or_else(unknown)?;
        Ok((id, coordinate(&self.model, &self.model.cubes()[id], at)?))
    }
}
