//! A model together with the cells of its cubes, addressed by name and changed through
//! transactions.
//!
//! A transaction keeps its writes to itself until it commits. While it is open it holds an
//! intent lock on the server and on every cube it has written to, and no other transaction
//! may write a cell it has written. Nothing waits: a conflicting request fails at once and
//! may be retried. A transaction left idle for longer than the timeout is rolled back.
//!
//! Reads take no locks and see committed values plus the transaction's own writes. A client
//! that writes a value computed from what it read must first lock the cube, or another
//! transaction may commit a change between its read and its write.

mod error;
mod locks;
mod migrate;
mod spread;

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::{fmt, io};

use serde::{Deserialize, Serialize};

use crate::Scalar;
use crate::cells::CellError;
use crate::cells::{Cells, Writes, check, coordinate};
use crate::mdx::{self, MdxError, QueryResult};
use crate::model::{ElementId, Model, ModelDef, ModelError};
use crate::reader::{Fed, Reader, feed};
use locks::{Locks, Mode, Resource};

pub use error::Error;
pub use locks::LockMode;
pub use spread::{Spread, SpreadMethod};

pub type TransactionId = u64;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

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
    pub value: Scalar,
}

/// How a cell comes by its value.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Explanation {
    pub value: Scalar,
    pub kind: CellKind,
    /// What is stored at the cell, for a leaf or text cell. A rule may hide it.
    pub stored: Option<Scalar>,
    /// Whether a value can be written to the cell: it is not consolidated and no rule covers it.
    pub writable: bool,
    /// The rule that gives the cell its value, if one does.
    pub rule: Option<Statement>,
    /// For a numeric leaf of a cube that is fed, whether the cell is fed. A calculated cell
    /// that holds a value but is not fed is missing from the cube's consolidations.
    pub fed: Option<bool>,
    /// The feeders that feed the cell, when asked for.
    pub fed_by: Vec<FedBy>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum CellKind {
    Leaf,
    Consolidated,
    Text,
}

/// A rule or feeder of a cube: its place in the cube's list, counted from one, and its text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Statement {
    pub number: usize,
    pub text: String,
}

/// A feeder that feeds a cell, and the cell it feeds it from. The feeder belongs to the cube
/// of that cell.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FedBy {
    pub feeder: Statement,
    pub from: CellRef,
}

/// The outcome of a model update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ModelUpdate {
    /// The new model version.
    pub version: u64,
    /// Stored cells that have no place in the new model and were discarded.
    pub dropped_cells: usize,
}

/// Where a database records its changes so that they outlive the process.
///
/// A commit or a new model is recorded before it takes effect; if recording fails, the
/// change is refused. A snapshot holds every stored cell and replaces the commits recorded
/// before it.
pub trait Journal: Send + Sync + fmt::Debug {
    /// Records the writes of one commit.
    fn commit(&mut self, writes: &[CellWrite]) -> io::Result<()>;

    /// Records a new model version.
    fn model(&mut self, version: u64, definition: &ModelDef) -> io::Result<()>;

    /// Whether enough commits have been recorded that a snapshot should replace them.
    fn is_full(&self) -> bool;

    /// Records every stored cell, replacing the commits recorded so far.
    fn snapshot(&mut self, model_version: u64, cells: &[CellWrite]) -> io::Result<()>;
}

#[derive(Debug)]
struct Transaction {
    /// Pending writes to each cube, in [`Model::cubes`] order.
    writes: Vec<Writes>,
    /// When the transaction was last used, in milliseconds since the database started.
    used: AtomicU64,
}

#[derive(Debug)]
pub struct Database {
    definition: ModelDef,
    model: Model,
    model_version: u64,
    /// The committed cells of each cube, in [`Model::cubes`] order.
    cells: Vec<Cells>,
    transactions: HashMap<TransactionId, Transaction>,
    last_transaction: TransactionId,
    /// Where transaction ids come from, if not counted up from one.
    ids: Option<fn() -> TransactionId>,
    locks: Locks,
    started: Instant,
    timeout: Duration,
    journal: Option<Box<dyn Journal>>,
    /// The cells the feeders feed, worked out when first needed after the data changes.
    fed: OnceLock<Result<Fed, MdxError>>,
}

impl Database {
    /// An empty database at model version one.
    pub fn new(definition: ModelDef) -> Result<Self, ModelError> {
        Self::restore(definition, 1)
    }

