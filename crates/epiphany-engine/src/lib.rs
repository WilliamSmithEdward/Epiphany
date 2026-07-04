//! Epiphany engine: the concurrent layer over durable cube stores.
//!
//! Realizes ADR-0001 (MVCC / copy-on-write). Each cube is published as an
//! immutable `Arc` behind an [`arc_swap::ArcSwap`], so reads are lock-free and
//! never block (or get blocked by) writes, and a reader's snapshot pins one
//! consistent whole-cube version. Writes take a per-cube writer lock that
//! validates a batch against a clone, durably logs it as one WAL unit
//! ([`epiphany_persist::Store::set_batch`]), then atomically publishes the new
//! version: a batch is applied all-or-nothing, and concurrent readers see the
//! full batch or none of it.
//!
//! At M2 scale a commit clones the whole cube (cheap for small cubes); a
//! structural-sharing store is a benchmark-gated later optimization behind this
//! same handle (ROADMAP section 13). The engine adds no per-cell memory: the
//! live cube keeps its packed layout (ADR-0006).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use arc_swap::ArcSwap;
use epiphany_core::{
    AttributeKind, AttributeValue, CellResolver, Cube, Dimension, DimensionDef, EdgeSpec,
    ElementMask, ElementSpec, Fixed, Model, ModelError, QueryError, RuleSet, RuleTest, Sandbox,
    Subset, View,
};
use epiphany_determinism::IdGen;
use epiphany_persist::{load_registry, save_registry, slug, PersistError, RegistryEntry, Store};

pub use epiphany_persist::CellWrite;

mod dimensions;
pub use dimensions::{DimensionId, DimensionRegistry, SharedDimension};
pub use epiphany_persist::DimensionEdit;

/// Stable crate identifier, reported by the server's wiring banner.
pub const CRATE: &str = "epiphany-engine";

/// A monotonic commit version: the global id assigned to a cube's most recent
/// commit (0 before any commit). Versions are globally ordered across cubes, so
/// a single cube's versions need not be contiguous.
pub type Version = u64;

/// The outcome of a successful commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitOutcome {
    /// The committed cube's new version (also the global commit id).
    pub version: Version,
}

/// A commit event emitted from inside the engine's commit path (E4), carrying the
/// cube that changed and the new global [`Version`] assigned to it. The version
/// is the global commit id (minted from the shared, monotonic counter), so a
/// consumer can totally-order events across cubes by `version` even though the
/// per-cube writer locks fire independently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitEvent {
    /// The cube whose state advanced.
    pub cube: String,
    /// The new global commit version published for that cube.
    pub version: Version,
    /// The sandbox this commit wrote into, when it was a private what-if write
    /// (`sandbox_set_cells`); `None` for a base-cube commit (a base write, a
    /// definition op, a structural edit, a create, or a sandbox *commit* that
    /// applies overrides to the base). Lets an observer route a private
    /// sandbox-write notification only to the sandbox's owner while a base commit
    /// is delivered to every cube reader (the API change feed relies on this to
    /// preserve ADR-0014 sandbox privacy while ordering events in commit order).
    pub sandbox: Option<String>,
}

/// An additive seam for observing commits in order (E4). The engine invokes
/// [`on_commit`](CommitObserver::on_commit) from inside the commit path, right
/// after a new version is published and while the cube's writer lock is still
/// held — so two commits to the *same* cube are observed strictly in version
/// order. Across cubes, order events by [`CommitEvent::version`] (the global
/// commit id). The default engine has no observer, so this changes no existing
/// behavior; the API layer subscribes later to fix change-feed ordering. The
/// callback must be cheap and non-blocking (it runs under the writer lock); a
/// real consumer forwards the event to its own queue/broadcast and returns.
pub trait CommitObserver: Send + Sync {
    /// Called once per successful commit, in per-cube version order, before the
    /// committing call returns to its caller.
    fn on_commit(&self, event: &CommitEvent);
}

/// Why a batch did not commit. On any variant the cube is left unchanged.
#[derive(Debug)]
pub enum BatchError {
    /// No cube by that name.
    UnknownCube(String),
    /// The supplied base version did not match the cube's current version
    /// (a concurrent commit won the race); the batch was not applied.
    Conflict { expected: Version, actual: Version },
    /// A write was rejected by the model; the batch was not applied.
    Rejected { index: usize, source: ModelError },
    /// A subset/view definition was structurally invalid; nothing was changed.
    Invalid(QueryError),
    /// A create named a cube that already exists (ADR-0021); nothing was changed.
    AlreadyExists(String),
    /// The operation is not available on this engine (e.g. cube creation when no
    /// on-disk root was configured); nothing was changed.
    Unsupported(String),
    /// Durably logging the batch failed.
    Persist(PersistError),
}

impl std::fmt::Display for BatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BatchError::UnknownCube(name) => write!(f, "unknown cube '{name}'"),
            BatchError::Conflict { expected, actual } => write!(
                f,
                "version conflict: batch staged on version {expected} but the cube is at {actual}"
            ),
            BatchError::Rejected { index, source } => {
                write!(f, "batch write {index} rejected: {source}")
            }
            BatchError::Invalid(e) => write!(f, "invalid definition: {e}"),
            BatchError::AlreadyExists(name) => write!(f, "cube '{name}' already exists"),
            BatchError::Unsupported(what) => write!(f, "operation not supported: {what}"),
            BatchError::Persist(e) => write!(f, "could not persist batch: {e}"),
        }
    }
}

impl std::error::Error for BatchError {}

/// One dimension of a cube being created: either an inline definition or a
/// reference to a registered shared dimension materialized at create time
/// (ADR-0024 v1).
#[derive(Debug, Clone)]
pub enum CubeDimensionSpec {
    /// A cube-local dimension defined inline.
    Inline(DimensionDef),
    /// A reference to a registered shared dimension, by id.
    Ref(DimensionId),
}

/// Why a shared-dimension library operation failed.
#[derive(Debug)]
pub enum DimensionError {
    /// No registered dimension by that id.
    Unknown(DimensionId),
    /// The dimension is still referenced by these cubes, so it cannot be deleted.
    Referenced(Vec<String>),
    /// Durably persisting the registry after the change failed (E3). The
    /// in-memory registry is left unchanged (the delete is not published), so a
    /// caller sees a clean failure instead of a silent divergence from disk.
    Persist(PersistError),
}

impl std::fmt::Display for DimensionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DimensionError::Unknown(id) => write!(f, "unknown shared dimension #{}", id.0),
            DimensionError::Referenced(cubes) => write!(
                f,
                "shared dimension is referenced by {} cube(s): {}",
                cubes.len(),
                cubes.join(", ")
            ),
            DimensionError::Persist(e) => {
                write!(f, "could not persist the dimension registry: {e}")
            }
        }
    }
}

impl std::error::Error for DimensionError {}

/// Why promoting a cube's embedded dimension into the global registry failed
/// (ADR-0031 Phase 0/1).
#[derive(Debug)]
pub enum PromoteError {
    /// No cube by that name (or it is not readable).
    UnknownCube(String),
    /// The cube has no dimension by that name.
    UnknownDimension { cube: String, dimension: String },
    /// The dimension is already a global (registry-backed) dimension for this
    /// cube, so there is nothing to promote.
    AlreadyGlobal(DimensionId),
    /// Durably persisting the registry after the promotion failed (E3). The
    /// minted identity is not published, so the caller never holds a global
    /// dimension id whose durable home was silently lost.
    Persist(PersistError),
}

impl std::fmt::Display for PromoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromoteError::UnknownCube(cube) => write!(f, "unknown cube '{cube}'"),
            PromoteError::UnknownDimension { cube, dimension } => {
                write!(f, "cube '{cube}' has no dimension '{dimension}'")
            }
            PromoteError::AlreadyGlobal(id) => {
                write!(f, "dimension is already a global dimension (#{})", id.0)
            }
            PromoteError::Persist(e) => {
                write!(f, "could not persist the dimension registry: {e}")
            }
        }
    }
}

impl std::error::Error for PromoteError {}

/// Flatten a `DimensionDef` into the `ElementSpec`/`EdgeSpec` lists `define_elements`
/// expects, stamping each with the dimension's name (ADR-0024 fan-out/reconcile).
fn def_to_specs(def: &DimensionDef) -> (Vec<ElementSpec>, Vec<EdgeSpec>) {
    let elements = def
        .elements
        .iter()
        .map(|(name, kind)| ElementSpec {
            dimension: def.name.clone(),
            name: name.clone(),
            kind: *kind,
        })
        .collect();
    let edges = def
        .edges
        .iter()
        .map(|(parent, child, weight)| EdgeSpec {
            dimension: def.name.clone(),
            parent: parent.clone(),
            child: child.clone(),
            weight: *weight,
        })
        .collect();
    (elements, edges)
}

/// One published, immutable model version (cube plus its subsets and views).
#[derive(Debug)]
struct Published {
    version: Version,
    model: Model,
}

/// A lock-free, immutable read snapshot of one cube's model. Holding it pins a
/// single committed version (cube, subsets, and views) for the life of a query;
/// concurrent commits never mutate it.
#[derive(Debug, Clone)]
pub struct ReadSnapshot {
    inner: Arc<Published>,
}

impl ReadSnapshot {
    /// The pinned cube version.
    pub fn cube(&self) -> &Cube {
        &self.inner.model.cube
    }

    /// The pinned model (cube plus its named subsets and views).
    pub fn model(&self) -> &Model {
        &self.inner.model
    }

    /// A subset in this snapshot, by dimension and name.
    pub fn subset(&self, dimension: &str, name: &str) -> Option<&Subset> {
        self.inner.model.subset(dimension, name)
    }

    /// A view in this snapshot, by name.
    pub fn view(&self, name: &str) -> Option<&View> {
        self.inner.model.view(name)
    }

    /// The cube's rules in this snapshot (opaque source text).
    pub fn rules(&self) -> &RuleSet {
        &self.inner.model.rules
    }

    /// The rule unit tests in this snapshot, keyed by name.
    pub fn tests(&self) -> &BTreeMap<String, RuleTest> {
        &self.inner.model.tests
    }

    /// A rule test in this snapshot, by name.
    pub fn test(&self, name: &str) -> Option<&RuleTest> {
        self.inner.model.tests.get(name)
    }

    /// The version this snapshot pins.
    pub fn version(&self) -> Version {
        self.inner.version
    }
}

/// Per-cube writer state, guarded by a mutex so commits are serialized (one
/// linearization point, which makes version assignment deterministic).
#[derive(Debug)]
struct Writer {
    store: Store,
    version: Version,
    /// Set when a durability failure left the on-disk snapshot/WAL relationship
    /// unverified AND the recovery checkpoint (which would rewrite both from the
    /// last published model) also failed. Recovery posture: reads keep serving
    /// the last published version (it is immutable and already replicated to
    /// readers), while every write to this cube is rejected with a persist
    /// error — accepting one could append WAL records that replay onto a
    /// different on-disk element order after a crash (wrong cells). Cleared by
    /// a later successful [`Engine::checkpoint`] (which re-establishes
    /// snapshot + WAL == published) or by reopening the store (restart).
    fail_stopped: bool,
}

/// Per-cube shared state: the serialized writer plus the lock-free published version.
struct CubeState {
    writer: Mutex<Writer>,
    published: ArcSwap<Published>,
}

/// Lock one of the engine's coarse topology mutexes (`topology`,
/// `dim_topology`), recovering from a poisoned lock instead of panicking. The
/// guarded value is `()` and every structure mutated under these locks is
/// staged on a clone and published atomically via `ArcSwap` (readers always
/// see a consistent old-or-new value), so a panic under the lock cannot leave
/// partial state behind: recovery is safe, and it keeps dimension and cube
/// topology operations available instead of turning every later call into a
/// cascade of panics.
fn lock_coarse(mutex: &Mutex<()>) -> MutexGuard<'_, ()> {
    mutex.lock().unwrap_or_else(|poisoned| {
        mutex.clear_poison();
        poisoned.into_inner()
    })
}

/// Lock a cube's writer, recovering from a poisoned lock instead of panicking.
///
/// A panic mid-commit (a bug in a core op, an allocation failure during the
/// model clone) may have left the store's in-memory model and the writer's
/// version mid-mutation. `published` is only ever updated on success, so it is
/// the engine's known-good state: recovery resynchronizes the writer's version
/// and the store's in-memory model from it, then re-checkpoints so the on-disk
/// snapshot/WAL match it too (the panicked op may have already checkpointed a
/// state that was never published). If that resync checkpoint fails the writer
/// is fail-stopped (see [`Writer::fail_stopped`]) rather than left pointing at
/// unverified disk state. Either way the caller gets a usable guard and a
/// structured error path instead of a poisoned-mutex panic on every write.
fn lock_writer(state: &CubeState) -> MutexGuard<'_, Writer> {
    match state.writer.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            state.writer.clear_poison();
            let mut writer = poisoned.into_inner();
            let published = state.published.load();
            writer.version = published.version;
            writer.store.restore_model(published.model.clone());
            if writer.store.checkpoint().is_err() {
                writer.fail_stopped = true;
            }
            writer
        }
    }
}

/// The error a fail-stopped cube writer returns for every write until it heals
/// (see [`Writer::fail_stopped`]): reads keep serving the last published
/// version; a successful [`Engine::checkpoint`] or a store reopen recovers.
fn fail_stopped_error(cube: &str) -> BatchError {
    BatchError::Persist(PersistError::Io(std::io::Error::other(format!(
        "cube '{cube}' is fail-stopped: a durability failure left its on-disk \
         state unverified and the recovery checkpoint failed; reads continue \
         from the last published version, writes are rejected until a \
         checkpoint succeeds or the server restarts"
    ))))
}

/// The engine: a set of named, durably-backed cubes with snapshot-isolation reads
/// and atomic batch commits. Cheap to clone (shares one inner state) and
/// `Send + Sync` for sharing across request handlers.
///
/// The cube *set* is itself an immutable `BTreeMap` behind an `ArcSwap`
/// (ADR-0021 extends ADR-0001): reads do one lock-free atomic load, and creating
/// a cube swaps in a copy-on-write map under the coarse `topology` lock without
/// blocking reads or per-cube commits.
#[derive(Clone)]
pub struct Engine {
    cubes: Arc<ArcSwap<BTreeMap<String, Arc<CubeState>>>>,
    ids: Arc<IdGen>,
    /// On-disk root (`<data_dir>/cubes`) for new cube stores. `None` disables
    /// cube creation (e.g. embedded/test engines built without a directory).
    cubes_dir: Option<PathBuf>,
    /// Serializes cube create/registration so concurrent creates cannot lose a
    /// cube. Per-cube commits never take this lock.
    topology: Arc<Mutex<()>>,
    /// The shared-dimension registry (ADR-0024, Phase 0): a server-level set of
    /// dimensions cubes will reference by id. Held behind an `ArcSwap` like the
    /// cube set; mutated under `dim_topology`. Additive and not yet wired into the
    /// live read/commit path.
    dimensions: Arc<ArcSwap<DimensionRegistry>>,
    /// Serializes the *registry read-modify-write* (clone → durable persist →
    /// ArcSwap publish). The lock order is `dim_topology` before any per-cube
    /// `writer` (ADR-0024) to preclude an AB/BA deadlock. Held only briefly for the
    /// registry swap itself; the expensive per-cube fan-out repacks run *outside*
    /// it, serialized instead by [`registry_edit`](Self::registry_edit) (E2).
    dim_topology: Arc<Mutex<()>>,
    /// Serializes the whole of a shared-dimension **grow / edit** (registry swap +
    /// the fan-out to every referencing cube), so two structural fan-outs to the
    /// same dimension never interleave and apply out of order (consistency, E2).
    /// It is the OUTERMOST registry lock: the total order is `registry_edit` →
    /// `dim_topology` → per-cube `writer`, never the reverse, so no cycle. Because
    /// grow/edit release `dim_topology` before their fan-out (holding only
    /// `registry_edit` across the per-cube fsyncs), other registry ops that need
    /// just the brief `dim_topology` — `register_dimension`, `attach_dimension`,
    /// `create_cube_with_refs`, `delete_dimension`, `promote_cube_dimension` — are
    /// no longer blocked for the entire fan-out, shrinking the reader-visible
    /// critical section while preserving the ADR-0024 lock order and the
    /// durable-registry-write-before-repack invariant.
    registry_edit: Arc<Mutex<()>>,
    /// On-disk root (`<data_dir>/dimensions`) for the durable registry. `None`
    /// keeps the registry in memory only (e.g. tests without a directory).
    dimensions_dir: Option<PathBuf>,
    /// Stable, monotonic source of `DimensionId`s, seeded past the max loaded id
    /// on `with_dimensions_dir` so ids never collide across restarts (the commit
    /// `IdGen` restarts each boot, so dimension ids use this separate counter).
    next_dim_id: Arc<AtomicU64>,
    /// Data directory for the durable commit high-water mark (E1). When set, every
    /// commit records its freshly-minted version here (fsynced) BEFORE publishing,
    /// so a restart seeds the version counter past it and versions strictly
    /// increase across restarts (no ABA reuse of a `base_version`/cache key). The
    /// composition root seeds the shared [`IdGen`] from this mark, then sets the
    /// dir so subsequent commits keep it current. `None` (tests, embedded engines)
    /// keeps versions in memory only.
    commit_watermark_dir: Option<PathBuf>,
    /// Serializes durable high-water writes so two cubes committing concurrently
    /// cannot race the watermark file (each commit already advances the shared,
    /// atomic [`IdGen`], so versions are globally ordered; this lock only guards
    /// the tiny file write, never the commit's data path).
    commit_watermark_lock: Arc<Mutex<()>>,
    /// Optional commit observer (E4): invoked in per-cube version order from the
    /// commit path, after publish. Empty by default (no behavior change); the API
    /// injects one to emit an ordered change feed. Held in a SHARED, swappable slot
    /// so registering it on any `Engine` handle takes effect for every clone
    /// (including handles cloned before registration) — the composition root clones
    /// the engine into `AppState` and the scheduler before the API installs the
    /// observer, and the commit that fires the hook may come through any of those
    /// clones. Read once per commit under a tiny mutex (just an `Arc` clone, far
    /// cheaper than the commit's own fsync and uncontended vs the lock-free reads).
    commit_observer: Arc<Mutex<Option<Arc<dyn CommitObserver>>>>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("cubes", &self.cube_names())
            .finish()
    }
}

