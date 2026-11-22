//! Sandboxes: transactions kept under a name for the user who owns them, to try values out
//! in before they are published to the stored cells.
//!
//! A sandbox is a transaction that is kept. It holds no locks, so it holds up nobody and
//! nobody's writes hold it up; it never expires; the journal keeps it, so it outlives the
//! process; and a change to the model carries its values to wherever their elements are, by
//! name, as it does the stored cells. Reads, writes, spreads and queries are made in it with
//! its transaction id, as in any transaction. Committing it publishes its values through a
//! transaction of their own, which takes the locks and makes the checks any commit does,
//! and leaves the sandbox open and empty; rolling it back empties it.

use serde::Serialize;

use super::{
    CellDetail, CellWrite, CellWrites, Database, Error, NamedDetails, NamedWrites, Transaction,
    TransactionId,
};
use crate::Scalar;
use crate::cells::{Coordinate, Writes, check};
use crate::model::key;

/// A sandbox's values and supporting detail, by name, carried into a new model.
pub(super) type Carried = (TransactionId, Vec<CellWrite>, Vec<CellDetail>);

/// The longest a sandbox's name may be, in characters.
const LONGEST: usize = 100;

/// A sandbox as its owner sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Sandbox {
    pub name: String,
    /// The transaction to read, write and query in it with. It is kept and never expires.
    pub transaction: TransactionId,
    /// How many cells it holds values for, not yet published.
    pub pending: usize,
}

/// What makes a transaction a sandbox: the name its owner keeps it under.
#[derive(Debug)]
pub(super) struct Kept {
    name: String,
    owner: String,
}

/// A sandbox as the journal keeps it: its name, its owner, its values and the supporting
/// detail written with them or taken away, addressed by name so that they can be placed
/// again in whatever the model has become.
#[derive(Serialize)]
pub struct KeptSandbox<'a> {
    pub name: &'a str,
    pub owner: &'a str,
    pub cells: NamedWrites<'a>,
    pub details: NamedDetails<'a>,
}

impl Database {
    /// A user's sandboxes, in the order of their names.
    pub fn sandboxes(&self, owner: &str) -> Vec<Sandbox> {
        let mut found: Vec<Sandbox> = self
            .transactions
            .iter()
            .filter_map(|(&id, transaction)| {
                let kept = transaction.kept.as_ref()?;
                let theirs = kept.owner.eq_ignore_ascii_case(owner);
                theirs.then(|| Sandbox {
                    name: kept.name.clone(),
                    transaction: id,
                    pending: transaction.pending(),
                })
            })
            .collect();
        found.sort_by_cached_key(|sandbox| key(&sandbox.name));
        found
    }

