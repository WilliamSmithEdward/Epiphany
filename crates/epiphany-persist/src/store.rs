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
    validate_subset, validate_view, AttributeKind, AttributeValue, BatchWrite, Cube, EdgeSpec,
    ElementKind, ElementSpec, Fixed, LoadError, Model, ModelError, Position, QueryError, RuleSet,
    RuleTest, Sandbox, SaveError, Subset, View,
};

use crate::wal::{self, Record};

const SNAPSHOT_FILE: &str = "snapshot.model";
const SNAPSHOT_TMP: &str = "snapshot.model.tmp";
const WAL_FILE: &str = "wal.log";

/// Default WAL byte budget past which the store auto-checkpoints (P1): once the
/// log grows beyond this many bytes it folds itself into a fresh snapshot and
/// truncates, bounding recovery-replay work and on-disk WAL size instead of
/// letting the log grow unbounded until an explicit [`Store::checkpoint`].
///
/// The trigger is purely *size*-based (deterministic, ADR-0009: no wall clock),
/// so a given sequence of writes always checkpoints at the same points. 8 MiB is
/// a balance: large enough that steady cell writes rarely pay a snapshot rewrite,
/// small enough to keep the replay tail and WAL footprint bounded. Configurable
/// per store via [`Store::set_wal_checkpoint_threshold`].
pub const DEFAULT_WAL_CHECKPOINT_THRESHOLD: u64 = 8 * 1024 * 1024;

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
    /// A prior WAL append or fsync failed and left the log in an unknown state;
    /// the store is poisoned and refuses further writes until it is reopened
    /// (which truncates the WAL back to its last intact record on recovery).
    Poisoned,
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
            PersistError::Poisoned => write!(
                f,
                "store poisoned by a failed WAL append/fsync; reopen to recover"
            ),
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
    /// The byte offset of the end of the last known-good (fully written and, when
    /// `sync_on_write`, fsynced) WAL record. Every append restores the WAL to this
    /// offset if it fails partway, so the log can never hold a torn frame followed
    /// by live records (recovery's stop-at-first-torn-frame scan stays sound).
    good_len: u64,
    /// Set once a WAL append/fsync fails and the log's tail cannot be trusted. A
    /// poisoned store rejects all further writes with [`PersistError::Poisoned`];
    /// the engine must republish the last durable model and reopen the store,
    /// whose recovery truncates the WAL back to its last intact record.
    poisoned: bool,
    /// The WAL byte budget past which a successful write auto-checkpoints (P1),
    /// folding the log into a fresh snapshot and truncating it. Defaults to
    /// [`DEFAULT_WAL_CHECKPOINT_THRESHOLD`]; `0` disables auto-checkpoint.
    wal_checkpoint_threshold: u64,
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
            good_len: wal::WAL_HEADER_LEN,
            poisoned: false,
            wal_checkpoint_threshold: DEFAULT_WAL_CHECKPOINT_THRESHOLD,
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
        let mut good_len = wal::WAL_HEADER_LEN;
        let wal = if wal_path.exists() && fs::metadata(&wal_path)?.len() >= wal::WAL_HEADER_LEN {
            let bytes = fs::read(&wal_path)?;
            let replay = wal::replay(&bytes).map_err(|e| PersistError::Corrupt(e.to_string()))?;
            // A bad frame with well-formed frames after it is not a torn tail: this
            // crate's append path can never produce that (a failed append truncates
            // back and poisons the store), so it is mid-log corruption. Silently
            // truncating would physically erase acknowledged records after the bad
            // frame; instead preserve the WAL as `wal.log.corrupt` and refuse to
            // open, so an operator can inspect it rather than lose data unseen.
            if replay.corrupt_with_valid_tail {
                let quarantine = dir.join("wal.log.corrupt");
                fs::rename(&wal_path, &quarantine)?;
                return Err(PersistError::Corrupt(format!(
                    "WAL has intact records after a corrupt frame (mid-log corruption, \
                     not a torn tail); preserved as {} for inspection",
                    quarantine.display()
                )));
            }
            good_len = replay.good_len;
            for record in &replay.records {
                match record {
                    Record::SetLeaf { coord, value } => model.cube.set_leaf(coord, *value)?,
                    Record::SetString { coord, value } => model.cube.set_string(coord, value)?,
                    // A name-addressed top-level pin/unpin (ADR-0038); replays onto
                    // the snapshot regardless of element order (index-stable).
                    Record::SetPin {
                        dimension,
                        element,
                        pinned,
                    } => {
                        if *pinned {
                            model.cube.pin_element_to_top(dimension, element)?;
                        } else {
                            model.cube.unpin_element_from_top(dimension, element)?;
                        }
                    }
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
            good_len,
            poisoned: false,
            wal_checkpoint_threshold: DEFAULT_WAL_CHECKPOINT_THRESHOLD,
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

    /// Set the WAL byte budget past which a successful write auto-checkpoints
    /// (P1). `0` disables auto-checkpoint (the log grows until an explicit
    /// [`checkpoint`](Self::checkpoint)). Defaults to
    /// [`DEFAULT_WAL_CHECKPOINT_THRESHOLD`].
    pub fn set_wal_checkpoint_threshold(&mut self, bytes: u64) {
        self.wal_checkpoint_threshold = bytes;
    }

    /// The current known-good WAL length in bytes (the durable framing, header
    /// included). Exposed for tests and metrics.
    pub fn wal_len(&self) -> u64 {
        self.good_len
    }

    /// Fold the WAL into a fresh snapshot and truncate it if it has grown past the
    /// configured byte budget (P1). Called after a successful write, once the
    /// in-memory model already reflects the just-logged change, so the rewritten
    /// snapshot is consistent. Size-triggered only (deterministic, ADR-0009); a
    /// disabled threshold (`0`) or a poisoned store is a no-op. A checkpoint
    /// failure surfaces to the caller exactly like an explicit checkpoint would,
    /// leaving the durable WAL (with the acknowledged write) intact for recovery.
    fn maybe_auto_checkpoint(&mut self) -> Result<(), PersistError> {
        if self.wal_checkpoint_threshold != 0
            && !self.poisoned
            && self.good_len > self.wal_checkpoint_threshold
        {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// The cube, for reads.
    pub fn cube(&self) -> &Cube {
        &self.model.cube
    }

    /// The cube's true display name, read from the loaded model (ADR-0037). The
    /// engine keys a cube by this, NOT by its on-disk folder name, so a cube
    /// loads with its real name regardless of the folder's casing or slugging.
    pub fn cube_name(&self) -> &str {
        self.model.cube.name()
    }

    /// The whole durable model (cube plus named subsets and views), for reads.
    pub fn model(&self) -> &Model {
        &self.model
    }

    /// Replace the in-memory model ONLY, reverting an orphaned partial mutation
    /// after a failed definition op. The engine calls this on the error path with
    /// the last published model, so a structural edit that mutated the cube and
    /// then failed to checkpoint cannot leak into the next successful commit.
    ///
    /// # Durability hazard (E5): this is memory-only and does NOT undo disk
    ///
    /// This touches neither the snapshot nor the WAL. If durable side effects were
    /// already committed past the point being restored to — a WAL unit fsynced
    /// (e.g. `commit_sandbox`'s batch), or a post-op snapshot renamed into place —
    /// then after this call **disk is ahead of the restored in-memory model** and
    /// a crash before the next checkpoint would recover the very state the caller
    /// meant to discard (and, for a reindexing edit, could replay a stale-index WAL
    /// record onto a re-typed layout). Restoring memory alone cannot roll durable
    /// WAL side effects back.
    ///
    /// The caller is therefore responsible for making disk agree afterward: either
    /// [`checkpoint`](Self::checkpoint) the restored model (rewrites the snapshot,
    /// clears the WAL) — which is what the engine does, failing the writer closed
    /// if that checkpoint also fails — or use
    /// [`restore_model_durably`](Self::restore_model_durably), which does both and
    /// fails loudly when it cannot. Use [`has_uncheckpointed_wal`](Self::has_uncheckpointed_wal)
    /// to detect whether a bare restore would leave disk ahead.
    pub fn restore_model(&mut self, model: Model) {
        self.model = model;
    }

    /// Restore the in-memory model AND make disk agree (E5), returning an error if
    /// it cannot — the fail-loud counterpart of [`restore_model`](Self::restore_model).
    ///
    /// Swaps in `model`, then [`checkpoint`](Self::checkpoint)s it: the snapshot is
    /// rewritten from the restored model and the WAL cleared, so any durable side
    /// effect appended past the restore point (a fsynced WAL unit, a post-op
    /// snapshot) is superseded rather than left to resurrect on recovery. If the
    /// checkpoint fails (I/O, or a poisoned store) the error is propagated so the
    /// caller can fail-stop rather than continue with disk silently ahead of the
    /// restored model. On success, disk == the restored model.
    pub fn restore_model_durably(&mut self, model: Model) -> Result<(), PersistError> {
        self.model = model;
        self.checkpoint()
    }

    /// Whether the WAL holds acknowledged records not yet folded into the snapshot
    /// (its known-good length is past the bare header). When true, a memory-only
    /// [`restore_model`](Self::restore_model) would leave **disk ahead** of the
    /// restored model (E5): those records replay on the next open. A caller that
    /// must not diverge should checkpoint (or use
    /// [`restore_model_durably`](Self::restore_model_durably)) instead.
    pub fn has_uncheckpointed_wal(&self) -> bool {
        self.good_len > wal::WAL_HEADER_LEN
    }

    /// Whether a prior WAL append/fsync failed and left the log's tail untrusted.
    /// A poisoned store rejects further writes; the engine must republish the last
    /// durable model and reopen the directory (recovery truncates the torn tail).
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Durably append a pre-framed WAL payload as one unit. On any write/fsync
    /// failure this truncates the WAL back to the last known-good offset (so the
    /// log can never hold a torn frame followed by later live records) and poisons
    /// the store; the caller must NOT have mutated the in-memory cube yet, so a
    /// poisoned append leaves memory consistent with the durable log. On success it
    /// advances the known-good offset past the just-appended bytes.
    fn append_wal(&mut self, framed: &[u8]) -> Result<(), PersistError> {
        if self.poisoned {
            return Err(PersistError::Poisoned);
        }
        // A partial write_all or a failed fsync leaves the tail in an unknown
        // state: restore it to the last durable offset and refuse further writes.
        if let Err(e) = self.wal.write_all(framed) {
            self.poison_wal();
            return Err(PersistError::Io(e));
        }
        if self.sync_on_write {
            if let Err(e) = self.wal.sync_data() {
                self.poison_wal();
                return Err(PersistError::Io(e));
            }
        }
        self.good_len += framed.len() as u64;
        Ok(())
    }

    /// Roll the WAL back to the last known-good offset and mark the store poisoned.
    /// Best-effort: if the rollback itself fails, the store stays poisoned so no
    /// further write can land after the torn frame, and recovery on the next open
    /// discards everything past the first torn record.
    fn poison_wal(&mut self) {
        self.poisoned = true;
        let _ = self.wal.set_len(self.good_len);
        let _ = self.wal.seek(SeekFrom::Start(self.good_len));
    }

    /// Write a leaf cell: apply it to the in-memory cube and append it to the
    /// WAL. The model validates the coordinate first, so a rejected write is
    /// never logged.
    pub fn set_leaf(&mut self, coord: &[u32], value: Fixed) -> Result<(), PersistError> {
        if self.poisoned {
            return Err(PersistError::Poisoned);
        }
        self.model.cube.set_leaf(coord, value)?;
        let framed = wal::encode(&Record::SetLeaf {
            coord: coord.to_vec(),
            value,
        });
        // If the append fails the store is poisoned; the engine republishes the
        // last durable model and reopens, discarding this diverged in-memory write.
        self.append_wal(&framed)?;
        // Fold the log into a snapshot if it has crossed the byte budget (P1).
        self.maybe_auto_checkpoint()
    }

    /// Write a string cell: apply it to the in-memory cube and append it to the
    /// WAL. Like [`set_leaf`](Self::set_leaf), the model validates first.
    pub fn set_string(&mut self, coord: &[u32], value: &str) -> Result<(), PersistError> {
        if self.poisoned {
            return Err(PersistError::Poisoned);
        }
        self.model.cube.set_string(coord, value)?;
        let framed = wal::encode(&Record::SetString {
            coord: coord.to_vec(),
            value: value.to_string(),
        });
        self.append_wal(&framed)?;
        // Fold the log into a snapshot if it has crossed the byte budget (P1).
        self.maybe_auto_checkpoint()
    }

    /// Apply a batch of writes atomically (all-or-nothing). Validates and applies
    /// every write to a throwaway clone first: any rejected write returns
    /// [`PersistError::BatchRejected`] with its index and leaves the live cube
    /// untouched. On success the framed batch (begin .. records .. end) is
    /// appended as one WAL unit with a single fsync, then the trial is adopted; a
    /// batch torn by a crash before its end marker is discarded whole on recovery.
    pub fn set_batch(&mut self, writes: &[CellWrite]) -> Result<(), PersistError> {
        if self.poisoned {
            return Err(PersistError::Poisoned);
        }
        // 1. Validate the whole batch read-only against the LIVE cube (no clone):
        //    `Cube::validate_batch` reports the exact `(index, error)` a mutating
        //    trial-apply on a throwaway clone would, because a cell write changes
        //    values, not the schema, so no earlier write can invalidate a later
        //    write's coordinate. Abort the whole batch on the first rejection with
        //    the live cube untouched.
        self.model
            .cube
            .validate_batch(&batch_writes(writes))
            .map_err(|(index, source)| PersistError::BatchRejected { index, source })?;
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
        // Durably append the whole batch as one unit BEFORE mutating memory
        // (stage-then-commit). On failure the WAL is rolled back to the last good
        // offset (so a fully-framed-but-unsynced batch cannot be resurrected on
        // recovery) and the store is poisoned; nothing has touched the in-memory
        // cube yet, so memory stays consistent with the durable log.
        self.append_wal(&framed)?;
        // 3. The WAL now durably reflects the batch, so apply it to the live cube.
        //    Every write was proven writable by `validate_batch` above, and a cell
        //    write never changes another coordinate's writability, so each apply is
        //    infallible here; a failure would be a core invariant break, not a
        //    client error, so it panics rather than leaving memory half-applied.
        apply_writes(&mut self.model.cube, writes)
            .expect("validated batch must apply cleanly to the live cube");
        // Fold the log into a snapshot if it has crossed the byte budget (P1).
        self.maybe_auto_checkpoint()
    }

    /// Full-persist: rewrite the snapshot from the current in-memory model and
    /// clear the WAL. After this, recovery needs only the snapshot. Because the
    /// snapshot is written from the in-memory cube (which already reflects every
    /// outstanding WAL write), a checkpoint also folds those writes in safely. The
    /// snapshot is made durable (fsync + atomic rename) before the WAL is cleared,
    /// so the WAL is never truncated while the new snapshot is not yet on disk.
    pub fn checkpoint(&mut self) -> Result<(), PersistError> {
        // A poisoned store's in-memory model may diverge from the (untrusted) WAL
        // tail, so folding it into a fresh snapshot could persist a write whose
        // caller was told it failed. Refuse; the engine republishes and reopens.
        if self.poisoned {
            return Err(PersistError::Poisoned);
        }
        self.checkpoint_model(None)
    }

    /// Rewrite the snapshot and clear the WAL, optionally from a `staged` model
    /// instead of the live one. `None` checkpoints the live `self.model` (the
    /// explicit full-persist). `Some(model)` is the stage-then-commit path (P3):
    /// the would-be model is made durable (temp + fsync + atomic rename) and the
    /// WAL cleared BEFORE it is committed into `self.model`, so a save failure
    /// returns with the in-memory model untouched (no memory/disk divergence).
    fn checkpoint_model(&mut self, staged: Option<Model>) -> Result<(), PersistError> {
        let model = staged.as_ref().unwrap_or(&self.model);
        // Durably rewrite the snapshot from the chosen model first. On failure
        // nothing below runs, so neither the WAL nor `self.model` changed.
        write_snapshot(&self.dir, model)?;
        self.wal.set_len(0)?;
        self.wal.seek(SeekFrom::Start(0))?;
        self.wal.write_all(&wal::header())?;
        self.wal.sync_data()?;
        self.good_len = wal::WAL_HEADER_LEN;
        // Snapshot + empty WAL are now durable; commit the staged model to memory.
        if let Some(model) = staged {
            self.model = model;
        }
        Ok(())
    }

    /// Stage-then-commit a definition change (P3): clone the model, apply `op` to
    /// the clone, and durably checkpoint the clone BEFORE adopting it, so a save
    /// (snapshot-write) failure leaves the live `self.model` exactly as it was —
    /// memory never diverges from disk. `op`'s own error (e.g. a rejected
    /// definition) is returned with nothing changed either. A poisoned store
    /// refuses, matching [`checkpoint`](Self::checkpoint).
    ///
    /// The staged clone is cheap relative to the snapshot serialize + fsync a
    /// checkpoint already performs, and definition changes are infrequent
    /// structural edits, so cloning here is not on any hot path.
    fn staged_define<T>(
        &mut self,
        op: impl FnOnce(&mut Model) -> Result<T, PersistError>,
    ) -> Result<T, PersistError> {
        if self.poisoned {
            return Err(PersistError::Poisoned);
        }
        let mut staged = self.model.clone();
        let value = op(&mut staged)?;
        self.checkpoint_model(Some(staged))?;
        Ok(value)
    }

    /// Define (create or replace) a subset, then checkpoint so the definition is
    /// durable. Structural validation runs first: an invalid subset returns
    /// [`PersistError::Query`] and leaves the model and snapshot untouched.
    pub fn define_subset(&mut self, subset: Subset) -> Result<(), PersistError> {
        self.staged_define(|model| {
            validate_subset(&model.cube, &subset)?;
            model
                .subsets
                .insert((subset.dimension.clone(), subset.name.clone()), subset);
            Ok(())
        })
    }

    /// Delete a subset by dimension and name. Returns whether one was removed;
    /// checkpoints only when something changed.
    pub fn delete_subset(&mut self, dimension: &str, name: &str) -> Result<bool, PersistError> {
        let key = (dimension.to_string(), name.to_string());
        if !self.model.subsets.contains_key(&key) {
            return Ok(false);
        }
        self.staged_define(|model| {
            model.subsets.remove(&key);
            Ok(())
        })?;
        Ok(true)
    }

    /// Define (create or replace) a view, then checkpoint. Structural validation
    /// (coverage, subset references, member/context resolution) runs first; an
    /// invalid view returns [`PersistError::Query`] and changes nothing.
    pub fn define_view(&mut self, view: View) -> Result<(), PersistError> {
        self.staged_define(|model| {
            validate_view(model, &view)?;
            model.views.insert(view.name.clone(), view);
            Ok(())
        })
    }

    /// Delete a view by name. Returns whether one was removed; checkpoints only
    /// when something changed.
    pub fn delete_view(&mut self, name: &str) -> Result<bool, PersistError> {
        if !self.model.views.contains_key(name) {
            return Ok(false);
        }
        let name = name.to_string();
        self.staged_define(|model| {
            model.views.remove(&name);
            Ok(())
        })?;
        Ok(true)
    }

    /// Set the cube's rules source, then checkpoint. The source is stored
    /// verbatim; its validity is checked by the calc layer at the API boundary
    /// (the store and persist crate stay calc-free).
    pub fn define_rules(&mut self, source: String) -> Result<(), PersistError> {
        self.staged_define(|model| {
            model.rules = RuleSet { source };
            Ok(())
        })
    }

    /// Clear the cube's rules. Returns whether there were any; checkpoints only
    /// when something changed.
    pub fn delete_rules(&mut self) -> Result<bool, PersistError> {
        if self.model.rules.is_empty() {
            return Ok(false);
        }
        self.staged_define(|model| {
            model.rules = RuleSet::default();
            Ok(())
        })?;
        Ok(true)
    }

    /// Define (create or replace) a rule unit test, then checkpoint.
    pub fn define_rule_test(&mut self, test: RuleTest) -> Result<(), PersistError> {
        self.staged_define(|model| {
            model.tests.insert(test.name.clone(), test);
            Ok(())
        })
    }

    /// Delete a rule test by name. Returns whether one was removed; checkpoints
    /// only when something changed.
    pub fn delete_rule_test(&mut self, name: &str) -> Result<bool, PersistError> {
        if !self.model.tests.contains_key(name) {
            return Ok(false);
        }
        let name = name.to_string();
        self.staged_define(|model| {
            model.tests.remove(&name);
            Ok(())
        })?;
        Ok(true)
    }

    // Flows, flow tests, connections, and jobs are no longer per-cube (ADR-0035);
    // they are persisted by the server-global `AutomationStore`, so the per-cube
    // `Store` no longer defines or deletes them.

    /// Define (create or replace) a sandbox, then checkpoint. A sandbox is a
    /// per-user what-if overlay (ADR-0014); it is persisted in the model snapshot
    /// and recovered on reopen, never in the base WAL. A create carries empty
    /// deltas; replacing an existing sandbox overwrites it.
    pub fn define_sandbox(&mut self, sandbox: Sandbox) -> Result<(), PersistError> {
        self.staged_define(|model| {
            model.sandboxes.insert(sandbox.name.clone(), sandbox);
            Ok(())
        })
    }

    /// Delete a sandbox by name (discard). Returns whether one was removed;
    /// checkpoints only when something changed.
    pub fn delete_sandbox(&mut self, name: &str) -> Result<bool, PersistError> {
        if !self.model.sandboxes.contains_key(name) {
            return Ok(false);
        }
        let name = name.to_string();
        self.staged_define(|model| {
            model.sandboxes.remove(&name);
            Ok(())
        })?;
        Ok(true)
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
        // Validate every override read-only against the live cube (leaf-only,
        // in-range) via `validate_batch`; this never mutates base cells, only
        // confirms each coordinate is writable (P4: no clone). String writes were
        // already rejected above, so only leaf coordinates reach here.
        self.model
            .cube
            .validate_batch(&batch_writes(writes))
            .map_err(|(index, source)| PersistError::BatchRejected { index, source })?;
        // Record the numeric overrides in the sandbox overlay (the value verbatim,
        // so an explicit zero override is kept rather than dropped). String writes
        // were rejected above, so `string_cells` stays empty this phase.
        // Stage-then-commit (P3): the overlay change is applied to a clone and made
        // durable before it is adopted, so a checkpoint failure leaves the live
        // sandbox untouched.
        let name = name.to_string();
        self.staged_define(|model| {
            let sb = model
                .sandboxes
                .get_mut(&name)
                .expect("sandbox presence checked above");
            for write in writes {
                if let CellWrite::Leaf { coord, value } = write {
                    sb.cells.insert(coord.clone(), *value);
                }
            }
            sb.updated = updated;
            Ok(())
        })
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
        // Apply to base (validates against the live cube; WALs on success). A
        // rejected write propagates and leaves base and the sandbox unchanged.
        self.set_batch(&writes)?;
        // Clear the now-merged deltas (the sandbox stays, empty) and checkpoint,
        // which folds the just-applied batch into the snapshot and clears the WAL.
        // Stage-then-commit (P3): the delta clear is adopted only after the
        // snapshot is durable, so a checkpoint failure does not leave the sandbox
        // emptied in memory while the snapshot still shows the old deltas.
        let name = name.to_string();
        self.staged_define(|model| {
            let sb = model
                .sandboxes
                .get_mut(&name)
                .expect("sandbox presence checked above");
            sb.cells.clear();
            sb.string_cells.clear();
            sb.updated = updated;
            Ok(())
        })
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
        // commits on full success), so a rejected change leaves the staged model
        // untouched. Stage-then-commit (P3): the grow is applied to a model clone
        // and made durable before it is adopted, so a checkpoint failure leaves the
        // live cube untouched.
        self.staged_define(|model| {
            model
                .cube
                .extend_schema(elements, edges)
                .map_err(Into::into)
        })
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
        self.staged_define(|model| {
            model.cube.define_attribute(dimension, name, kind)?;
            Ok(())
        })
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
        self.staged_define(|model| {
            model
                .cube
                .set_attribute_values(dimension, attribute, values)?;
            Ok(())
        })
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
    // the new snapshot and clearing the WAL. (remove-child keeps every member's
    // kind and cells, so its single post-edit checkpoint suffices.)
    //
    // set-kind, add-child, and reparent are index-stable but NOT writability-stable:
    // each can re-type an element to a consolidation/string (set_element_kind
    // directly; add_child/reparent convert a numeric/string parent that gains a
    // child, dropping its stored cell). An outstanding WAL SetLeaf/SetString for
    // that element, written since the last checkpoint, would then replay onto the
    // post-edit snapshot and be rejected by the model, failing Store::open and
    // aborting boot. So these three ALSO checkpoint BEFORE the edit, emptying the
    // WAL so no pre-edit cell write survives to be replayed onto the re-typed layout.

    /// Reorder a dimension's members, remapping every stored cell, then checkpoint.
    pub fn reorder_elements(
        &mut self,
        dimension: &str,
        new_order: &[String],
    ) -> Result<(), PersistError> {
        // Reindexing op: checkpoint first so the WAL holds no old-index writes
        // when the post-edit snapshot is written (see the block comment above).
        // (If this pre-edit checkpoint fails nothing has been mutated, so there is
        // no divergence.)
        self.checkpoint()?;
        // Stage-then-commit the edit (P3): a post-edit checkpoint failure leaves
        // the live model untouched. The WAL is already empty from the pre-edit
        // checkpoint, so the rolled-back model still agrees with disk.
        self.staged_define(|model| {
            model
                .reorder_elements(dimension, new_order)
                .map_err(Into::into)
        })
    }

    /// Reparent a member (or detach to a root), then checkpoint.
    pub fn reparent_element(
        &mut self,
        dimension: &str,
        child: &str,
        new_parent: Option<&str>,
        weight: i64,
    ) -> Result<(), PersistError> {
        // Writability-changing edit: checkpoint first so no outstanding cell write
        // lingers in the WAL to be replayed onto a re-typed element (see the block
        // comment above).
        self.checkpoint()?;
        // Route through the Model wrapper so sandbox overrides the edit made
        // unwritable are pruned alongside the cube's own cell drops. Stage-then-
        // commit (P3): a post-edit checkpoint failure leaves the live model as-is.
        self.staged_define(|model| {
            model
                .reparent_element(dimension, child, new_parent, weight)
                .map_err(Into::into)
        })
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
        // Writability-changing edit (a numeric/string parent becomes a
        // consolidation): checkpoint first so no outstanding cell write for it
        // lingers in the WAL (see the block comment above).
        self.checkpoint()?;
        // Model wrapper: prunes sandbox overrides stranded by the edit. Stage-then-
        // commit (P3): a post-edit checkpoint failure leaves the live model as-is.
        self.staged_define(|model| {
            model
                .add_child_element(dimension, parent, child, weight)
                .map_err(Into::into)
        })
    }

    /// Remove the single `parent -> child` consolidation edge (keeping the member,
    /// its cells, and its other parents), then checkpoint.
    pub fn remove_child_element(
        &mut self,
        dimension: &str,
        parent: &str,
        child: &str,
    ) -> Result<(), PersistError> {
        // Stage-then-commit (P3): apply on a clone and adopt only after the
        // snapshot is durable, so a checkpoint failure leaves the live model as-is.
        self.staged_define(|model| {
            model
                .cube
                .remove_child_element(dimension, parent, child)
                .map_err(Into::into)
        })
    }

    /// Pin a member to the top level (ADR-0038): apply it to the in-memory cube and
    /// append a `SetPin` record to the WAL. The pin is a display-only marker (no
    /// rollup edge, value, or element index changes), so it is index-stable and the
    /// name-addressed WAL record stays valid against the snapshot it replays onto
    /// (like a cell write, and unlike a reindexing edit which must checkpoint). The
    /// model validates first, so a rejected pin (unknown element) is never logged.
    /// Idempotent (pinning an already-pinned or no-parent member is a no-op).
    pub fn pin_element_to_top(
        &mut self,
        dimension: &str,
        element: &str,
    ) -> Result<(), PersistError> {
        self.set_pin(dimension, element, true)
    }

    /// Unpin a member from the top level (ADR-0038): apply it and append a `SetPin`
    /// record. Index-stable (see [`pin_element_to_top`](Self::pin_element_to_top)).
    /// Idempotent (unpinning an unpinned member is a no-op).
    pub fn unpin_element_from_top(
        &mut self,
        dimension: &str,
        element: &str,
    ) -> Result<(), PersistError> {
        self.set_pin(dimension, element, false)
    }

    /// Apply a pin/unpin and append its `SetPin` WAL record. Shared by
    /// [`pin_element_to_top`](Self::pin_element_to_top) and
    /// [`unpin_element_from_top`](Self::unpin_element_from_top).
    fn set_pin(
        &mut self,
        dimension: &str,
        element: &str,
        pinned: bool,
    ) -> Result<(), PersistError> {
        if self.poisoned {
            return Err(PersistError::Poisoned);
        }
        if pinned {
            self.model.cube.pin_element_to_top(dimension, element)?;
        } else {
            self.model.cube.unpin_element_from_top(dimension, element)?;
        }
        let framed = wal::encode(&Record::SetPin {
            dimension: dimension.to_string(),
            element: element.to_string(),
            pinned,
        });
        self.append_wal(&framed)?;
        // Fold the log into a snapshot if it has crossed the byte budget (P1).
        self.maybe_auto_checkpoint()
    }

    /// Convert a member's kind (re-typing or clearing its cells), then checkpoint.
    pub fn set_element_kind(
        &mut self,
        dimension: &str,
        element: &str,
        kind: ElementKind,
    ) -> Result<(), PersistError> {
        // Writability-changing edit (re-types the element): checkpoint first so no
        // outstanding cell write for it lingers in the WAL to be replayed onto the
        // re-typed element and rejected at recovery (see the block comment above).
        self.checkpoint()?;
        // Model wrapper: prunes sandbox overrides stranded by the re-typing.
        // Stage-then-commit (P3): a post-edit checkpoint failure leaves the live
        // model as-is.
        self.staged_define(|model| {
            model
                .set_element_kind(dimension, element, kind)
                .map_err(Into::into)
        })
    }

    /// Delete a member, its edges, and its cells, reindexing the rest, then
    /// checkpoint.
    pub fn delete_element(&mut self, dimension: &str, element: &str) -> Result<(), PersistError> {
        // Reindexing op: checkpoint first (see reorder_elements).
        self.checkpoint()?;
        // Stage-then-commit (P3): a post-edit checkpoint failure leaves the live
        // model as-is (the WAL is already empty from the pre-edit checkpoint).
        self.staged_define(|model| model.delete_element(dimension, element).map_err(Into::into))
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
        // Stage-then-commit (P3): a post-edit checkpoint failure leaves the live
        // model as-is.
        self.staged_define(|model| {
            model
                .insert_element_at(dimension, name, kind, position)
                .map_err(Into::into)
        })
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
    /// Pin `element` to the top level (ADR-0038), so it shows as a display root
    /// even while it also rolls up under its consolidations. A display-only marker:
    /// no rollup edge, value, or index changes. Idempotent (pinning an
    /// already-pinned or no-parent member is a no-op).
    PinToTop { element: String },
    /// Unpin `element` from the top level (ADR-0038). It reverts to a display root
    /// only if it has no parent. Idempotent (unpinning an unpinned member is a
    /// no-op). Distinct from `RemoveChild`/`Reparent`, which change rollup edges.
    UnpinFromTop { element: String },
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
            DimensionEdit::PinToTop { element } => self.pin_element_to_top(dimension, element),
            DimensionEdit::UnpinFromTop { element } => {
                self.unpin_element_from_top(dimension, element)
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

/// Borrow a slice of [`CellWrite`]s as the read-only [`BatchWrite`]s that
/// [`Cube::validate_batch`] checks, so a whole batch is validated against the
/// live cube without cloning it (P4). The value is carried but only the
/// coordinate is inspected.
fn batch_writes(writes: &[CellWrite]) -> Vec<BatchWrite<'_>> {
    writes
        .iter()
        .map(|w| match w {
            CellWrite::Leaf { coord, value } => BatchWrite::Leaf {
                coord,
                value: *value,
            },
            CellWrite::Str { coord, value } => BatchWrite::Str { coord, value },
        })
        .collect()
}

/// Apply an already-validated batch to `cube` in order. Returns the first
/// `ModelError` if any write is rejected; callers that pre-checked with
/// [`Cube::validate_batch`] treat a rejection here as an invariant break.
fn apply_writes(cube: &mut Cube, writes: &[CellWrite]) -> Result<(), ModelError> {
    for write in writes {
        match write {
            CellWrite::Leaf { coord, value } => cube.set_leaf(coord, *value)?,
            CellWrite::Str { coord, value } => cube.set_string(coord, value)?,
        }
    }
    Ok(())
}

/// Write the snapshot durably: serialize to a temp file and fsync its contents,
/// rename over the live snapshot (rename replaces the destination on all supported
/// platforms), then make the rename itself durable before returning. Flushing the
/// temp file before the rename is what lets [`Store::checkpoint`] clear the WAL
/// safely: the new snapshot's bytes are on disk before the WAL is truncated.
///
/// Making the rename durable is platform-specific. On Unix we fsync the directory.
/// On Windows a directory handle cannot be flushed, and `fs::rename` uses
/// `MoveFileExW` WITHOUT `MOVEFILE_WRITE_THROUGH`, so the NTFS metadata journal only
/// guarantees the rename is *consistent*, not that it has reached disk before the
/// subsequent fsynced WAL truncation. If the rename were still buffered on a power
/// loss just after a checkpoint, recovery could see the OLD snapshot beside an
/// already-truncated (empty) WAL and silently lose every acknowledged write since
/// the previous checkpoint. So on Windows we reopen the renamed snapshot and
/// `sync_all()` (`FlushFileBuffers`, which flushes the file's MFT record including
/// its name attribute), forcing the rename to disk before the WAL is cleared.
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
    let snapshot = dir.join(SNAPSHOT_FILE);
    fs::rename(&tmp, &snapshot)?;
    sync_dir(dir)?;
    sync_renamed_file(&snapshot)?;
    Ok(())
}

/// Make a just-renamed file durable on platforms where a directory handle cannot
/// be fsynced (Windows). Reopening the file and calling `sync_all()` flushes its
/// metadata, including the name attribute the rename set, so the rename reaches
/// disk. A no-op on Unix, where [`sync_dir`] already made the rename durable.
fn sync_renamed_file(path: &Path) -> Result<(), PersistError> {
    #[cfg(not(unix))]
    {
        // FlushFileBuffers (what sync_all maps to on Windows) needs write access,
        // so the handle must be opened for writing, not read-only.
        OpenOptions::new().write(true).open(path)?.sync_all()?;
    }
    #[cfg(unix)]
    {
        let _ = path;
    }
    Ok(())
}

/// fsync a directory so a contained rename or create is durable. Unix supports
/// opening a directory as a file and fsync-ing it; Windows does not (a directory
/// handle cannot be flushed, and NTFS records the rename in its own metadata
/// journal), so this is a no-op there (see [`sync_renamed_file`], which forces the
/// rename to disk on Windows instead).
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
    fn writability_changing_edit_folds_outstanding_writes_and_recovers() {
        // A kind-converting edit (set_element_kind here) can re-type an element to a
        // consolidation, which would reject an outstanding pre-edit WAL SetLeaf for
        // it on recovery and fail boot. Like the reindexing ops, these edits must
        // checkpoint BEFORE the edit, folding any outstanding cell write into the
        // snapshot and emptying the WAL, so a crash in the window between the
        // post-edit snapshot and the WAL truncation cannot resurrect a stale record.
        let dir = scratch("writability-folds");
        let f = fixture();
        let (r, p) = (f.r.clone(), f.p.clone());
        {
            let mut store = Store::create(&dir, f.cube).unwrap();
            // Outstanding, un-checkpointed writes: they live only in the WAL.
            store.set_leaf(&[r[0], p[0]], Fixed::from(10)).unwrap();
            store.set_leaf(&[r[1], p[0]], Fixed::from(20)).unwrap();
            assert!(
                fs::metadata(dir.join(WAL_FILE)).unwrap().len() > wal::WAL_HEADER_LEN,
                "outstanding writes are in the WAL before the edit"
            );
            // Convert R2 (a leaf) to a consolidation. The pre-edit checkpoint must
            // fold R0/R1's writes into the snapshot and empty the WAL first.
            store
                .set_element_kind("Region", "R2", ElementKind::Consolidated)
                .unwrap();
            assert_eq!(
                fs::metadata(dir.join(WAL_FILE)).unwrap().len(),
                wal::WAL_HEADER_LEN,
                "the writability-changing edit emptied the WAL"
            );
        }
        // Reopen with no extra checkpoint: boot succeeds and the folded writes
        // survived (they came from the snapshot, not a replayed WAL record).
        let store = Store::open(&dir).unwrap();
        assert_eq!(
            store.cube().get_leaf(&[r[0], p[0]]).unwrap(),
            Fixed::from(10)
        );
        assert_eq!(
            store.cube().get_leaf(&[r[1], p[0]]).unwrap(),
            Fixed::from(20)
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn recovery_survives_a_stale_write_onto_a_retyped_element() {
        // Belt-and-braces: even if a pre-edit SetLeaf somehow reached the WAL after
        // the element was re-typed in the snapshot (the exact crash-window state the
        // checkpoint-first fix prevents), Store::open must not spuriously succeed by
        // applying it; the guarantee is that this state is never produced. Here we
        // assert the produced state after the fix: the snapshot has R2 consolidated
        // and the WAL is empty, so no stale coordinate can replay.
        let dir = scratch("retype-no-stale");
        let f = fixture();
        let (r, p) = (f.r.clone(), f.p.clone());
        let mut store = Store::create(&dir, f.cube).unwrap();
        store.set_leaf(&[r[2], p[0]], Fixed::from(7)).unwrap();
        store
            .set_element_kind("Region", "R2", ElementKind::Consolidated)
            .unwrap();
        // The WAL is empty (no SetLeaf(R2) lingering to be replayed).
        assert_eq!(
            fs::metadata(dir.join(WAL_FILE)).unwrap().len(),
            wal::WAL_HEADER_LEN
        );
        drop(store);
        let store = Store::open(&dir).unwrap();
        let region = store
            .cube()
            .dimensions()
            .iter()
            .find(|d| d.name() == "Region")
            .unwrap();
        assert_eq!(
            region.element(region.index_of("R2").unwrap()).unwrap().kind,
            ElementKind::Consolidated
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pin_to_top_recovers_via_wal_replay() {
        // ADR-0038: a pin is logged as a name-addressed WAL record and recovers by
        // replay onto the snapshot WITHOUT an explicit checkpoint (the crash case).
        let dir = scratch("pin-wal");
        let f = fixture();
        {
            let mut store = Store::create(&dir, f.cube).unwrap();
            // R0 rolls up under Total; pin it to the top level.
            store.pin_element_to_top("Region", "R0").unwrap();
            // Drop WITHOUT a checkpoint: the pin must replay from the WAL.
        }
        let store = Store::open(&dir).unwrap();
        let region = store
            .cube()
            .dimensions()
            .iter()
            .find(|d| d.name() == "Region")
            .unwrap();
        let r0 = region.index_of("R0").unwrap();
        let total = region.index_of("Total").unwrap();
        assert!(region.is_pinned_to_top(r0).unwrap(), "the pin replayed");
        // The rollup edge is intact: R0 is still a child of Total.
        assert_eq!(region.children_of(total).unwrap()[0], r0);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pin_persists_through_checkpoint_and_unpin_reverts() {
        // A pin survives a checkpoint (snapshot rewrite + WAL clear), and a later
        // unpin (also WAL-logged) reverts it across a reopen.
        let dir = scratch("pin-checkpoint");
        let f = fixture();
        {
            let mut store = Store::create(&dir, f.cube).unwrap();
            store.pin_element_to_top("Region", "R0").unwrap();
            store.checkpoint().unwrap();
            // After the checkpoint the WAL is back to its header (the pin folded
            // into the snapshot).
            assert_eq!(
                fs::metadata(dir.join(WAL_FILE)).unwrap().len(),
                wal::WAL_HEADER_LEN
            );
        }
        {
            let store = Store::open(&dir).unwrap();
            let region = store
                .cube()
                .dimensions()
                .iter()
                .find(|d| d.name() == "Region")
                .unwrap();
            assert!(region
                .is_pinned_to_top(region.index_of("R0").unwrap())
                .unwrap());
        }
        // Unpin (WAL-logged), drop without checkpoint, reopen: the unpin replays.
        {
            let mut store = Store::open(&dir).unwrap();
            store.unpin_element_from_top("Region", "R0").unwrap();
        }
        let store = Store::open(&dir).unwrap();
        let region = store
            .cube()
            .dimensions()
            .iter()
            .find(|d| d.name() == "Region")
            .unwrap();
        assert!(!region
            .is_pinned_to_top(region.index_of("R0").unwrap())
            .unwrap());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edit_dimension_dispatches_pin_and_unpin() {
        // The DimensionEdit dispatch (ADR-0038) reaches the pin/unpin Store methods.
        fn r1_pinned(store: &Store) -> bool {
            let region = store
                .cube()
                .dimensions()
                .iter()
                .find(|d| d.name() == "Region")
                .unwrap();
            region
                .is_pinned_to_top(region.index_of("R1").unwrap())
                .unwrap()
        }
        let dir = scratch("pin-edit-dispatch");
        let f = fixture();
        let mut store = Store::create(&dir, f.cube).unwrap();
        store
            .edit_dimension(
                "Region",
                &DimensionEdit::PinToTop {
                    element: "R1".into(),
                },
            )
            .unwrap();
        assert!(r1_pinned(&store), "PinToTop dispatched and applied");
        store
            .edit_dimension(
                "Region",
                &DimensionEdit::UnpinFromTop {
                    element: "R1".into(),
                },
            )
            .unwrap();
        assert!(!r1_pinned(&store), "UnpinFromTop dispatched and applied");
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
    fn mid_log_corruption_is_preserved_not_silently_truncated() {
        // A corrupt frame with intact, acknowledged frames after it is mid-log
        // corruption, not a torn tail. Recovery must preserve the WAL (as
        // wal.log.corrupt) and refuse to open, rather than silently set_len the
        // later records away and boot with a truncated cube.
        let dir = scratch("mid-log-corrupt");
        let f = fixture();
        let (r, p) = (f.r.clone(), f.p.clone());
        {
            let mut store = Store::create(&dir, f.cube).unwrap();
            store.set_leaf(&[r[0], p[0]], Fixed::from(10)).unwrap();
            store.set_leaf(&[r[1], p[0]], Fixed::from(20)).unwrap();
            store.set_leaf(&[r[2], p[0]], Fixed::from(30)).unwrap();
        }
        let wal_path = dir.join(WAL_FILE);
        // Corrupt a byte in the MIDDLE of the WAL (flip a byte after the header but
        // before the last record) so a valid frame still follows the damage.
        let mut bytes = fs::read(&wal_path).unwrap();
        let mid = wal::WAL_HEADER_LEN as usize + 6; // inside the first record's payload
        bytes[mid] ^= 0xFF;
        fs::write(&wal_path, &bytes).unwrap();

        let err = Store::open(&dir).unwrap_err();
        assert!(matches!(err, PersistError::Corrupt(_)));
        // The original WAL is preserved for inspection; the live WAL is gone.
        assert!(dir.join("wal.log.corrupt").exists());
        assert!(!wal_path.exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn poisoned_store_refuses_all_writes() {
        // Once poisoned by a failed append, every write path fails closed with
        // PersistError::Poisoned instead of appending after an untrusted tail.
        let dir = scratch("poison-refuses");
        let f = fixture();
        let (r, p) = (f.r.clone(), f.p.clone());
        let mut store = Store::create(&dir, f.cube).unwrap();
        store.set_leaf(&[r[0], p[0]], Fixed::from(10)).unwrap();
        // Simulate the state left by a failed WAL append/fsync.
        store.poison_wal();
        assert!(store.is_poisoned());

        assert!(matches!(
            store.set_leaf(&[r[1], p[0]], Fixed::from(20)),
            Err(PersistError::Poisoned)
        ));
        assert!(matches!(
            store.set_string(&[r[1], p[0]], "x"),
            Err(PersistError::Poisoned)
        ));
        assert!(matches!(
            store.set_batch(&[CellWrite::Leaf {
                coord: vec![r[1], p[0]],
                value: Fixed::from(20),
            }]),
            Err(PersistError::Poisoned)
        ));
        assert!(matches!(
            store.pin_element_to_top("Region", "R0"),
            Err(PersistError::Poisoned)
        ));
        // A poisoned store must not fold its (untrusted) memory into a snapshot.
        assert!(matches!(store.checkpoint(), Err(PersistError::Poisoned)));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn poison_rolls_the_wal_back_to_the_last_good_offset() {
        // The invariant that keeps recovery's stop-at-first-torn-frame scan sound:
        // after a failed append the WAL is truncated back to the last known-good
        // record, so a torn frame can never be followed by later live records.
        let dir = scratch("poison-truncates");
        let f = fixture();
        let (r, p) = (f.r.clone(), f.p.clone());
        let wal_path = dir.join(WAL_FILE);
        let mut store = Store::create(&dir, f.cube).unwrap();
        store.set_leaf(&[r[0], p[0]], Fixed::from(10)).unwrap();
        let good = fs::metadata(&wal_path).unwrap().len();

        // Emulate a partial frame reaching disk before the append failed, then the
        // store poisoning itself (as append_wal does on a write/fsync error).
        store.wal.write_all(&[7u8, 0, 0, 0, 1, 2, 3]).unwrap();
        store.wal.sync_data().unwrap();
        assert!(fs::metadata(&wal_path).unwrap().len() > good);
        store.poison_wal();

        // The torn bytes are gone: the file is back to the last durable record.
        assert_eq!(fs::metadata(&wal_path).unwrap().len(), good);
        // And recovery from that file keeps exactly the acknowledged write.
        drop(store);
        let store = Store::open(&dir).unwrap();
        assert_eq!(store.cube().cell_count(), 1);
        assert_eq!(
            store.cube().get_leaf(&[r[0], p[0]]).unwrap(),
            Fixed::from(10)
        );
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
    fn bare_restore_can_leave_disk_ahead_but_durable_restore_does_not() {
        // E5: `restore_model` is memory-only, so after a durable WAL side effect it
        // leaves DISK ahead of the restored model (the WAL record replays on the
        // next open). `has_uncheckpointed_wal` detects this, and
        // `restore_model_durably` makes disk agree by checkpointing the restored
        // model, so a reopen sees exactly the restored state.
        let dir = scratch("e5-restore-durability");
        let f = fixture();
        let (r, p) = (f.r.clone(), f.p.clone());
        let baseline = {
            let mut store = Store::create(&dir, f.cube).unwrap();
            let snapshot = store.model().clone();
            // A durable, fsynced cell write: the WAL now holds an acknowledged
            // record past the header.
            store.set_leaf(&[r[0], p[0]], Fixed::from(99)).unwrap();
            assert!(
                store.has_uncheckpointed_wal(),
                "the fsynced write left the WAL ahead of the snapshot"
            );
            // A bare (memory-only) restore reverts the value in memory, but the
            // WAL record is still on disk -> disk is ahead of the restored model.
            store.restore_model(snapshot.clone());
            assert_eq!(store.cube().get_leaf(&[r[0], p[0]]).unwrap(), Fixed::ZERO);
            assert!(
                store.has_uncheckpointed_wal(),
                "a bare restore does NOT clear the WAL: disk is still ahead"
            );
            snapshot
        };
        // Proof that disk was ahead: reopening replays the WAL and resurrects 99,
        // NOT the restored (zero) state.
        {
            let reopened = Store::open(&dir).unwrap();
            assert_eq!(
                reopened.cube().get_leaf(&[r[0], p[0]]).unwrap(),
                Fixed::from(99),
                "a bare restore left the durable WAL side effect to resurrect"
            );
        }

        // Now the fail-loud path: write again, then restore DURABLY. It checkpoints
        // the restored model, so the WAL is cleared and a reopen sees zero.
        {
            let mut store = Store::open(&dir).unwrap();
            store.set_leaf(&[r[1], p[0]], Fixed::from(7)).unwrap();
            store.restore_model_durably(baseline.clone()).unwrap();
            assert!(
                !store.has_uncheckpointed_wal(),
                "a durable restore checkpointed the restored model and cleared the WAL"
            );
        }
        let reopened = Store::open(&dir).unwrap();
        assert_eq!(
            reopened.cube().get_leaf(&[r[0], p[0]]).unwrap(),
            Fixed::ZERO,
            "durable restore made disk agree with the restored model"
        );
        assert_eq!(
            reopened.cube().get_leaf(&[r[1], p[0]]).unwrap(),
            Fixed::ZERO
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

    /// Force the next snapshot write to fail by occupying the temp-snapshot path
    /// with a DIRECTORY, so `write_snapshot`'s file open errors deterministically
    /// on every platform. Returns the blocking path (remove it to heal the store).
    fn block_snapshot_writes(dir: &Path) -> PathBuf {
        let blocker = dir.join(SNAPSHOT_TMP);
        fs::create_dir_all(&blocker).unwrap();
        blocker
    }

    #[test]
    fn validate_batch_rejects_without_mutating_the_store() {
        // P4: a batch with a single bad write (targeting a consolidated element) is
        // rejected with its index, and the LIVE store is left untouched — proving
        // the reject path no longer trial-applies to a clone it then discards, but
        // still reports the same rejection and mutates nothing.
        let dir = scratch("p4-reject-no-mutation");
        let f = fixture();
        let (r, p, region_total) = (f.r.clone(), f.p.clone(), f.region_total);
        let mut store = Store::create(&dir, f.cube).unwrap();
        // Seed one good cell so we can prove the store is unchanged afterwards.
        store.set_leaf(&[r[0], p[0]], Fixed::from(10)).unwrap();
        let wal_before = store.wal_len();

        let err = store
            .set_batch(&[
                CellWrite::Leaf {
                    coord: vec![r[1], p[0]],
                    value: Fixed::from(20),
                },
                // Second write targets a consolidated element: rejected.
                CellWrite::Leaf {
                    coord: vec![region_total, p[0]],
                    value: Fixed::from(1),
                },
            ])
            .unwrap_err();
        assert!(matches!(err, PersistError::BatchRejected { index: 1, .. }));
        // Nothing from the rejected batch landed: R1/P0 is still empty, the seed
        // cell is intact, and the WAL did not grow (no frame was appended).
        assert_eq!(store.cube().get_leaf(&[r[1], p[0]]).unwrap(), Fixed::ZERO);
        assert_eq!(
            store.cube().get_leaf(&[r[0], p[0]]).unwrap(),
            Fixed::from(10)
        );
        assert_eq!(store.cube().cell_count(), 1);
        assert_eq!(
            store.wal_len(),
            wal_before,
            "no WAL frame for a rejected batch"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wal_auto_checkpoints_past_the_threshold_and_recovers() {
        // P1: with a small WAL byte budget, steady writes cross the threshold and
        // trigger an automatic checkpoint that folds the log into the snapshot and
        // shrinks the WAL back to its header — bounding the log instead of letting
        // it grow unbounded. Recovery from the checkpointed store reconstructs the
        // identical state.
        let dir = scratch("p1-auto-checkpoint");
        let f = fixture();
        let (r, p, region_total, period_total) =
            (f.r.clone(), f.p.clone(), f.region_total, f.period_total);
        let mut store = Store::create(&dir, f.cube).unwrap();
        // A tiny budget so a handful of writes crosses it deterministically.
        store.set_wal_checkpoint_threshold(64);
        // Header only to start.
        assert_eq!(store.wal_len(), wal::WAL_HEADER_LEN);

        // Write cells one at a time; each append grows the WAL until it crosses 64
        // bytes, at which point the next write auto-checkpoints and the WAL shrinks.
        let mut shrank = false;
        for (i, &leaf) in r.iter().enumerate() {
            store
                .set_leaf(&[leaf, p[0]], Fixed::from((i as i32 + 1) * 10))
                .unwrap();
            if store.wal_len() == wal::WAL_HEADER_LEN && i > 0 {
                shrank = true;
            }
        }
        // Add a couple more writes to be sure we crossed the budget at least once.
        store.set_leaf(&[r[0], p[1]], Fixed::from(5)).unwrap();
        store.set_leaf(&[r[1], p[1]], Fixed::from(7)).unwrap();
        assert!(
            shrank || store.wal_len() < 64 * 4,
            "the WAL must have auto-checkpointed and shrunk at least once"
        );
        // The on-disk WAL is bounded (never grew without bound): it is at most a
        // few records past the last auto-checkpoint, far below a naive linear log.
        let wal_on_disk = fs::metadata(dir.join(WAL_FILE)).unwrap().len();
        assert!(
            wal_on_disk <= 64 * 4,
            "WAL stays bounded near the threshold"
        );

        // Capture the live aggregate, then recover and prove identical state.
        let live_total = store.cube().get(&[region_total, period_total]).unwrap();
        drop(store);
        let recovered = Store::open(&dir).unwrap();
        assert_eq!(
            recovered.cube().get(&[region_total, period_total]).unwrap(),
            live_total,
            "recovery after auto-checkpoint reconstructs identical state"
        );
        assert_eq!(
            recovered.cube().get_leaf(&[r[1], p[1]]).unwrap(),
            Fixed::from(7)
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_failure_leaves_memory_unchanged_no_divergence() {
        // P3 (stage-then-commit): a definition mutator whose snapshot write fails
        // must leave the in-memory model EXACTLY as it was, so memory never
        // diverges from disk (before the fix, the model was mutated first and a
        // failed checkpoint left the "failed" definition live until restart).
        let dir = scratch("p3-no-divergence");
        let f = fixture();
        let mut store = Store::create(&dir, f.cube).unwrap();
        store.define_subset(static_subset("Keep", &["R0"])).unwrap();
        assert!(store.model().subset("Region", "Keep").is_some());

        // Block the next snapshot write, then attempt a define. It must fail with a
        // persist/I-O error and change NOTHING in memory.
        let blocker = block_snapshot_writes(&dir);
        let err = store
            .define_subset(static_subset("Ghost", &["R1"]))
            .unwrap_err();
        assert!(
            matches!(err, PersistError::Io(_) | PersistError::Save(_)),
            "a blocked snapshot write surfaces as a save/I-O error, got {err:?}"
        );
        assert!(
            store.model().subset("Region", "Ghost").is_none(),
            "the failed definition must NOT be live in memory (no divergence)"
        );
        assert!(
            store.model().subset("Region", "Keep").is_some(),
            "the prior committed definition is untouched"
        );

        // Heal the store (remove the blocker) and confirm disk agrees with memory:
        // a reopen sees "Keep" and not "Ghost".
        fs::remove_dir_all(&blocker).unwrap();
        store.checkpoint().unwrap();
        drop(store);
        let reopened = Store::open(&dir).unwrap();
        assert!(reopened.model().subset("Region", "Keep").is_some());
        assert!(
            reopened.model().subset("Region", "Ghost").is_none(),
            "disk never recorded the failed definition"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn staged_structural_edit_save_failure_leaves_cube_unchanged() {
        // P3 for a structural (cell-remapping) edit: a blocked snapshot write on the
        // post-edit checkpoint must leave the live cube's cells and dimension order
        // exactly as before the edit.
        let dir = scratch("p3-structural-no-divergence");
        let f = fixture();
        let (r, p) = (f.r.clone(), f.p.clone());
        let mut store = Store::create(&dir, f.cube).unwrap();
        store.set_leaf(&[r[0], p[0]], Fixed::from(10)).unwrap();
        store.set_leaf(&[r[1], p[0]], Fixed::from(20)).unwrap();
        store.checkpoint().unwrap();

        // Block snapshot writes, then attempt a reorder. The pre-edit checkpoint
        // (no outstanding writes -> just rewrites the same snapshot) fails first,
        // so the model is untouched.
        let blocker = block_snapshot_writes(&dir);
        let err = store
            .reorder_elements(
                "Region",
                &["R1".into(), "R0".into(), "R2".into(), "Total".into()],
            )
            .unwrap_err();
        assert!(matches!(err, PersistError::Io(_) | PersistError::Save(_)));
        // The dimension order is unchanged: R0 is still index 0.
        let region = store
            .cube()
            .dimensions()
            .iter()
            .find(|d| d.name() == "Region")
            .unwrap();
        assert_eq!(region.index_of("R0"), Some(r[0]));
        assert_eq!(
            store.cube().get_leaf(&[r[0], p[0]]).unwrap(),
            Fixed::from(10)
        );
        fs::remove_dir_all(&blocker).unwrap();
        fs::remove_dir_all(&dir).ok();
    }
}