impl Engine {
    /// Build an engine from already-opened stores, sharing one id generator so
    /// commit versions are globally ordered across cubes.
    pub fn from_stores(stores: BTreeMap<String, Store>, ids: Arc<IdGen>) -> Self {
        let cubes: BTreeMap<String, Arc<CubeState>> = stores
            .into_iter()
            .map(|(name, store)| {
                let published = ArcSwap::from_pointee(Published {
                    version: 0,
                    model: store.model().clone(),
                });
                let state = Arc::new(CubeState {
                    writer: Mutex::new(Writer {
                        store,
                        version: 0,
                        fail_stopped: false,
                    }),
                    published,
                });
                (name, state)
            })
            .collect();
        Engine {
            cubes: Arc::new(ArcSwap::from_pointee(cubes)),
            ids,
            cubes_dir: None,
            topology: Arc::new(Mutex::new(())),
            dimensions: Arc::new(ArcSwap::from_pointee(DimensionRegistry::default())),
            dim_topology: Arc::new(Mutex::new(())),
            registry_edit: Arc::new(Mutex::new(())),
            dimensions_dir: None,
            next_dim_id: Arc::new(AtomicU64::new(1)),
            commit_watermark_dir: None,
            commit_watermark_lock: Arc::new(Mutex::new(())),
            commit_observer: Arc::new(Mutex::new(None)),
        }
    }

    /// Enable durable shared dimensions by loading the registry from `dir`
    /// (`<data_dir>/dimensions`) and persisting future mutations there (ADR-0024,
    /// SD-2). On load it seeds the dimension-id counter past the max stored id (so
    /// new ids never collide across restarts) and reconciles every referencing
    /// cube forward to the loaded dimension (idempotent append), so a cube that
    /// missed a fan-out before a crash catches up.
    ///
    /// # Panics
    ///
    /// Panics if the registry is present but unreadable, or if a referencing
    /// cube cannot be reconciled — see
    /// [`try_with_dimensions_dir`](Self::try_with_dimensions_dir) for why boot
    /// must fail loudly here rather than continue. Use the `try_` variant to
    /// handle the error instead.
    pub fn with_dimensions_dir(self, dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        match self.try_with_dimensions_dir(dir.clone()) {
            Ok(engine) => engine,
            Err(e) => panic!(
                "failed to bring up the shared-dimension registry from '{}': {e}",
                dir.display()
            ),
        }
    }

    /// Fallible form of [`with_dimensions_dir`](Self::with_dimensions_dir).
    ///
    /// Fail-closed: an *absent* registry is an empty one (first run), but a
    /// registry that is present and unreadable (corrupt `index.toml`, a listed
    /// `<id>.model` body missing or unparsable) is an error, never silently
    /// substituted with an empty registry — the registry is the durable
    /// authority for shared-dimension identity (ADR-0024), and booting with an
    /// empty substitute would quarantine every dimension body on the next
    /// save, drop every cube reference set (silently stopping fan-out), and
    /// re-mint `DimensionId`s that were already handed out. A failed reconcile
    /// of a referencing cube is likewise an error: reconcile is the mechanism
    /// ADR-0024 relies on to keep a cube in lockstep with its shared
    /// dimension, so a cube that cannot be brought forward would silently
    /// serve rollups that diverge from every sibling cube.
    pub fn try_with_dimensions_dir(
        mut self,
        dir: impl Into<PathBuf>,
    ) -> Result<Self, PersistError> {
        let dir = dir.into();
        let entries = load_registry(&dir)?;
        let mut registry = DimensionRegistry::default();
        let mut max_id = 0u64;
        let mut reconcile: Vec<(String, Vec<ElementSpec>, Vec<EdgeSpec>)> = Vec::new();
        for entry in entries {
            max_id = max_id.max(entry.id);
            let shared = Arc::new(SharedDimension {
                id: DimensionId(entry.id),
                generation: entry.generation,
                dimension: entry.dimension,
            });
            // Stage the fan-out reconcile inputs while we still hold the entry.
            let def = shared.to_dimension_def();
            let (els, edgs) = def_to_specs(&def);
            registry.put(shared);
            for cube in &entry.references {
                registry.attach(DimensionId(entry.id), cube);
                reconcile.push((cube.clone(), els.clone(), edgs.clone()));
            }
        }
        self.next_dim_id.store(max_id + 1, Ordering::SeqCst);
        self.dimensions.store(Arc::new(registry));
        self.dimensions_dir = Some(dir);
        // Bring any cube that lagged a fan-out forward (idempotent, append-only).
        // A failure here means the cube genuinely diverged from the registry (or
        // its store is failing); surface it rather than boot a silently
        // inconsistent server.
        for (cube, els, edgs) in reconcile {
            if self.has_cube(&cube) {
                if let Err(e) = self.define_elements(&cube, None, &els, &edgs) {
                    return Err(match e {
                        BatchError::Persist(e) => e,
                        e => PersistError::Corrupt(format!(
                            "cube '{cube}' could not be reconciled to its shared \
                             dimension: {e}"
                        )),
                    });
                }
            }
        }
        Ok(self)
    }

    /// Enable runtime cube creation by telling the engine where to create new
    /// cube stores on disk (ADR-0021). The directory matches the boot layout
    /// (`<data_dir>/cubes/<name>/`), so a created cube reloads on restart.
    pub fn with_cubes_dir(mut self, cubes_dir: impl Into<PathBuf>) -> Self {
        self.cubes_dir = Some(cubes_dir.into());
        self
    }

