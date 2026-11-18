//! Spreading: writing a value to a consolidated cell by sharing it out among its leaves.

use serde::{Deserialize, Serialize};

use super::{CellWrite, Database, Error, TransactionId};
use crate::Scalar;
use crate::cells::{CellError, Leaves, advance, weight};
use crate::model::{Cube, ElementId, ElementType};

/// The most leaf cells one spread may write.
const MOST_WRITTEN: usize = 1_000_000;

/// A value to share out among the leaves of a consolidated cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct Spread {
    pub cube: String,
    pub at: Vec<String>,
    pub value: f64,
    pub method: SpreadMethod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum SpreadMethod {
    /// Scales the leaves that hold a value so that the cell comes to the value given, keeping
    /// their proportions. If none holds a value, spreads equally instead.
    Proportional,
    /// Gives every leaf the same value, such that the cell comes to the value given.
    Equal,
    /// Gives every leaf the value given itself.
    Repeat,
}

impl Database {
    /// Shares a value out among the leaves of a cell, as one batch of writes: all of them or
    /// none. Leaves that a rule calculates are left alone. Returns how many cells were
    /// written. A leaf cell is simply written.
    pub fn spread(
        &mut self,
        transaction: Option<TransactionId>,
        spread: &Spread,
    ) -> Result<usize, Error> {
        self.expire();
        let (id, at) = self.locate(&spread.cube, &spread.at)?;
        let cube = &self.model.cubes()[id];
        let elements = || {
            let pairs = cube.dimensions.iter().zip(&at);
            pairs.map(|(&d, &e)| &self.model.dimensions()[d as usize].elements[e as usize])
        };
        let refuse = |error: CellError| Error::Write { index: 0, error };
        if elements()
            .next_back()
            .is_some_and(|e| e.kind == ElementType::Text)
        {
            return Err(refuse(CellError::WrongType));
        }
        if !spread.value.is_finite() {
            return Err(refuse(CellError::NotFinite));
        }
        let consolidated = elements().any(|e| e.kind == ElementType::Consolidated);
        if cube
            .rules
            .iter()
            .any(|rule| rule.covers(&at, false, consolidated))
        {
            return Err(refuse(CellError::Calculated));
        }

        let pending = match transaction {
            Some(transaction) => Some(&self.transaction(transaction)?.writes[id]),
            None => None,
        };
        let leaves: Vec<Leaves> = elements().map(|e| e.leaves.as_slice()).collect();
        let writable = |leaf: &[ElementId]| cube.rule_for_leaf(leaf, false).is_none();
        // The leaves that hold a value now, each with its weight in the cell.
        let held: Vec<(Vec<ElementId>, f64, f64)> = self.cells[id]
            .numbers_with(pending)
            .filter(|(leaf, _)| writable(leaf))
            .filter_map(|(leaf, value)| Some((leaf.to_vec(), weight(&leaves, leaf)?, value)))
            .collect();
        let total: f64 = held.iter().map(|(_, weight, value)| weight * value).sum();

        let values: Vec<(Vec<ElementId>, f64)> = match spread.method {
            SpreadMethod::Proportional if total != 0.0 => {
                let scale = spread.value / total;
                held.into_iter()
                    .map(|(leaf, _, value)| (leaf, value * scale))
                    .collect()
            }
            method => {
                let every = every_leaf(cube, &leaves, &writable)?;
                let weights: f64 = every.iter().map(|(_, weight)| weight).sum();
                let each = match method {
                    SpreadMethod::Repeat => spread.value,
                    _ if weights == 0.0 => {
                        let reason = "the weights of the cell's leaves cancel out, so no equal share adds up to the value";
                        return Err(refuse(CellError::Spread(reason.to_owned())));
                    }
                    _ => spread.value / weights,
                };
                every.into_iter().map(|(leaf, _)| (leaf, each)).collect()
            }
        };
        let writes: Vec<CellWrite> = values
            .into_iter()
            .map(|(leaf, value)| self.named(id, &leaf, Scalar::Number(value)))
            .collect();
        self.write(transaction, &writes)?;
        Ok(writes.len())
    }
}

/// Every leaf cell beneath a cell that can be written, with its weight in the cell.
fn every_leaf(
    cube: &Cube,
    leaves: &[Leaves],
    writable: &dyn Fn(&[ElementId]) -> bool,
) -> Result<Vec<(Vec<ElementId>, f64)>, Error> {
    let count = leaves
        .iter()
        .try_fold(1usize, |count, l| count.checked_mul(l.len()));
    if count.is_none_or(|count| count > MOST_WRITTEN) {
        let reason = format!(
            "the cell has more than {MOST_WRITTEN} leaves in cube '{}'; spread proportionally, or lower down",
            cube.name
        );
        return Err(Error::Write {
            index: 0,
            error: CellError::Spread(reason),
        });
    }
    let mut found = Vec::new();
    if count == Some(0) {
        return Ok(found);
    }
    let mut positions = vec![0; leaves.len()];
    let mut leaf: Vec<ElementId> = leaves.iter().map(|l| l[0].0).collect();
    loop {
        if writable(&leaf) {
            let weights = positions.iter().zip(leaves).map(|(&p, l)| l[p].1);
            found.push((leaf.clone(), weights.product()));
        }
        if !advance(leaves, &mut positions, &mut leaf) {
            return Ok(found);
        }
    }
}
