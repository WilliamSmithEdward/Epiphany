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
//!
//! A sandbox is a transaction kept under a name for its owner: it holds no locks and never
//! expires, and its values wait in it until they are published ([`sandboxes`]).

mod batch;
use batch::Resolver;
mod comments;
mod details;
use details::{Details, has_detail};
mod error;
mod history;
mod load;
mod locks;
mod migrate;
mod sandboxes;
use sandboxes::Kept;
mod spread;

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};
use std::{fmt, io};

use serde::{Deserialize, Serialize};

use crate::Scalar;
use crate::access::{Access, Permit};
use crate::cells::CellError;
use crate::cells::{At, Cells, Coordinate, Writes, check, coordinate, offered};
use crate::mdx::{self, Asker, MdxError, QueryResult};
use crate::model::{
    Cube, CubeDef, DimensionDef, DimensionId, ElementDef, ElementId, Model, ModelDef, ModelError,
    PicklistDef, Shared,
};
use crate::reader::{Fed, Reader, Totals, feed};
use locks::{Locks, Mode, Resource};

pub use batch::{CellRefs, CellWrites};
pub use comments::{Comment, CommentChange};
pub use details::{CellDetail, Line, NamedDetails};
pub use error::Error;
pub use history::{Asked, CellChange, Changes, History, HistoryRead};
pub use locks::LockMode;
pub use sandboxes::{KeptSandbox, Sandbox};
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
    /// The checks of the cube that the cell's value breaks, in the order they are made.
    pub invalid: Vec<Invalid>,
    /// The lines the cell's value is the sum of, where it was written with supporting detail.
    pub detail: Vec<Line>,
}

/// A check of a cube that a cell's value breaks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Invalid {
    /// What the check says of such a value.
    pub message: String,
    /// Whether the check refuses such a value as it is written. One it breaks all the same
    /// was there before the check was.
    pub refuses: bool,
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
    fn commit(&mut self, writes: &NamedWrites<'_>) -> io::Result<()>;

    /// Records a new model version.
    fn model(&mut self, version: u64, definition: &ModelDef) -> io::Result<()>;

    /// Whether enough commits have been recorded that a snapshot should replace them.
    fn is_full(&self) -> bool;

    /// Records every stored cell, replacing the commits recorded so far.
    fn snapshot(&mut self, model_version: u64, cells: &NamedCells<'_>) -> io::Result<()>;

    /// How many bytes the journal keeps. Nothing if it keeps none it can count.
    fn bytes(&self) -> Option<OnDisk> {
        None
    }

    /// The models the journal keeps from before the current one, newest first. None if it
    /// keeps none.
    fn earlier_models(&self) -> Vec<EarlierModel> {
        Vec::new()
    }

    /// Records a change to the comments.
    fn comment(&mut self, change: &CommentChange) -> io::Result<()> {
        let _ = change;
        Ok(())
    }

    /// An earlier model the journal keeps, by the name it lists it under; nothing if it
    /// lists none of that name.
    fn earlier_model(&self, name: &str) -> io::Result<Option<ModelDef>> {
        let _ = name;
        Ok(None)
    }

    /// Records what a commit changed, for the history of cells, once the commit is
    /// recorded. Seeing that it is kept is the journal's affair: the commit stands either way.
    fn changes(&mut self, changes: &Changes<'_>) {
        let _ = changes;
    }

    /// Where the history of cells can be read, apart from the journal; nowhere if it keeps none.
    fn history(&self) -> Option<Box<dyn History>> {
        None
    }

    /// Keeps a sandbox as it now is, under its transaction id, or forgets it once it is
    /// deleted. Its values are its owner's alone until they are published, so they are kept
    /// apart from the commits; if keeping them fails, the change to the sandbox is refused.
    fn sandbox(&mut self, id: TransactionId, sandbox: Option<&KeptSandbox<'_>>) -> io::Result<()> {
        let _ = (id, sandbox);
        Ok(())
    }
}

/// A model kept from before the current one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct EarlierModel {
    /// The name it is kept under, to ask for it by.
    pub name: String,
    /// The model version it was.
    pub version: u64,
    /// When it was saved, in seconds since 1970, where the journal can say.
    pub saved: Option<u64>,
}

/// What a journal keeps on disk, in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OnDisk {
    /// The commits recorded since the last snapshot.
    pub log: u64,
    /// The last snapshot.
    pub snapshot: u64,
    /// The model as it is.
    pub model: u64,
    /// Every model kept from before it.
    pub earlier_models: u64,
}

/// An open transaction, as an administrator sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenTransaction {
    pub id: TransactionId,
    /// How long since it was last used.
    pub idle: Duration,
    /// How many writes it holds, waiting to be committed.
    pub pending: usize,
}

/// The writes of one commit, addressed by name, written out as a list of [`CellWrite`]s
/// is, from names borrowed from the model: copied, they took as much again as the
/// request that brought them.
pub struct NamedWrites<'a> {
    model: &'a Model,
    writes: &'a [Writes],
    /// The supporting detail written with the cells, or taken away by them.
    details: &'a Details<Option<Vec<Line>>>,
}

impl NamedWrites<'_> {
    fn is_empty(&self) -> bool {
        self.writes.iter().all(Writes::is_empty)
    }

    /// The supporting detail the writes bring, or take away, to be recorded with them.
    pub fn details(&self) -> NamedDetails<'_> {
        NamedDetails::pending(self.model, self.details)
    }
}

