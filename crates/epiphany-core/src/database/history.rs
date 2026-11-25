//! The history of cells: what each commit changed, by whom and when. It is kept apart from
//! the journal's commits, which a snapshot replaces, so that it lasts; and it is read apart
//! from the database, so that reading a long one holds up nobody.

use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize};

use super::{Database, Error, Names, Transaction};
use crate::Scalar;
use crate::model::{ElementId, Model};

/// A change to a cell, as the history gives it back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CellChange {
    /// When it was committed, in seconds since 1970.
    pub time: u64,
    /// Who made it: the user who began the transaction it came in, where the server knew.
    pub author: Option<String>,
    pub cube: String,
    /// One element name for each dimension of the cube.
    pub at: Vec<String>,
    /// What the cell held before: zero, or empty text, where it held nothing.
    pub before: Scalar,
    /// What it held after.
    pub after: Scalar,
}

/// What to read from the history of cells: the changes to a cube, or to one cell of it, by
/// a user, from a time and until one, in seconds since 1970. What is left out is not asked
/// about. Names are as the history knows them: the cube's and the elements' own, a user's
/// in any case.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Asked {
    pub cube: Option<String>,
    pub at: Option<Vec<String>>,
    pub author: Option<String>,
    pub since: Option<u64>,
    pub until: Option<u64>,
}

impl Asked {
    /// Whether a commit, by when it was made and by whom, is among those asked for.
    pub fn takes_commit(&self, time: u64, author: Option<&str>) -> bool {
        self.since.is_none_or(|since| time >= since)
            && self.until.is_none_or(|until| time <= until)
            && self.author.as_ref().is_none_or(|wanted| {
                author.is_some_and(|author| author.to_lowercase() == wanted.to_lowercase())
            })
    }

    /// Whether a cell a commit changed is among those asked for.
    pub fn takes_cell(&self, cube: &str, at: &[String]) -> bool {
        self.cube.as_ref().is_none_or(|wanted| wanted == cube)
            && self.at.as_ref().is_none_or(|wanted| wanted == at)
    }
}

/// Where the history of cells is kept, to be read apart from the database.
pub trait History: Send {
    /// The changes recorded as asked, the newest first, at most `most` of them after
    /// passing over the newest `skip`.
    fn find(&self, asked: &Asked, skip: usize, most: usize) -> io::Result<Vec<CellChange>>;
}

/// The history of cells as asked of it, to be read once the database is let go.
pub struct HistoryRead {
    asked: Asked,
    history: Option<Box<dyn History>>,
}

impl HistoryRead {
    /// What is asked, its names as the history knows them.
    pub fn asked(&self) -> &Asked {
        &self.asked
    }

    /// The changes recorded as asked, the newest first, at most `most` of them after passing
    /// over the newest `skip`; none where the journal keeps no history. Asked again until the
    /// time of the oldest given, passing over those of that time already given, they go on
    /// from where they left off, however many share a time and whatever is recorded meanwhile.
    pub fn read(&self, skip: usize, most: usize) -> io::Result<Vec<CellChange>> {
        match &self.history {
            Some(history) => history.find(&self.asked, skip, most),
            None => Ok(Vec::new()),
        }
    }
}

/// What one commit changed, as the history keeps it: when, by whom, and each cell it
/// changed, with its value before and after. Written out as
/// `{"time", "author", "cells": [{"cube", "at", "before", "after"}]}`, the cells named from
/// names borrowed from the model, as the journal's commits are.
pub struct Changes<'a> {
    model: &'a Model,
    time: u64,
    author: Option<&'a str>,
    cells: Vec<Changed<'a>>,
}

/// One cell a commit changed.
struct Changed<'a> {
    cube: usize,
    at: &'a [ElementId],
    before: Scalar,
    after: &'a Scalar,
}

impl Changes<'_> {
    /// Whether the commit changed nothing: every value it wrote was there already.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }
}

impl Serialize for Changes<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut changes = serializer.serialize_struct("Changes", 3)?;
        changes.serialize_field("time", &self.time)?;
        changes.serialize_field("author", &self.author)?;
        changes.serialize_field("cells", &Cells(self))?;
        changes.end()
    }
}

/// The cells a commit changed, each named as it is written out.
struct Cells<'a>(&'a Changes<'a>);

impl Serialize for Cells<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let model = self.0.model;
        serializer.collect_seq(self.0.cells.iter().map(|changed| {
            let cube = &model.cubes()[changed.cube];
            Cell {
                cube: &cube.name,
                at: Names {
                    model,
                    cube,
                    at: changed.at,
                },
                before: &changed.before,
                after: changed.after,
            }
        }))
    }
}

#[derive(Serialize)]
struct Cell<'a> {
    cube: &'a str,
    at: Names<'a>,
    before: &'a Scalar,
    after: &'a Scalar,
}

impl Database {
    /// Signs what is begun next with a user's name, for the history of cells: each
    /// transaction keeps the name it was begun under, and its commit is recorded as theirs.
    /// `None` signs nothing.
    pub fn sign(&mut self, author: Option<&str>) {
        self.author = author.map(str::to_owned);
    }

    /// The history of cells as asked of it, to be read once the database is let go: a cube
    /// named as the model has it, in any case, and a cell by one element name or alias for
    /// each of its dimensions, put as the history knows them. The history knows a cell by its
    /// elements' names, so changes made before an element was renamed are not found under
    /// its new one.
    pub fn history(&self, mut asked: Asked) -> Result<HistoryRead, Error> {
        match (&asked.cube, &asked.at) {
            (Some(cube), Some(at)) => {
                let (id, coordinate) = self.locate(cube, at)?;
                let named = self.named(id, &coordinate, Scalar::Number(0.0));
                (asked.cube, asked.at) = (Some(named.cube), Some(named.at));
            }
            (Some(cube), None) => {
                asked.cube = Some(self.model.cubes()[self.cube_id(cube)?].name.clone());
            }
            (None, Some(_)) => {
                return Err(Error::NotFound(
                    "a cell is asked about within its cube: name the cube".to_owned(),
                ));
            }
            (None, None) => {}
        }
        Ok(HistoryRead {
            asked,
            history: self.journal().and_then(|journal| journal.history()),
        })
    }

    /// What a transaction's writes change: each cell as it is stored now and as it will
    /// be, those it leaves as they are passed over; signed with who began it, and dated now.
    pub(super) fn changes<'a>(&'a self, transaction: &'a Transaction) -> Changes<'a> {
        let cells = transaction
            .writes
            .iter()
            .enumerate()
            .flat_map(|(cube, writes)| {
                writes.iter().filter_map(move |(at, after)| {
                    let before = match (self.cells[cube].value(at), after) {
                        // A text cell holding nothing reads as no number: it held no text.
                        (Scalar::Number(0.0), Scalar::Text(_)) => Scalar::Text(String::new()),
                        (before, _) => before,
                    };
                    (before != *after).then_some(Changed {
                        cube,
                        at,
                        before,
                        after,
                    })
                })
            });
        Changes {
            model: &self.model,
            time: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_secs()),
            author: transaction.author.as_deref(),
            cells: cells.collect(),
        }
    }
}
