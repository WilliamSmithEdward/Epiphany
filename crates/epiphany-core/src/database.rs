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

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::Value;
use crate::cells::{Cells, Writes, check, coordinate};
use crate::model::{ElementId, Model, ModelDef, ModelError};
use locks::{Locks, Mode, Resource};

pub use error::Error;
pub use locks::LockMode;

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
    pub value: Value,
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
    locks: Locks,
    started: Instant,
    timeout: Duration,
}

impl Database {
    pub fn new(definition: ModelDef) -> Result<Self, ModelError> {
        let model = Model::compile(&definition)?;
        Ok(Self {
            cells: vec![Cells::default(); model.cubes().len()],
            definition,
            model,
            model_version: 1,
            transactions: HashMap::new(),
            last_transaction: 0,
            locks: Locks::default(),
            started: Instant::now(),
            timeout: DEFAULT_TIMEOUT,
        })
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
    pub fn read(&self, transaction: Option<TransactionId>, cell: &CellRef) -> Result<Value, Error> {
        let (cube, at) = self.locate(&cell.cube, &cell.at)?;
        let (cells, definition) = (&self.cells[cube], &self.model.cubes()[cube]);
        Ok(match transaction {
            None => cells.read(&self.model, definition, &at),
            Some(id) => {
                let pending = &self.transaction(id)?.writes[cube];
                cells.read_with(&self.model, definition, &at, pending)
            }
        })
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
        let staged = self.stage(id, writes);
        let finished = match staged {
            Ok(()) => self.commit(id),
            Err(_) => self.rollback(id),
        };
        staged.and(finished)
    }

    pub fn begin(&mut self) -> TransactionId {
        self.expire();
        self.last_transaction += 1;
        let transaction = Transaction {
            writes: vec![Writes::default(); self.cells.len()],
            used: AtomicU64::new(self.now()),
        };
        self.transactions.insert(self.last_transaction, transaction);
        self.last_transaction
    }

    /// Stores a transaction's writes and ends it.
    pub fn commit(&mut self, id: TransactionId) -> Result<(), Error> {
        self.expire();
        let transaction = self.end(id)?;
        for (cells, writes) in self.cells.iter_mut().zip(transaction.writes) {
            for (at, value) in writes {
                cells.set(&at, value);
            }
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
        let (cells, dropped_cells) =
            migrate::migrate(&self.model, &model, std::mem::take(&mut self.cells));
        for transaction in self.transactions.values_mut() {
            transaction.writes = vec![Writes::default(); cells.len()];
        }
        self.cells = cells;
        self.model = model;
        self.definition = definition;
        self.model_version += 1;
        Ok(ModelUpdate {
            version: self.model_version,
            dropped_cells,
        })
    }

    /// Checks a batch of writes and adds it to a transaction's pending writes.
    fn stage(&mut self, id: TransactionId, writes: &[CellWrite]) -> Result<(), Error> {
        self.transaction(id)?;
        let locate = |(index, write): (usize, &CellWrite)| {
            let located = self.locate(&write.cube, &write.at).and_then(|(cube, at)| {
                check(&self.model, &self.model.cubes()[cube], &at, &write.value)?;
                Ok((cube, at))
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
