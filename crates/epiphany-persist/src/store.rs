//! A durable cube store: an in-memory cube backed by a snapshot plus a WAL.
//!
//! The snapshot is the canonical model-as-code text (ADR-0003) written by
//! `epiphany-core`; it is the latest checkpoint of the whole cube. The WAL
//! (`crate::wal`) is the append-only tail of leaf writes since that checkpoint.
//! Recovery loads the snapshot, then replays the WAL tail. A checkpoint (the
//! explicit full-persist command) rewrites the snapshot and clears the WAL.
//!
//! The store mutates the cube's cells through `set_leaf`, and grows its
//! dimensions only by appending elements (`extend_schema`, used by flows), which
//! is checkpointed immediately. Because growth is append-only, the element
//! indices a WAL record names stay valid against the snapshot they replay onto;
//! elements are never removed or reordered.
//!
//! Single-process: one process owns a cube's data directory at a time. Within a
//! process the engine serializes writers with a per-cube lock; the store does
//! not take an OS file lock, so concurrent processes over the same directory are
//! unsupported. A checkpoint makes the snapshot durable (fsync the temp file,
//! atomic rename over the live snapshot, then fsync the directory on Unix) BEFORE
//! it clears the WAL, so a crash can never leave a cleared WAL beside a snapshot
//! whose contents had not yet reached disk.

use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use epiphany_core::{
    validate_subset, validate_view, AttributeKind, AttributeValue, Cube, EdgeSpec, ElementKind,
    ElementSpec, Fixed, LoadError, Model, ModelError, Position, QueryError, RuleSet, RuleTest,
    Sandbox, SaveError, Subset, View,
};

use crate::wal::{self, Record};

const SNAPSHOT_FILE: &str = "snapshot.model";
const SNAPSHOT_TMP: &str = "snapshot.model.tmp";
const WAL_FILE: &str = "wal.log";

/// A single write in a batch: a numeric leaf value or a string cell value at a
/// coordinate (element indices, in dimension order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellWrite {
    Leaf { coord: Vec<u32>, value: Fixed },
    Str { coord: Vec<u32>, value: String },
}

/// An error from the durability layer.
#[derive(Debug)]
pub enum PersistError {
    /// A filesystem operation failed.
    Io(std::io::Error),
    /// Replaying the WAL produced a write the model rejected.
    Model(ModelError),
    /// The snapshot could not be loaded.
    Load(LoadError),
    /// The snapshot could not be written.
    Save(SaveError),
    /// The WAL header was missing or unrecognized.
    Corrupt(String),
    /// A write in a batch was rejected by the model; the batch was not applied.
    BatchRejected { index: usize, source: ModelError },
    /// A subset/view definition was structurally invalid; nothing was changed.
    Query(QueryError),
}

impl std::fmt::Display for PersistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PersistError::Io(e) => write!(f, "persistence I/O error: {e}"),
            PersistError::Model(e) => write!(f, "WAL replay rejected by model: {e}"),
            PersistError::Load(e) => write!(f, "could not load snapshot: {e}"),
            PersistError::Save(e) => write!(f, "could not write snapshot: {e}"),
            PersistError::Corrupt(m) => write!(f, "corrupt persistence: {m}"),
            PersistError::BatchRejected { index, source } => {
                write!(f, "batch write {index} rejected: {source}")
            }
            PersistError::Query(e) => write!(f, "invalid definition: {e}"),
        }
    }
}

impl std::error::Error for PersistError {}

impl From<std::io::Error> for PersistError {
    fn from(e: std::io::Error) -> Self {
        PersistError::Io(e)
    }
}
impl From<ModelError> for PersistError {
    fn from(e: ModelError) -> Self {
        PersistError::Model(e)
    }
}
impl From<LoadError> for PersistError {
    fn from(e: LoadError) -> Self {
        PersistError::Load(e)
    }
}
impl From<SaveError> for PersistError {
    fn from(e: SaveError) -> Self {
        PersistError::Save(e)
    }
}
impl From<QueryError> for PersistError {
    fn from(e: QueryError) -> Self {
        PersistError::Query(e)
    }
}

/// A model made durable by a snapshot plus a write-ahead log in a directory.
///
/// The snapshot is the whole model-as-code text (cube + named subsets + views);
/// the WAL is the append-only tail of cell writes since the last checkpoint.
/// Structural changes (defining or deleting a subset/view) are captured by an
/// immediate checkpoint, not the log, so the WAL/cell-write path is unchanged
/// and the element indices a record names stay valid against the snapshot.
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    model: Model,
    wal: File,
    sync_on_write: bool,
}