    /// An empty database at a given model version, to be filled with [`Database::load`].
    pub fn restore(definition: ModelDef, model_version: u64) -> Result<Self, ModelError> {
        let model = Model::compile(&definition)?;
        Ok(Self {
            cells: vec![Cells::default(); model.cubes().len()],
            definition,
            model,
            model_version,
            transactions: HashMap::new(),
            last_transaction: 0,
            ids: None,
            locks: Locks::default(),
            started: Instant::now(),
            timeout: DEFAULT_TIMEOUT,
            journal: None,
            fed: OnceLock::new(),
        })
    }

    /// Stores previously recorded cells without recording them again. Cells that no longer
    /// fit the model are skipped; returns how many.
    pub fn load(&mut self, cells: &[CellWrite]) -> usize {
        let mut skipped = 0;
        for write in cells {
            let located = self.locate(&write.cube, &write.at).and_then(|(cube, at)| {
                check(&self.model, &self.model.cubes()[cube], &at, &write.value)?;
                Ok((cube, at))
            });
            match located {
                Ok((cube, at)) => self.cells[cube].set(&at, write.value.clone()),
                Err(_) => skipped += 1,
            }
        }
        self.fed = OnceLock::new();
        skipped
    }

    /// Records every later commit and model update in a journal.
    pub fn set_journal(&mut self, journal: Box<dyn Journal>) {
        self.journal = Some(journal);
    }

    /// Every stored cell, addressed by name.
    pub fn cells(&self) -> Vec<CellWrite> {
        let mut named = Vec::new();
        for (cube, cells) in self.cells.iter().enumerate() {
            for (at, value) in cells.entries() {
                named.push(self.named(cube, at, value));
            }
        }
        named
    }

    /// Takes transaction ids from a source of its own choosing, such as random numbers that
    /// cannot be guessed, instead of counting up from one.
    pub fn set_transaction_ids(&mut self, ids: fn() -> TransactionId) {
        self.ids = Some(ids);
    }

    /// Sets how long a transaction may stay idle before it is rolled back.
    pub fn set_transaction_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    pub fn definition(&self) -> &ModelDef {
        &self.definition
    }

    pub fn model(&self) -> &Model {
        &self.model
    }

    pub fn model_version(&self) -> u64 {
        self.model_version
    }

    /// Reads a cell: its committed value, or within a transaction its value as that
    /// transaction would leave it.
    pub fn read(
        &self,
        transaction: Option<TransactionId>,
        cell: &CellRef,
    ) -> Result<Scalar, Error> {
        let (cube, at) = self.locate(&cell.cube, &cell.at)?;
        let value = self.reader(transaction)?.read(cube, &at);
        value.map_err(|error| Error::Rule(error.to_string()))
    }

    /// The leaf cells of a fed cube that a rule gives a value but nothing feeds, so that
    /// its consolidations leave them out. At most `limit` are returned; none means the cube
    /// is not fed, or its feeders are complete for the data it holds.
    pub fn unfed(&self, cube: &str, limit: usize) -> Result<Vec<CellRef>, Error> {
        let id = self.cube_id(cube)?;
        let cells = self.reader(None)?.unfed(id, limit);
        let cells = cells.map_err(|error| Error::Rule(error.to_string()))?;
        let named = |at: Vec<ElementId>| {
            let cell = self.named(id, &at, Scalar::Number(0.0));
            CellRef {
                cube: cell.cube,
                at: cell.at,
            }
        };
        Ok(cells.into_iter().map(named).collect())
    }

