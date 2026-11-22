//! Supporting detail: the lines a leaf cell's value is the sum of, kept with the value.
//!
//! A cell's detail is written with its value, the lines' sum, as one write in a transaction
//! like any other: it waits there with the value, is taken back or rolled back with it, and
//! is recorded with the commit that stores it. A value written to the cell plainly takes the
//! detail away, for the lines no longer add up to it.

use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use super::{CellRef, CellWrite, CellWrites, Database, Error, Names, TransactionId};
use crate::Scalar;
use crate::cells::{CellError, Coordinate, check};
use crate::model::{ElementId, Model};

/// The most lines a detail may have.
const MOST_LINES: usize = 1000;
/// The longest a line's label may be, in characters.
const LONGEST_LABEL: usize = 200;

/// One line of a cell's supporting detail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct Line {
    /// What the line is, such as `Flights to Lisbon`.
    #[serde(default)]
    pub label: String,
    pub value: f64,
}

/// A cell's supporting detail as it is recorded, addressed by name: its lines, or none where
/// a commit took the detail away.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellDetail {
    pub cube: String,
    pub at: Vec<String>,
    pub lines: Option<Vec<Line>>,
}

/// Details of each cube by cell: committed, the lines; pending, the lines or none.
pub(super) type Details<T> = Vec<FxHashMap<Coordinate, T>>;

/// A detail where it is: its cube, its cell, and its lines, none where it is taken away.
type Placed<'a> = (usize, &'a [ElementId], Option<&'a [Line]>);

/// Details addressed by name, written out as [`CellDetail`]s are, from names borrowed from
/// the model.
pub struct NamedDetails<'a> {
    model: &'a Model,
    details: Vec<Placed<'a>>,
}

impl<'a> NamedDetails<'a> {
    /// Details pending in a transaction: their lines, or none where they are taken away.
    pub(super) fn pending(model: &'a Model, details: &'a Details<Option<Vec<Line>>>) -> Self {
        let cubes = details.iter().enumerate();
        let each = cubes.flat_map(|(cube, details)| {
            details
                .iter()
                .map(move |(at, lines)| (cube, &**at, lines.as_deref()))
        });
        Self {
            model,
            details: each.collect(),
        }
    }

    /// Details as they are committed.
    pub(super) fn kept(model: &'a Model, details: &'a Details<Vec<Line>>) -> Self {
        let cubes = details.iter().enumerate();
        let each = cubes.flat_map(|(cube, details)| {
            details
                .iter()
                .map(move |(at, lines)| (cube, &**at, Some(lines.as_slice())))
        });
        Self {
            model,
            details: each.collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.details.is_empty()
    }

    /// Each detail as a [`CellDetail`].
    pub(super) fn each(&self) -> impl Iterator<Item = CellDetail> + '_ {
        self.details.iter().map(|&(cube, at, lines)| {
            let named = super::named(self.model, cube, at, Scalar::Number(0.0));
            CellDetail {
                cube: named.cube,
                at: named.at,
                lines: lines.map(<[Line]>::to_vec),
            }
        })
    }
}

impl Serialize for NamedDetails<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let model = self.model;
        serializer.collect_seq(self.details.iter().map(|&(cube, at, lines)| {
            let cube = &model.cubes()[cube];
            Detail {
                cube: &cube.name,
                at: Names { model, cube, at },
                lines,
            }
        }))
    }
}

/// One detail as a [`CellDetail`] is written, its names borrowed.
#[derive(Serialize)]
struct Detail<'a> {
    cube: &'a str,
    at: Names<'a>,
    lines: Option<&'a [Line]>,
}

impl Database {
    /// A cell's supporting detail, as a transaction would leave it, or as committed; no lines
    /// if it has none.
    pub fn detail(
        &self,
        transaction: Option<TransactionId>,
        cell: &CellRef,
    ) -> Result<Vec<Line>, Error> {
        let (cube, at) = self.locate(&cell.cube, &cell.at)?;
        Ok(self
            .detail_at(transaction, cube, &at)?
            .unwrap_or_default()
            .to_vec())
    }