impl Store {
    /// Create a fresh store for `cube` in `dir`, writing the initial snapshot and
    /// an empty WAL. Any existing WAL in `dir` is replaced.
    pub fn create(dir: impl Into<PathBuf>, cube: Cube) -> Result<Self, PersistError> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let model = Model::new(cube);
        write_snapshot(&dir, &model)?;
        let wal = open_fresh_wal(&dir)?;
        Ok(Self {
            dir,
            model,
            wal,
            sync_on_write: true,
        })
    }

    /// Open an existing store in `dir`: load the snapshot, then replay the WAL
    /// tail. A trailing record torn by a crash is discarded and the WAL is
    /// truncated to its last intact write.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, PersistError> {
        let dir = dir.into();
        let mut model = Model::load_from_path(dir.join(SNAPSHOT_FILE))?;

        let wal_path = dir.join(WAL_FILE);
        // Replay an existing WAL only when it is at least header-sized. A missing
        // file, or one truncated below its header by a crash between creating it
        // and writing the header, is treated as fresh (the snapshot stands alone).
        let wal = if wal_path.exists() && fs::metadata(&wal_path)?.len() >= wal::WAL_HEADER_LEN {
            let bytes = fs::read(&wal_path)?;
            let replay = wal::replay(&bytes).map_err(|e| PersistError::Corrupt(e.to_string()))?;
            for record in &replay.records {
                match record {
                    Record::SetLeaf { coord, value } => model.cube.set_leaf(coord, *value)?,
                    Record::SetString { coord, value } => model.cube.set_string(coord, value)?,
                    // Batch markers are consumed by wal::replay and never surface here.
                    Record::BatchBegin { .. } | Record::BatchEnd => {}
                }
            }
            // Drop any torn tail, then position at the end for new appends.
            let mut file = OpenOptions::new().write(true).open(&wal_path)?;
            file.set_len(replay.good_len)?;
            file.seek(SeekFrom::End(0))?;
            file
        } else {
            open_fresh_wal(&dir)?
        };

        Ok(Self {
            dir,
            model,
            wal,
            sync_on_write: true,
        })
    }

    /// Open the store in `dir` if it exists, otherwise create it from `cube`.
    /// `cube` is only built (and only consumed) when creating.
    pub fn open_or_create(
        dir: impl Into<PathBuf>,
        cube: impl FnOnce() -> Cube,
    ) -> Result<Self, PersistError> {
        let dir = dir.into();
        if dir.join(SNAPSHOT_FILE).exists() {
            Self::open(dir)
        } else {
            Self::create(dir, cube())
        }
    }

    /// Whether each write is flushed to disk (`fsync`) before returning. On by
    /// default: every acknowledged write survives a crash. Turning it off trades
    /// durability for throughput (the WAL still frames every record).
    pub fn set_sync(&mut self, on: bool) {
        self.sync_on_write = on;
    }

    /// The cube, for reads.
    pub fn cube(&self) -> &Cube {
        &self.model.cube
    }

    /// The whole durable model (cube plus named subsets and views), for reads.
    pub fn model(&self) -> &Model {
        &self.model
    }

    /// Replace the in-memory model, reverting an orphaned partial mutation after a
    /// failed definition op. The engine calls this on the error path with the last
    /// published model, so a structural edit that mutated the cube and then failed
    /// to checkpoint cannot leak into the next successful commit. The on-disk
    /// snapshot and WAL are not touched here; the next successful checkpoint
    /// rewrites the snapshot from this restored model.
    pub fn restore_model(&mut self, model: Model) {
        self.model = model;
    }

    /// Write a leaf cell: apply it to the in-memory cube and append it to the
    /// WAL. The model validates the coordinate first, so a rejected write is
    /// never logged.
    pub fn set_leaf(&mut self, coord: &[u32], value: Fixed) -> Result<(), PersistError> {
        self.model.cube.set_leaf(coord, value)?;
        let framed = wal::encode(&Record::SetLeaf {
            coord: coord.to_vec(),
            value,
        });
        self.wal.write_all(&framed)?;
        if self.sync_on_write {
            self.wal.sync_data()?;
        }
        Ok(())
    }

    /// Write a string cell: apply it to the in-memory cube and append it to the
    /// WAL. Like [`set_leaf`](Self::set_leaf), the model validates first.
    pub fn set_string(&mut self, coord: &[u32], value: &str) -> Result<(), PersistError> {
        self.model.cube.set_string(coord, value)?;
        let framed = wal::encode(&Record::SetString {
            coord: coord.to_vec(),
            value: value.to_string(),
        });
        self.wal.write_all(&framed)?;
        if self.sync_on_write {
            self.wal.sync_data()?;
        }
        Ok(())
    }

    /// Apply a batch of writes atomically (all-or-nothing). Validates and applies
    /// every write to a throwaway clone first: any rejected write returns
    /// [`PersistError::BatchRejected`] with its index and leaves the live cube
    /// untouched. On success the framed batch (begin .. records .. end) is
    /// appended as one WAL unit with a single fsync, then the trial is adopted; a
    /// batch torn by a crash before its end marker is discarded whole on recovery.
    pub fn set_batch(&mut self, writes: &[CellWrite]) -> Result<(), PersistError> {
        // 1. Validate + apply to a throwaway clone; abort the whole batch on error.
        let mut trial = self.model.cube.clone();
        for (index, write) in writes.iter().enumerate() {
            let applied = match write {
                CellWrite::Leaf { coord, value } => trial.set_leaf(coord, *value),
                CellWrite::Str { coord, value } => trial.set_string(coord, value),
            };
            applied.map_err(|source| PersistError::BatchRejected { index, source })?;
        }
        // 2. Durably append the framed batch as one unit, a single fsync.
        let mut framed = wal::encode(&Record::BatchBegin {
            count: writes.len() as u32,
        });
        for write in writes {
            let record = match write {
                CellWrite::Leaf { coord, value } => Record::SetLeaf {
                    coord: coord.clone(),
                    value: *value,
                },
                CellWrite::Str { coord, value } => Record::SetString {
                    coord: coord.clone(),
                    value: value.clone(),
                },
            };
            framed.extend_from_slice(&wal::encode(&record));
        }
        framed.extend_from_slice(&wal::encode(&Record::BatchEnd));
        self.wal.write_all(&framed)?;
        if self.sync_on_write {
            self.wal.sync_data()?;
        }
        // 3. Adopt the validated trial; the WAL already reflects it durably.
        self.model.cube = trial;
        Ok(())
    }

    /// Full-persist: rewrite the snapshot from the current in-memory model and
    /// clear the WAL. After this, recovery needs only the snapshot. Because the
    /// snapshot is written from the in-memory cube (which already reflects every
    /// outstanding WAL write), a checkpoint also folds those writes in safely. The
    /// snapshot is made durable (fsync + atomic rename) before the WAL is cleared,
    /// so the WAL is never truncated while the new snapshot is not yet on disk.
    pub fn checkpoint(&mut self) -> Result<(), PersistError> {
        write_snapshot(&self.dir, &self.model)?;
        self.wal.set_len(0)?;
        self.wal.seek(SeekFrom::Start(0))?;
        self.wal.write_all(&wal::header())?;
        self.wal.sync_data()?;
        Ok(())
    }

    /// Define (create or replace) a subset, then checkpoint so the definition is
    /// durable. Structural validation runs first: an invalid subset returns
    /// [`PersistError::Query`] and leaves the model and snapshot untouched.
    pub fn define_subset(&mut self, subset: Subset) -> Result<(), PersistError> {
        validate_subset(&self.model.cube, &subset)?;
        self.model
            .subsets
            .insert((subset.dimension.clone(), subset.name.clone()), subset);
        self.checkpoint()
    }

    /// Delete a subset by dimension and name. Returns whether one was removed;
    /// checkpoints only when something changed.
    pub fn delete_subset(&mut self, dimension: &str, name: &str) -> Result<bool, PersistError> {
        let removed = self
            .model
            .subsets
            .remove(&(dimension.to_string(), name.to_string()))
            .is_some();
        if removed {
            self.checkpoint()?;
        }
        Ok(removed)
    }

    /// Define (create or replace) a view, then checkpoint. Structural validation
    /// (coverage, subset references, member/context resolution) runs first; an
    /// invalid view returns [`PersistError::Query`] and changes nothing.
    pub fn define_view(&mut self, view: View) -> Result<(), PersistError> {
        validate_view(&self.model, &view)?;
        self.model.views.insert(view.name.clone(), view);
        self.checkpoint()
    }

    /// Delete a view by name. Returns whether one was removed; checkpoints only
    /// when something changed.
    pub fn delete_view(&mut self, name: &str) -> Result<bool, PersistError> {
        let removed = self.model.views.remove(name).is_some();
        if removed {
            self.checkpoint()?;
        }
        Ok(removed)
    }

    /// Set the cube's rules source, then checkpoint. The source is stored
    /// verbatim; its validity is checked by the calc layer at the API boundary
    /// (the store and persist crate stay calc-free).
    pub fn define_rules(&mut self, source: String) -> Result<(), PersistError> {
        self.model.rules = RuleSet { source };
        self.checkpoint()
    }

    /// Clear the cube's rules. Returns whether there were any; checkpoints only
    /// when something changed.
    pub fn delete_rules(&mut self) -> Result<bool, PersistError> {
        if self.model.rules.is_empty() {
            return Ok(false);
        }
        self.model.rules = RuleSet::default();
        self.checkpoint()?;
        Ok(true)
    }

    /// Define (create or replace) a rule unit test, then checkpoint.
    pub fn define_rule_test(&mut self, test: RuleTest) -> Result<(), PersistError> {
        self.model.tests.insert(test.name.clone(), test);
        self.checkpoint()
    }

    /// Delete a rule test by name. Returns whether one was removed; checkpoints
    /// only when something changed.
    pub fn delete_rule_test(&mut self, name: &str) -> Result<bool, PersistError> {
        let removed = self.model.tests.remove(name).is_some();
        if removed {
            self.checkpoint()?;
        }
        Ok(removed)
    }

    // Flows, flow tests, connections, and jobs are no longer per-cube (ADR-0035);
    // they are persisted by the server-global `AutomationStore`, so the per-cube
    // `Store` no longer defines or deletes them.

    /// Define (create or replace) a sandbox, then checkpoint. A sandbox is a
    /// per-user what-if overlay (ADR-0014); it is persisted in the model snapshot
    /// and recovered on reopen, never in the base WAL. A create carries empty
    /// deltas; replacing an existing sandbox overwrites it.
    pub fn define_sandbox(&mut self, sandbox: Sandbox) -> Result<(), PersistError> {
        self.model.sandboxes.insert(sandbox.name.clone(), sandbox);
        self.checkpoint()
    }

    /// Delete a sandbox by name (discard). Returns whether one was removed;
    /// checkpoints only when something changed.
    pub fn delete_sandbox(&mut self, name: &str) -> Result<bool, PersistError> {
        let removed = self.model.sandboxes.remove(name).is_some();
        if removed {
            self.checkpoint()?;
        }
        Ok(removed)
    }

    /// Stage leaf overrides into a sandbox's delta and checkpoint. The base cube
    /// is never touched: each write is validated against a throwaway cube clone
    /// (so a non-leaf or out-of-range coordinate is rejected wholesale, exactly
    /// like [`set_batch`](Self::set_batch)), then the value is recorded in the
    /// sandbox's overlay. `updated` is the injected id stamped on the sandbox.
    pub fn sandbox_set_cells(
        &mut self,
        name: &str,
        writes: &[CellWrite],
        updated: u64,
    ) -> Result<(), PersistError> {
        if !self.model.sandboxes.contains_key(name) {
            return Err(PersistError::Query(QueryError::Calc {
                message: format!("no sandbox '{name}'"),
            }));
        }
        // String what-if is out of scope for this phase: the overlay is numeric
        // only (ADR-0014), so reject a string override loudly rather than stage a
        // value the read path cannot surface and would silently commit to base.
        if writes.iter().any(|w| matches!(w, CellWrite::Str { .. })) {
            return Err(PersistError::Query(QueryError::Calc {
                message: "string what-if values are not supported in a sandbox".to_string(),
            }));
        }
        // Validate every override against a throwaway clone (leaf-only, in-range);
        // this never mutates base cells, only confirms the coordinate is writable.
        let mut trial = self.model.cube.clone();
        for (index, write) in writes.iter().enumerate() {
            let applied = match write {
                CellWrite::Leaf { coord, value } => trial.set_leaf(coord, *value),
                CellWrite::Str { coord, value } => trial.set_string(coord, value),
            };
            applied.map_err(|source| PersistError::BatchRejected { index, source })?;
        }
        // Record the numeric overrides in the sandbox overlay (the value verbatim,
        // so an explicit zero override is kept rather than dropped). String writes
        // were rejected above, so `string_cells` stays empty this phase.
        let sb = self
            .model
            .sandboxes
            .get_mut(name)
            .expect("sandbox presence checked above");
        for write in writes {
            if let CellWrite::Leaf { coord, value } = write {
                sb.cells.insert(coord.clone(), *value);
            }
        }
        sb.updated = updated;
        self.checkpoint()
    }

    /// Commit a sandbox's overrides into the base cube, then clear the deltas and
    /// checkpoint. The overrides are applied through the same validated batch path
    /// as any other write ([`set_batch`](Self::set_batch)), so a rejected write
    /// aborts wholesale and leaves base and the sandbox untouched. On success the
    /// base cells are updated, the sandbox is emptied (it stays alive for reuse),
    /// and the single checkpoint folds the batch into the snapshot and clears the
    /// WAL. An unknown sandbox returns [`PersistError::Query`].
    pub fn commit_sandbox(&mut self, name: &str, updated: u64) -> Result<(), PersistError> {
        let writes: Vec<CellWrite> = {
            let sb = self.model.sandbox(name).ok_or_else(|| {
                PersistError::Query(QueryError::Calc {
                    message: format!("no sandbox '{name}'"),
                })
            })?;
            let mut w: Vec<CellWrite> = sb
                .cells
                .iter()
                .map(|(coord, value)| CellWrite::Leaf {
                    coord: coord.clone(),
                    value: *value,
                })
                .collect();
            w.extend(sb.string_cells.iter().map(|(coord, value)| CellWrite::Str {
                coord: coord.clone(),
                value: value.clone(),
            }));
            w
        };
        // Apply to base (validates on a clone; WALs on success). A rejected write
        // propagates and leaves base and the sandbox unchanged.
        self.set_batch(&writes)?;
        // Clear the now-merged deltas (the sandbox stays, empty) and checkpoint,
        // which folds the just-applied batch into the snapshot and clears the WAL.
        let sb = self
            .model
            .sandboxes
            .get_mut(name)
            .expect("sandbox presence checked above");
        sb.cells.clear();
        sb.string_cells.clear();
        sb.updated = updated;
        self.checkpoint()
    }

    /// Append dimension elements and consolidation edges (append-only,
    /// idempotent), then checkpoint. Returns the number of newly-created
    /// elements. This is the durable side of a flow's "build dimension elements"
    /// stage: structural validation runs first, and an invalid change leaves the
    /// model and snapshot untouched. Existing cells are preserved (the cube
    /// re-packs internally when a dimension's bit-width grows).
    pub fn extend_schema(
        &mut self,
        elements: &[ElementSpec],
        edges: &[EdgeSpec],
    ) -> Result<usize, PersistError> {
        // Cube::extend_schema is transactional (it stages on a clone and only
        // commits on full success), so a rejected change leaves the model
        // untouched and we only checkpoint when something actually changed.
        let added = self.model.cube.extend_schema(elements, edges)?;
        self.checkpoint()?;
        Ok(added)
    }

    /// Define an attribute on a dimension (ADR-0021), then checkpoint. Idempotent
    /// for the same kind; re-declaring with a different kind is a conflict and
    /// leaves the model and snapshot untouched.
    pub fn define_attribute(
        &mut self,
        dimension: &str,
        name: &str,
        kind: AttributeKind,
    ) -> Result<(), PersistError> {
        self.model.cube.define_attribute(dimension, name, kind)?;
        self.checkpoint()
    }

    /// Set an attribute's value for one or more elements (ADR-0021), then
    /// checkpoint. The core operation is transactional, so a rejected value
    /// (unknown element, kind mismatch, alias collision) leaves the model and
    /// snapshot untouched.
    pub fn set_attribute_values(
        &mut self,
        dimension: &str,
        attribute: &str,
        values: &[(String, AttributeValue)],
    ) -> Result<(), PersistError> {
        self.model
            .cube
            .set_attribute_values(dimension, attribute, values)?;
        self.checkpoint()
    }

    // ---- structural dimension editing (ADR-0036) ----
    //
    // Each remaps the cube's stored cells transactionally (the core op stages on a
    // clone), so a rejected edit leaves the model untouched. A successful edit
    // changes element order/membership, so the WAL (which names elements by index)
    // would be stale against the new order; we checkpoint immediately, rewriting
    // the snapshot from the remapped in-memory cube and clearing the WAL, so the
    // edit is durable and recovery never replays a pre-edit coordinate.
    //
    // The three reindexing ops (reorder, delete, insert) shift existing indices, so
    // they ALSO checkpoint BEFORE the edit: that folds any outstanding cell writes
    // into the snapshot and empties the WAL, so when the post-edit snapshot is
    // written the WAL holds no old-index records. Recovery therefore cannot replay
    // a stale coordinate onto the new layout even if a crash lands between writing
    // the new snapshot and clearing the WAL. (reparent / add-child / remove-child /
    // set-kind keep indices stable, so their single post-edit checkpoint suffices.)

    /// Reorder a dimension's members, remapping every stored cell, then checkpoint.
    pub fn reorder_elements(
        &mut self,
        dimension: &str,
        new_order: &[String],
    ) -> Result<(), PersistError> {
        // Reindexing op: checkpoint first so the WAL holds no old-index writes
        // when the post-edit snapshot is written (see the block comment above).
        self.checkpoint()?;
        self.model.reorder_elements(dimension, new_order)?;
        self.checkpoint()
    }

    /// Reparent a member (or detach to a root), then checkpoint.
    pub fn reparent_element(
        &mut self,
        dimension: &str,
        child: &str,
        new_parent: Option<&str>,
        weight: i64,
    ) -> Result<(), PersistError> {
        self.model
            .cube
            .reparent_element(dimension, child, new_parent, weight)?;
        self.checkpoint()
    }

    /// Add a member to a consolidation additively (keeping its other parents),
    /// then checkpoint.
    pub fn add_child_element(
        &mut self,
        dimension: &str,
        parent: &str,
        child: &str,
        weight: i64,
    ) -> Result<(), PersistError> {
        self.model
            .cube
            .add_child_element(dimension, parent, child, weight)?;
        self.checkpoint()
    }

    /// Remove the single `parent -> child` consolidation edge (keeping the member,
    /// its cells, and its other parents), then checkpoint.
    pub fn remove_child_element(
        &mut self,
        dimension: &str,
        parent: &str,
        child: &str,
    ) -> Result<(), PersistError> {
        self.model
            .cube
            .remove_child_element(dimension, parent, child)?;
        self.checkpoint()
    }

    /// Convert a member's kind (re-typing or clearing its cells), then checkpoint.
    pub fn set_element_kind(
        &mut self,
        dimension: &str,
        element: &str,
        kind: ElementKind,
    ) -> Result<(), PersistError> {
        self.model.cube.set_element_kind(dimension, element, kind)?;
        self.checkpoint()
    }

    /// Delete a member, its edges, and its cells, reindexing the rest, then
    /// checkpoint.
    pub fn delete_element(&mut self, dimension: &str, element: &str) -> Result<(), PersistError> {
        // Reindexing op: checkpoint first (see reorder_elements).
        self.checkpoint()?;
        self.model.delete_element(dimension, element)?;
        self.checkpoint()
    }

    /// Insert a member at a position, remapping cells, then checkpoint.
    pub fn insert_element_at(
        &mut self,
        dimension: &str,
        name: &str,
        kind: ElementKind,
        position: Position,
    ) -> Result<(), PersistError> {
        // Reindexing op: checkpoint first (see reorder_elements).
        self.checkpoint()?;
        self.model
            .insert_element_at(dimension, name, kind, position)?;
        self.checkpoint()
    }
}