    /// Says how a cell comes by its value: whether it is stored, calculated by a rule or added
    /// up, whether it is fed, and, when `feeders` is set, which feeders feed it and from
    /// where. Finding the feeders looks at every stored and fed cell, so it is slow for many
    /// cells at once.
    pub fn explain(
        &self,
        transaction: Option<TransactionId>,
        cell: &CellRef,
        feeders: bool,
    ) -> Result<Explanation, Error> {
        let (cube, at) = self.locate(&cell.cube, &cell.at)?;
        let reader = self.reader(transaction)?;
        let failed = |error: MdxError| Error::Rule(error.to_string());
        let (text, consolidated) = reader.kind(cube, &at);
        let definition = &self.definition.cubes[cube];
        let rule = reader.ruling(cube, &at).map_err(failed)?;
        let rule = rule.map(|(index, _)| Statement {
            number: index + 1,
            text: definition.rules[index].clone(),
        });
        let leaf = !text && !consolidated;
        let fed_by = match feeders && leaf {
            true => reader.fed_by(cube, &at, 20).map_err(failed)?,
            false => Vec::new(),
        };
        let fed_by = fed_by.into_iter().map(|(source, index, from)| {
            let from = self.named(source, &from, Scalar::Number(0.0));
            FedBy {
                feeder: Statement {
                    number: index + 1,
                    text: self.definition.cubes[source].feeders[index].clone(),
                },
                from: CellRef {
                    cube: from.cube,
                    at: from.at,
                },
            }
        });
        let pending = transaction.and_then(|id| self.transactions.get(&id));
        let stored = self.cells[cube].read_with(
            &self.model,
            &self.model.cubes()[cube],
            &at,
            pending.map_or(&Writes::default(), |pending| &pending.writes[cube]),
        );
        Ok(Explanation {
            value: reader.read(cube, &at).map_err(failed)?,
            kind: match (text, consolidated) {
                (true, _) => CellKind::Text,
                (_, true) => CellKind::Consolidated,
                _ => CellKind::Leaf,
            },
            stored: (!consolidated).then_some(stored),
            writable: !consolidated && self.model.cubes()[cube].rule_for_leaf(&at, text).is_none(),
            rule,
            fed: leaf.then(|| reader.is_fed(cube, &at)).flatten(),
            fed_by: fed_by.collect(),
        })
    }