impl serde::Serialize for NamedWrites<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let model = self.model;
        let cubes = model.cubes().iter().zip(self.writes);
        serializer.collect_seq(cubes.flat_map(|(cube, writes)| {
            writes.iter().map(move |(at, value)| Named {
                model,
                cube,
                at: At::kept(at),
                value: value.clone(),
            })
        }))
    }
}

/// Every stored cell of a database, addressed by name. It is named a cell at a time as it is
/// written out: a snapshot of a million cells that named them all first took some 300 bytes
/// a cell beside the cells themselves, three times what they take.
pub struct NamedCells<'a> {
    model: &'a Model,
    cells: &'a [Cells],
    details: &'a Details<Vec<Line>>,
}

impl<'a> NamedCells<'a> {
    /// The supporting detail of the cells that have one, to be written out with them.
    pub fn details(&self) -> NamedDetails<'a> {
        NamedDetails::kept(self.model, self.details)
    }

    /// How many numbers each cube stores, by its name: written ahead of the cells, it lets
    /// whoever loads them make room for them at once ([`Database::reserve`]). Texts, few
    /// as a rule, are given room as they come.
    pub fn counts(&self) -> std::collections::BTreeMap<&'a str, usize> {
        let cubes = self.model.cubes().iter().zip(self.cells);
        cubes
            .map(|(cube, cells)| (cube.name.as_str(), cells.stored()))
            .collect()
    }

    fn each(&self) -> impl Iterator<Item = CellWrite> + '_ {
        let cubes = self.cells.iter().enumerate();
        cubes.flat_map(move |(cube, cells)| {
            let entries = cells.entries();
            entries.map(move |(at, value)| named(self.model, cube, &at, value))
        })
    }
}

impl serde::Serialize for NamedCells<'_> {
    /// Written as [`CellWrite`]s are, from names borrowed from the model rather than copied.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let model = self.model;
        let cubes = model.cubes().iter().zip(self.cells);
        serializer.collect_seq(cubes.flat_map(|(cube, cells)| {
            cells.entries().map(move |(at, value)| Named {
                model,
                cube,
                at,
                value,
            })
        }))
    }
}

/// A cell as a [`CellWrite`] is written, its names borrowed.
struct Named<'a> {
    model: &'a Model,
    cube: &'a Cube,
    at: At<'a>,
    value: Scalar,
}

impl serde::Serialize for Named<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut cell = serializer.serialize_struct("CellWrite", 3)?;
        cell.serialize_field("cube", &self.cube.name)?;
        let at = Names {
            model: self.model,
            cube: self.cube,
            at: &self.at,
        };
        cell.serialize_field("at", &at)?;
        cell.serialize_field("value", &self.value)?;
        cell.end()
    }
}

/// The names of a cell's elements, borrowed from the model.
struct Names<'a> {
    model: &'a Model,
    cube: &'a Cube,
    at: &'a [ElementId],
}

impl serde::Serialize for Names<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let dimensions = self.cube.dimensions.iter().zip(self.at);
        serializer.collect_seq(dimensions.map(|(&dimension, &element)| {
            &self.model.dimensions()[dimension as usize].elements[element as usize].name
        }))
    }
}

/// Whether two definitions give every cell the same place: the same dimensions, with the same
/// elements in the same order, each of the same type with the same children, and the same
/// cubes of the same dimensions. Subsets, attributes, picklists, the names of levels, views,
/// rules, feeders, checks and dashboards place no cell. A dimension the two share is alike
/// without a look at its elements, and nothing is copied to be compared.
fn places_cells_alike(a: &ModelDef, b: &ModelDef) -> bool {
    let element = |a: &ElementDef, b: &ElementDef| {
        let ElementDef {
            name,
            kind,
            children,
            attributes: _,
            picklist: _,
        } = a;
        *name == b.name && *kind == b.kind && *children == b.children
    };
    let dimension = |a: &Shared<DimensionDef>, b: &Shared<DimensionDef>| {
        let DimensionDef {
            name,
            kind,
            elements,
            attributes: _,
            subsets: _,
            levels: _,
        } = &**a;
        Shared::same(a, b)
            || (*name == b.name
                && *kind == b.kind
                && elements.len() == b.elements.len()
                && elements.iter().zip(&b.elements).all(|(a, b)| element(a, b)))
    };
    let cube = |a: &CubeDef, b: &CubeDef| {
        let CubeDef {
            name,
            dimensions,
            views: _,
            rules: _,
            feeders: _,
            validations: _,
        } = a;
        *name == b.name && *dimensions == b.dimensions
    };
    let ModelDef {
        dimensions,
        cubes,
        dashboards: _,
    } = a;
    dimensions.len() == b.dimensions.len()
        && dimensions
            .iter()
            .zip(&b.dimensions)
            .all(|(a, b)| dimension(a, b))
        && cubes.len() == b.cubes.len()
        && cubes.iter().zip(&b.cubes).all(|(a, b)| cube(a, b))
}

/// A cell and its value, addressed by name.
fn named(model: &Model, cube: usize, at: &[ElementId], value: Scalar) -> CellWrite {
    let cube = &model.cubes()[cube];
    let name = |(&dimension, &element): (&u32, &ElementId)| {
        let dimension = &model.dimensions()[dimension as usize];
        dimension.elements[element as usize].name.clone()
    };
    CellWrite {
        cube: cube.name.clone(),
        at: cube.dimensions.iter().zip(at).map(name).collect(),
        value,
    }
}