    /// Persist the durable commit high-water mark under `dir` (E1). Once set,
    /// every commit records its minted version here (fsynced) *before* it is
    /// published, so no restart can reissue an already-observable version.
    ///
    /// The caller MUST first seed the shared [`IdGen`] past the existing mark
    /// (see [`read_commit_watermark`](epiphany_persist::read_commit_watermark) and
    /// [`IdGen::starting_at`]) so the counter continues above the last durable
    /// version; this method only keeps the mark current from then on. Without it
    /// (tests, embedded engines) versions live in memory only and restart at the
    /// counter's seed.
    pub fn with_commit_watermark_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.commit_watermark_dir = Some(dir.into());
        self
    }

    /// Durably record `version` as the commit high-water mark (E1), if a watermark
    /// directory is configured. Called on the commit path AFTER a version is
    /// minted and BEFORE it is published, so a crash can never leave a published
    /// version above the durable mark. Monotonic and idempotent at the persist
    /// layer, so an out-of-order advance (two cubes racing) never moves it
    /// backwards. A persist failure is propagated as [`BatchError::Persist`]: the
    /// caller must not publish a version whose high-water could not be recorded,
    /// or a later restart might alias it (the minted number is simply skipped,
    /// leaving a harmless gap). A no-op when no directory is set.
    fn record_commit_version(&self, version: Version) -> Result<(), BatchError> {
        let Some(dir) = self.commit_watermark_dir.as_ref() else {
            return Ok(());
        };
        let _guard = lock_coarse(&self.commit_watermark_lock);
        epiphany_persist::write_commit_watermark(dir, version).map_err(BatchError::Persist)
    }

    /// Register a commit observer (E4). The engine invokes it in per-cube version
    /// order from inside the commit path (after publish), so a consumer can build
    /// an ordered change feed. Additive: without this the engine emits nothing and
    /// behaves exactly as before. Replaces any previously-set observer.
    pub fn with_commit_observer(self, observer: Arc<dyn CommitObserver>) -> Self {
        // The slot is SHARED across clones, so this registration is visible to every
        // handle (the composition root cloned the engine into `AppState` and the
        // scheduler before installing the observer). Replaces any previous observer.
        *self.commit_observer.lock().expect("commit observer mutex") = Some(observer);
        self
    }

    /// Emit a commit event to the registered observer (E4), if any. Called from
    /// the commit path after a version is published, while the cube's writer lock
    /// is still held, so same-cube commits are observed in version order. A no-op
    /// when no observer is set (the default), so existing behavior is unchanged.
    fn notify_commit(&self, cube: &str, version: Version, sandbox: Option<&str>) {
        // Clone the shared observer out under a tiny lock, then invoke it OUTSIDE the
        // guard; `None` (the default) is a cheap no-op, so a commit costs nothing
        // extra when no feed is installed.
        let observer = self
            .commit_observer
            .lock()
            .expect("commit observer mutex")
            .clone();
        if let Some(observer) = observer {
            observer.on_commit(&CommitEvent {
                cube: cube.to_string(),
                version,
                sandbox: sandbox.map(str::to_string),
            });
        }
    }

    /// Look up a cube's shared state, cloning the `Arc` so callers can drop the
    /// map guard before doing work (and so per-cube commits never pin the map).
    fn state(&self, cube: &str) -> Option<Arc<CubeState>> {
        self.cubes.load().get(cube).cloned()
    }

    // ---- shared-dimension registry (ADR-0024, Phase 0) ----
    //
    // Additive: these manage the server-level dimension registry but are not yet
    // consulted by the live read/commit path (cubes still own their dimensions).
    // Phase 1 makes cubes reference the registry and threads a pinned snapshot
    // through reads.

    /// A lock-free snapshot of the shared-dimension registry.
    pub fn dimension_registry(&self) -> Arc<DimensionRegistry> {
        self.dimensions.load_full()
    }

    /// Register a new shared dimension and return its server-unique, restart-stable
    /// id. Mints the id from the dedicated dimension counter, copy-on-write swaps
    /// it into the registry under `dim_topology`, and persists. Not yet referenced
    /// by any cube.
    ///
    /// Fail-closed (ADR-0024: the registry is the durable authority): the durable
    /// registry write happens *before* the new dimension is published, so a
    /// persist failure returns [`BatchError::Persist`] with the registry
    /// unchanged — the caller never holds a minted id whose durable home was
    /// silently lost (the unused id leaves a harmless gap in the sequence).
    pub fn register_dimension(&self, dimension: Dimension) -> Result<DimensionId, BatchError> {
        let _topo = lock_coarse(&self.dim_topology);
        let id = DimensionId(self.next_dim_id.fetch_add(1, Ordering::SeqCst));
        let shared = Arc::new(SharedDimension::new(id, dimension));
        let mut next = (**self.dimensions.load()).clone();
        next.put(shared);
        self.persist_registry_state(&next)
            .map_err(BatchError::Persist)?;
        self.dimensions.store(Arc::new(next));
        Ok(id)
    }

    /// Build a shared dimension from a [`DimensionDef`] through the same validated
    /// element/edge path used to grow one (kind conflicts, parent-must-be-
    /// consolidated, edge-weight conflicts, no cycles), then register it. A
    /// rejected definition returns the model error without touching the registry.
    pub fn register_dimension_def(&self, def: &DimensionDef) -> Result<DimensionId, BatchError> {
        let (elements, edges) = def_to_specs(def);
        let built = SharedDimension::new(DimensionId(0), Dimension::new(&def.name))
            .grown(&elements, &edges)
            .map_err(|e| BatchError::Invalid(QueryError::Model(e)))?;
        self.register_dimension(built.dimension)
    }

    /// Record that `cube` references shared dimension `id` (ADR-0024 v1): the cube
    /// has materialized a copy of it, and a later grow fans out to the cube.
    ///
    /// Fail-closed like [`register_dimension`](Self::register_dimension): the
    /// reference is durably persisted before it is published, so a persist
    /// failure returns [`BatchError::Persist`] with the registry unchanged (a
    /// silently lost reference would stop future grows/edits from fanning out
    /// to the cube).
    pub fn attach_dimension(&self, id: DimensionId, cube: &str) -> Result<(), BatchError> {
        let _topo = lock_coarse(&self.dim_topology);
        let mut next = (**self.dimensions.load()).clone();
        next.attach(id, cube);
        self.persist_registry_state(&next)
            .map_err(BatchError::Persist)?;
        self.dimensions.store(Arc::new(next));
        Ok(())
    }

    /// Durably persist `registry` to `dimensions_dir` (ADR-0024: the durable
    /// registry write happens *before* the new state is published or fanned
    /// out, so the registry on disk is always the authority). Callers stage the
    /// next registry on a clone, persist it here, and only publish on success.
    /// An engine without a dimensions dir (tests, embedded engines) keeps the
    /// registry in memory only and always succeeds. Each save rewrites the
    /// whole registry, so a later successful save also heals any earlier
    /// best-effort failure.
    fn persist_registry_state(&self, registry: &DimensionRegistry) -> Result<(), PersistError> {
        let Some(dir) = self.dimensions_dir.as_ref() else {
            return Ok(());
        };
        let entries: Vec<RegistryEntry> = registry
            .all()
            .into_iter()
            .map(|shared| RegistryEntry {
                id: shared.id.0,
                generation: shared.generation,
                references: registry.referencing(shared.id),
                dimension: shared.dimension.clone(),
            })
            .collect();
        save_registry(dir, &entries)
    }

    /// Append elements/edges to a registered shared dimension, publish the new
    /// generation, and **fan the same append out to every referencing cube**
    /// (ADR-0024 v1: materialized references). The registry grow is the
    /// authoritative event; per-cube application reuses the append-only,
    /// idempotent `define_elements` path keyed by the dimension's name, so every
    /// referencing cube converges to the grown dimension. A rejected change leaves
    /// the registry untouched.
    ///
    /// Locking (E2): the whole grow is serialized by `registry_edit`, but the
    /// `dim_topology` critical section is shrunk to just the registry read-modify-
    /// write (build → durable persist → ArcSwap publish). The per-cube fan-out
    /// repacks then run WITHOUT `dim_topology` held (only `registry_edit`), so they
    /// no longer block a concurrent `register_dimension`/`attach`/create-with-refs
    /// that needs only the brief `dim_topology`. The durable registry write still
    /// precedes any repack (ADR-0024), and `registry_edit` keeps two fan-outs from
    /// interleaving; the lock order `registry_edit` → `dim_topology` → `writer` is
    /// acyclic.
    pub fn grow_dimension(
        &self,
        id: DimensionId,
        elements: &[ElementSpec],
        edges: &[EdgeSpec],
    ) -> Result<u64, BatchError> {
        // Outermost: serialize the whole grow (registry swap + fan-out) so no two
        // structural fan-outs to the registry interleave.
        let _edit = lock_coarse(&self.registry_edit);

        // Registry read-modify-write under the SHRUNK dim_topology section. Publish
        // the new generation durably, then release dim_topology before the fan-out.
        let (generation, dim_name, referrers) = {
            let _topo = lock_coarse(&self.dim_topology);
            let snapshot = self.dimensions.load_full();
            let current = snapshot.get(id).cloned().ok_or_else(|| {
                BatchError::Invalid(QueryError::Model(ModelError::UnknownDimension {
                    cube: "registry".to_string(),
                    dimension: format!("#{}", id.0),
                }))
            })?;
            let grown = current
                .grown(elements, edges)
                .map_err(|e| BatchError::Invalid(QueryError::Model(e)))?;
            let generation = grown.generation;
            let dim_name = grown.dimension.name().to_string();
            // Durably persist, then publish, the new registry generation (the
            // authoritative event; the durable write comes first, fail-closed, per
            // ADR-0024 — a persist failure leaves the registry untouched).
            let mut next = (**self.dimensions.load()).clone();
            next.put(Arc::new(grown));
            self.persist_registry_state(&next)
                .map_err(BatchError::Persist)?;
            self.dimensions.store(Arc::new(next));
            (generation, dim_name, snapshot.referencing(id))
        };

        // Fan out to every referencing cube WITHOUT dim_topology held (still under
        // registry_edit), re-stamping the dimension name so the append targets each
        // cube's materialized copy. The append is idempotent and forward-only, so a
        // cube already at this generation is a no-op.
        let els: Vec<ElementSpec> = elements
            .iter()
            .map(|e| ElementSpec {
                dimension: dim_name.clone(),
                name: e.name.clone(),
                kind: e.kind,
            })
            .collect();
        let edgs: Vec<EdgeSpec> = edges
            .iter()
            .map(|e| EdgeSpec {
                dimension: dim_name.clone(),
                parent: e.parent.clone(),
                child: e.child.clone(),
                weight: e.weight,
            })
            .collect();
        for cube in referrers {
            if self.has_cube(&cube) {
                self.define_elements(&cube, None, &els, &edgs)?;
            }
        }
        Ok(generation)
    }

    /// Apply a structural edit (ADR-0036) to a cube's dimension, remapping stored
    /// cells transactionally. If the dimension is registry-backed for this cube,
    /// the edit applies to the registry generation **and fans the same
    /// name-addressed edit out to every referencing cube** (mirroring
    /// [`grow_dimension`](Self::grow_dimension)), so every materialized copy and
    /// its cells stay consistent. A cube-embedded (non-registry) dimension edits
    /// only that cube. Holds `dim_topology` before any per-cube `writer`, the
    /// ADR-0024 lock order.
    ///
    /// A rejected edit (not a permutation, a cycle, a child-bearing delete, a
    /// duplicate insert) leaves that target unchanged: the registry edit is staged
    /// on a clone and validated before publish, and each per-cube edit is itself
    /// transactional.
    ///
    /// Cross-cube atomicity is best-effort, matching
    /// [`grow_dimension`](Self::grow_dimension): the registry generation is
    /// published first, then each referencing cube is edited in turn, propagating
    /// the first error without rolling back cubes already edited. In normal
    /// operation every referencing cube is materialized identically from the
    /// registry and stays in lockstep, so a name-addressed edit applies cleanly to
    /// each; a per-cube failure (e.g. a cube whose member set has somehow diverged)
    /// can leave a partial result. A two-phase validate-all-cubes-before-publish is
    /// a deferred hardening (ADR-0036).
    pub fn edit_dimension(
        &self,
        cube: &str,
        dim_name: &str,
        edit: &DimensionEdit,
    ) -> Result<CommitOutcome, BatchError> {
        // Outermost: serialize the whole edit (registry swap + fan-out) so two
        // structural fan-outs to the same dimension never interleave and apply the
        // (non-commutative) edits out of order across cubes (consistency, E2).
        let _edit = lock_coarse(&self.registry_edit);

        // Resolve backing and, if registry-backed, do the registry read-modify-
        // write under the SHRUNK dim_topology section, then release dim_topology
        // before the fan-out. `referrers` is `None` for a cube-embedded dimension.
        let referrers: Option<Vec<String>> = {
            let _topo = lock_coarse(&self.dim_topology);
            match self.dimensions.load().backing_of(cube, dim_name) {
                Some(id) => {
                    // Validate the registry edit first; a rejection touches nothing.
                    let snapshot = self.dimensions.load_full();
                    let current = snapshot.get(id).cloned().ok_or_else(|| {
                        BatchError::Invalid(QueryError::Model(ModelError::UnknownDimension {
                            cube: "registry".to_string(),
                            dimension: format!("#{}", id.0),
                        }))
                    })?;
                    let edited = current
                        .edited(edit)
                        .map_err(|e| BatchError::Invalid(QueryError::Model(e)))?;
                    // Durably persist, then publish, the new registry generation
                    // (the authoritative event; durable write first, fail-closed,
                    // per ADR-0024 — a persist failure leaves the registry
                    // untouched).
                    let mut next = (**self.dimensions.load()).clone();
                    next.put(Arc::new(edited));
                    self.persist_registry_state(&next)
                        .map_err(BatchError::Persist)?;
                    self.dimensions.store(Arc::new(next));
                    Some(snapshot.referencing(id))
                }
                None => None,
            }
        };

        let Some(referrers) = referrers else {
            // Cube-embedded dimension: edit only this cube (no registry, no fan-out).
            return self.apply_dimension_edit_to_cube(cube, dim_name, edit);
        };

        // Fan the same name-addressed edit out to every referencing cube WITHOUT
        // dim_topology held (still serialized by registry_edit); each cube remaps
        // its own cells. The requesting cube is included in the referrer set, so it
        // is edited here too. Return the outcome of the cube the CALLER named (not
        // whichever referrer the sorted fan-out visited last): the caller feeds this
        // version back into its optimistic base-version bookkeeping for that cube.
        let mut requested = None;
        for referrer in referrers {
            if self.has_cube(&referrer) {
                let outcome = self.apply_dimension_edit_to_cube(&referrer, dim_name, edit)?;
                if referrer == cube {
                    requested = Some(outcome);
                }
            }
        }
        // If the cube somehow is not in the referrer set, edit it directly so the
        // caller still gets a committed version for it.
        match requested {
            Some(outcome) => Ok(outcome),
            None => self.apply_dimension_edit_to_cube(cube, dim_name, edit),
        }
    }

    /// Apply one structural edit to a single named cube's dimension through the
    /// shared commit path (clone the store's cube, remap, publish a new version).
    fn apply_dimension_edit_to_cube(
        &self,
        cube: &str,
        dim_name: &str,
        edit: &DimensionEdit,
    ) -> Result<CommitOutcome, BatchError> {
        let edit = edit.clone();
        let dim_name = dim_name.to_string();
        self.define(cube, None, move |store| {
            store.edit_dimension(&dim_name, &edit)
        })
    }

    /// Create a cube whose dimensions mix inline definitions and references to
    /// registered shared dimensions (ADR-0024 v1). Each [`CubeDimensionSpec::Ref`]
    /// is materialized from the registry at its current generation, and the new
    /// cube is recorded as a referrer so a later [`grow_dimension`](Self::grow_dimension)
    /// fans out to it. Atomic against a concurrent grow: it holds `dim_topology`
    /// across the materialize, the create, and the reference attach. An unknown
    /// referenced id (or any cube-build/registration error) leaves the registry
    /// untouched.
    pub fn create_cube_with_refs(
        &self,
        name: &str,
        dims: &[CubeDimensionSpec],
    ) -> Result<CommitOutcome, BatchError> {
        let _topo = lock_coarse(&self.dim_topology);
        let registry = self.dimensions.load_full();

        // Resolve every reference to a materialized def before creating anything,
        // so an unknown id fails the whole create with the registry untouched.
        let mut defs = Vec::with_capacity(dims.len());
        let mut refs = Vec::new();
        for spec in dims {
            match spec {
                CubeDimensionSpec::Inline(def) => defs.push(def.clone()),
                CubeDimensionSpec::Ref(id) => {
                    let shared = registry.get(*id).ok_or_else(|| {
                        BatchError::Invalid(QueryError::Model(ModelError::UnknownDimension {
                            cube: name.to_string(),
                            dimension: format!("#{}", id.0),
                        }))
                    })?;
                    defs.push(shared.to_dimension_def());
                    refs.push(*id);
                }
            }
        }

        // create_cube takes `topology` (order: dim_topology -> topology, never the
        // reverse), validates, persists, and publishes the cube.
        let outcome = self.create_cube(name, &defs)?;

        // Record the cube as a referrer of each shared dimension it materialized
        // (durable write before publish, fail-closed like `attach_dimension`). If
        // the persist fails the cube itself stays created and readable — undoing
        // a durable create is riskier than the gap — but the error tells the
        // caller the references were NOT recorded, so grows will not fan out to
        // this cube until it is re-attached.
        if !refs.is_empty() {
            let mut next = (**self.dimensions.load()).clone();
            for id in &refs {
                next.attach(*id, name);
            }
            self.persist_registry_state(&next)
                .map_err(BatchError::Persist)?;
            self.dimensions.store(Arc::new(next));
        }
        Ok(outcome)
    }

    /// Delete a shared dimension from the registry. Fail-closed: a dimension still
    /// referenced by any cube cannot be deleted (the cubes keep their materialized
    /// copies; only the library entry would vanish). Holds `dim_topology` so the
    /// reference check and removal are atomic against a concurrent attach.
    pub fn delete_dimension(&self, id: DimensionId) -> Result<(), DimensionError> {
        let _topo = lock_coarse(&self.dim_topology);
        let registry = self.dimensions.load();
        if registry.get(id).is_none() {
            return Err(DimensionError::Unknown(id));
        }
        let referencing = registry.referencing(id);
        if !referencing.is_empty() {
            return Err(DimensionError::Referenced(referencing));
        }
        let mut next = (**registry).clone();
        next.remove(id);
        // Fail-closed (E3): durably persist the new registry BEFORE publishing it,
        // so a save failure leaves the in-memory registry untouched and surfaces as
        // `DimensionError::Persist` rather than a silent divergence from disk (a lost
        // delete that a restart would resurrect). Matches every other registry
        // mutation's durable-write-first discipline (ADR-0024).
        self.persist_registry_state(&next)
            .map_err(DimensionError::Persist)?;
        self.dimensions.store(Arc::new(next));
        Ok(())
    }

    /// Promote a cube's embedded dimension into the global registry (ADR-0031
    /// Phase 1): register a copy of the cube's current dimension as a global
    /// dimension and attach the cube as its first referrer, so the dimension
    /// becomes referenceable by other cubes while this cube keeps its own data
    /// unchanged (the materialized-reference model: the cube still owns its copy,
    /// the registry now owns the identity). A dimension that is already
    /// registry-backed for this cube returns `AlreadyGlobal`. Holds `dim_topology`
    /// so the mint, register, attach, and persist are one critical section.
    pub fn promote_cube_dimension(
        &self,
        cube: &str,
        dim_name: &str,
    ) -> Result<DimensionId, PromoteError> {
        let _topo = lock_coarse(&self.dim_topology);
        // The cube's current dimension definition (its elements, hierarchy, and
        // attributes) becomes the canonical registry copy.
        let snapshot = self
            .snapshot(cube)
            .ok_or_else(|| PromoteError::UnknownCube(cube.to_string()))?;
        let dimension = snapshot
            .cube()
            .dimensions()
            .iter()
            .find(|d| d.name() == dim_name)
            .ok_or_else(|| PromoteError::UnknownDimension {
                cube: cube.to_string(),
                dimension: dim_name.to_string(),
            })?
            .clone();
        // Already global for this cube? (a registry dimension of this name that the
        // cube already references). Nothing to promote.
        if let Some(existing) = self.dimensions.load().backing_of(cube, dim_name) {
            return Err(PromoteError::AlreadyGlobal(existing));
        }
        // Mint a fresh id, register the copy, and attach the cube as a referrer.
        let id = DimensionId(self.next_dim_id.fetch_add(1, Ordering::SeqCst));
        let mut next = (**self.dimensions.load()).clone();
        next.put(Arc::new(SharedDimension::new(id, dimension)));
        next.attach(id, cube);
        // Fail-closed (E3): the promotion's new identity (and the cube-to-id
        // backing) lives ONLY in the registry, so durably persist BEFORE publishing.
        // A save failure leaves the registry untouched and returns
        // `PromoteError::Persist`, so the caller never holds a minted id whose
        // durable home was silently lost (the unused id leaves a harmless gap). The
        // cube's own data is untouched either way (promote only copies).
        self.persist_registry_state(&next)
            .map_err(PromoteError::Persist)?;
        self.dimensions.store(Arc::new(next));
        Ok(id)
    }

    /// If `cube`'s dimension named `dim_name` is a materialized reference to a
    /// registered shared dimension, return that dimension's id (ADR-0024 v1). A
    /// cube has at most one dimension of a given name, so the (cube, name) pair
    /// resolves to at most one backing shared dimension. Used to block cube-local
    /// edits to a shared dimension (they must go through the library so every
    /// referencing cube stays consistent).
    pub fn dimension_backing(&self, cube: &str, dim_name: &str) -> Option<DimensionId> {
        self.dimensions.load().backing_of(cube, dim_name)
    }

    /// All of `cube`'s registry-backed dimensions as name -> id, resolved in a
    /// single registry pass (ADR-0031). Lets cube detail annotate every dimension
    /// with its global id without a per-dimension full-registry scan.
    pub fn dimension_backings(&self, cube: &str) -> BTreeMap<String, DimensionId> {
        self.dimensions.load().backings_for(cube)
    }

    /// The cube names, in deterministic sorted order.
    pub fn cube_names(&self) -> Vec<String> {
        self.cubes.load().keys().cloned().collect()
    }

    /// Whether a cube exists.
    pub fn has_cube(&self, cube: &str) -> bool {
        self.cubes.load().contains_key(cube)
    }

    /// Take a lock-free read snapshot of a cube. Never blocks and is never blocked
    /// by writers; the returned snapshot is a consistent whole-cube version.
    pub fn snapshot(&self, cube: &str) -> Option<ReadSnapshot> {
        let state = self.state(cube)?;
        Some(ReadSnapshot {
            inner: state.published.load_full(),
        })
    }

    /// The current committed version of a cube.
    pub fn version(&self, cube: &str) -> Option<Version> {
        self.state(cube).map(|s| s.published.load().version)
    }

    /// Apply a batch of writes atomically. With `base = Some(v)` the commit
    /// succeeds only if the cube is still at version `v` (optimistic concurrency);
    /// `None` is last-writer-wins. Any rejected write aborts the whole batch with
    /// the cube unchanged. On success the new version is durable (logged before
    /// publish) and concurrent readers observe the full batch or none of it.
    pub fn apply_batch(
        &self,
        cube: &str,
        base: Option<Version>,
        writes: &[CellWrite],
    ) -> Result<CommitOutcome, BatchError> {
        let state = self
            .state(cube)
            .ok_or_else(|| BatchError::UnknownCube(cube.to_string()))?;
        let mut writer = lock_writer(&state);
        if writer.fail_stopped {
            return Err(fail_stopped_error(cube));
        }

        if let Some(base) = base {
            if base != writer.version {
                return Err(BatchError::Conflict {
                    expected: base,
                    actual: writer.version,
                });
            }
        }

        // Validate + durably log the batch (all-or-nothing). On success the
        // store's in-memory cube reflects exactly what we are about to publish.
        match writer.store.set_batch(writes) {
            Ok(()) => {}
            Err(PersistError::BatchRejected { index, source }) => {
                return Err(BatchError::Rejected { index, source })
            }
            // Durability failure — including [`PersistError::Poisoned`]: a failed
            // WAL append/fsync rolled the log back to its last good offset,
            // dropped the trial cube (memory stays consistent with the durable
            // log), and poisoned the store. (An auto-checkpoint that fails AFTER a
            // successful WAL append leaves the batch durable in the WAL and applied
            // in memory — still consistent with the durable log, which replays it
            // on recovery — without poisoning; the next commit re-publishes.)
            // Nothing is published here, so readers keep serving the last good
            // version; a poisoned store keeps rejecting writes for this cube until
            // the server restarts (reopen truncates the WAL to its last intact
            // record).
            Err(e) => return Err(BatchError::Persist(e)),
        }

        // Mint the new global version, durably record it as the commit high-water
        // BEFORE publishing (E1: no restart may reissue an observable version),
        // then publish the immutable version (lock-free for readers) and record it
        // as the per-cube CAS base under the held writer lock. If the high-water
        // write fails the batch is already durable in the WAL but is NOT published;
        // the store's model is ahead of `published` exactly as in any other
        // post-WAL durability failure, and recovery reconstructs the same state.
        let version = self.ids.next_id();
        self.record_commit_version(version)?;
        state.published.store(Arc::new(Published {
            version,
            model: writer.store.model().clone(),
        }));
        writer.version = version;
        // Emit the ordered commit event (E4) while still under the writer lock, so
        // same-cube commits are observed in version order. `apply_batch` is a
        // base-cube write (no sandbox).
        self.notify_commit(cube, version, None);

        Ok(CommitOutcome { version })
    }

    /// Define (create or replace) a subset and publish a new version. Like
    /// [`apply_batch`](Self::apply_batch), `base` gives optimistic concurrency.
    /// An invalid definition returns [`BatchError::Invalid`] and changes nothing.
    pub fn define_subset(
        &self,
        cube: &str,
        base: Option<Version>,
        subset: Subset,
    ) -> Result<CommitOutcome, BatchError> {
        self.define(cube, base, |store| store.define_subset(subset))
    }

    /// Delete a subset by dimension and name and publish a new version. A missing
    /// subset returns [`BatchError::Invalid`] (it changed nothing).
    pub fn delete_subset(
        &self,
        cube: &str,
        base: Option<Version>,
        dimension: &str,
        name: &str,
    ) -> Result<CommitOutcome, BatchError> {
        self.define(cube, base, |store| {
            if store.delete_subset(dimension, name)? {
                Ok(())
            } else {
                Err(PersistError::Query(QueryError::UnknownSubset {
                    name: name.to_string(),
                }))
            }
        })
    }

    /// Define (create or replace) a view and publish a new version.
    pub fn define_view(
        &self,
        cube: &str,
        base: Option<Version>,
        view: View,
    ) -> Result<CommitOutcome, BatchError> {
        self.define(cube, base, |store| store.define_view(view))
    }

    /// Delete a view by name and publish a new version. A missing view returns
    /// [`BatchError::Invalid`].
    pub fn delete_view(
        &self,
        cube: &str,
        base: Option<Version>,
        name: &str,
    ) -> Result<CommitOutcome, BatchError> {
        self.define(cube, base, |store| {
            if store.delete_view(name)? {
                Ok(())
            } else {
                Err(PersistError::Query(QueryError::UnknownSubset {
                    name: name.to_string(),
                }))
            }
        })
    }

    /// Set the cube's rules source and publish a new version. The source is
    /// stored verbatim; the caller validates it (via the calc layer) first.
    pub fn define_rules(
        &self,
        cube: &str,
        base: Option<Version>,
        source: String,
    ) -> Result<CommitOutcome, BatchError> {
        self.define(cube, base, |store| store.define_rules(source))
    }

    /// Clear the cube's rules and publish a new version. Returns
    /// [`BatchError::Invalid`] if there were none.
    pub fn delete_rules(
        &self,
        cube: &str,
        base: Option<Version>,
    ) -> Result<CommitOutcome, BatchError> {
        self.define(cube, base, |store| {
            if store.delete_rules()? {
                Ok(())
            } else {
                Err(PersistError::Query(QueryError::Calc {
                    message: "no rules to delete".to_string(),
                }))
            }
        })
    }

    /// Define (create or replace) a rule unit test and publish a new version.
    pub fn define_rule_test(
        &self,
        cube: &str,
        base: Option<Version>,
        test: RuleTest,
    ) -> Result<CommitOutcome, BatchError> {
        self.define(cube, base, |store| store.define_rule_test(test))
    }

    /// Delete a rule test by name and publish a new version. A missing test
    /// returns [`BatchError::Invalid`].
    pub fn delete_rule_test(
        &self,
        cube: &str,
        base: Option<Version>,
        name: &str,
    ) -> Result<CommitOutcome, BatchError> {
        self.define(cube, base, |store| {
            if store.delete_rule_test(name)? {
                Ok(())
            } else {
                Err(PersistError::Query(QueryError::Calc {
                    message: format!("no rule test '{name}'"),
                }))
            }
        })
    }

    // Flows, flow tests, connections, and jobs are no longer per-cube (ADR-0035):
    // the API mutates them through the server-global `AutomationStore`, not the
    // cube engine.

    /// The registry id of the global dimension named `name` (ADR-0031/0035), if
    /// any. A flow addresses a global dimension by bare name; the apply path
    /// resolves the name to an id here, then grows it via [`grow_dimension`].
    pub fn dimension_id_by_name(&self, name: &str) -> Option<DimensionId> {
        self.dimensions.load().id_of(name)
    }

    /// The current member names of the global dimension named `name`, if it
    /// exists (used by a flow's `ctx.dimension(name).members()` read).
    pub fn registry_dimension_members(&self, name: &str) -> Option<Vec<String>> {
        self.dimensions.load().named(name).map(|s| {
            s.dimension
                .iter_elements()
                .map(|e| e.name.clone())
                .collect()
        })
    }

    /// Create a new, empty sandbox owned by `owner` (ADR-0014), stamping an
    /// injected created id, and publish a new version. Returns
    /// [`BatchError::Invalid`] if a sandbox of that name already exists.
    pub fn create_sandbox(
        &self,
        cube: &str,
        base: Option<Version>,
        name: &str,
        owner: &str,
    ) -> Result<CommitOutcome, BatchError> {
        let created = self.ids.next_id();
        self.define(cube, base, |store| {
            if store.model().sandbox(name).is_some() {
                return Err(PersistError::Query(QueryError::Calc {
                    message: format!("sandbox '{name}' already exists"),
                }));
            }
            store.define_sandbox(Sandbox::new(name, owner, created))
        })
    }

    /// Stage leaf overrides into a sandbox (a what-if write) and publish a new
    /// version. The base cube is never touched; the overrides live in the
    /// sandbox overlay. A non-leaf or out-of-range coordinate is rejected
    /// wholesale ([`BatchError::Rejected`]).
    pub fn sandbox_set_cells(
        &self,
        cube: &str,
        base: Option<Version>,
        name: &str,
        writes: &[CellWrite],
    ) -> Result<CommitOutcome, BatchError> {
        let updated = self.ids.next_id();
        // A private what-if write: tag the commit event with the sandbox so the API
        // change feed delivers it only to the sandbox's owner (ADR-0014), never to
        // every cube reader.
        self.define_sandbox(cube, base, Some(name), |store| {
            store.sandbox_set_cells(name, writes, updated)
        })
    }

    /// Commit a sandbox's overrides into the base cube and publish a new version,
    /// clearing the deltas (the sandbox stays, empty). Uses the same optimistic
    /// base-version check as [`apply_batch`](Self::apply_batch): a stale base
    /// conflicts and changes nothing.
    pub fn commit_sandbox(
        &self,
        cube: &str,
        base: Option<Version>,
        name: &str,
    ) -> Result<CommitOutcome, BatchError> {
        let updated = self.ids.next_id();
        self.define(cube, base, |store| store.commit_sandbox(name, updated))
    }

    /// Discard a sandbox (drop it and its deltas) and publish a new version. A
    /// missing sandbox returns [`BatchError::Invalid`]; base data is untouched.
    pub fn discard_sandbox(
        &self,
        cube: &str,
        base: Option<Version>,
        name: &str,
    ) -> Result<CommitOutcome, BatchError> {
        self.define(cube, base, |store| {
            if store.delete_sandbox(name)? {
                Ok(())
            } else {
                Err(PersistError::Query(QueryError::Calc {
                    message: format!("no sandbox '{name}'"),
                }))
            }
        })
    }

    /// Append dimension elements and consolidation edges (append-only,
    /// idempotent) and publish a new version, returning the commit outcome and
    /// the number of newly-created elements. This is the durable side of a flow's
    /// "build dimension elements" stage.
    pub fn define_elements(
        &self,
        cube: &str,
        base: Option<Version>,
        elements: &[ElementSpec],
        edges: &[EdgeSpec],
    ) -> Result<(CommitOutcome, usize), BatchError> {
        self.define_with(cube, base, |store| store.extend_schema(elements, edges))
    }

    /// Define an attribute on a dimension (ADR-0021) and publish a new version.
    /// Idempotent for the same kind; a different kind is a conflict
    /// ([`BatchError::Invalid`]) and changes nothing.
    pub fn define_attribute(
        &self,
        cube: &str,
        base: Option<Version>,
        dimension: &str,
        name: &str,
        kind: AttributeKind,
    ) -> Result<CommitOutcome, BatchError> {
        self.define(cube, base, |store| {
            store.define_attribute(dimension, name, kind)
        })
    }

    /// Set an attribute's value for one or more elements by name (ADR-0021) and
    /// publish a new version. Transactional: a rejected value (unknown element,
    /// kind mismatch, alias collision) returns [`BatchError::Invalid`] and changes
    /// nothing.
    pub fn set_attribute_values(
        &self,
        cube: &str,
        base: Option<Version>,
        dimension: &str,
        attribute: &str,
        values: &[(String, AttributeValue)],
    ) -> Result<CommitOutcome, BatchError> {
        self.define(cube, base, |store| {
            store.set_attribute_values(dimension, attribute, values)
        })
    }

    /// Create a brand-new cube from dimension definitions (ADR-0021), persist it
    /// on disk, and register it in the live cube set. Requires the engine to have
    /// an on-disk root ([`with_cubes_dir`](Self::with_cubes_dir)); otherwise
    /// returns [`BatchError::Unsupported`]. A duplicate name returns
    /// [`BatchError::AlreadyExists`]; an invalid structure returns
    /// [`BatchError::Invalid`]. On success the new cube is durable and visible to
    /// readers, and existing cubes are untouched. A name whose on-disk slug
    /// equals an existing cube's slug — which covers case-insensitive collisions
    /// AND slug-equivalent characters ("Sales Plan" vs "Sales?Plan") — is also
    /// rejected with [`BatchError::AlreadyExists`] (ADR-0037): both names map to
    /// one on-disk folder, so creating the second would overwrite the first
    /// cube's snapshot and WAL. As a final guard, a slug folder that already
    /// holds a cube snapshot on disk (e.g. one that failed to load at boot) is
    /// never overwritten either.
    pub fn create_cube(
        &self,
        name: &str,
        dims: &[DimensionDef],
    ) -> Result<CommitOutcome, BatchError> {
        let cubes_dir = self.cubes_dir.clone().ok_or_else(|| {
            BatchError::Unsupported("cube creation is not enabled on this server".to_string())
        })?;

        // Build and validate the cube before taking any lock or touching disk.
        let cube =
            Cube::build(name, dims).map_err(|e| BatchError::Invalid(QueryError::Model(e)))?;

        // Serialize registration so two concurrent creates cannot lose a cube.
        let _topo = lock_coarse(&self.topology);
        // Reject any name whose SLUG collides with an existing cube's (ADR-0037):
        // on-disk identity is `slug(name)`, and `slug` maps every character
        // outside `[a-z0-9-_]` to '-', so names far beyond case variants ("Sales
        // Plan", "Sales-Plan", "Sales?Plan") share one folder. `Store::create`
        // replaces what is there, so admitting such a name would silently
        // destroy the existing cube's snapshot and WAL. Slug equality subsumes
        // the exact-duplicate and case-insensitive checks (slug lowercases).
        let new_slug = slug(name);
        if let Some(existing) = self.cubes.load().keys().find(|k| slug(k) == new_slug) {
            return Err(BatchError::AlreadyExists(existing.clone()));
        }
        // The same guard for a cube the engine does NOT have loaded (e.g. a
        // store that failed to open at boot, or a folder restored out-of-band):
        // never overwrite an existing on-disk snapshot.
        if cubes_dir.join(&new_slug).join("snapshot.model").exists() {
            return Err(BatchError::AlreadyExists(new_slug));
        }

        // Persist on disk in the boot layout (ADR-0037): the folder is the
        // lowercase, filesystem-safe slug of the display name, not the name.
        let store = Store::create(cubes_dir.join(new_slug), cube).map_err(BatchError::Persist)?;
        // Mint and durably record the commit high-water (E1) before the cube is
        // registered/published, so a restart never reissues this create's version.
        let version = self.ids.next_id();
        self.record_commit_version(version)?;
        let state = Arc::new(CubeState {
            published: ArcSwap::from_pointee(Published {
                version,
                model: store.model().clone(),
            }),
            writer: Mutex::new(Writer {
                store,
                version,
                fail_stopped: false,
            }),
        });

        // Copy-on-write swap: clone the map (Arc values are cheap to clone), add
        // the new cube, and publish atomically. In-flight reads keep their map.
        let mut next = (**self.cubes.load()).clone();
        next.insert(name.to_string(), state);
        self.cubes.store(Arc::new(next));
        // Emit the ordered commit event for the newly-created cube (E4); a create
        // is a base-cube commit (no sandbox).
        self.notify_commit(name, version, None);
        Ok(CommitOutcome { version })
    }

    /// Shared commit path for definition changes: take the writer lock, check the
    /// optional base version, run `op` against the store (which validates and
    /// checkpoints), then publish the new immutable model version.
    fn define(
        &self,
        cube: &str,
        base: Option<Version>,
        op: impl FnOnce(&mut Store) -> Result<(), PersistError>,
    ) -> Result<CommitOutcome, BatchError> {
        self.define_sandbox(cube, base, None, op)
    }

    /// [`define`](Self::define) for a commit that writes into a named sandbox
    /// (`sandbox`), so the emitted [`CommitEvent`] carries it and an observer can
    /// keep the private what-if write off every non-owner's change feed (ADR-0014).
    /// A base commit uses [`define`](Self::define) (`sandbox: None`).
    fn define_sandbox(
        &self,
        cube: &str,
        base: Option<Version>,
        sandbox: Option<&str>,
        op: impl FnOnce(&mut Store) -> Result<(), PersistError>,
    ) -> Result<CommitOutcome, BatchError> {
        self.define_with_sandbox(cube, base, sandbox, op)
            .map(|(outcome, ())| outcome)
    }

    /// Like [`define`](Self::define) but threads a value back from `op` (e.g. a
    /// count of changes), alongside the commit outcome.
    fn define_with<T>(
        &self,
        cube: &str,
        base: Option<Version>,
        op: impl FnOnce(&mut Store) -> Result<T, PersistError>,
    ) -> Result<(CommitOutcome, T), BatchError> {
        self.define_with_sandbox(cube, base, None, op)
    }

    /// [`define_with`](Self::define_with) carrying the sandbox context for the
    /// emitted [`CommitEvent`] (see [`define_sandbox`](Self::define_sandbox)).
    fn define_with_sandbox<T>(
        &self,
        cube: &str,
        base: Option<Version>,
        sandbox: Option<&str>,
        op: impl FnOnce(&mut Store) -> Result<T, PersistError>,
    ) -> Result<(CommitOutcome, T), BatchError> {
        let state = self
            .state(cube)
            .ok_or_else(|| BatchError::UnknownCube(cube.to_string()))?;
        let mut writer = lock_writer(&state);
        if writer.fail_stopped {
            return Err(fail_stopped_error(cube));
        }

        if let Some(base) = base {
            if base != writer.version {
                return Err(BatchError::Conflict {
                    expected: base,
                    actual: writer.version,
                });
            }
        }

        let value = match op(&mut writer.store) {
            Ok(value) => value,
            Err(e) => {
                // The op may have mutated the store's in-memory model before
                // failing (e.g. a structural edit applied, then the checkpoint's
                // I/O failed). Restore it from the last published model so the
                // orphaned mutation cannot ride out on the next successful commit:
                // `published` is updated only on success, so it is the last good
                // state, and on a clean validation error nothing was mutated, so
                // the restore is a harmless no-op.
                writer
                    .store
                    .restore_model(state.published.load().model.clone());
                return Err(match e {
                    PersistError::BatchRejected { index, source } => {
                        BatchError::Rejected { index, source }
                    }
                    PersistError::Query(e) => BatchError::Invalid(e),
                    // A bare model rejection from a definition op (e.g. unknown
                    // dimension, kind conflict, alias collision) is a client-
                    // correctable error, not a durability failure, so it surfaces
                    // as Invalid (422), not Persist. Neither class touched disk,
                    // so the in-memory restore above fully recovers.
                    PersistError::Model(e) => BatchError::Invalid(QueryError::Model(e)),
                    // A durability-path failure (I/O, save, corruption, a
                    // poisoned WAL) may have left DISK ahead of the restored
                    // model: the op could have renamed a post-op snapshot into
                    // place before failing (so a crash would recover the "failed"
                    // op), or durably appended a WAL unit whose batch the caller
                    // was told failed (`commit_sandbox`), or — worst — left a
                    // post-reindex snapshot under a WAL that later collects
                    // pre-reindex coordinates, which recovery would replay onto
                    // the wrong elements. Re-checkpointing the restored model
                    // rewrites the snapshot and clears the WAL, making disk agree
                    // with what callers were told. If even that fails (or the
                    // store is poisoned), fail-stop the writer: reads keep
                    // serving the last published version, writes are rejected
                    // until a checkpoint succeeds or the store is reopened.
                    e => {
                        if writer.store.checkpoint().is_err() {
                            writer.fail_stopped = true;
                        }
                        BatchError::Persist(e)
                    }
                });
            }
        };

        // Mint, durably record the commit high-water (E1) BEFORE publishing, then
        // publish. A high-water failure leaves the definition durable on disk but
        // unpublished (like any post-checkpoint durability failure here).
        let version = self.ids.next_id();
        self.record_commit_version(version)?;
        state.published.store(Arc::new(Published {
            version,
            model: writer.store.model().clone(),
        }));
        writer.version = version;
        // Emit the ordered commit event (E4) under the still-held writer lock,
        // carrying the sandbox context (if any) so an observer can keep a private
        // what-if write off non-owners' feeds.
        self.notify_commit(cube, version, sandbox);
        Ok((CommitOutcome { version }, value))
    }

    /// Force a checkpoint (full-persist) of a cube. A success also heals a
    /// fail-stopped writer (see [`Writer::fail_stopped`]): a fail-stopped cube
    /// rejected every write since the failure, so its in-memory model still
    /// equals the last published version, and a full checkpoint re-establishes
    /// snapshot + WAL == that model — disk agrees with readers again.
    pub fn checkpoint(&self, cube: &str) -> Result<(), BatchError> {
        let state = self
            .state(cube)
            .ok_or_else(|| BatchError::UnknownCube(cube.to_string()))?;
        let mut writer = lock_writer(&state);
        writer.store.checkpoint().map_err(BatchError::Persist)?;
        writer.fail_stopped = false;
        Ok(())
    }
}