    /// Makes an empty sandbox for a user, under a name none of their others has.
    pub fn create_sandbox(&mut self, owner: &str, name: &str) -> Result<Sandbox, Error> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > LONGEST || name.chars().any(char::is_control) {
            return Err(Error::Sandbox(format!(
                "a sandbox's name is 1 to {LONGEST} characters, none of them control characters"
            )));
        }
        if self.sandbox_id(owner, name).is_some() {
            return Err(Error::Sandbox(format!(
                "there is a sandbox called '{name}' already"
            )));
        }
        let id = self.begin();
        let transaction = self.transactions.get_mut(&id).expect("just begun");
        transaction.author = Some(owner.to_owned());
        transaction.kept = Some(Kept {
            name: name.to_owned(),
            owner: owner.to_owned(),
        });
        if let Err(error) = self.keep(id) {
            self.end(id)?;
            return Err(error);
        }
        Ok(Sandbox {
            name: name.to_owned(),
            transaction: id,
            pending: 0,
        })
    }

    /// Deletes one of a user's sandboxes, with the values it holds.
    pub fn delete_sandbox(&mut self, owner: &str, name: &str) -> Result<(), Error> {
        let id = self
            .sandbox_id(owner, name)
            .ok_or_else(|| Error::NotFound(format!("there is no sandbox called '{name}'")))?;
        if let Some(mut journal) = self.journal() {
            journal.sandbox(id, None)?;
        }
        self.end(id).map(drop)
    }

    /// Takes in a sandbox the journal kept, under the transaction id it had, with those of
    /// its values and their supporting detail that still have a place in the model. Says how
    /// many values have none.
    pub fn restore_sandbox(
        &mut self,
        id: TransactionId,
        owner: &str,
        name: &str,
        cells: &[CellWrite],
        details: &[CellDetail],
    ) -> usize {
        let mut transaction =
            Transaction::new(self.cells.len(), self.now(), Some(owner.to_owned()));
        transaction.kept = Some(Kept {
            name: name.to_owned(),
            owner: owner.to_owned(),
        });
        self.transactions.insert(id, transaction);
        self.place(id, cells, details)
    }

    /// The transaction of a user's sandbox, found by its name.
    fn sandbox_id(&self, owner: &str, name: &str) -> Option<TransactionId> {
        let name = key(name);
        self.transactions.iter().find_map(|(&id, transaction)| {
            let kept = transaction.kept.as_ref()?;
            let found = kept.owner.eq_ignore_ascii_case(owner) && key(&kept.name) == name;
            found.then_some(id)
        })
    }

    /// Puts values, and the supporting detail written with them or taken away, into a
    /// sandbox wherever they have a place in the model, as stored cells are loaded; says how
    /// many values have none.
    fn place(&mut self, id: TransactionId, cells: &[CellWrite], details: &[CellDetail]) -> usize {
        let mut dropped = 0;
        let put = |database: &mut Self, cube: &str, at: &[String], value: &Scalar| {
            let located = database.locate(cube, at).and_then(|(cube, at)| {
                check(&database.model, &database.model.cubes()[cube], &at, value)?;
                Ok((cube, Coordinate::from(at.as_slice())))
            });
            located.ok()
        };
        for cell in cells {
            match put(self, &cell.cube, &cell.at, &cell.value) {
                Some((cube, at)) => {
                    let transaction = self.transactions.get_mut(&id).expect("placed into");
                    transaction.writes[cube].insert(at, cell.value.clone());
                }
                None => dropped += 1,
            }
        }
        for detail in details {
            if let Some((cube, at)) = put(self, &detail.cube, &detail.at, &Scalar::Number(0.0)) {
                let transaction = self.transactions.get_mut(&id).expect("placed into");
                transaction.details[cube].insert(at, detail.lines.clone());
            }
        }
        dropped
    }

    /// Has the journal keep a sandbox as it now is. Nothing for a plain transaction.
    pub(super) fn keep(&self, id: TransactionId) -> Result<(), Error> {
        let Some(transaction) = self.transactions.get(&id) else {
            return Ok(());
        };
        let (Some(kept), Some(mut journal)) = (&transaction.kept, self.journal()) else {
            return Ok(());
        };
        let sandbox = KeptSandbox {
            name: &kept.name,
            owner: &kept.owner,
            cells: NamedWrites {
                model: &self.model,
                writes: &transaction.writes,
                details: &transaction.details,
            },
            details: NamedDetails::pending(&self.model, &transaction.details),
        };
        Ok(journal.sandbox(id, Some(&sandbox))?)
    }

    /// Changes a transaction; a sandbox is then kept as the change leaves it or, should it
    /// not be kept, put back as it was and the change refused.
    pub(super) fn changing<T>(
        &mut self,
        id: TransactionId,
        change: impl FnOnce(&mut Self) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let transaction = self.transaction(id)?;
        let before = transaction.kept.is_some().then(|| {
            (
                transaction.writes.clone(),
                transaction.details.clone(),
                transaction.undo.clone(),
            )
        });
        let done = change(self)?;
        if let Some(before) = before
            && let Err(error) = self.keep(id)
        {
            let transaction = self.transactions.get_mut(&id).expect("changed above");
            (transaction.writes, transaction.details, transaction.undo) = before;
            return Err(error);
        }
        Ok(done)
    }

    /// Publishes a sandbox's values to the stored cells through a transaction of their own,
    /// signed by the sandbox's owner, then empties the sandbox. A value that can no longer
    /// be written there refuses the whole, naming its cell, and the sandbox keeps them all.
    pub(super) fn publish(&mut self, id: TransactionId) -> Result<(), Error> {
        let sandbox = self.transaction(id)?;
        let author = sandbox.author.clone();
        let writes = self.named_writes(&sandbox.writes);
        let details = sandbox.details.clone();
        if !writes.is_empty() {
            let published = self.begin();
            self.transactions
                .get_mut(&published)
                .expect("just begun")
                .author = author;
            let batch = CellWrites::from(writes.as_slice());
            // Staged plainly, the values took any detail away; the sandbox's own, written
            // with them or taken away, stand in its place.
            let detailed = |database: &mut Self| {
                let transaction = database.transactions.get_mut(&published).expect("begun");
                for (pending, sandboxed) in transaction.details.iter_mut().zip(details) {
                    pending.extend(sandboxed);
                }
            };
            let done = self
                .stage(published, &batch)
                .map(|()| detailed(self))
                .and_then(|()| self.commit(published));
            if let Err(error) = done {
                // Refused before or by the commit, the transaction is still open.
                self.end(published)?;
                return Err(match error {
                    Error::Write { index, error } => {
                        let cell = &writes[index];
                        Error::Sandbox(format!(
                            "cell ({}) of cube '{}' cannot be published: {error}",
                            cell.at.join(", "),
                            cell.cube
                        ))
                    }
                    error => error,
                });
            }
        }
        self.changing(id, |database| {
            database.empty(id);
            Ok(())
        })
    }

    /// Takes every value and detail out of a transaction, and every point it came to.
    pub(super) fn empty(&mut self, id: TransactionId) {
        if let Some(transaction) = self.transactions.get_mut(&id) {
            let cubes = transaction.writes.len();
            transaction.writes = vec![Writes::default(); cubes];
            transaction.details = vec![Default::default(); cubes];
            transaction.undo.clear();
        }
    }

    /// A transaction's writes, addressed by name.
    pub(super) fn named_writes(&self, writes: &[Writes]) -> Vec<CellWrite> {
        let cubes = writes.iter().enumerate();
        let named = |cube: usize, (at, value): (&Coordinate, &crate::Scalar)| {
            self.named(cube, at, value.clone())
        };
        cubes
            .flat_map(|(cube, writes)| writes.iter().map(move |write| named(cube, write)))
            .collect()
    }

    /// Carries the sandboxes' values and detail, named under the model they were written in,
    /// into the model that replaced it, dropping those that have no place in it. The journal
    /// then keeps each as it is. Should it fail to, what it keeps still names the values,
    /// and they are placed afresh from it in the model when the process starts again.
    pub(super) fn carry(&mut self, named: Vec<Carried>) {
        for (id, cells, details) in named {
            self.empty(id);
            self.place(id, &cells, &details);
            let _ = self.keep(id);
        }
    }
}

impl Transaction {
    /// How many cells it holds values for.
    pub(super) fn pending(&self) -> usize {
        self.writes.iter().map(Writes::len).sum()
    }
}