#[derive(Debug)]
struct Transaction {
    /// Pending writes to each cube, in [`Model::cubes`] order.
    writes: Vec<Writes>,
    /// Supporting detail written with pending writes, or taken away by them, by cube.
    details: Details<Option<Vec<Line>>>,
    /// What each write did, in order. Played back, it takes the transaction to an earlier
    /// point, as [`Database::undo_to`] does.
    undo: Vec<Undo>,
    /// When the transaction was last used, in milliseconds since the database started.
    used: AtomicU64,
    /// Who began it, as the history of cells records its commit.
    author: Option<String>,
    /// Its name and owner, for a sandbox.
    kept: Option<Kept>,
}

/// What a write did to a transaction: the cube, the cell, and the value and the supporting
/// detail the cell had pending before, if any.
#[derive(Debug, Clone)]
struct Undo {
    cube: usize,
    at: Coordinate,
    value: Option<Scalar>,
    detail: Option<Option<Vec<Line>>>,
}

impl Transaction {
    /// An empty transaction over a model's cubes, used now and signed by its author.
    fn new(cubes: usize, now: u64, author: Option<String>) -> Self {
        Self {
            writes: vec![Writes::default(); cubes],
            details: vec![Default::default(); cubes],
            undo: Vec::new(),
            used: AtomicU64::new(now),
            author,
            kept: None,
        }
    }
}