/// The seam the API injects to build a per-query value resolver over a pinned
/// snapshot, mirroring how `SetEvaluator` is injected. The default
/// [`StoredCellsFactory`] reads stored cells and consolidation (no rules); the
/// server injects a rule-aware factory that overlays calc. The returned resolver
/// owns its snapshot, so it is independent of any borrow.
pub trait CellResolverFactory: Send + Sync {
    /// Build a value resolver bound to a pinned snapshot. The resolver is `Sync`
    /// so a view's value grid may be filled from several threads (ADR-0028
    /// Stage B); it is only ever read across them, never mutated.
    fn resolver(&self, snapshot: &ReadSnapshot) -> Box<dyn CellResolver + Sync>;

    /// Build a resolver that overlays a sandbox's what-if values beneath the
    /// rules (ADR-0014) and enforces a caller's element deny mask (ADR-0015): a
    /// read of a coordinate that names, or rolls up, a denied element returns
    /// [`QueryError::AccessDenied`]. The default ignores both, so a factory that
    /// supports neither -- and a `None` sandbox/mask -- behaves exactly like
    /// [`resolver`](Self::resolver). The rule-aware factory the server injects,
    /// and [`StoredCellsFactory`], override this.
    fn resolver_with(
        &self,
        snapshot: &ReadSnapshot,
        sandbox: Option<&epiphany_core::Sandbox>,
        mask: Option<&ElementMask>,
    ) -> Box<dyn CellResolver + Sync> {
        let _ = (sandbox, mask);
        self.resolver(snapshot)
    }
}