/// One structural edit to a dimension (ADR-0036), addressed by member name. The
/// engine builds these and dispatches them through [`Store::edit_dimension`] so
/// the registry copy and every referencing cube apply the identical edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DimensionEdit {
    /// Reorder the members to this exact permutation of the current member names.
    Reorder { new_order: Vec<String> },
    /// Reparent `child` under `new_parent` (or detach to a root when `None`).
    Reparent {
        child: String,
        new_parent: Option<String>,
        weight: i64,
    },
    /// Add `child` to the consolidation `parent` additively, keeping the child's
    /// existing parents (a member may roll up to multiple consolidations). A
    /// numeric/string `parent` is converted to a consolidation first. Idempotent
    /// when the edge already exists. Unlike `Reparent`, it never detaches the
    /// child from any other consolidation.
    AddChild {
        parent: String,
        child: String,
        weight: i64,
    },
    /// Remove the single `parent -> child` consolidation edge, keeping the child
    /// member, its data, and its other parent edges (a member may roll up to
    /// multiple consolidations). Idempotent when the edge is absent. Distinct from
    /// `Reparent` with `None` (which detaches the child from EVERY parent) and from
    /// `Delete` (which removes the member). Drops no stored value.
    RemoveChild { parent: String, child: String },
    /// Convert `element` to `kind`.
    SetKind { element: String, kind: ElementKind },
    /// Delete `element` (rejected if it still has children).
    Delete { element: String },
    /// Insert a new member `name` of `kind` at `position`.
    Insert {
        name: String,
        kind: ElementKind,
        position: Position,
    },
}