    /// A cell's detail as a transaction would leave it, or as committed.
    pub(super) fn detail_at(
        &self,
        transaction: Option<TransactionId>,
        cube: usize,
        at: &[ElementId],
    ) -> Result<Option<&[Line]>, Error> {
        if let Some(id) = transaction
            && let Some(pending) = self.transaction(id)?.details[cube].get(at)
        {
            return Ok(pending.as_deref());
        }
        Ok(self.details[cube].get(at).map(Vec::as_slice))
    }

    /// Writes a leaf cell's supporting detail and, as one write with it, the lines' sum to the
    /// cell, within a transaction or committed at once. No lines take the detail away and
    /// leave the value as it is.
    pub fn write_detail(
        &mut self,
        transaction: Option<TransactionId>,
        cell: &CellRef,
        lines: Vec<Line>,
    ) -> Result<(), Error> {
        let refused = |reason: String| Error::Cell(CellError::Detail(reason));
        if lines.len() > MOST_LINES {
            return Err(refused(format!("a detail has at most {MOST_LINES} lines")));
        }
        for line in &lines {
            let label = line.label.trim();
            if label.chars().count() > LONGEST_LABEL || label.chars().any(char::is_control) {
                return Err(refused(format!(
                    "a line's label is at most {LONGEST_LABEL} characters, none of them control characters"
                )));
            }
            if !line.value.is_finite() {
                return Err(refused("a line's value is a finite number".to_owned()));
            }
        }
        let (cube, at) = self.locate(&cell.cube, &cell.at)?;
        let value = match lines.is_empty() {
            true => self.read(transaction, cell)?,
            false => Scalar::Number(lines.iter().map(|line| line.value).sum()),
        };
        let lines = (!lines.is_empty()).then(|| {
            let tidy = |line: Line| Line {
                label: line.label.trim().to_owned(),
                ..line
            };
            lines.into_iter().map(tidy).collect::<Vec<_>>()
        });
        let write = CellWrite {
            cube: cell.cube.clone(),
            at: cell.at.clone(),
            value,
        };
        self.within(transaction, |database, id| {
            database.stage(id, &CellWrites::from(std::slice::from_ref(&write)))?;
            // Staged plainly, the value took any detail away; the lines take its place.
            if let Some(lines) = lines {
                let transaction = database.transactions.get_mut(&id).expect("staged in");
                transaction.details[cube].insert(Coordinate::from(at.as_slice()), Some(lines));
            }
            Ok(())
        })
    }

    /// Takes in details as they were recorded, by name, where their cells still have a place
    /// in the model: lines kept, or a detail taken away. Says how many have none.
    pub fn load_details(&mut self, details: &[CellDetail]) -> usize {
        let mut skipped = 0;
        for detail in details {
            let located = self
                .locate(&detail.cube, &detail.at)
                .and_then(|(cube, at)| {
                    let number = Scalar::Number(0.0);
                    check(&self.model, &self.model.cubes()[cube], &at, &number)?;
                    Ok((cube, at))
                });
            let Ok((cube, at)) = located else {
                skipped += 1;
                continue;
            };
            let at = Coordinate::from(at.as_slice());
            match &detail.lines {
                Some(lines) => self.details[cube].insert(at, lines.clone()),
                None => self.details[cube].remove(&at),
            };
        }
        skipped
    }
}

/// Whether a cell has a detail, pending in a transaction or committed: the cell a plain write
/// to which takes its detail away.
pub(super) fn has_detail(
    pending: &FxHashMap<Coordinate, Option<Vec<Line>>>,
    committed: &FxHashMap<Coordinate, Vec<Line>>,
    at: &[ElementId],
) -> bool {
    match pending.get(at) {
        Some(lines) => lines.is_some(),
        None => committed.contains_key(at),
    }
}