/// The default factory: a resolver reading stored cells, byte-identical to the
/// no-rules behavior. Stateless.
#[derive(Debug, Clone, Copy, Default)]
pub struct StoredCellsFactory;

impl CellResolverFactory for StoredCellsFactory {
    fn resolver(&self, snapshot: &ReadSnapshot) -> Box<dyn CellResolver + Sync> {
        Box::new(StoredResolver {
            snapshot: snapshot.clone(),
            mask: None,
        })
    }

    /// The stored-cell path has no rules or what-if, so the sandbox is ignored,
    /// but the element deny mask (ADR-0015) is honored: a no-rules deployment is
    /// still least-privilege. The check expands consolidated coordinates to their
    /// contributing leaves (`Cube::get` consolidates internally), so a rollup of
    /// a denied leaf is denied.
    fn resolver_with(
        &self,
        snapshot: &ReadSnapshot,
        sandbox: Option<&epiphany_core::Sandbox>,
        mask: Option<&ElementMask>,
    ) -> Box<dyn CellResolver + Sync> {
        let _ = sandbox;
        Box::new(StoredResolver {
            snapshot: snapshot.clone(),
            mask: mask.cloned(),
        })
    }
}

/// A [`CellResolver`] that owns a pinned snapshot and reads stored values,
/// optionally enforcing an element deny mask (ADR-0015).
#[derive(Debug)]
struct StoredResolver {
    snapshot: ReadSnapshot,
    mask: Option<ElementMask>,
}

impl StoredResolver {
    /// Deny a read that names, or rolls up, an element the caller may not see.
    fn check(&self, coord: &[u32]) -> Result<(), QueryError> {
        if let Some(mask) = &self.mask {
            if mask.denies(self.snapshot.cube(), coord) {
                return Err(QueryError::AccessDenied);
            }
        }
        Ok(())
    }
}

impl CellResolver for StoredResolver {
    fn value(&self, coord: &[u32]) -> Result<Fixed, QueryError> {
        self.check(coord)?;
        Ok(self.snapshot.cube().get(coord)?)
    }

    fn string_value(&self, coord: &[u32]) -> Result<Option<String>, QueryError> {
        self.check(coord)?;
        Ok(self.snapshot.cube().get_string(coord)?.map(str::to_string))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epiphany_core::{Dimension, ElementKind, Fixed};

    /// A Region(3 leaves under Total) x Period(2 leaves under Total) cube.
    struct Fixture {
        engine: Engine,
        r: Vec<u32>,
        region_total: u32,
        p: Vec<u32>,
        period_total: u32,
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("epiphany-engine-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        dir
    }

    fn sum_dim(name: &str, n: u32) -> (Dimension, u32, Vec<u32>) {
        let mut d = Dimension::new(name);
        let leaves: Vec<u32> = (0..n).map(|i| d.add_leaf(format!("{name}{i}"))).collect();
        let total = d.add_consolidated("Total");
        for &leaf in &leaves {
            d.add_child(total, leaf, 1).unwrap();
        }
        (d, total, leaves)
    }

    fn fixture(name: &str) -> Fixture {
        let (region, region_total, r) = sum_dim("R", 3);
        let (period, period_total, p) = sum_dim("P", 2);
        let cube = Cube::new("Sales", vec![region, period]).unwrap();
        let store = Store::create(scratch(name), cube).unwrap();
        let mut stores = BTreeMap::new();
        stores.insert("Sales".to_string(), store);
        let engine = Engine::from_stores(stores, Arc::new(IdGen::default()));
        Fixture {
            engine,
            r,
            region_total,
            p,
            period_total,
        }
    }

    fn leaf(coord: Vec<u32>, value: i32) -> CellWrite {
        CellWrite::Leaf {
            coord,
            value: Fixed::from(value),
        }
    }

    #[test]
    fn failed_definition_does_not_leak_into_next_commit() {
        // A definition op that mutates the in-memory model and then fails (modeling
        // a checkpoint I/O failure after the model was already changed) must be
        // rolled back, so its mutation cannot ride out on the next successful
        // commit. Before the fix, the orphaned member surfaced in the next publish.
        let f = fixture("failed-define-rollback");
        let res = f.engine.define_with("Sales", None, |store| {
            // Append a member (mutates the in-memory cube), then fail.
            store.extend_schema(
                &[ElementSpec {
                    dimension: "R".into(),
                    name: "Ghost".into(),
                    kind: ElementKind::Leaf,
                }],
                &[EdgeSpec {
                    dimension: "R".into(),
                    parent: "Total".into(),
                    child: "Ghost".into(),
                    weight: 1,
                }],
            )?;
            Err::<usize, PersistError>(PersistError::Io(std::io::Error::other(
                "simulated checkpoint failure",
            )))
        });
        assert!(res.is_err());

        // A later successful commit must publish a cube WITHOUT the failed member.
        f.engine
            .define_rules("Sales", None, "['R':'R0'] = 1;".to_string())
            .unwrap();
        let snap = f.engine.snapshot("Sales").unwrap();
        assert!(
            snap.cube().dimension(0).resolve("Ghost").is_none(),
            "the failed op's appended member must not leak into the next commit"
        );
    }

    #[test]
    fn failed_durability_op_cannot_resurrect_after_recovery() {
        // Models a commit_sandbox-style failure: the op durably WAL-appends a
        // whole batch, then a later step (its checkpoint) fails with I/O. The
        // caller is told the commit failed and the live engine rolls back — and
        // the resync checkpoint must make DISK agree, so a crash + recovery
        // cannot resurrect the "failed" batch (before the fix, the durable WAL
        // unit replayed it into the base cube).
        let dir = scratch("durability-resync");
        let (region, _rt, r) = sum_dim("R", 3);
        let (period, _pt, p) = sum_dim("P", 2);
        let cube = Cube::new("Sales", vec![region, period]).unwrap();
        let store = Store::create(dir.clone(), cube).unwrap();
        let mut stores = BTreeMap::new();
        stores.insert("Sales".to_string(), store);
        let engine = Engine::from_stores(stores, Arc::new(IdGen::default()));

        let coord = vec![r[0], p[0]];
        let batch_coord = coord.clone();
        let res = engine.define_with("Sales", None, move |store| {
            store.set_batch(&[CellWrite::Leaf {
                coord: batch_coord,
                value: Fixed::from(500),
            }])?;
            Err::<(), PersistError>(PersistError::Io(std::io::Error::other(
                "simulated checkpoint failure after a durable WAL append",
            )))
        });
        assert!(matches!(res, Err(BatchError::Persist(_))));

        // Live: rolled back, still readable, and still writable (the resync
        // checkpoint succeeded, so the writer is not fail-stopped).
        assert_eq!(
            engine
                .snapshot("Sales")
                .unwrap()
                .cube()
                .get_leaf(&coord)
                .unwrap(),
            Fixed::ZERO
        );
        engine
            .apply_batch("Sales", None, &[leaf(vec![r[1], p[0]], 7)])
            .unwrap();

        // Durable: recovery agrees with what callers were told. The "failed"
        // 500 is gone; the later acknowledged 7 survives.
        drop(engine);
        let reopened = Store::open(dir).unwrap();
        assert_eq!(
            reopened.cube().get_leaf(&coord).unwrap(),
            Fixed::ZERO,
            "a batch whose commit was reported failed must not survive recovery"
        );
        assert_eq!(
            reopened.cube().get_leaf(&[r[1], p[0]]).unwrap(),
            Fixed::from(7),
            "an acknowledged later write must survive recovery"
        );
    }

    #[test]
    fn fail_stopped_writer_rejects_writes_serves_reads_and_heals() {
        // The documented recovery posture (Writer::fail_stopped): when disk
        // state cannot be re-verified, reads keep serving the last published
        // version while writes are rejected, until a checkpoint succeeds.
        let f = fixture("fail-stop-posture");
        f.engine
            .apply_batch("Sales", Some(0), &[leaf(vec![f.r[0], f.p[0]], 10)])
            .unwrap();
        let state = f.engine.state("Sales").unwrap();
        state.writer.lock().unwrap().fail_stopped = true;

        // Every write path is rejected with a persist error...
        assert!(matches!(
            f.engine
                .apply_batch("Sales", None, &[leaf(vec![f.r[1], f.p[0]], 1)]),
            Err(BatchError::Persist(_))
        ));
        assert!(matches!(
            f.engine
                .define_rules("Sales", None, "['R':'R0'] = 1;".to_string()),
            Err(BatchError::Persist(_))
        ));
        // ...while reads keep serving the last published version.
        assert_eq!(
            f.engine
                .snapshot("Sales")
                .unwrap()
                .cube()
                .get_leaf(&[f.r[0], f.p[0]])
                .unwrap(),
            Fixed::from(10)
        );

        // A successful forced checkpoint re-verifies disk and lifts the stop.
        f.engine.checkpoint("Sales").unwrap();
        f.engine
            .apply_batch("Sales", None, &[leaf(vec![f.r[1], f.p[0]], 5)])
            .unwrap();
        assert_eq!(
            f.engine
                .snapshot("Sales")
                .unwrap()
                .cube()
                .get_leaf(&[f.r[1], f.p[0]])
                .unwrap(),
            Fixed::from(5)
        );
    }

    #[test]
    fn poisoned_writer_recovers_instead_of_panicking() {
        // A panic while holding a cube's writer lock (a bug in a core op) must
        // not brick the cube: the next lock recovers by resynchronizing the
        // store from the last published version and returns to normal service,
        // instead of panicking on every later write.
        let f = fixture("writer-poison-recovery");
        f.engine
            .apply_batch("Sales", Some(0), &[leaf(vec![f.r[0], f.p[0]], 10)])
            .unwrap();

        let engine = f.engine.clone();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _ = engine.define("Sales", None, |_store| -> Result<(), PersistError> {
                panic!("simulated panic inside a commit");
            });
        }));
        assert!(panicked.is_err(), "the op's panic must propagate");