impl Store {
    /// Dispatch one structural dimension edit (ADR-0036) to the named dimension,
    /// remapping cells and checkpointing. Each branch is itself transactional, so
    /// a rejected edit leaves the model and snapshot untouched.
    pub fn edit_dimension(
        &mut self,
        dimension: &str,
        edit: &DimensionEdit,
    ) -> Result<(), PersistError> {
        match edit {
            DimensionEdit::Reorder { new_order } => self.reorder_elements(dimension, new_order),
            DimensionEdit::Reparent {
                child,
                new_parent,
                weight,
            } => self.reparent_element(dimension, child, new_parent.as_deref(), *weight),
            DimensionEdit::AddChild {
                parent,
                child,
                weight,
            } => self.add_child_element(dimension, parent, child, *weight),
            DimensionEdit::RemoveChild { parent, child } => {
                self.remove_child_element(dimension, parent, child)
            }
            DimensionEdit::SetKind { element, kind } => {
                self.set_element_kind(dimension, element, *kind)
            }
            DimensionEdit::Delete { element } => self.delete_element(dimension, element),
            DimensionEdit::Insert {
                name,
                kind,
                position,
            } => self.insert_element_at(dimension, name, *kind, position.clone()),
        }
    }
}

/// Write the snapshot durably: serialize to a temp file and fsync its contents,
/// rename over the live snapshot (rename replaces the destination on all supported
/// platforms), then fsync the directory so the rename itself is durable. Flushing
/// the temp file before the rename is what lets [`Store::checkpoint`] clear the WAL
/// safely: the new snapshot's bytes are on disk before the WAL is truncated.
fn write_snapshot(dir: &Path, model: &Model) -> Result<(), PersistError> {
    let tmp = dir.join(SNAPSHOT_TMP);
    let text = model.to_model_text()?;
    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&tmp, dir.join(SNAPSHOT_FILE))?;
    sync_dir(dir)?;
    Ok(())
}