#[derive(Debug)]
pub struct Database {
    definition: ModelDef,
    model: Model,
    model_version: u64,
    /// The model version in which each dimension last changed, in the model's order.
    dimension_versions: Vec<u64>,
    /// The committed cells of each cube, in [`Model::cubes`] order.
    cells: Vec<Cells>,
    /// The supporting detail of committed cells, by cube.
    details: Details<Vec<Line>>,
    transactions: HashMap<TransactionId, Transaction>,
    last_transaction: TransactionId,
    /// Where transaction ids come from, if not counted up from one.
    ids: Option<fn() -> TransactionId>,
    locks: Locks,
    started: Instant,
    timeout: Duration,
    /// Behind a lock of its own, so that a snapshot can be written while the database is
    /// only being read.
    journal: Option<Mutex<Box<dyn Journal>>>,
    /// Whether a commit that fills the log leaves the snapshot to `snapshot_if_wanted`.
    deferred: bool,
    /// Whether a snapshot is wanted and has not been taken.
    wanted: AtomicBool,
    /// The cells the feeders feed, worked out when first needed after the data changes.
    fed: OnceLock<Result<Fed, MdxError>>,
    /// The consolidated cells worked out since the data last changed.
    totals: Totals,
    /// Notes pinned to cells, the oldest first, and the id the last was given.
    comments: Vec<Comment>,
    last_comment: u64,
    /// Who the transactions begun next are signed with, for the history of cells.
    author: Option<String>,
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
            cells: model
                .cubes()
                .iter()
                .map(|cube| Cells::for_cube(&model, cube))
                .collect(),
            details: vec![Default::default(); model.cubes().len()],
            dimension_versions: vec![model_version; definition.dimensions.len()],
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
            deferred: false,
            wanted: AtomicBool::new(false),
            fed: OnceLock::new(),
            totals: Totals::default(),
            comments: Vec::new(),
            last_comment: 0,
            author: None,
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
        self.changed();
        skipped
    }

    /// Makes room in a cube for numbers about to be loaded. Nothing if there is no such cube.
    pub fn reserve(&mut self, cube: &str, cells: usize) {
        if let Some(id) = self.model.cube_id(cube) {
            self.cells[id].reserve(cells);
        }
    }

    /// Records every later commit and model update in a journal.
    pub fn set_journal(&mut self, journal: Box<dyn Journal>) {
        self.journal = Some(Mutex::new(journal));
    }

    /// Leaves the snapshot that a full log calls for to [`Database::snapshot_if_wanted`],
    /// instead of taking it in the commit that fills the log. A snapshot of millions of cells
    /// takes seconds; taken in a commit, it holds the database for writing all that while,
    /// and nobody can read. Its owner can take it afterwards holding the database only for
    /// reading, and readers carry on.
    pub fn defer_snapshots(&mut self) {
        self.deferred = true;
    }

    /// Whether a commit has called for a snapshot that has not been taken.
    pub fn wants_snapshot(&self) -> bool {
        self.wanted.load(Ordering::Acquire)
    }

    /// Takes the snapshot a commit has called for, if one has. Commits wait for it, but
    /// reads do not: it needs the database only for reading. Says whether it took one.
    pub fn snapshot_if_wanted(&self) -> bool {
        let wanted = self.wanted.swap(false, Ordering::AcqRel);
        if wanted {
            self.snapshot();
        }
        wanted
    }

    fn journal(&self) -> Option<MutexGuard<'_, Box<dyn Journal>>> {
        let journal = self.journal.as_ref()?;
        Some(journal.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Every stored cell, addressed by name.
    pub fn cells(&self) -> Vec<CellWrite> {
        self.named_cells().each().collect()
    }

    /// Every stored cell, addressed by name, to be written out a cell at a time.
    pub fn named_cells(&self) -> NamedCells<'_> {
        NamedCells {
            model: &self.model,
            cells: &self.cells,
            details: &self.details,
        }
    }

    /// The stored cells of one cube, addressed by name: how many there are, and those from
    /// `offset` on, at most `limit` of them. They come in the order of their elements in
    /// each dimension, the last dimension turning fastest, so that the same data always
    /// lists the same way and can be taken a page at a time. One whom `access` binds is shown
    /// only the cells they may see.
    pub fn stored(
        &self,
        cube: &str,
        offset: usize,
        limit: usize,
        access: Option<&Access>,
    ) -> Result<(usize, Vec<CellWrite>), Error> {
        let id = self.cube_id(cube)?;
        let cells = &self.cells[id];
        let definition = &self.model.cubes()[id];
        let (total, page) = match access {
            Some(access) => cells.page_kept(offset, limit, |at| {
                access.cell(definition, at) > Permit::Hidden
            }),
            None => cells.page(offset, limit),
        };
        let named = page.iter().map(|at| self.named(id, at, cells.value(at)));
        Ok((total, named.collect()))
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

    /// The model version in which a dimension last changed: what it holds, and what is worked
    /// out from it, is the same in every version since. One that offers another dimension's
    /// members in a picklist changes with the model, as that dimension may have.
    pub fn dimension_version(&self, dimension: DimensionId) -> u64 {
        self.dimension_versions[dimension as usize]
    }

    /// How many cells each cube stores, by its name, in the model's order.
    pub fn cell_counts(&self) -> Vec<(String, usize)> {
        let cubes = self.model.cubes().iter().zip(&self.cells);
        cubes
            .map(|(cube, cells)| (cube.name.clone(), cells.len()))
            .collect()
    }

    /// The transactions that are open, the longest idle first. Sandboxes, which hold no
    /// locks and never expire, are not among them.
    pub fn open_transactions(&self) -> Vec<OpenTransaction> {
        let now = self.now();
        let mut open: Vec<OpenTransaction> = self
            .transactions
            .iter()
            .filter(|(_, transaction)| transaction.kept.is_none())
            .map(|(&id, transaction)| OpenTransaction {
                id,
                idle: Duration::from_millis(
                    now.saturating_sub(transaction.used.load(Ordering::Relaxed)),
                ),
                pending: transaction.pending(),
            })
            .collect();
        open.sort_by_key(|transaction| std::cmp::Reverse(transaction.idle));
        open
    }

    /// How many bytes the journal keeps on disk.
    pub fn journal_bytes(&self) -> Option<OnDisk> {
        self.journal()?.bytes()
    }

    /// The models kept from before the current one, newest first.
    pub fn earlier_models(&self) -> Vec<EarlierModel> {
        self.journal()
            .map(|journal| journal.earlier_models())
            .unwrap_or_default()
    }

    /// A model kept from before the current one, by the name it is kept under.
    pub fn earlier_model(&self, name: &str) -> Result<ModelDef, Error> {
        let kept = match self.journal() {
            Some(journal) => journal.earlier_model(name)?,
            None => None,
        };
        kept.ok_or_else(|| Error::NotFound(format!("no earlier model is kept as '{name}'")))
    }

    /// Reads a cell: its committed value, or within a transaction its value as that
    /// transaction would leave it.
    pub fn read(
        &self,
        transaction: Option<TransactionId>,
        cell: &CellRef,
    ) -> Result<Scalar, Error> {
        self.read_many(transaction, std::slice::from_ref(cell))
            .remove(0)
    }

    /// Reads many cells, each as [`Database::read`] would. The totals among them are added
    /// up together where that is less work than adding up each by itself.
    pub fn read_many(
        &self,
        transaction: Option<TransactionId>,
        cells: &[CellRef],
    ) -> Vec<Result<Scalar, Error>> {
        self.read_batch(transaction, &CellRefs::from(cells), None)
    }

    /// Reads every cell of a batch, as [`Database::read_many`] does, for one whom `access`
    /// binds, where elements' permits bind them: a cell at an element hidden from them is
    /// refused as one at an element there is not.
    pub fn read_batch(
        &self,
        transaction: Option<TransactionId>,
        cells: &CellRefs,
        access: Option<&Access>,
    ) -> Vec<Result<Scalar, Error>> {
        let reader = match self.reader(transaction) {
            Ok(reader) => reader,
            Err(error) => return vec![Err(error); cells.len()],
        };
        let mut names = Resolver::new(&self.model, cells.listed());
        let located: Vec<_> = (0..cells.len())
            .map(|cell| {
                let (cube, at) = names.locate(cell)?;
                self.permitted(access, cube, at, cells.listed().at(cell))?;
                Ok((cube, Coordinate::from(at)))
            })
            .collect();
        let mut expected: Vec<Vec<&[ElementId]>> = vec![Vec::new(); self.cells.len()];
        for (cube, at) in located.iter().flatten() {
            expected[*cube].push(at);
        }
        for (cube, cells) in expected.iter().enumerate() {
            reader.expect(cube, cells);
        }
        let read = |located: &Result<(usize, Coordinate), CellError>| {
            let (cube, at) = located.as_ref().map_err(Clone::clone)?;
            let value = reader.read(*cube, at);
            value.map_err(|error| Error::Rule(error.to_string()))
        };
        located.iter().map(read).collect()
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
    /// cells at once. For one whom `access` binds, a cell at an element hidden from them is
    /// refused as one at an element there is not, one they may only read is not writable, and
    /// the cells feeding it that they may not see are left out.
    pub fn explain(
        &self,
        transaction: Option<TransactionId>,
        cell: &CellRef,
        feeders: bool,
        access: Option<&Access>,
    ) -> Result<Explanation, Error> {
        let (cube, at) = self.locate(&cell.cube, &cell.at)?;
        let permit = self.permitted(access, cube, &at, cell.at.iter().map(String::as_str))?;
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
        let seen = |source: usize, from: &[ElementId]| {
            access.is_none_or(|access| {
                access.cell(&self.model.cubes()[source], from) > Permit::Hidden
            })
        };
        let fed_by = fed_by
            .into_iter()
            .filter(|(source, _, from)| seen(*source, from));
        let fed_by = fed_by.map(|(source, index, from)| {
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
        let mut invalid = Vec::new();
        for validation in &self.model.cubes()[cube].validations {
            let covered = validation.check.covers(&at, text, consolidated);
            if covered && !reader.holds(cube, &validation.check, &at).map_err(failed)? {
                invalid.push(Invalid {
                    message: validation.message.clone(),
                    refuses: validation.refuse,
                });
            }
        }
        Ok(Explanation {
            value: reader.read(cube, &at).map_err(failed)?,
            kind: match (text, consolidated) {
                (true, _) => CellKind::Text,
                (_, true) => CellKind::Consolidated,
                _ => CellKind::Leaf,
            },
            stored: (!consolidated).then_some(stored),
            writable: !consolidated
                && permit == Permit::Write
                && self.model.cubes()[cube].rule_for_leaf(&at, text).is_none(),
            rule,
            fed: leaf.then(|| reader.is_fed(cube, &at)).flatten(),
            fed_by: fed_by.collect(),
            invalid,
            detail: self
                .detail_at(transaction, cube, &at)?
                .unwrap_or_default()
                .to_vec(),
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
        let (fed, totals) =
            match pending.filter(|pending| pending.iter().any(|writes| !writes.is_empty())) {
                Some(pending) => {
                    let fed = feed(&self.model, &self.cells, Some(pending));
                    (Cow::Owned(fed.map_err(|error| rule_error(&error))?), None)
                }
                None => {
                    let fed = self
                        .fed
                        .get_or_init(|| feed(&self.model, &self.cells, None));
                    let fed = Cow::Borrowed(fed.as_ref().map_err(rule_error)?);
                    (fed, Some(&self.totals))
                }
            };
        Ok(Reader::new(&self.model, &self.cells, pending, fed, totals))
    }

    /// Answers an MDX query from committed values, or within a transaction from the values
    /// that transaction would leave.
    pub fn query(
        &self,
        transaction: Option<TransactionId>,
        text: &str,
    ) -> Result<QueryResult, Error> {
        self.query_as(transaction, text, &Asker::ANYONE)
    }

    /// Answers an MDX query for one who asks it: the cubes they may read, the members they
    /// stand at, the subsets they keep and the elements they may see, as [`Asker`] says.
    pub fn query_as(
        &self,
        transaction: Option<TransactionId>,
        text: &str,
        asker: &Asker,
    ) -> Result<QueryResult, Error> {
        let query = mdx::parse(text)?;
        let cube = self.cube_id(&query.cube)?;
        let refusal =
            |cube: usize| format!("cube '{}' may not be read", self.model.cubes()[cube].name);
        let readable: Vec<bool> = self
            .model
            .cubes()
            .iter()
            .map(|cube| (asker.may_read)(&cube.name))
            .collect();
        if !readable[cube] {
            return Err(Error::Forbidden(refusal(cube)));
        }
        let reader = self.reader(transaction)?;
        let read = |cube: usize, at: &[ElementId]| match readable[cube] {
            true => reader.read(cube, at),
            false => Err(MdxError::new(refusal(cube))),
        };
        let expect = |cells: &[&[ElementId]]| reader.expect(cube, cells);
        let occupied =
            |most: usize, show: &mut dyn FnMut(&[ElementId])| reader.occupied(cube, most, show);
        Ok(query.evaluate(&self.model, cube, &read, &expect, &occupied, asker)?)
    }

    /// The elements a subset of a dimension selects now: as the model worked them out, or,
    /// for a subset that reads cells, as the stored cells stand.
    pub fn subset_elements(&self, dimension: &str, subset: &str) -> Result<Vec<ElementId>, Error> {
        let missing = |what: &str| Error::NotFound(format!("unknown {what}"));
        let id = self
            .model
            .dimension_id(dimension)
            .ok_or_else(|| missing(&format!("dimension '{dimension}'")))?;
        let found = self.model.dimensions()[id as usize]
            .subset(subset)
            .ok_or_else(|| missing(&format!("subset '{subset}' of dimension '{dimension}'")))?;
        let Some(set) = found.reading() else {
            return Ok(found.elements.clone());
        };
        let reader = self.reader(None)?;
        let read = |cube: usize, at: &[ElementId]| reader.read(cube, at);
        Ok(mdx::subset_now(&self.model, id, set, &read)?)
    }

    /// Writes every cell, or none of them if any write is refused. Within a transaction the
    /// writes stay pending until it commits; otherwise they are committed at once.
    pub fn write(
        &mut self,
        transaction: Option<TransactionId>,
        writes: &[CellWrite],
    ) -> Result<(), Error> {
        self.write_batch(transaction, &CellWrites::from(writes), None)
    }

    /// Writes every cell of a batch, as [`Database::write`] does, for one whom `access`
    /// binds, where elements' permits bind them: a write to a cell at an element hidden from
    /// them is refused as one at an element there is not, and one to a cell they may only read
    /// as forbidden.
    pub fn write_batch(
        &mut self,
        transaction: Option<TransactionId>,
        batch: &CellWrites,
        access: Option<&Access>,
    ) -> Result<(), Error> {
        self.within(transaction, |database, id| {
            database.stage(id, batch, access)
        })
    }

    /// Makes a change to a transaction's pending cells, or makes it alone and commits it at
    /// once, all of it or none.
    fn within(
        &mut self,
        transaction: Option<TransactionId>,
        change: impl FnOnce(&mut Self, TransactionId) -> Result<(), Error>,
    ) -> Result<(), Error> {
        self.expire();
        if let Some(id) = transaction {
            return self.changing(id, |database| change(database, id));
        }
        let id = self.begin();
        let written = change(self, id).and_then(|()| self.commit(id));
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
        let transaction = Transaction::new(self.cells.len(), self.now(), self.author.clone());
        self.transactions.insert(id, transaction);
        id
    }

    /// Stores a transaction's writes and ends it. If the writes cannot be recorded in the
    /// journal, nothing is stored and the transaction stays open. A sandbox is published
    /// instead, and stays open, empty.
    pub fn commit(&mut self, id: TransactionId) -> Result<(), Error> {
        self.expire();
        if self.transaction(id)?.kept.is_some() {
            return self.publish(id);
        }
        let pending = self.transaction(id)?;
        let record = NamedWrites {
            model: &self.model,
            writes: &pending.writes,
            details: &pending.details,
        };
        if let Some(mut journal) = self.journal()
            && !record.is_empty()
        {
            journal.commit(&record)?;
        }

        let transaction = self.end(id)?;
        // What changed goes to the history of cells, read from the cells before they change.
        if let Some(mut journal) = self.journal() {
            let changes = self.changes(&transaction);
            if !changes.is_empty() {
                journal.changes(&changes);
            }
        }
        for (cells, writes) in self.cells.iter_mut().zip(transaction.writes) {
            for (at, value) in writes {
                cells.set(&at, value);
            }
        }
        for (details, pending) in self.details.iter_mut().zip(transaction.details) {
            for (at, lines) in pending {
                match lines {
                    Some(lines) => details.insert(at, lines),
                    None => details.remove(&at),
                };
            }
        }
        self.changed();
        if self.journal().is_some_and(|journal| journal.is_full()) {
            match self.deferred {
                true => self.wanted.store(true, Ordering::Release),
                false => self.snapshot(),
            }
        }
        Ok(())
    }

    /// Discards a transaction's writes and ends it. A sandbox is emptied, and stays open.
    pub fn rollback(&mut self, id: TransactionId) -> Result<(), Error> {
        self.expire();
        if self.transaction(id)?.kept.is_some() {
            return self.changing(id, |database| {
                database.empty(id);
                Ok(())
            });
        }
        self.end(id).map(drop)
    }

    /// The names of the cubes a transaction holds writes for.
    pub fn pending_cubes(&self, id: TransactionId) -> Result<Vec<String>, Error> {
        let transaction = self.transaction(id)?;
        let cubes = self.model.cubes().iter().zip(&transaction.writes);
        let written = cubes.filter(|(_, writes)| !writes.is_empty());
        Ok(written.map(|(cube, _)| cube.name.clone()).collect())
    }

    /// The point a transaction has come to: how many writes it has been given. Its writes
    /// since then can be taken back with [`Database::undo_to`].
    pub fn savepoint(&self, id: TransactionId) -> Result<usize, Error> {
        Ok(self.transaction(id)?.undo.len())
    }

    /// Takes back a transaction's writes since a point it came to, the last first, so that
    /// each cell is pending as it was then; the transaction stays open. Spreads and batches
    /// are taken back whole, being writes like any other.
    pub fn undo_to(&mut self, id: TransactionId, point: usize) -> Result<(), Error> {
        self.expire();
        let transaction = self.transaction(id)?;
        if point > transaction.undo.len() {
            return Err(Error::NotFound(format!(
                "transaction {id} has not come to point {point}; it is at {}",
                transaction.undo.len()
            )));
        }
        self.changing(id, |database| {
            database.take_back(id, point);
            Ok(())
        })
    }

    /// Takes back a transaction's writes since a point, the last first: each cell's pending
    /// value and detail are as they were.
    fn take_back(&mut self, id: TransactionId, point: usize) {
        let transaction = self.transactions.get_mut(&id).expect("a live transaction");
        for Undo {
            cube,
            at,
            value,
            detail,
        } in transaction.undo.drain(point..).rev()
        {
            match detail {
                Some(detail) => transaction.details[cube].insert(at.clone(), detail),
                None => transaction.details[cube].remove(&at),
            };
            match value {
                Some(value) => transaction.writes[cube].insert(at, value),
                None => transaction.writes[cube].remove(&at),
            };
        }
    }

    /// Locks a cube, or the whole server when no cube is named, until the transaction ends.
    /// A sandbox takes no locks: it would hold them for as long as it is kept.
    pub fn lock(
        &mut self,
        id: TransactionId,
        cube: Option<&str>,
        mode: LockMode,
    ) -> Result<(), Error> {
        self.expire();
        if self.transaction(id)?.kept.is_some() {
            return Err(Error::Sandbox(
                "a sandbox holds no locks; its values are checked and locked as it is published"
                    .to_owned(),
            ));
        }
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

        let model = Model::compile_change(&definition, &self.definition, &self.model)?;
        // Saving a dashboard need not carry millions of cells over and write them all out again.
        let moved = (!places_cells_alike(&definition, &self.definition))
            .then(|| migrate::migrate(&self.model, &model, &self.cells));
        if let Some(mut journal) = self.journal() {
            journal.model(self.model_version + 1, &definition)?;
        }
        // Supporting detail and a sandbox's values go where their names are in the new model,
        // as stored cells do; a plain transaction's are let go.
        let (details, sandboxes): (Vec<CellDetail>, Vec<sandboxes::Carried>) = match moved {
            Some(_) => (
                self.named_cells().details().each().collect(),
                self.transactions
                    .iter()
                    .filter(|(_, transaction)| transaction.kept.is_some())
                    .map(|(&id, transaction)| {
                        let pending = NamedDetails::pending(&self.model, &transaction.details);
                        (
                            id,
                            self.named_writes(&transaction.writes),
                            pending.each().collect(),
                        )
                    })
                    .collect(),
            ),
            None => Default::default(),
        };
        let version = self.model_version + 1;
        self.dimension_versions = definition
            .dimensions
            .iter()
            .map(|dimension| {
                let offers = |element: &ElementDef| {
                    matches!(element.picklist, Some(PicklistDef::Members(_)))
                };
                let was = self
                    .model
                    .dimension_id(&dimension.name)
                    .map(|id| id as usize);
                let kept = was.filter(|&id| self.definition.dimensions[id] == *dimension);
                match kept {
                    Some(id) if !dimension.elements.iter().any(offers) => {
                        self.dimension_versions[id]
                    }
                    _ => version,
                }
            })
            .collect();
        self.model = model;
        self.definition = definition;
        self.model_version = version;
        self.changed();
        let mut dropped_cells = 0;
        if let Some((cells, dropped)) = moved {
            for transaction in self.transactions.values_mut() {
                transaction.writes = vec![Writes::default(); cells.len()];
                transaction.details = vec![Default::default(); cells.len()];
                transaction.undo.clear();
            }
            self.details = vec![Default::default(); cells.len()];
            self.cells = cells;
            dropped_cells = dropped;
            self.load_details(&details);
            self.carry(sandboxes);
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
        asker: &Asker,
    ) -> Result<QueryResult, Error> {
        let cube = &self.model.cubes()[self.cube_id(cube)?];
        let saved = cube
            .view(view)
            .ok_or_else(|| Error::NotFound(format!("cube '{}' has no view '{view}'", cube.name)))?;
        self.query_as(transaction, &saved.mdx, asker)
    }

    /// Checks a batch of writes and adds it to a transaction's pending writes. A sandbox's
    /// writes take no locks and wait on nobody's: the locks are taken as it is published.
    fn stage(
        &mut self,
        id: TransactionId,
        batch: &CellWrites,
        access: Option<&Access>,
    ) -> Result<(), Error> {
        let kept = self.transaction(id)?.kept.is_some();
        let mut names = Resolver::new(&self.model, batch.listed());
        let mut located: Vec<(usize, Coordinate)> = Vec::with_capacity(batch.len());
        for index in 0..batch.len() {
            let mut locate = || {
                let (cube, at) = names.locate(index)?;
                self.writable(access, cube, at, batch.listed().at(index))?;
                let definition = &self.model.cubes()[cube];
                let value = batch.value(index);
                check(&self.model, definition, at, value)?;
                offered(&self.model, definition, at, value)?;
                let text = matches!(value, Scalar::Text(_));
                match definition.rule_for_leaf(at, text) {
                    Some(_) => Err(CellError::Calculated),
                    None => Ok((cube, Coordinate::from(at))),
                }
            };
            located.push(locate().map_err(|error| Error::Write { index, error })?);
        }
        if !kept {
            self.claim(id, &located, batch)?;
        }
        let transaction = self.transactions.get_mut(&id).expect("checked above");
        let point = transaction.undo.len();
        for (index, (cube, at)) in located.iter().enumerate() {
            let value = transaction.writes[*cube].insert(at.clone(), batch.value(index).clone());
            // A value written plainly takes the cell's detail away: its lines no longer add
            // up to it.
            let detail = transaction.details[*cube].get(at).cloned();
            if has_detail(&transaction.details[*cube], &self.details[*cube], at) {
                transaction.details[*cube].insert(at.clone(), None);
            }
            let cube = *cube;
            let at = at.clone();
            transaction.undo.push(Undo {
                cube,
                at,
                value,
                detail,
            });
        }
        // The checks are made with the whole batch in place, for one may read other cells
        // the batch writes; a value one refuses takes the batch back.
        if let Err(error) = self.validate(id, &located) {
            self.take_back(id, point);
            return Err(error);
        }
        Ok(())
    }

    /// Makes the checks that refuse values at each cell of a batch pending in a transaction,
    /// reading cells as the transaction would leave them.
    fn validate(&self, id: TransactionId, located: &[(usize, Coordinate)]) -> Result<(), Error> {
        let refusing = |cube: usize| {
            let validations = self.model.cubes()[cube].validations.iter();
            validations.filter(|validation| validation.refuse)
        };
        if located
            .iter()
            .all(|&(cube, _)| refusing(cube).next().is_none())
        {
            return Ok(());
        }
        let reader = self.reader(Some(id))?;
        for (index, (cube, at)) in located.iter().enumerate() {
            let (text, consolidated) = reader.kind(*cube, at);
            for validation in refusing(*cube) {
                if !validation.check.covers(at, text, consolidated) {
                    continue;
                }
                let held = reader.holds(*cube, &validation.check, at);
                if !held.map_err(|error| Error::Rule(error.to_string()))? {
                    let error = CellError::Invalid(validation.message.clone());
                    return Err(Error::Write { index, error });
                }
            }
        }
        Ok(())
    }

    /// Takes the locks a batch of writes needs, and makes sure no other transaction has
    /// any of its cells pending. A sandbox's pending values are no other's concern.
    fn claim(
        &mut self,
        id: TransactionId,
        located: &[(usize, Coordinate)],
        batch: &CellWrites,
    ) -> Result<(), Error> {
        // The locks are asked for once for the batch and once for each cube in it, not
        // again for every cell.
        let mut cubes: Vec<usize> = located.iter().map(|&(cube, _)| cube).collect();
        cubes.sort_unstable();
        cubes.dedup();
        if !cubes.is_empty() {
            self.acquire(id, Resource::Server, Mode::IntentExclusive)?;
        }
        for cube in cubes {
            self.acquire(id, Resource::Cube(cube), Mode::IntentExclusive)?;
        }
        for (index, (cube, at)) in located.iter().enumerate() {
            let pending_elsewhere = |(&other, transaction): (&TransactionId, &Transaction)| {
                let plain = transaction.kept.is_none();
                other != id && plain && transaction.writes[*cube].contains_key(&**at)
            };
            if self.transactions.iter().any(pending_elsewhere) {
                let names: Vec<&str> = batch.listed().at(index).collect();
                let cell = format!(
                    "cell ({}) of cube '{}'",
                    names.join(", "),
                    batch.cube(index)
                );
                return Err(Error::Locked(cell));
            }
        }
        Ok(())
    }

    /// Forgets what was worked out from the cells and the model as they were.
    fn changed(&mut self) {
        self.fed = OnceLock::new();
        self.totals = Totals::default();
    }

    /// Replaces the journal's recorded commits with a snapshot. The commits already recorded
    /// say the same thing, so if the snapshot fails they are kept and it is tried again after
    /// a later commit. Nothing can be committed meanwhile: a commit needs the database for
    /// writing, and this holds it at least for reading.
    fn snapshot(&self) {
        if let Some(mut journal) = self.journal() {
            let _ = journal.snapshot(self.model_version, &self.named_cells());
        }
    }

    /// A cell and its value, addressed by name.
    fn named(&self, cube: usize, at: &[ElementId], value: Scalar) -> CellWrite {
        named(&self.model, cube, at, value)
    }

    /// Removes a transaction and releases its locks.
    fn end(&mut self, id: TransactionId) -> Result<Transaction, Error> {
        self.locks.release(id);
        self.transactions
            .remove(&id)
            .ok_or(Error::UnknownTransaction(id))
    }

    /// Rolls back every transaction that has been idle for longer than the timeout. A
    /// sandbox is kept however long it waits.
    fn expire(&mut self) {
        let now = self.now();
        let timeout = self.timeout.as_millis() as u64;
        let idle = |transaction: &Transaction| {
            let waited = now.saturating_sub(transaction.used.load(Ordering::Relaxed));
            transaction.kept.is_none() && waited > timeout
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
            let waited = now.saturating_sub(transaction.used.load(Ordering::Relaxed));
            transaction.kept.is_some() || waited <= self.timeout.as_millis() as u64
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

    /// What one may do with a cell named by its cube and elements, where elements' permits
    /// bind them, as [`Database::read_batch`] finds it.
    pub fn permit(&self, cell: &CellRef, access: &Access) -> Result<Permit, Error> {
        let (cube, at) = self.locate(&cell.cube, &cell.at)?;
        let names = cell.at.iter().map(String::as_str);
        Ok(self.permitted(Some(access), cube, &at, names)?)
    }

    /// What one may do with a cell of a cube, where elements' permits bind them. A cell at an
    /// element hidden from them is refused as one at an element the dimension does not have,
    /// by the name it was asked for by, so that what is hidden cannot be told from what is
    /// not there.
    fn permitted<'n>(
        &self,
        access: Option<&Access>,
        cube: usize,
        at: &[ElementId],
        names: impl IntoIterator<Item = &'n str>,
    ) -> Result<Permit, CellError> {
        let Some(access) = access else {
            return Ok(Permit::Write);
        };
        let definition = &self.model.cubes()[cube];
        for ((&dimension, &element), name) in definition.dimensions.iter().zip(at).zip(names) {
            if !access.sees(dimension, element) {
                let dimension = self.model.dimensions()[dimension as usize].name.clone();
                let element = name.to_owned();
                return Err(CellError::UnknownElement { dimension, element });
            }
        }
        Ok(access.cell(definition, at))
    }

    /// Refuses a change to a cell that one may not see, as [`Database::permitted`] does, or
    /// may only read, saying which of its elements they may only read.
    fn writable<'n>(
        &self,
        access: Option<&Access>,
        cube: usize,
        at: &[ElementId],
        names: impl IntoIterator<Item = &'n str>,
    ) -> Result<(), CellError> {
        let (Some(access), Permit::Read) = (access, self.permitted(access, cube, at, names)?)
        else {
            return Ok(());
        };
        let dimensions = self.model.cubes()[cube].dimensions.iter().zip(at);
        let mut read_only = dimensions.filter(|&(&d, &e)| access.of(d, e) < Permit::Write);
        let (&dimension, &element) = read_only
            .next()
            .expect("a cell is read only for one of them");
        let dimension = &self.model.dimensions()[dimension as usize];
        let element = &dimension.elements[element as usize].name;
        Err(CellError::Forbidden(format!(
            "you may only read element '{element}' of dimension '{}'",
            dimension.name
        )))
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