    /// A reader of committed cells, or within a transaction of the cells it would leave.
    fn reader(&self, transaction: Option<TransactionId>) -> Result<Reader<'_>, Error> {
        let pending = match transaction {
            Some(id) => Some(self.transaction(id)?.writes.as_slice()),
            None => None,
        };
        let rule_error = |error: &MdxError| Error::Rule(error.to_string());
        // What is fed depends on what is stored, so a transaction with writes of its own
        // has it worked out afresh; everyone else shares one answer until the data changes.
        let fed = match pending.filter(|pending| pending.iter().any(|writes| !writes.is_empty())) {
            Some(pending) => {
                let fed = feed(&self.model, &self.cells, Some(pending));
                Cow::Owned(fed.map_err(|error| rule_error(&error))?)
            }
            None => {
                let fed = self
                    .fed
                    .get_or_init(|| feed(&self.model, &self.cells, None));
                Cow::Borrowed(fed.as_ref().map_err(rule_error)?)
            }
        };
        Ok(Reader::new(&self.model, &self.cells, pending, fed))
    }

    /// Answers an MDX query from committed values, or within a transaction from the values
    /// that transaction would leave.
    pub fn query(
        &self,
        transaction: Option<TransactionId>,
        text: &str,
    ) -> Result<QueryResult, Error> {
        let query = mdx::parse(text)?;
        let cube = self.cube_id(&query.cube)?;
        let reader = self.reader(transaction)?;
        let read = |cube: usize, at: &[ElementId]| reader.read(cube, at);
        Ok(query.evaluate(&self.model, cube, &read)?)
    }

    /// Writes every cell, or none of them if any write is refused. Within a transaction the
    /// writes stay pending until it commits; otherwise they are committed at once.
    pub fn write(
        &mut self,
        transaction: Option<TransactionId>,
        writes: &[CellWrite],
    ) -> Result<(), Error> {
        self.expire();
        if let Some(id) = transaction {
            return self.stage(id, writes);
        }
        let id = self.begin();
        let written = self.stage(id, writes).and_then(|()| self.commit(id));
        if written.is_err() {
            self.end(id)?;
        }
        written
    }

    pub fn begin(&mut self) -> TransactionId {
        self.expire();
        // An id is never zero and never one in use.
        let id = loop {
            self.last_transaction += 1;
            let id = self.ids.map_or(self.last_transaction, |next| next());
            if id != 0 && !self.transactions.contains_key(&id) {
                break id;
            }
        };
        let transaction = Transaction {
            writes: vec![Writes::default(); self.cells.len()],
            used: AtomicU64::new(self.now()),
        };
        self.transactions.insert(id, transaction);
        id
    }

    /// Stores a transaction's writes and ends it. If the writes cannot be recorded in the
    /// journal, nothing is stored and the transaction stays open.
    pub fn commit(&mut self, id: TransactionId) -> Result<(), Error> {
        self.expire();
        let mut record = Vec::new();
        for (cube, writes) in self.transaction(id)?.writes.iter().enumerate() {
            for (at, value) in writes {
                record.push(self.named(cube, at, value.clone()));
            }
        }
        if let Some(journal) = &mut self.journal
            && !record.is_empty()
        {
            journal.commit(&record)?;
        }

        let transaction = self.end(id)?;
        for (cells, writes) in self.cells.iter_mut().zip(transaction.writes) {
            for (at, value) in writes {
                cells.set(&at, value);
            }
        }
        self.fed = OnceLock::new();
        if self
            .journal
            .as_ref()
            .is_some_and(|journal| journal.is_full())
        {
            self.snapshot();
        }
        Ok(())
    }

    /// Discards a transaction's writes and ends it.
    pub fn rollback(&mut self, id: TransactionId) -> Result<(), Error> {
        self.expire();
        self.end(id).map(drop)
    }

    /// Locks a cube, or the whole server when no cube is named, until the transaction ends.
    pub fn lock(
        &mut self,
        id: TransactionId,
        cube: Option<&str>,
        mode: LockMode,
    ) -> Result<(), Error> {
        self.expire();
        self.transaction(id)?;
        let Some(cube) = cube else {
            return self.acquire(id, Resource::Server, mode.mode());
        };
        let cube = self.cube_id(cube)?;
        self.acquire(id, Resource::Server, mode.intent())?;
        self.acquire(id, Resource::Cube(cube), mode.mode())
    }

    /// Replaces the model, keeping every stored cell that still has a place in it. The update
    /// must be based on the current version and is refused while any transaction holds a lock.
    pub fn update_model(
        &mut self,
        based_on: u64,
        definition: ModelDef,
    ) -> Result<ModelUpdate, Error> {
        self.expire();
        if based_on != self.model_version {
            return Err(Error::StaleVersion {
                based_on,
                current: self.model_version,
            });
        }
        // The update completes within this call, so the exclusive lock only has to be
        // obtainable; it need not be kept.
        let id = self.begin();
        let free = self.acquire(id, Resource::Server, Mode::Exclusive);
        self.end(id)?;
        free?;

        let model = Model::compile(&definition)?;
        // A change to subsets, views, rules and feeders alone leaves every cell where it is.
        let structure = |definition: &ModelDef| {
            let mut structure = definition.clone();
            structure
                .dimensions
                .iter_mut()
                .for_each(|d| d.subsets.clear());
            for cube in &mut structure.cubes {
                cube.views.clear();
                cube.rules.clear();
                cube.feeders.clear();
            }
            structure
        };
        let moved = (structure(&definition) != structure(&self.definition))
            .then(|| migrate::migrate(&self.model, &model, &self.cells));
        if let Some(journal) = &mut self.journal {
            journal.model(self.model_version + 1, &definition)?;
        }
        self.model = model;
        self.definition = definition;
        self.model_version += 1;
        self.fed = OnceLock::new();
        let mut dropped_cells = 0;
        if let Some((cells, dropped)) = moved {
            for transaction in self.transactions.values_mut() {
                transaction.writes = vec![Writes::default(); cells.len()];
            }
            self.cells = cells;
            dropped_cells = dropped;
            // The commits recorded so far name cells of the old model.
            self.snapshot();
        }
        Ok(ModelUpdate {
            version: self.model_version,
            dropped_cells,
        })
    }

    /// Runs a view saved with a cube.
    pub fn view(
        &self,
        transaction: Option<TransactionId>,
        cube: &str,
        view: &str,
    ) -> Result<QueryResult, Error> {
        let cube = &self.model.cubes()[self.cube_id(cube)?];
        let saved = cube
            .view(view)
            .ok_or_else(|| Error::NotFound(format!("cube '{}' has no view '{view}'", cube.name)))?;
        self.query(transaction, &saved.mdx)
    }

    /// Checks a batch of writes and adds it to a transaction's pending writes.
    fn stage(&mut self, id: TransactionId, writes: &[CellWrite]) -> Result<(), Error> {
        self.transaction(id)?;
        let locate = |(index, write): (usize, &CellWrite)| {
            let located = self.locate(&write.cube, &write.at).and_then(|(cube, at)| {
                let definition = &self.model.cubes()[cube];
                check(&self.model, definition, &at, &write.value)?;
                let text = matches!(write.value, Scalar::Text(_));
                match definition.rule_for_leaf(&at, text) {
                    Some(_) => Err(CellError::Calculated),
                    None => Ok((cube, at)),
                }
            });
            located.map_err(|error| Error::Write { index, error })
        };
        let located: Vec<(usize, Vec<ElementId>)> = writes
            .iter()
            .enumerate()
            .map(locate)
            .collect::<Result<_, _>>()?;

        for ((cube, at), write) in located.iter().zip(writes) {
            self.acquire(id, Resource::Server, Mode::IntentExclusive)?;
            self.acquire(id, Resource::Cube(*cube), Mode::IntentExclusive)?;
            let pending_elsewhere = |(&other, transaction): (&TransactionId, &Transaction)| {
                other != id && transaction.writes[*cube].contains_key(at.as_slice())
            };
            if self.transactions.iter().any(pending_elsewhere) {
                let cell = format!("cell ({}) of cube '{}'", write.at.join(", "), write.cube);
                return Err(Error::Locked(cell));
            }
        }

        let pending = &mut self
            .transactions
            .get_mut(&id)
            .expect("checked above")
            .writes;
        for ((cube, at), write) in located.into_iter().zip(writes) {
            pending[cube].insert(at.into(), write.value.clone());
        }
        Ok(())
    }

    /// Replaces the journal's recorded commits with a snapshot. The commits already recorded
    /// say the same thing, so if the snapshot fails they are kept and it is tried again after
    /// a later commit.
    fn snapshot(&mut self) {
        let cells = self.cells();
        if let Some(journal) = &mut self.journal {
            let _ = journal.snapshot(self.model_version, &cells);
        }
    }

    /// A cell and its value, addressed by name.
    fn named(&self, cube: usize, at: &[ElementId], value: Scalar) -> CellWrite {
        let cube = &self.model.cubes()[cube];
        let name = |(&dimension, &element): (&u32, &ElementId)| {
            let dimension = &self.model.dimensions()[dimension as usize];
            dimension.elements[element as usize].name.clone()
        };
        CellWrite {
            cube: cube.name.clone(),
            at: cube.dimensions.iter().zip(at).map(name).collect(),
            value,
        }
    }

    /// Removes a transaction and releases its locks.
    fn end(&mut self, id: TransactionId) -> Result<Transaction, Error> {
        self.locks.release(id);
        self.transactions
            .remove(&id)
            .ok_or(Error::UnknownTransaction(id))
    }

    /// Rolls back every transaction that has been idle for longer than the timeout.
    fn expire(&mut self) {
        let now = self.now();
        let timeout = self.timeout.as_millis() as u64;
        let idle = |transaction: &Transaction| {
            now.saturating_sub(transaction.used.load(Ordering::Relaxed)) > timeout
        };
        let expired: Vec<TransactionId> = self
            .transactions
            .iter()
            .filter_map(|(&id, transaction)| idle(transaction).then_some(id))
            .collect();
        for id in expired {
            self.end(id).expect("listed above");
        }
    }

    /// Finds a live transaction and marks it as used.
    fn transaction(&self, id: TransactionId) -> Result<&Transaction, Error> {
        let now = self.now();
        let live = |transaction: &&Transaction| {
            now.saturating_sub(transaction.used.load(Ordering::Relaxed))
                <= self.timeout.as_millis() as u64
        };
        let transaction = self.transactions.get(&id).filter(live);
        let transaction = transaction.ok_or(Error::UnknownTransaction(id))?;
        transaction.used.store(now, Ordering::Relaxed);
        Ok(transaction)
    }

    fn acquire(&mut self, id: TransactionId, resource: Resource, mode: Mode) -> Result<(), Error> {
        if self.locks.acquire(id, resource, mode) {
            return Ok(());
        }
        Err(Error::Locked(match resource {
            Resource::Server => "the server".to_owned(),
            Resource::Cube(cube) => format!("cube '{}'", self.model.cubes()[cube].name),
        }))
    }

    fn cube_id(&self, cube: &str) -> Result<usize, crate::cells::CellError> {
        let unknown = || crate::cells::CellError::UnknownCube {
            cube: cube.to_owned(),
        };
        self.model.cube_id(cube).ok_or_else(unknown)
    }

    /// Resolves a cube name and element names into a cube id and a coordinate.
    fn locate(
        &self,
        cube: &str,
        at: &[String],
    ) -> Result<(usize, Vec<ElementId>), crate::cells::CellError> {
        let id = self.cube_id(cube)?;
        Ok((id, coordinate(&self.model, &self.model.cubes()[id], at)?))
    }

    /// Milliseconds since the database started.
    fn now(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
}