/// fsync a directory so a contained rename or create is durable. Unix supports
/// opening a directory as a file and fsync-ing it; Windows does not (a directory
/// handle cannot be flushed, and NTFS records the rename in its own metadata
/// journal), so this is a no-op there.
fn sync_dir(dir: &Path) -> Result<(), PersistError> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

/// Create or truncate the WAL and write its header, leaving the cursor at the end
/// (ready to append).
fn open_fresh_wal(dir: &Path) -> Result<File, PersistError> {
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(dir.join(WAL_FILE))?;
    file.write_all(&wal::header())?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use epiphany_core::{AxisSpec, Dimension, RuleTest, SubsetKind, Visibility};

    /// A 2-D cube: Region (3 leaves under Total) x Period (2 leaves under Total).
    /// Returns the cube and the leaf/consolidated indices needed by tests.
    struct Fixture {
        cube: Cube,
        r: Vec<u32>,
        region_total: u32,
        p: Vec<u32>,
        period_total: u32,
    }

    fn fixture() -> Fixture {
        let mut region = Dimension::new("Region");
        let r: Vec<u32> = (0..3).map(|i| region.add_leaf(format!("R{i}"))).collect();
        let region_total = region.add_consolidated("Total");
        for &leaf in &r {
            region.add_child(region_total, leaf, 1).unwrap();
        }
        let mut period = Dimension::new("Period");
        let p: Vec<u32> = (0..2).map(|i| period.add_leaf(format!("P{i}"))).collect();
        let period_total = period.add_consolidated("Total");
        for &leaf in &p {
            period.add_child(period_total, leaf, 1).unwrap();
        }
        let cube = Cube::new("Sales", vec![region, period]).unwrap();
        Fixture {
            cube,
            r,
            region_total,
            p,
            period_total,
        }
    }

    /// A unique scratch directory for one test (cleaned up at the end).
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("epiphany-persist-{}-{name}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn create_then_open_round_trips_writes() {
        let dir = scratch("round-trip");
        let f = fixture();
        let (r, p, region_total, period_total) = (f.r, f.p, f.region_total, f.period_total);
        {
            let mut store = Store::create(&dir, f.cube).unwrap();
            store.set_leaf(&[r[0], p[0]], Fixed::from(10)).unwrap();
            store.set_leaf(&[r[1], p[0]], Fixed::from(20)).unwrap();
            store.set_leaf(&[r[0], p[1]], Fixed::from(30)).unwrap();
        }
        let store = Store::open(&dir).unwrap();
        assert_eq!(
            store.cube().get_leaf(&[r[0], p[0]]).unwrap(),
            Fixed::from(10)
        );
        assert_eq!(
            store.cube().get(&[region_total, period_total]).unwrap(),
            Fixed::from(60)
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn recovers_without_an_explicit_checkpoint() {
        // No checkpoint after the writes: recovery must come entirely from the
        // WAL replayed onto the empty initial snapshot (the crash case).
        let dir = scratch("crash-no-checkpoint");
        let f = fixture();
        let (r, p) = (f.r.clone(), f.p.clone());
        {
            let mut store = Store::create(&dir, f.cube).unwrap();
            for (i, &leaf) in r.iter().enumerate() {
                store
                    .set_leaf(&[leaf, p[0]], Fixed::from((i as i32 + 1) * 100))
                    .unwrap();
            }
            // Drop without checkpoint: simulates a crash with a populated WAL.
        }
        let store = Store::open(&dir).unwrap();
        assert_eq!(store.cube().cell_count(), r.len());
        assert_eq!(
            store.cube().get_leaf(&[r[2], p[0]]).unwrap(),
            Fixed::from(300)
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn checkpoint_clears_wal_and_preserves_state() {
        let dir = scratch("checkpoint");
        let f = fixture();
        let (r, p, region_total, period_total) = (f.r, f.p, f.region_total, f.period_total);
        {
            let mut store = Store::create(&dir, f.cube).unwrap();
            store.set_leaf(&[r[0], p[0]], Fixed::from(10)).unwrap();
            store.set_leaf(&[r[1], p[1]], Fixed::from(40)).unwrap();
            store.checkpoint().unwrap();
            // After a checkpoint the WAL holds only its header.
            let wal_len = fs::metadata(dir.join(WAL_FILE)).unwrap().len();
            assert_eq!(wal_len, wal::WAL_HEADER_LEN);
            // A further write lands in the (now-empty) WAL.
            store.set_leaf(&[r[2], p[0]], Fixed::from(5)).unwrap();
        }
        let store = Store::open(&dir).unwrap();
        // Pre-checkpoint writes come from the snapshot; the last from the WAL.
        assert_eq!(
            store.cube().get(&[region_total, period_total]).unwrap(),
            Fixed::from(55)
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reindexing_edit_folds_outstanding_writes_and_clears_wal() {
        // A reindexing edit (reorder) shifts element indices. Any outstanding,
        // un-checkpointed cell writes name OLD indices, so the store checkpoints
        // BEFORE the edit: the writes are folded into the snapshot and the WAL is
        // emptied, so the post-edit snapshot is backed by a WAL with no stale
        // records (recovery can never replay an old coordinate onto the new order).
        let dir = scratch("reindex-folds");
        let f = fixture();
        let (r, p, region_total, period_total) =
            (f.r.clone(), f.p.clone(), f.region_total, f.period_total);
        {
            let mut store = Store::create(&dir, f.cube).unwrap();
            // Writes that are NOT checkpointed: they live only in the WAL.
            store.set_leaf(&[r[0], p[0]], Fixed::from(10)).unwrap();
            store.set_leaf(&[r[1], p[0]], Fixed::from(20)).unwrap();
            assert!(
                fs::metadata(dir.join(WAL_FILE)).unwrap().len() > wal::WAL_HEADER_LEN,
                "outstanding writes are in the WAL before the edit"
            );
            // Reorder Region (swap R0 and R1); Total stays last.
            store
                .reorder_elements(
                    "Region",
                    &["R1".into(), "R0".into(), "R2".into(), "Total".into()],
                )
                .unwrap();
            // The WAL is back to its header: no old-index records linger.
            assert_eq!(
                fs::metadata(dir.join(WAL_FILE)).unwrap().len(),
                wal::WAL_HEADER_LEN,
                "the reindexing edit emptied the WAL"
            );
        }
        // Reopen with no extra checkpoint: the folded writes survived the reindex.
        let store = Store::open(&dir).unwrap();
        assert_eq!(
            store.cube().get(&[region_total, period_total]).unwrap(),
            Fixed::from(30)
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discards_a_torn_trailing_record() {
        let dir = scratch("torn-tail");
        let f = fixture();
        let (r, p) = (f.r, f.p);
        {
            let mut store = Store::create(&dir, f.cube).unwrap();
            store.set_leaf(&[r[0], p[0]], Fixed::from(10)).unwrap();
            store.set_leaf(&[r[1], p[0]], Fixed::from(20)).unwrap();
        }
        // Simulate a crash mid-write: append a half-written record to the WAL.
        let wal_path = dir.join(WAL_FILE);
        let mut bytes = fs::read(&wal_path).unwrap();
        let intact = bytes.len();
        bytes.extend_from_slice(&9u32.to_le_bytes()); // claims a 9-byte payload
        bytes.extend_from_slice(&[1, 2, 0]); // but only 3 bytes follow
        fs::write(&wal_path, &bytes).unwrap();

        let store = Store::open(&dir).unwrap();
        assert_eq!(store.cube().cell_count(), 2);
        assert_eq!(
            store.cube().get_leaf(&[r[1], p[0]]).unwrap(),
            Fixed::from(20)
        );
        // Recovery truncated the torn tail back to the last intact write.
        assert_eq!(fs::metadata(&wal_path).unwrap().len() as usize, intact);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn recovers_string_writes_via_wal() {
        let dir = scratch("string-wal");
        let mut measure = Dimension::new("Measure");
        let sales = measure.add_leaf("Sales");
        let note = measure.add_string("Note");
        let cube = Cube::new("M", vec![measure]).unwrap();
        {
            let mut store = Store::create(&dir, cube).unwrap();
            store.set_leaf(&[sales], Fixed::from(5)).unwrap();
            store.set_string(&[note], "checked").unwrap();
            // Drop without checkpoint: both writes must replay from the WAL.
        }
        let store = Store::open(&dir).unwrap();
        assert_eq!(store.cube().get_leaf(&[sales]).unwrap(), Fixed::from(5));
        assert_eq!(store.cube().get_string(&[note]).unwrap(), Some("checked"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn batch_is_atomic_and_recovers() {
        let dir = scratch("batch");
        let f = fixture();
        let (r, p, region_total) = (f.r, f.p, f.region_total);
        {
            let mut store = Store::create(&dir, f.cube).unwrap();
            // A valid batch applies fully.
            store
                .set_batch(&[
                    CellWrite::Leaf {
                        coord: vec![r[0], p[0]],
                        value: Fixed::from(10),
                    },
                    CellWrite::Leaf {
                        coord: vec![r[1], p[0]],
                        value: Fixed::from(20),
                    },
                ])
                .unwrap();
            // A batch whose second write targets a consolidated element is
            // rejected wholesale, leaving the prior state untouched.
            let err = store
                .set_batch(&[
                    CellWrite::Leaf {
                        coord: vec![r[2], p[0]],
                        value: Fixed::from(99),
                    },
                    CellWrite::Leaf {
                        coord: vec![region_total, p[0]],
                        value: Fixed::from(1),
                    },
                ])
                .unwrap_err();
            assert!(matches!(err, PersistError::BatchRejected { index: 1, .. }));
            assert_eq!(
                store.cube().get_leaf(&[r[2], p[0]]).unwrap(),
                Fixed::ZERO,
                "a rejected batch leaves no partial writes"
            );
            assert_eq!(store.cube().cell_count(), 2);
            // Drop without checkpoint: WAL replay must recover only the committed batch.
        }
        let store = Store::open(&dir).unwrap();
        assert_eq!(store.cube().cell_count(), 2);
        assert_eq!(
            store.cube().get(&[region_total, p[0]]).unwrap(),
            Fixed::from(30)
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn open_or_create_builds_then_reopens() {
        let dir = scratch("open-or-create");
        let f = fixture();
        let (r, p) = (f.r.clone(), f.p.clone());
        let cube = f.cube;
        let built = std::cell::Cell::new(false);
        {
            let mut store = Store::open_or_create(&dir, || {
                built.set(true);
                cube
            })
            .unwrap();
            assert!(built.get(), "first call must build the cube");
            store.set_leaf(&[r[0], p[0]], Fixed::from(7)).unwrap();
        }
        // Second call finds the snapshot and must not build.
        let store = Store::open_or_create(&dir, || panic!("must not rebuild")).unwrap();
        assert_eq!(
            store.cube().get_leaf(&[r[0], p[0]]).unwrap(),
            Fixed::from(7)
        );
        fs::remove_dir_all(&dir).ok();
    }

    fn static_subset(name: &str, members: &[&str]) -> Subset {
        Subset {
            name: name.into(),
            dimension: "Region".into(),
            owner: None,
            visibility: Visibility::Public,
            kind: SubsetKind::Static {
                members: members.iter().map(|s| s.to_string()).collect(),
            },
        }
    }

    #[test]
    fn restore_model_reverts_in_memory_changes() {
        // restore_model swaps the in-memory model back to a prior snapshot: the
        // primitive the engine uses to roll back a definition op that mutated the
        // model and then failed to persist.
        let dir = scratch("restore-model");
        let f = fixture();
        let mut store = Store::create(&dir, f.cube).unwrap();
        let good = store.model().clone();
        store.define_subset(static_subset("Temp", &["R0"])).unwrap();
        assert!(store.model().subset("Region", "Temp").is_some());
        store.restore_model(good);
        assert!(
            store.model().subset("Region", "Temp").is_none(),
            "restore reverted the in-memory definition"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn definitions_persist_through_reopen() {
        let dir = scratch("definitions");
        let f = fixture();
        let (r, p) = (f.r.clone(), f.p.clone());
        {
            let mut store = Store::create(&dir, f.cube).unwrap();
            store.set_leaf(&[r[0], p[0]], Fixed::from(10)).unwrap();
            store
                .define_subset(static_subset("Core", &["R0", "R1"]))
                .unwrap();
            store
                .define_view(View {
                    name: "Grid".into(),
                    cube: "Sales".into(),
                    owner: None,
                    visibility: Visibility::Public,
                    rows: vec![AxisSpec::Subset {
                        dimension: "Region".into(),
                        subset: "Core".into(),
                    }],
                    columns: vec![AxisSpec::Members {
                        dimension: "Period".into(),
                        members: vec!["P0".into()],
                    }],
                    context: Vec::new(),
                    suppress_zero_rows: true,
                    suppress_zero_columns: false,
                })
                .unwrap();
            // Drop WITHOUT a further checkpoint: define already checkpointed.
        }
        let store = Store::open(&dir).unwrap();
        assert!(store.model().subset("Region", "Core").is_some());
        let grid = store
            .model()
            .view("Grid")
            .expect("the view survives the snapshot");
        // The split zero-suppression flags round-trip independently through the
        // snapshot save/load (each persisted on its own, no legacy collapsing).
        assert!(grid.suppress_zero_rows);
        assert!(!grid.suppress_zero_columns);
        // The earlier cell write survived too (the define's checkpoint folded it
        // into the snapshot).
        assert_eq!(
            store.cube().get_leaf(&[r[0], p[0]]).unwrap(),
            Fixed::from(10)
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn define_checkpoints_outstanding_cell_writes() {
        let dir = scratch("interleave");
        let f = fixture();
        let (r, p, region_total, period_total) = (f.r, f.p, f.region_total, f.period_total);
        {
            let mut store = Store::create(&dir, f.cube).unwrap();
            // Cells via a batch, NOT checkpointed.
            store
                .set_batch(&[
                    CellWrite::Leaf {
                        coord: vec![r[0], p[0]],
                        value: Fixed::from(10),
                    },
                    CellWrite::Leaf {
                        coord: vec![r[1], p[0]],
                        value: Fixed::from(20),
                    },
                ])
                .unwrap();
            // Defining a subset triggers a checkpoint that folds the batch in and
            // clears the WAL back to its header.
            store.define_subset(static_subset("S", &["R0"])).unwrap();
            assert_eq!(
                fs::metadata(dir.join(WAL_FILE)).unwrap().len(),
                wal::WAL_HEADER_LEN
            );
        }
        let store = Store::open(&dir).unwrap();
        assert_eq!(
            store.cube().get(&[region_total, period_total]).unwrap(),
            Fixed::from(30)
        );
        assert!(store.model().subset("Region", "S").is_some());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_removes_a_definition() {
        let dir = scratch("delete-def");
        let f = fixture();
        {
            let mut store = Store::create(&dir, f.cube).unwrap();
            store.define_subset(static_subset("S", &["R0"])).unwrap();
            assert!(store.delete_subset("Region", "S").unwrap());
            assert!(!store.delete_subset("Region", "S").unwrap(), "already gone");
        }
        let store = Store::open(&dir).unwrap();
        assert!(store.model().subset("Region", "S").is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rules_and_tests_persist_through_reopen() {
        let dir = scratch("rules-persist");
        let f = fixture();
        {
            let mut store = Store::create(&dir, f.cube).unwrap();
            store
                .define_rules("['Region':'R0'] = 1;".to_string())
                .unwrap();
            store
                .define_rule_test(RuleTest {
                    name: "t".to_string(),
                    fixtures: Vec::new(),
                    assertions: Vec::new(),
                })
                .unwrap();
        }
        let store = Store::open(&dir).unwrap();
        assert!(!store.model().rules.is_empty());
        assert!(store.model().tests.contains_key("t"));
        // Deleting clears them.
        let mut store = store;
        assert!(store.delete_rules().unwrap());
        assert!(store.delete_rule_test("t").unwrap());
        let store = Store::open(&dir).unwrap();
        assert!(store.model().rules.is_empty());
        assert!(store.model().tests.is_empty());
    }

    #[test]
    fn invalid_definition_is_rejected_and_changes_nothing() {
        let dir = scratch("invalid-def");
        let f = fixture();
        let mut store = Store::create(&dir, f.cube).unwrap();
        let err = store
            .define_subset(static_subset("Bad", &["Atlantis"]))
            .unwrap_err();
        assert!(matches!(err, PersistError::Query(_)));
        assert!(store.model().subset("Region", "Bad").is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sandboxes_persist_and_do_not_touch_base() {
        let dir = scratch("sandbox-persist");
        let f = fixture();
        let (r, p, region_total, period_total) =
            (f.r.clone(), f.p.clone(), f.region_total, f.period_total);
        {
            let mut store = Store::create(&dir, f.cube).unwrap();
            // Base data: R0/P0 = 10, R1/P0 = 20 (Total = 30).
            store
                .set_batch(&[
                    CellWrite::Leaf {
                        coord: vec![r[0], p[0]],
                        value: Fixed::from(10),
                    },
                    CellWrite::Leaf {
                        coord: vec![r[1], p[0]],
                        value: Fixed::from(20),
                    },
                ])
                .unwrap();
            // A sandbox overriding R0/P0 -> 500 (what-if), never base.
            store
                .define_sandbox(Sandbox::new("whatif", "ann", 1))
                .unwrap();
            store
                .sandbox_set_cells(
                    "whatif",
                    &[CellWrite::Leaf {
                        coord: vec![r[0], p[0]],
                        value: Fixed::from(500),
                    }],
                    2,
                )
                .unwrap();
            // Base cube is unchanged by the sandbox override.
            assert_eq!(
                store.cube().get_leaf(&[r[0], p[0]]).unwrap(),
                Fixed::from(10)
            );
            // Drop without an extra checkpoint: define/sandbox already checkpointed.
        }
        let store = Store::open(&dir).unwrap();
        // Base survived and is still the un-overlaid value.
        assert_eq!(
            store.cube().get(&[region_total, period_total]).unwrap(),
            Fixed::from(30)
        );
        // The sandbox and its delta recovered intact.
        let sb = store.model().sandbox("whatif").unwrap();
        assert_eq!(sb.owner, "ann");
        assert_eq!(sb.created, 1);
        assert_eq!(sb.updated, 2);
        assert_eq!(sb.cell(&[r[0], p[0]]), Some(Fixed::from(500)));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sandbox_overrides_follow_structural_edits() {
        // Sandbox overrides are keyed by element index. A reindexing edit must
        // remap them so a what-if value follows its member (and is dropped when
        // the member is deleted), and must not leave a stale index that would make
        // the checkpoint serialize the wrong member or panic.
        let dir = scratch("sandbox-remap");
        let f = fixture();
        let (r, p) = (f.r.clone(), f.p.clone());
        let mut store = Store::create(&dir, f.cube).unwrap();
        store.define_sandbox(Sandbox::new("w", "ann", 1)).unwrap();
        // What-if override on R1/P0 -> 500.
        store
            .sandbox_set_cells(
                "w",
                &[CellWrite::Leaf {
                    coord: vec![r[1], p[0]],
                    value: Fixed::from(500),
                }],
                2,
            )
            .unwrap();

        // Reorder Region so R1 moves to the front; the override must follow it.
        store
            .reorder_elements(
                "Region",
                &["R1".into(), "R0".into(), "R2".into(), "Total".into()],
            )
            .unwrap();
        let region = store
            .cube()
            .dimensions()
            .iter()
            .find(|d| d.name() == "Region")
            .unwrap();
        let r1_new = region.index_of("R1").unwrap();
        let sb = store.model().sandbox("w").unwrap();
        assert_eq!(
            sb.cell(&[r1_new, p[0]]),
            Some(Fixed::from(500)),
            "the override followed R1 to its new index"
        );
        assert_eq!(sb.len(), 1, "still exactly one override");

        // Deleting R1 drops its override (and the checkpoint must not panic).
        store.delete_element("Region", "R1").unwrap();
        assert!(
            store.model().sandbox("w").unwrap().is_empty(),
            "deleting the member dropped its override"
        );

        // The remapped sandbox survives a reopen.
        drop(store);
        let store = Store::open(&dir).unwrap();
        assert!(store.model().sandbox("w").unwrap().is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_sandbox_removes_it() {
        let dir = scratch("sandbox-delete");
        let f = fixture();
        {
            let mut store = Store::create(&dir, f.cube).unwrap();
            store.define_sandbox(Sandbox::new("s", "ann", 1)).unwrap();
            assert!(store.delete_sandbox("s").unwrap());
            assert!(!store.delete_sandbox("s").unwrap(), "already gone");
        }
        let store = Store::open(&dir).unwrap();
        assert!(store.model().sandbox("s").is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sandbox_override_rejects_non_leaf_and_unknown_sandbox() {
        let dir = scratch("sandbox-reject");
        let f = fixture();
        let (p, region_total) = (f.p.clone(), f.region_total);
        let mut store = Store::create(&dir, f.cube).unwrap();
        // Writing to a sandbox that does not exist is rejected.
        let err = store
            .sandbox_set_cells(
                "ghost",
                &[CellWrite::Leaf {
                    coord: vec![0, p[0]],
                    value: Fixed::from(1),
                }],
                1,
            )
            .unwrap_err();
        assert!(matches!(err, PersistError::Query(_)));

        // An override targeting a consolidated element is rejected wholesale.
        store.define_sandbox(Sandbox::new("s", "ann", 1)).unwrap();
        let err = store
            .sandbox_set_cells(
                "s",
                &[CellWrite::Leaf {
                    coord: vec![region_total, p[0]],
                    value: Fixed::from(99),
                }],
                2,
            )
            .unwrap_err();
        assert!(matches!(err, PersistError::BatchRejected { index: 0, .. }));
        // The sandbox is left empty (no partial override).
        assert!(store.model().sandbox("s").unwrap().is_empty());
        fs::remove_dir_all(&dir).ok();
    }
}