        // The next write recovers the poisoned lock and commits normally.
        f.engine
            .apply_batch("Sales", None, &[leaf(vec![f.r[1], f.p[0]], 5)])
            .unwrap();
        let snap = f.engine.snapshot("Sales").unwrap();
        assert_eq!(
            snap.cube().get_leaf(&[f.r[0], f.p[0]]).unwrap(),
            Fixed::from(10),
            "the pre-panic committed value survives"
        );
        assert_eq!(
            snap.cube().get_leaf(&[f.r[1], f.p[0]]).unwrap(),
            Fixed::from(5),
            "the post-recovery commit landed"
        );
    }

    #[test]
    fn batch_is_all_or_nothing() {
        let f = fixture("all-or-nothing");
        f.engine
            .apply_batch("Sales", Some(0), &[leaf(vec![f.r[0], f.p[0]], 10)])
            .unwrap();

        // A batch whose second write targets a consolidated element is rejected
        // wholesale; the first write does not leak.
        let err = f
            .engine
            .apply_batch(
                "Sales",
                None,
                &[
                    leaf(vec![f.r[1], f.p[0]], 20),
                    leaf(vec![f.region_total, f.p[0]], 1),
                ],
            )
            .unwrap_err();
        assert!(matches!(err, BatchError::Rejected { index: 1, .. }));

        let snap = f.engine.snapshot("Sales").unwrap();
        assert_eq!(
            snap.cube().get_leaf(&[f.r[1], f.p[0]]).unwrap(),
            Fixed::ZERO
        );
        assert_eq!(snap.cube().cell_count(), 1);
    }

    #[test]
    fn define_elements_adds_members_and_rolls_up() {
        let f = fixture("define-elements");
        // Seed R0/P0 = 10.
        f.engine
            .apply_batch("Sales", Some(0), &[leaf(vec![f.r[0], f.p[0]], 10)])
            .unwrap();

        // A flow's Schema stage: append a new leaf R3 under Total.
        let (outcome, added) = f
            .engine
            .define_elements(
                "Sales",
                None,
                &[ElementSpec {
                    dimension: "R".into(),
                    name: "R3".into(),
                    kind: ElementKind::Leaf,
                }],
                &[EdgeSpec {
                    dimension: "R".into(),
                    parent: "Total".into(),
                    child: "R3".into(),
                    weight: 1,
                }],
            )
            .unwrap();
        assert_eq!(added, 1);

        // The new element is visible in a fresh snapshot and writable.
        let snap = f.engine.snapshot("Sales").unwrap();
        let r3 = snap.cube().dimension(0).resolve("R3").unwrap();
        f.engine
            .apply_batch("Sales", Some(outcome.version), &[leaf(vec![r3, f.p[0]], 5)])
            .unwrap();

        // Total over P0 now includes R0(10) + R3(5) = 15; the seeded cell survived.
        let snap = f.engine.snapshot("Sales").unwrap();
        assert_eq!(
            snap.cube().get(&[f.region_total, f.p[0]]).unwrap(),
            Fixed::from(15)
        );
        // Re-running the same schema change is idempotent (adds nothing).
        let (_, added_again) = f
            .engine
            .define_elements(
                "Sales",
                None,
                &[ElementSpec {
                    dimension: "R".into(),
                    name: "R3".into(),
                    kind: ElementKind::Leaf,
                }],
                &[],
            )
            .unwrap();
        assert_eq!(added_again, 0);
    }

    #[test]
    fn reads_are_snapshot_isolated() {
        let f = fixture("snapshot-iso");
        f.engine
            .apply_batch("Sales", Some(0), &[leaf(vec![f.r[0], f.p[0]], 10)])
            .unwrap();
        let before = f.engine.snapshot("Sales").unwrap();

        f.engine
            .apply_batch(
                "Sales",
                None,
                &[
                    leaf(vec![f.r[1], f.p[0]], 20),
                    leaf(vec![f.r[2], f.p[0]], 30),
                ],
            )
            .unwrap();

        // The snapshot taken before the batch still sees the old total...
        assert_eq!(
            before.cube().get(&[f.region_total, f.p[0]]).unwrap(),
            Fixed::from(10)
        );
        // ...while a fresh snapshot sees the whole committed batch.
        let after = f.engine.snapshot("Sales").unwrap();
        assert_eq!(
            after.cube().get(&[f.region_total, f.p[0]]).unwrap(),
            Fixed::from(60)
        );
        assert!(after.version() > before.version());
    }

    #[test]
    fn stale_base_version_conflicts_without_mutating() {
        let f = fixture("conflict");
        let v1 = f
            .engine
            .apply_batch("Sales", Some(0), &[leaf(vec![f.r[0], f.p[0]], 10)])
            .unwrap()
            .version;
        // Commit again so the cube moves past v1.
        f.engine
            .apply_batch("Sales", Some(v1), &[leaf(vec![f.r[1], f.p[0]], 20)])
            .unwrap();
        // A batch staged on the now-stale v1 is rejected and changes nothing.
        let err = f
            .engine
            .apply_batch("Sales", Some(v1), &[leaf(vec![f.r[2], f.p[0]], 99)])
            .unwrap_err();
        assert!(matches!(err, BatchError::Conflict { .. }));
        let snap = f.engine.snapshot("Sales").unwrap();
        assert_eq!(
            snap.cube().get_leaf(&[f.r[2], f.p[0]]).unwrap(),
            Fixed::ZERO
        );
    }

    #[test]
    fn unknown_cube_is_rejected() {
        let f = fixture("unknown");
        assert!(matches!(
            f.engine
                .apply_batch("Nope", None, &[leaf(vec![f.r[0], f.p[0]], 1)])
                .unwrap_err(),
            BatchError::UnknownCube(_)
        ));
        assert!(f.engine.snapshot("Nope").is_none());
    }

    #[test]
    fn commits_are_deterministic_across_engines() {
        let run = |name: &str| {
            let f = fixture(name);
            let batches = [
                vec![leaf(vec![f.r[0], f.p[0]], 5), leaf(vec![f.r[1], f.p[1]], 7)],
                vec![leaf(vec![f.r[2], f.p[0]], 3)],
            ];
            let mut versions = Vec::new();
            for b in &batches {
                versions.push(f.engine.apply_batch("Sales", None, b).unwrap().version);
            }
            let snap = f.engine.snapshot("Sales").unwrap();
            let mut cells: Vec<(Vec<u32>, Fixed)> = snap.cube().cell_entries().collect();
            cells.sort_by(|a, b| a.0.cmp(&b.0));
            (versions, cells)
        };
        assert_eq!(run("det-a"), run("det-b"));
    }

    #[test]
    fn concurrent_readers_never_see_a_partial_batch() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let f = fixture("concurrent");
        let (region_total, period_total) = (f.region_total, f.period_total);
        let engine = f.engine.clone();
        let stop = Arc::new(AtomicBool::new(false));

        // Readers assert the grand total is always a clean multiple of the
        // per-batch increment (each batch adds 1 to two leaves -> total += 2),
        // never an odd partial.
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let engine = engine.clone();
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let snap = engine.snapshot("Sales").unwrap();
                        let total = snap
                            .cube()
                            .get(&[region_total, period_total])
                            .unwrap()
                            .to_scaled();
                        assert_eq!(total % 2, 0, "reader observed a partial batch");
                    }
                })
            })
            .collect();

        for _ in 0..200 {
            f.engine
                .apply_batch(
                    "Sales",
                    None,
                    &[leaf(vec![f.r[0], f.p[0]], 1), leaf(vec![f.r[1], f.p[1]], 1)],
                )
                .unwrap();
            // Each commit overwrites the same two leaves, so the total alternates
            // 0 -> 2 -> 2 ...; every committed state has an even total.
            f.engine
                .apply_batch(
                    "Sales",
                    None,
                    &[leaf(vec![f.r[0], f.p[0]], 0), leaf(vec![f.r[1], f.p[1]], 0)],
                )
                .unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        for h in readers {
            h.join().unwrap();
        }
    }

    #[test]
    fn sandbox_create_set_commit_discard() {
        let f = fixture("sandbox-lifecycle");
        // Seed base R0/P0 = 10.
        let v = f
            .engine
            .apply_batch("Sales", Some(0), &[leaf(vec![f.r[0], f.p[0]], 10)])
            .unwrap()
            .version;
        // Create a sandbox; a duplicate create is rejected.
        let v = f
            .engine
            .create_sandbox("Sales", Some(v), "wi", "ann")
            .unwrap()
            .version;
        assert!(matches!(
            f.engine
                .create_sandbox("Sales", Some(v), "wi", "ann")
                .unwrap_err(),
            BatchError::Invalid(_)
        ));
        // Stage a what-if override R0/P0 -> 500.
        let v = f
            .engine
            .sandbox_set_cells("Sales", Some(v), "wi", &[leaf(vec![f.r[0], f.p[0]], 500)])
            .unwrap()
            .version;
        // Base is untouched; the sandbox holds the override.
        let snap = f.engine.snapshot("Sales").unwrap();
        assert_eq!(
            snap.cube().get_leaf(&[f.r[0], f.p[0]]).unwrap(),
            Fixed::from(10)
        );
        assert_eq!(
            snap.model().sandbox("wi").unwrap().cell(&[f.r[0], f.p[0]]),
            Some(Fixed::from(500))
        );
        // Commit merges into base and clears the delta (sandbox stays, empty).
        let v = f
            .engine
            .commit_sandbox("Sales", Some(v), "wi")
            .unwrap()
            .version;
        let snap = f.engine.snapshot("Sales").unwrap();
        assert_eq!(
            snap.cube().get_leaf(&[f.r[0], f.p[0]]).unwrap(),
            Fixed::from(500)
        );
        assert!(snap.model().sandbox("wi").unwrap().is_empty());
        // Discard removes the sandbox; base is untouched.
        f.engine.discard_sandbox("Sales", Some(v), "wi").unwrap();
        let snap = f.engine.snapshot("Sales").unwrap();
        assert!(snap.model().sandbox("wi").is_none());
        assert_eq!(
            snap.cube().get_leaf(&[f.r[0], f.p[0]]).unwrap(),
            Fixed::from(500)
        );
    }

    #[test]
    fn sandbox_commit_with_stale_base_conflicts() {
        let f = fixture("sandbox-commit-conflict");
        let v = f
            .engine
            .create_sandbox("Sales", Some(0), "wi", "ann")
            .unwrap()
            .version;
        let v = f
            .engine
            .sandbox_set_cells("Sales", Some(v), "wi", &[leaf(vec![f.r[0], f.p[0]], 500)])
            .unwrap()
            .version;
        // A concurrent base write moves the cube past v.
        f.engine
            .apply_batch("Sales", Some(v), &[leaf(vec![f.r[1], f.p[0]], 7)])
            .unwrap();
        // Committing on the now-stale base conflicts and changes nothing.
        let err = f.engine.commit_sandbox("Sales", Some(v), "wi").unwrap_err();
        assert!(matches!(err, BatchError::Conflict { .. }));
        let snap = f.engine.snapshot("Sales").unwrap();
        assert_eq!(
            snap.cube().get_leaf(&[f.r[0], f.p[0]]).unwrap(),
            Fixed::ZERO,
            "a conflicting commit must not merge into base"
        );
        // The sandbox still holds its override (the commit did not clear it).
        assert_eq!(
            snap.model().sandbox("wi").unwrap().cell(&[f.r[0], f.p[0]]),
            Some(Fixed::from(500))
        );
    }

    #[test]
    fn sandbox_override_of_consolidated_is_rejected() {
        let f = fixture("sandbox-reject");
        let v = f
            .engine
            .create_sandbox("Sales", Some(0), "wi", "ann")
            .unwrap()
            .version;
        let err = f
            .engine
            .sandbox_set_cells(
                "Sales",
                Some(v),
                "wi",
                &[leaf(vec![f.region_total, f.p[0]], 1)],
            )
            .unwrap_err();
        assert!(matches!(err, BatchError::Rejected { index: 0, .. }));
        assert!(f
            .engine
            .snapshot("Sales")
            .unwrap()
            .model()
            .sandbox("wi")
            .unwrap()
            .is_empty());
    }

    fn static_subset(name: &str, members: &[&str]) -> Subset {
        use epiphany_core::{SubsetKind, Visibility};
        Subset {
            name: name.into(),
            dimension: "R".into(),
            owner: None,
            visibility: Visibility::Public,
            kind: SubsetKind::Static {
                members: members.iter().map(|s| s.to_string()).collect(),
            },
        }
    }

    #[test]
    fn define_subset_commits_and_is_snapshot_isolated() {
        let f = fixture("define-subset");
        let before = f.engine.snapshot("Sales").unwrap();
        let outcome = f
            .engine
            .define_subset("Sales", Some(0), static_subset("Core", &["R0", "R1"]))
            .unwrap();
        assert!(outcome.version > 0);
        // The pre-define snapshot does not see it; a fresh one does.
        assert!(before.subset("R", "Core").is_none());
        let after = f.engine.snapshot("Sales").unwrap();
        assert!(after.subset("R", "Core").is_some());
        assert!(after.version() > before.version());
    }

    #[test]
    fn invalid_definition_is_rejected_without_publishing() {
        let f = fixture("define-invalid");
        let before = f.engine.version("Sales").unwrap();
        let err = f
            .engine
            .define_subset("Sales", None, static_subset("Bad", &["Nope"]))
            .unwrap_err();
        assert!(matches!(err, BatchError::Invalid(_)));
        assert_eq!(
            f.engine.version("Sales").unwrap(),
            before,
            "a rejected define must not publish a new version"
        );
        assert!(f
            .engine
            .snapshot("Sales")
            .unwrap()
            .subset("R", "Bad")
            .is_none());
    }

    #[test]
    fn deleting_a_missing_subset_is_invalid() {
        let f = fixture("delete-missing");
        let err = f
            .engine
            .delete_subset("Sales", None, "R", "Ghost")
            .unwrap_err();
        assert!(matches!(err, BatchError::Invalid(_)));
    }

    #[test]
    fn stale_base_rejects_a_definition() {
        let f = fixture("define-conflict");
        // Move the cube past version 0 with a cell commit.
        let v1 = f
            .engine
            .apply_batch("Sales", Some(0), &[leaf(vec![f.r[0], f.p[0]], 1)])
            .unwrap()
            .version;
        // A define staged on the stale base 0 conflicts and changes nothing.
        let err = f
            .engine
            .define_subset("Sales", Some(0), static_subset("Core", &["R0"]))
            .unwrap_err();
        assert!(matches!(err, BatchError::Conflict { .. }));
        assert_eq!(f.engine.version("Sales").unwrap(), v1);
    }

    #[test]
    fn define_rules_publishes_and_snapshot_exposes_them() {
        let f = fixture("define-rules");
        f.engine
            .define_rules("Sales", Some(0), "['R':'R0'] = 1;".to_string())
            .unwrap();
        let snap = f.engine.snapshot("Sales").unwrap();
        assert!(!snap.rules().is_empty());
        let v = f.engine.version("Sales").unwrap();
        f.engine.delete_rules("Sales", Some(v)).unwrap();
        assert!(f.engine.snapshot("Sales").unwrap().rules().is_empty());
    }

    #[test]
    fn stored_cells_factory_resolver_matches_get() {
        let f = fixture("stored-factory");
        f.engine
            .apply_batch("Sales", Some(0), &[leaf(vec![f.r[0], f.p[0]], 10)])
            .unwrap();
        let snap = f.engine.snapshot("Sales").unwrap();
        let resolver = StoredCellsFactory.resolver(&snap);
        let coord = [f.region_total, f.period_total];
        assert_eq!(
            resolver.value(&coord).unwrap(),
            snap.cube().get(&coord).unwrap()
        );
    }

    // ---- model editing (ADR-0021) ----

    /// An engine with one cube and an on-disk root, so `create_cube` is enabled.
    fn editable_engine(name: &str) -> Engine {
        let root = scratch(name);
        std::fs::create_dir_all(&root).unwrap();
        let (region, _t, _r) = sum_dim("R", 2);
        let cube = Cube::new("Sales", vec![region]).unwrap();
        let store = Store::create(root.join("Sales"), cube).unwrap();
        let mut stores = BTreeMap::new();
        stores.insert("Sales".to_string(), store);
        Engine::from_stores(stores, Arc::new(IdGen::default())).with_cubes_dir(root)
    }

    #[test]
    fn create_cube_registers_persists_and_leaves_others_intact() {
        let engine = editable_engine("create-cube");
        assert!(!engine.has_cube("Budget"));

        let outcome = engine
            .create_cube(
                "Budget",
                &[
                    DimensionDef {
                        name: "Account".into(),
                        elements: vec![
                            ("Sales".into(), ElementKind::Leaf),
                            ("Costs".into(), ElementKind::Leaf),
                            ("Profit".into(), ElementKind::Consolidated),
                        ],
                        edges: vec![
                            ("Profit".into(), "Sales".into(), 1),
                            ("Profit".into(), "Costs".into(), -1),
                        ],
                        ..Default::default()
                    },
                    DimensionDef {
                        name: "Period".into(),
                        elements: vec![("Jan".into(), ElementKind::Leaf)],
                        edges: vec![],
                        ..Default::default()
                    },
                ],
            )
            .unwrap();
        assert!(outcome.version > 0);

        // Visible to readers immediately, and the existing cube is untouched.
        assert!(engine.has_cube("Budget"));
        assert!(engine.has_cube("Sales"));
        let snap = engine.snapshot("Budget").unwrap();
        assert_eq!(snap.cube().rank(), 2);

        // Writing the leaves rolls up under the weighted consolidation.
        let acct = snap.cube().dimension(0);
        let (sales, costs, profit) = (
            acct.index_of("Sales").unwrap(),
            acct.index_of("Costs").unwrap(),
            acct.index_of("Profit").unwrap(),
        );
        let jan = snap.cube().dimension(1).index_of("Jan").unwrap();
        drop(snap);
        engine
            .apply_batch(
                "Budget",
                None,
                &[leaf(vec![sales, jan], 100), leaf(vec![costs, jan], 30)],
            )
            .unwrap();
        let snap = engine.snapshot("Budget").unwrap();
        assert_eq!(snap.cube().get(&[profit, jan]).unwrap(), Fixed::from(70));
    }

    #[test]
    fn create_cube_rejects_duplicate_and_disabled() {
        let engine = editable_engine("create-dup");
        let dims = [DimensionDef {
            name: "D".into(),
            elements: vec![("a".into(), ElementKind::Leaf)],
            edges: vec![],
            ..Default::default()
        }];
        assert!(matches!(
            engine.create_cube("Sales", &dims),
            Err(BatchError::AlreadyExists(_))
        ));

        // An engine with no on-disk root cannot create cubes.
        let f = fixture("create-disabled");
        assert!(matches!(
            f.engine.create_cube("New", &dims),
            Err(BatchError::Unsupported(_))
        ));
    }

    #[test]
    fn create_cube_rejects_a_case_insensitive_name_collision() {
        // "Sales" already exists; "SALES"/"sales" would slug to the same on-disk
        // folder (ADR-0037), so they must be rejected as already-existing, and no
        // new cube/folder may be created.
        let engine = editable_engine("create-case-collision");
        let dims = [DimensionDef {
            name: "D".into(),
            elements: vec![("a".into(), ElementKind::Leaf)],
            edges: vec![],
            ..Default::default()
        }];
        for collision in ["SALES", "sales", "sAlEs"] {
            assert!(
                matches!(
                    engine.create_cube(collision, &dims),
                    Err(BatchError::AlreadyExists(_))
                ),
                "creating {collision:?} should collide with the existing 'Sales'"
            );
        }
        // The original cube is untouched and no extra cube was registered.
        assert!(engine.has_cube("Sales"));
        assert_eq!(engine.cube_names(), vec!["Sales".to_string()]);
    }

    #[test]
    fn create_cube_rejects_a_slug_equivalent_name_collision() {
        // On-disk identity is slug(name), and slug() maps every character
        // outside [a-z0-9-_] to '-': "Sales Plan", "Sales-Plan", and
        // "Sales?Plan" are distinct case-insensitively yet share one folder.
        // Admitting the second such name would silently overwrite the first
        // cube's snapshot and WAL (ADR-0037 requires rejecting it).
        let engine = editable_engine("create-slug-collision");
        let dims = [DimensionDef {
            name: "D".into(),
            elements: vec![("a".into(), ElementKind::Leaf)],
            edges: vec![],
            ..Default::default()
        }];
        engine.create_cube("Sales Plan", &dims).unwrap();
        engine
            .apply_batch("Sales Plan", None, &[leaf(vec![0], 42)])
            .unwrap();

        for collision in [
            "Sales Plan",
            "sales plan",
            "Sales-Plan",
            "Sales?Plan",
            "SALES//PLAN",
            "sales-plan",
        ] {
            match engine.create_cube(collision, &dims) {
                Err(BatchError::AlreadyExists(existing)) => assert_eq!(
                    existing, "Sales Plan",
                    "rejecting {collision:?} must name the surviving cube"
                ),
                other => panic!("creating {collision:?} should collide, got {other:?}"),
            }
        }

        // No extra cube was registered and the original cube's data survived.
        assert_eq!(
            engine.cube_names(),
            vec!["Sales".to_string(), "Sales Plan".to_string()]
        );
        assert_eq!(
            engine
                .snapshot("Sales Plan")
                .unwrap()
                .cube()
                .get_leaf(&[0])
                .unwrap(),
            Fixed::from(42),
            "the existing cube's data must not be overwritten"
        );
    }

    #[test]
    fn create_cube_never_overwrites_an_unloaded_on_disk_store() {
        // A cube can exist on disk without being loaded (e.g. its store failed
        // to open at boot). Creating a cube whose slug lands on that folder
        // must refuse rather than replace the snapshot and WAL.
        let root = scratch("create-ondisk-guard");
        std::fs::create_dir_all(&root).unwrap();
        let (region, _t, _r) = sum_dim("R", 2);
        let ghost = Cube::new("My Cube", vec![region]).unwrap();
        drop(Store::create(root.join("my-cube"), ghost).unwrap());

        let engine = Engine::from_stores(BTreeMap::new(), Arc::new(IdGen::default()))
            .with_cubes_dir(root.clone());
        let dims = [DimensionDef {
            name: "D".into(),
            elements: vec![("a".into(), ElementKind::Leaf)],
            edges: vec![],
            ..Default::default()
        }];
        assert!(matches!(
            engine.create_cube("My?Cube", &dims),
            Err(BatchError::AlreadyExists(_))
        ));
        assert!(!engine.has_cube("My?Cube"));

        // The on-disk store survived untouched (still the 3-member R cube, not
        // the 1-member D cube that would have replaced it).
        let survivor = Store::open(root.join("my-cube")).unwrap();
        assert_eq!(survivor.cube_name(), "My Cube");
        assert_eq!(survivor.cube().dimension(0).len(), 3);
    }

    #[test]
    fn define_and_set_attributes_commit() {
        let f = fixture("attrs");
        f.engine
            .define_attribute("Sales", None, "R", "Currency", AttributeKind::Text)
            .unwrap();
        f.engine
            .set_attribute_values(
                "Sales",
                None,
                "R",
                "Currency",
                &[("R0".into(), AttributeValue::Text("USD".into()))],
            )
            .unwrap();
        let snap = f.engine.snapshot("Sales").unwrap();
        let r0 = snap.cube().dimension(0).index_of("R0").unwrap();
        assert_eq!(
            snap.cube().dimension(0).attribute(r0, "Currency"),
            Some(&AttributeValue::Text("USD".into()))
        );

        // A kind conflict is rejected and changes nothing.
        assert!(matches!(
            f.engine
                .define_attribute("Sales", None, "R", "Currency", AttributeKind::Numeric),
            Err(BatchError::Invalid(_))
        ));
    }

    #[test]
    fn dimension_registry_register_and_grow() {
        let f = fixture("dim-registry");
        let mut product = Dimension::new("Product");
        product.add_leaf("Widget");
        let id = f.engine.register_dimension(product).unwrap();

        // The registry snapshot sees it at generation 0.
        let reg = f.engine.dimension_registry();
        assert_eq!(reg.get(id).unwrap().generation, 0);
        assert_eq!(reg.get(id).unwrap().dimension.index_of("Widget"), Some(0));

        // Growing it appends with a stable index and bumps the generation.
        let generation = f
            .engine
            .grow_dimension(
                id,
                &[ElementSpec {
                    dimension: "Product".into(),
                    name: "Gadget".into(),
                    kind: ElementKind::Leaf,
                }],
                &[],
            )
            .unwrap();
        assert_eq!(generation, 1);
        let reg = f.engine.dimension_registry();
        assert_eq!(reg.get(id).unwrap().dimension.index_of("Gadget"), Some(1));

        // Growing an unknown dimension is rejected.
        assert!(f
            .engine
            .grow_dimension(DimensionId(999_999), &[], &[])
            .is_err());
    }

    #[test]
    fn shared_dimension_grow_fans_out_to_referencing_cubes() {
        let engine = editable_engine("dim-fanout");

        // A shared Product dimension with one member.
        let mut product = Dimension::new("Product");
        product.add_leaf("Widget");
        let id = engine.register_dimension(product).unwrap();
        let product_def = engine
            .dimension_registry()
            .get(id)
            .unwrap()
            .to_dimension_def();

        let measure = || DimensionDef {
            name: "Measure".into(),
            elements: vec![("Amount".into(), ElementKind::Leaf)],
            edges: vec![],
            ..Default::default()
        };

        // Two cubes each materialize a copy of Product and record the reference.
        for cube in ["CubeA", "CubeB"] {
            engine
                .create_cube(cube, &[product_def.clone(), measure()])
                .unwrap();
            engine.attach_dimension(id, cube).unwrap();
        }

        // Growing the shared dimension fans out to both cubes.
        let generation = engine
            .grow_dimension(
                id,
                &[ElementSpec {
                    dimension: "Product".into(),
                    name: "Gadget".into(),
                    kind: ElementKind::Leaf,
                }],
                &[],
            )
            .unwrap();
        assert_eq!(generation, 1);

        for cube in ["CubeA", "CubeB"] {
            let snap = engine.snapshot(cube).unwrap();
            let product = snap
                .cube()
                .dimensions()
                .iter()
                .find(|d| d.name() == "Product")
                .unwrap();
            assert!(
                product.index_of("Gadget").is_some(),
                "{cube} should have received the fanned-out member"
            );
        }
        // The registry itself is at the new generation.
        assert_eq!(engine.dimension_registry().get(id).unwrap().generation, 1);
    }

    #[test]
    fn shrunk_lock_grow_persists_durably_and_fans_out_consistently() {
        // E2: with the shrunk dim_topology critical section (registry swap under
        // dim_topology; fan-out under registry_edit only), a DURABLE grow still
        // persists the registry before fanning out, and every referencing cube
        // converges. Interleaving a register + a second grow keeps all cubes
        // consistent, and a reload reconstructs identical state (proving the
        // durable-write-before-repack invariant survived the lock change).
        let root = scratch("e2-shrunk-lock");
        std::fs::create_dir_all(&root).unwrap();
        let dims_dir = root.join("dimensions");

        let engine = {
            let (region, _t, _r) = sum_dim("R", 1);
            let cube = Cube::new("Seed", vec![region]).unwrap();
            let store = Store::create(root.join("seed"), cube).unwrap();
            let mut stores = BTreeMap::new();
            stores.insert("Seed".to_string(), store);
            Engine::from_stores(stores, Arc::new(IdGen::default()))
                .with_cubes_dir(root.clone())
                .with_dimensions_dir(dims_dir.clone())
        };

        // A shared Product dimension referenced by two cubes.
        let mut product = Dimension::new("Product");
        product.add_leaf("Widget");
        let id = engine.register_dimension(product).unwrap();
        let product_def = engine
            .dimension_registry()
            .get(id)
            .unwrap()
            .to_dimension_def();
        let measure = || DimensionDef {
            name: "Measure".into(),
            elements: vec![("Amount".into(), ElementKind::Leaf)],
            edges: vec![],
            ..Default::default()
        };
        for cube in ["CubeA", "CubeB"] {
            engine
                .create_cube(cube, &[product_def.clone(), measure()])
                .unwrap();
            engine.attach_dimension(id, cube).unwrap();
        }

        // Grow the shared dimension (fan-out runs outside dim_topology now).
        engine
            .grow_dimension(
                id,
                &[ElementSpec {
                    dimension: "Product".into(),
                    name: "Gadget".into(),
                    kind: ElementKind::Leaf,
                }],
                &[],
            )
            .unwrap();
        // Interleave an unrelated register (needs only the brief dim_topology) and
        // a second grow to the same dimension (serialized by registry_edit).
        let mut other = Dimension::new("Other");
        other.add_leaf("X");
        engine.register_dimension(other).unwrap();
        engine
            .grow_dimension(
                id,
                &[ElementSpec {
                    dimension: "Product".into(),
                    name: "Gizmo".into(),
                    kind: ElementKind::Leaf,
                }],
                &[],
            )
            .unwrap();

        // Both cubes converged to BOTH new members, in order.
        for cube in ["CubeA", "CubeB"] {
            let snap = engine.snapshot(cube).unwrap();
            let product = snap
                .cube()
                .dimensions()
                .iter()
                .find(|d| d.name() == "Product")
                .unwrap();
            assert!(product.index_of("Gadget").is_some(), "{cube} has Gadget");
            assert!(product.index_of("Gizmo").is_some(), "{cube} has Gizmo");
        }
        assert_eq!(engine.dimension_registry().get(id).unwrap().generation, 2);
        drop(engine);

        // Reload: the durable registry (written under the shrunk lock) reconstructs
        // the same generation and the cubes reconcile forward identically.
        let reloaded = {
            let stores = {
                let mut s = BTreeMap::new();
                for (name, folder) in [("CubeA", "cubea"), ("CubeB", "cubeb"), ("Seed", "seed")] {
                    let path = root.join(folder);
                    if path.join("snapshot.model").exists() {
                        s.insert(name.to_string(), Store::open(&path).unwrap());
                    }
                }
                s
            };
            Engine::from_stores(stores, Arc::new(IdGen::default()))
                .with_cubes_dir(root.clone())
                .with_dimensions_dir(dims_dir.clone())
        };
        assert_eq!(reloaded.dimension_registry().get(id).unwrap().generation, 2);
        for cube in ["CubeA", "CubeB"] {
            let snap = reloaded.snapshot(cube).unwrap();
            let product = snap
                .cube()
                .dimensions()
                .iter()
                .find(|d| d.name() == "Product")
                .unwrap();
            assert!(product.index_of("Gadget").is_some());
            assert!(product.index_of("Gizmo").is_some());
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn registry_persists_and_reloads_across_restart() {
        let root = scratch("dim-reload");
        std::fs::create_dir_all(&root).unwrap();
        let dims_dir = root.join("dimensions");

        // First boot: a durable engine registers a shared dimension, materializes
        // it into a referencing cube, records the reference (plus a second cube),
        // and grows it once (fanning out to the materialized cube).
        let id = {
            let mut product = Dimension::new("Product");
            product.add_leaf("Widget");
            let mut measure = Dimension::new("Measure");
            measure.add_leaf("Amount");
            // The Sales cube materializes a copy of Product (ADR-0024 v1).
            let cube = Cube::new("Sales", vec![product.clone(), measure]).unwrap();
            let store = Store::create(root.join("Sales"), cube).unwrap();
            let mut stores = BTreeMap::new();
            stores.insert("Sales".to_string(), store);
            let engine = Engine::from_stores(stores, Arc::new(IdGen::default()))
                .with_cubes_dir(root.clone())
                .with_dimensions_dir(dims_dir.clone());

            let id = engine.register_dimension(product).unwrap();
            engine.attach_dimension(id, "Sales").unwrap();
            engine.attach_dimension(id, "Budget").unwrap();
            engine
                .grow_dimension(
                    id,
                    &[ElementSpec {
                        dimension: "Product".into(),
                        name: "Gadget".into(),
                        kind: ElementKind::Leaf,
                    }],
                    &[],
                )
                .unwrap();
            id
        };

        // Second boot: a fresh engine loading the same dimensions dir recovers the
        // registry at the grown generation, with stable indices and both refs.
        let engine = Engine::from_stores(BTreeMap::new(), Arc::new(IdGen::default()))
            .with_dimensions_dir(dims_dir);
        let reg = engine.dimension_registry();
        let shared = reg.get(id).expect("dimension reloaded");
        assert_eq!(shared.generation, 1);
        assert_eq!(shared.dimension.index_of("Widget"), Some(0));
        assert_eq!(shared.dimension.index_of("Gadget"), Some(1));
        assert_eq!(
            reg.referencing(id),
            vec!["Budget".to_string(), "Sales".to_string()]
        );

        // The id counter is seeded past the reloaded max, so a new registration
        // never collides with a restored id.
        let mut other = Dimension::new("Other");
        other.add_leaf("X");
        let new_id = engine.register_dimension(other).unwrap();
        assert!(new_id.0 > id.0);
    }

    #[test]
    fn present_but_unreadable_registry_fails_boot_instead_of_loading_empty() {
        // A corrupt index must fail loudly: silently substituting an empty
        // registry would quarantine every dimension body on the next save,
        // drop every cube reference set, and re-mint already-issued ids.
        let dir = scratch("dim-corrupt-registry");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.toml"), "this is not valid toml { [").unwrap();
        let res = Engine::from_stores(BTreeMap::new(), Arc::new(IdGen::default()))
            .try_with_dimensions_dir(&dir);
        assert!(
            matches!(res, Err(PersistError::Corrupt(_))),
            "a corrupt registry index must fail boot, not load as empty"
        );

        // An index whose listed dimension body is missing (the historical
        // save-crash window) is equally fatal.
        let dir = scratch("dim-missing-body-registry");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("index.toml"),
            "[[dimension]]\nid = 7\ngeneration = 0\nreferences = []\n",
        )
        .unwrap();
        let res = Engine::from_stores(BTreeMap::new(), Arc::new(IdGen::default()))
            .try_with_dimensions_dir(&dir);
        assert!(
            res.is_err(),
            "an index naming a missing dimension body must fail boot"
        );

        // An absent registry (first run) is still simply empty.
        let dir = scratch("dim-absent-registry");
        std::fs::create_dir_all(&dir).unwrap();
        let engine = Engine::from_stores(BTreeMap::new(), Arc::new(IdGen::default()))
            .try_with_dimensions_dir(&dir)
            .unwrap();
        assert!(engine.dimension_registry().is_empty());
    }

    #[test]
    fn registry_persist_failure_is_fail_closed() {
        // The durable registry write happens BEFORE the mutation is published
        // (ADR-0024): if the save fails, the caller gets an error and the
        // in-memory registry is unchanged — no minted id whose durable home
        // was silently lost. The save is forced to fail by occupying the
        // dimensions-dir path with a plain file.
        let root = scratch("dim-persist-failclosed");
        std::fs::create_dir_all(&root).unwrap();
        let blocker = root.join("dimensions");
        std::fs::write(&blocker, "not a directory").unwrap();
        let engine = Engine::from_stores(BTreeMap::new(), Arc::new(IdGen::default()))
            .with_dimensions_dir(&blocker);

        let mut product = Dimension::new("Product");
        product.add_leaf("Widget");
        assert!(
            matches!(
                engine.register_dimension(product),
                Err(BatchError::Persist(_))
            ),
            "a failed registry save must surface, not vanish"
        );
        assert!(
            engine.dimension_registry().is_empty(),
            "a failed save must leave the registry unchanged (fail-closed)"
        );
    }

    #[test]
    fn delete_and_promote_propagate_a_persist_failure() {
        // E3: `delete_dimension` and `promote_cube_dimension` used to persist
        // best-effort (a lost save vanished silently). They now propagate a save
        // failure as `DimensionError::Persist` / `PromoteError::Persist`, leaving
        // the in-memory registry unchanged. First build the registry with a WORKING
        // dir, then redirect the private `dimensions_dir` at a plain file so the
        // next save fails on every platform (a portable, dependency-free injection;
        // the same-file `tests` module may set the private field).
        let root = scratch("dim-delete-promote-persist");
        std::fs::create_dir_all(&root).unwrap();
        let dims_dir = root.join("dimensions");
        // `editable_engine` gives a "Sales" cube whose embedded "R" dimension we can
        // promote. Register a separate UNREFERENCED dimension to delete.
        let mut engine = editable_engine("delete-promote-persist").with_dimensions_dir(&dims_dir);
        let mut region = Dimension::new("Region");
        region.add_leaf("North");
        let del_id = engine.register_dimension(region).unwrap();

        // Break persistence: occupy the (child) save path with a plain file so
        // `create_dir_all` inside the registry save fails.
        let blocker = root.join("blocker");
        std::fs::write(&blocker, "not a directory").unwrap();
        engine.dimensions_dir = Some(blocker.join("sub"));

        // The delete now fails on save and must NOT drop the entry in memory.
        assert!(
            matches!(
                engine.delete_dimension(del_id),
                Err(DimensionError::Persist(_))
            ),
            "a failed registry save on delete must surface, not silently drop"
        );
        assert!(
            engine.dimension_registry().get(del_id).is_some(),
            "the failed delete must be rolled back (the entry stays)"
        );

        // The promote likewise fails on save and mints nothing durable/visible.
        let before = engine.dimension_registry().all().len();
        assert!(
            matches!(
                engine.promote_cube_dimension("Sales", "R"),
                Err(PromoteError::Persist(_))
            ),
            "a failed registry save on promote must surface"
        );
        assert_eq!(
            engine.dimension_registry().all().len(),
            before,
            "the failed promote must not publish a new registry entry"
        );
        assert!(
            engine.dimension_backing("Sales", "R").is_none(),
            "the failed promote must not attach a backing"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    // ---- structural editing fan-out (ADR-0036) ----

    /// Build an engine with two cubes that each materialize a shared Region
    /// dimension (North, South, East under Total) crossed with a local Measure
    /// (one Amount leaf), seed distinct numeric cells in each, and return the
    /// engine plus the shared dimension id.
    fn fanout_engine(name: &str) -> (Engine, DimensionId) {
        let engine = editable_engine(name);

        let mut region = Dimension::new("Region");
        let north = region.add_leaf("North");
        let south = region.add_leaf("South");
        let east = region.add_leaf("East");
        let total = region.add_consolidated("Total");
        region.add_child(total, north, 1).unwrap();
        region.add_child(total, south, 1).unwrap();
        region.add_child(total, east, 1).unwrap();
        let id = engine.register_dimension(region).unwrap();
        let region_def = engine
            .dimension_registry()
            .get(id)
            .unwrap()
            .to_dimension_def();

        let measure = || DimensionDef {
            name: "Measure".into(),
            elements: vec![("Amount".into(), ElementKind::Leaf)],
            edges: vec![],
            ..Default::default()
        };

        for cube in ["CubeA", "CubeB"] {
            engine
                .create_cube(cube, &[region_def.clone(), measure()])
                .unwrap();
            engine.attach_dimension(id, cube).unwrap();
        }

        // Seed cells: CubeA gets 10/20/30, CubeB gets 1/2/3 across North/South/East.
        for (cube, base) in [("CubeA", 10i32), ("CubeB", 1)] {
            let snap = engine.snapshot(cube).unwrap();
            let r = |n: &str| snap.cube().dimension(0).index_of(n).unwrap();
            let amount = snap.cube().dimension(1).index_of("Amount").unwrap();
            engine
                .apply_batch(
                    cube,
                    None,
                    &[
                        leaf(vec![r("North"), amount], base),
                        leaf(vec![r("South"), amount], base * 2),
                        leaf(vec![r("East"), amount], base * 3),
                    ],
                )
                .unwrap();
        }
        (engine, id)
    }

    /// Read a numeric cell in a cube by element names (resolving indices freshly).
    fn read_named(engine: &Engine, cube: &str, region: &str, measure: &str) -> Fixed {
        let snap = engine.snapshot(cube).unwrap();
        let r = snap.cube().dimension(0).index_of(region).unwrap();
        let m = snap.cube().dimension(1).index_of(measure).unwrap();
        snap.cube().get(&[r, m]).unwrap()
    }

    #[test]
    fn structural_reorder_fans_out_to_both_cubes() {
        let (engine, id) = fanout_engine("dim-reorder-fanout");

        engine
            .edit_dimension(
                "CubeA",
                "Region",
                &DimensionEdit::Reorder {
                    new_order: vec![
                        "East".into(),
                        "North".into(),
                        "South".into(),
                        "Total".into(),
                    ],
                },
            )
            .unwrap();

        // The registry generation advanced.
        assert_eq!(engine.dimension_registry().get(id).unwrap().generation, 1);

        // BOTH cubes see the new member order AND each value followed its member.
        for cube in ["CubeA", "CubeB"] {
            let snap = engine.snapshot(cube).unwrap();
            let names: Vec<String> = snap
                .cube()
                .dimension(0)
                .iter_elements()
                .map(|e| e.name.clone())
                .collect();
            assert_eq!(
                names,
                vec![
                    "East".to_string(),
                    "North".into(),
                    "South".into(),
                    "Total".into()
                ],
                "{cube} member order"
            );
        }
        // Values are intact and follow their members in each cube's own data.
        assert_eq!(
            read_named(&engine, "CubeA", "North", "Amount"),
            Fixed::from(10)
        );
        assert_eq!(
            read_named(&engine, "CubeA", "East", "Amount"),
            Fixed::from(30)
        );
        assert_eq!(
            read_named(&engine, "CubeA", "Total", "Amount"),
            Fixed::from(60)
        );
        assert_eq!(
            read_named(&engine, "CubeB", "South", "Amount"),
            Fixed::from(2)
        );
        assert_eq!(
            read_named(&engine, "CubeB", "Total", "Amount"),
            Fixed::from(6)
        );
    }

    #[test]
    fn edit_dimension_returns_the_requested_cubes_outcome() {
        // The fan-out visits referrers in sorted order ([CubeA, CubeB]); the
        // outcome returned to the caller must be the one for the cube the
        // caller named, not whichever referrer happened to be visited last
        // (clients feed this version into per-cube optimistic bookkeeping).
        let (engine, _id) = fanout_engine("dim-edit-outcome");

        let outcome = engine
            .edit_dimension(
                "CubeA",
                "Region",
                &DimensionEdit::Reorder {
                    new_order: vec![
                        "East".into(),
                        "North".into(),
                        "South".into(),
                        "Total".into(),
                    ],
                },
            )
            .unwrap();
        assert_eq!(
            Some(outcome.version),
            engine.version("CubeA"),
            "the outcome must be the requested cube's new version"
        );
        assert_ne!(
            Some(outcome.version),
            engine.version("CubeB"),
            "not the last fanned-out cube's"
        );
    }

    #[test]
    fn structural_delete_fans_out_to_both_cubes() {
        let (engine, id) = fanout_engine("dim-delete-fanout");

        engine
            .edit_dimension(
                "CubeB",
                "Region",
                &DimensionEdit::Delete {
                    element: "South".into(),
                },
            )
            .unwrap();

        assert_eq!(engine.dimension_registry().get(id).unwrap().generation, 1);
        // South is gone in the registry and in both cubes; remaining cells intact.
        assert!(engine
            .dimension_registry()
            .get(id)
            .unwrap()
            .dimension
            .index_of("South")
            .is_none());
        for cube in ["CubeA", "CubeB"] {
            let snap = engine.snapshot(cube).unwrap();
            assert!(
                snap.cube().dimension(0).index_of("South").is_none(),
                "{cube} should have South removed"
            );
        }
        // Totals reflect the removed member (its edge under Total was dropped):
        // CubeA: North 10 + East 30 = 40; CubeB: 1 + 3 = 4.
        assert_eq!(
            read_named(&engine, "CubeA", "North", "Amount"),
            Fixed::from(10)
        );
        assert_eq!(
            read_named(&engine, "CubeA", "East", "Amount"),
            Fixed::from(30)
        );
        assert_eq!(
            read_named(&engine, "CubeA", "Total", "Amount"),
            Fixed::from(40)
        );
        assert_eq!(
            read_named(&engine, "CubeB", "Total", "Amount"),
            Fixed::from(4)
        );
    }

    #[test]
    fn structural_edit_rejection_leaves_registry_untouched() {
        let (engine, id) = fanout_engine("dim-edit-reject");
        // Deleting a parent with children is rejected; nothing changes.
        let err = engine.edit_dimension(
            "CubeA",
            "Region",
            &DimensionEdit::Delete {
                element: "Total".into(),
            },
        );
        assert!(matches!(err, Err(BatchError::Invalid(_))));
        assert_eq!(
            engine.dimension_registry().get(id).unwrap().generation,
            0,
            "a rejected edit must not bump the generation"
        );
        // Both cubes still have all four members and their totals.
        assert_eq!(
            read_named(&engine, "CubeA", "Total", "Amount"),
            Fixed::from(60)
        );
        assert_eq!(
            read_named(&engine, "CubeB", "Total", "Amount"),
            Fixed::from(6)
        );
    }

    #[test]
    fn structural_edit_on_embedded_dimension_edits_only_that_cube() {
        // A cube with a purely embedded (non-registry) dimension.
        let engine = editable_engine("dim-embedded-edit");
        engine
            .create_cube(
                "Local",
                &[
                    DimensionDef {
                        name: "Thing".into(),
                        elements: vec![
                            ("A".into(), ElementKind::Leaf),
                            ("B".into(), ElementKind::Leaf),
                        ],
                        edges: vec![],
                        ..Default::default()
                    },
                    DimensionDef {
                        name: "Measure".into(),
                        elements: vec![("Amount".into(), ElementKind::Leaf)],
                        edges: vec![],
                        ..Default::default()
                    },
                ],
            )
            .unwrap();
        let snap = engine.snapshot("Local").unwrap();
        let a = snap.cube().dimension(0).index_of("A").unwrap();
        let b = snap.cube().dimension(0).index_of("B").unwrap();
        let amount = snap.cube().dimension(1).index_of("Amount").unwrap();
        engine
            .apply_batch(
                "Local",
                None,
                &[leaf(vec![a, amount], 5), leaf(vec![b, amount], 7)],
            )
            .unwrap();

        // Reorder the embedded dimension (no registry backing).
        engine
            .edit_dimension(
                "Local",
                "Thing",
                &DimensionEdit::Reorder {
                    new_order: vec!["B".into(), "A".into()],
                },
            )
            .unwrap();
        let snap = engine.snapshot("Local").unwrap();
        let names: Vec<String> = snap
            .cube()
            .dimension(0)
            .iter_elements()
            .map(|e| e.name.clone())
            .collect();
        assert_eq!(names, vec!["B".to_string(), "A".into()]);
        // Values followed their members.
        assert_eq!(read_named(&engine, "Local", "A", "Amount"), Fixed::from(5));
        assert_eq!(read_named(&engine, "Local", "B", "Amount"), Fixed::from(7));
    }

    #[test]
    fn commit_versions_strictly_increase_across_a_restart() {
        // E1: commit versions must never be reused across a restart. A run records
        // a durable high-water mark on every commit; a restart seeds the version
        // counter past it, so the first post-restart version is strictly greater
        // than any version the previous run handed out (no ABA aliasing of a
        // base_version / sandbox / cache key).
        let dir = scratch("e1-version-aba");

        // Build a store on disk and an engine whose commits persist the high-water.
        fn build_engine(dir: &std::path::Path, seed: u64) -> (Engine, Vec<u32>, Vec<u32>) {
            let (region, _rt, r) = sum_dim("R", 3);
            let (period, _pt, p) = sum_dim("P", 2);
            let store = if dir.join("snapshot.model").exists() {
                Store::open(dir).unwrap()
            } else {
                let cube = Cube::new("Sales", vec![region, period]).unwrap();
                Store::create(dir, cube).unwrap()
            };
            let mut stores = BTreeMap::new();
            stores.insert("Sales".to_string(), store);
            let engine = Engine::from_stores(stores, Arc::new(IdGen::starting_at(seed)))
                .with_commit_watermark_dir(dir.to_path_buf());
            (engine, r, p)
        }

        // First run: seed from the (absent) mark, i.e. start at 1, and commit twice.
        let seed0 = epiphany_persist::read_commit_watermark(&dir).saturating_add(1);
        let pre_restart_max;
        {
            let (engine, r, p) = build_engine(&dir, seed0);
            let v1 = engine
                .apply_batch("Sales", None, &[leaf(vec![r[0], p[0]], 10)])
                .unwrap()
                .version;
            let v2 = engine
                .apply_batch("Sales", None, &[leaf(vec![r[1], p[0]], 20)])
                .unwrap()
                .version;
            assert!(v2 > v1, "versions advance within a run");
            pre_restart_max = v2;
            // Drop the engine (simulated shutdown/crash): the mark is durable.
        }

        // The durable mark is at least the last version handed out.
        let mark = epiphany_persist::read_commit_watermark(&dir);
        assert!(
            mark >= pre_restart_max,
            "the high-water mark ({mark}) covers the last pre-restart version ({pre_restart_max})"
        );

        // Restart: seed the counter past the durable mark and commit again.
        let seed1 = mark.saturating_add(1);
        let (engine, r, p) = build_engine(&dir, seed1);
        let post_restart = engine
            .apply_batch("Sales", None, &[leaf(vec![r[2], p[0]], 30)])
            .unwrap()
            .version;
        assert!(
            post_restart > pre_restart_max,
            "the first post-restart version ({post_restart}) must exceed every \
             pre-restart version ({pre_restart_max}); a reused number would alias"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn commit_observer_receives_events_in_commit_order() {
        // E4: with an observer registered, every successful commit emits a
        // CommitEvent carrying the cube and its new version, in version order.
        // Without an observer (the default) nothing is emitted (proven by the
        // other tests, which register none and still pass).
        use std::sync::Mutex as StdMutex;

        #[derive(Default)]
        struct Recorder {
            events: StdMutex<Vec<CommitEvent>>,
        }
        impl CommitObserver for Recorder {
            fn on_commit(&self, event: &CommitEvent) {
                self.events.lock().unwrap().push(event.clone());
            }
        }

        let f = fixture("e4-commit-order");
        let recorder = Arc::new(Recorder::default());
        // Rebuild the engine wrapping the same store, now with an observer. (The
        // fixture engine has none; `with_commit_observer` is additive.)
        let engine = f.engine.with_commit_observer(recorder.clone());

        let v1 = engine
            .apply_batch("Sales", None, &[leaf(vec![f.r[0], f.p[0]], 10)])
            .unwrap()
            .version;
        let v2 = engine
            .define_rules("Sales", None, "['R':'R0'] = 1;".to_string())
            .unwrap()
            .version;
        let v3 = engine
            .apply_batch("Sales", None, &[leaf(vec![f.r[1], f.p[0]], 20)])
            .unwrap()
            .version;

        let events = recorder.events.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![
                CommitEvent {
                    cube: "Sales".into(),
                    version: v1,
                    sandbox: None,
                },
                CommitEvent {
                    cube: "Sales".into(),
                    version: v2,
                    sandbox: None,
                },
                CommitEvent {
                    cube: "Sales".into(),
                    version: v3,
                    sandbox: None,
                },
            ],
            "the observer saw every base commit exactly once, in version order"
        );
        // Versions are strictly increasing, so ordering by version is total.
        assert!(v1 < v2 && v2 < v3);
    }

    #[test]
    fn commit_observer_tags_a_sandbox_write_with_its_sandbox() {
        // A1: a private what-if write (`sandbox_set_cells`) emits a CommitEvent
        // carrying the sandbox name, so the API change feed can keep it off every
        // non-owner's stream; a base write carries `sandbox: None`.
        use std::sync::Mutex as StdMutex;
        #[derive(Default)]
        struct Recorder {
            events: StdMutex<Vec<CommitEvent>>,
        }
        impl CommitObserver for Recorder {
            fn on_commit(&self, event: &CommitEvent) {
                self.events.lock().unwrap().push(event.clone());
            }
        }

        let f = fixture("e4-sandbox-tag");
        let recorder = Arc::new(Recorder::default());
        let engine = f.engine.with_commit_observer(recorder.clone());

        // Create the sandbox (a base-cube definition commit), then write into it.
        engine.create_sandbox("Sales", None, "wf", "ann").unwrap();
        engine
            .sandbox_set_cells("Sales", None, "wf", &[leaf(vec![f.r[0], f.p[0]], 5)])
            .unwrap();

        let events = recorder.events.lock().unwrap().clone();
        assert_eq!(
            events.len(),
            2,
            "the create and the sandbox write both emit"
        );
        assert_eq!(events[0].sandbox, None, "create is a base commit");
        assert_eq!(
            events[1].sandbox.as_deref(),
            Some("wf"),
            "the what-if write carries its sandbox name for owner-only delivery"
        );
    }
}
