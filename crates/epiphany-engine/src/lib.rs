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
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use epiphany_core::{
    AttributeKind, AttributeValue, CellResolver, Connection, Cube, Dimension, DimensionDef,
    EdgeSpec, ElementMask, ElementSpec, Fixed, Flow, FlowTest, Job, Model, ModelError, QueryError,
    RuleSet, RuleTest, Sandbox, Subset, View,
};
use epiphany_determinism::IdGen;
use epiphany_persist::{load_registry, save_registry, PersistError, RegistryEntry, Store};

pub use epiphany_persist::CellWrite;

mod dimensions;
pub use dimensions::{DimensionId, DimensionRegistry, SharedDimension};

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
/// linearization point, which makes versions and notifications deterministic).
#[derive(Debug)]
struct Writer {
    store: Store,
    version: Version,
}

/// Per-cube shared state: the serialized writer plus the lock-free published version.
struct CubeState {
    writer: Mutex<Writer>,
    published: ArcSwap<Published>,
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
    /// Serializes registry mutations. The lock order is `dim_topology` before any
    /// per-cube `writer` (ADR-0024) to preclude an AB/BA deadlock.
    dim_topology: Arc<Mutex<()>>,
    /// On-disk root (`<data_dir>/dimensions`) for the durable registry. `None`
    /// keeps the registry in memory only (e.g. tests without a directory).
    dimensions_dir: Option<PathBuf>,
    /// Stable, monotonic source of `DimensionId`s, seeded past the max loaded id
    /// on `with_dimensions_dir` so ids never collide across restarts (the commit
    /// `IdGen` restarts each boot, so dimension ids use this separate counter).
    next_dim_id: Arc<AtomicU64>,
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
                    writer: Mutex::new(Writer { store, version: 0 }),
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
            dimensions_dir: None,
            next_dim_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Enable durable shared dimensions by loading the registry from `dir`
    /// (`<data_dir>/dimensions`) and persisting future mutations there (ADR-0024,
    /// SD-2). On load it seeds the dimension-id counter past the max stored id (so
    /// new ids never collide across restarts) and reconciles every referencing
    /// cube forward to the loaded dimension (idempotent append), so a cube that
    /// missed a fan-out before a crash catches up.
    pub fn with_dimensions_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let entries = load_registry(&dir).unwrap_or_default();
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
        for (cube, els, edgs) in reconcile {
            if self.has_cube(&cube) {
                let _ = self.define_elements(&cube, None, &els, &edgs);
            }
        }
        self
    }

    /// Enable runtime cube creation by telling the engine where to create new
    /// cube stores on disk (ADR-0021). The directory matches the boot layout
    /// (`<data_dir>/cubes/<name>/`), so a created cube reloads on restart.
    pub fn with_cubes_dir(mut self, cubes_dir: impl Into<PathBuf>) -> Self {
        self.cubes_dir = Some(cubes_dir.into());
        self
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
    pub fn register_dimension(&self, dimension: Dimension) -> DimensionId {
        let _topo = self
            .dim_topology
            .lock()
            .expect("dim_topology mutex poisoned");
        let id = DimensionId(self.next_dim_id.fetch_add(1, Ordering::SeqCst));
        let shared = Arc::new(SharedDimension::new(id, dimension));
        let mut next = (**self.dimensions.load()).clone();
        next.put(shared);
        self.dimensions.store(Arc::new(next));
        self.persist_registry();
        id
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
        Ok(self.register_dimension(built.dimension))
    }

    /// Record that `cube` references shared dimension `id` (ADR-0024 v1): the cube
    /// has materialized a copy of it, and a later grow fans out to the cube.
    pub fn attach_dimension(&self, id: DimensionId, cube: &str) {
        let _topo = self
            .dim_topology
            .lock()
            .expect("dim_topology mutex poisoned");
        let mut next = (**self.dimensions.load()).clone();
        next.attach(id, cube);
        self.dimensions.store(Arc::new(next));
        self.persist_registry();
    }

    /// Persist the current registry to `dimensions_dir`, if durable. Best-effort:
    /// a write failure is not fatal because every referencing cube already holds
    /// its own durable copy of the dimension (the registry reconciles on reload).
    fn persist_registry(&self) {
        let Some(dir) = self.dimensions_dir.as_ref() else {
            return;
        };
        let registry = self.dimensions.load();
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
        let _ = save_registry(dir, &entries);
    }

    /// Append elements/edges to a registered shared dimension, publish the new
    /// generation, and **fan the same append out to every referencing cube**
    /// (ADR-0024 v1: materialized references). The registry grow is the
    /// authoritative event; per-cube application reuses the append-only,
    /// idempotent `define_elements` path keyed by the dimension's name, so every
    /// referencing cube converges to the grown dimension. A rejected change leaves
    /// the registry untouched. Holds `dim_topology` across the per-cube writer
    /// locks (the `dim_topology` -> `writer` order, never the reverse).
    pub fn grow_dimension(
        &self,
        id: DimensionId,
        elements: &[ElementSpec],
        edges: &[EdgeSpec],
    ) -> Result<u64, BatchError> {
        let _topo = self
            .dim_topology
            .lock()
            .expect("dim_topology mutex poisoned");
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

        // Publish the new registry generation (the authoritative event).
        let mut next = (**self.dimensions.load()).clone();
        next.put(Arc::new(grown));
        self.dimensions.store(Arc::new(next));

        // Fan out to every referencing cube, re-stamping the dimension name so the
        // append targets each cube's materialized copy of this dimension.
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
        self.persist_registry();
        for cube in snapshot.referencing(id) {
            if self.has_cube(&cube) {
                self.define_elements(&cube, None, &els, &edgs)?;
            }
        }
        Ok(generation)
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
        let _topo = self
            .dim_topology
            .lock()
            .expect("dim_topology mutex poisoned");
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

        // Record the cube as a referrer of each shared dimension it materialized.
        if !refs.is_empty() {
            let mut next = (**self.dimensions.load()).clone();
            for id in &refs {
                next.attach(*id, name);
            }
            self.dimensions.store(Arc::new(next));
            self.persist_registry();
        }
        Ok(outcome)
    }

    /// Delete a shared dimension from the registry. Fail-closed: a dimension still
    /// referenced by any cube cannot be deleted (the cubes keep their materialized
    /// copies; only the library entry would vanish). Holds `dim_topology` so the
    /// reference check and removal are atomic against a concurrent attach.
    pub fn delete_dimension(&self, id: DimensionId) -> Result<(), DimensionError> {
        let _topo = self
            .dim_topology
            .lock()
            .expect("dim_topology mutex poisoned");
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
        self.dimensions.store(Arc::new(next));
        self.persist_registry();
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
        let _topo = self
            .dim_topology
            .lock()
            .expect("dim_topology mutex poisoned");
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
        self.dimensions.store(Arc::new(next));
        self.persist_registry();
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
        let mut writer = state.writer.lock().expect("writer mutex poisoned");

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
            Err(e) => return Err(BatchError::Persist(e)),
        }

        // Publish the new immutable version (lock-free for readers), then record
        // the version (also the per-cube CAS base) under the held writer lock.
        let version = self.ids.next_id();
        state.published.store(Arc::new(Published {
            version,
            model: writer.store.model().clone(),
        }));
        writer.version = version;

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

    /// Define (create or replace) a flow definition and publish a new version.
    /// The source is stored verbatim; the caller validates it (via the flow
    /// layer) first.
    pub fn define_flow(
        &self,
        cube: &str,
        base: Option<Version>,
        flow: Flow,
    ) -> Result<CommitOutcome, BatchError> {
        self.define(cube, base, |store| store.define_flow(flow))
    }

    /// Delete a flow by name and publish a new version. A missing flow returns
    /// [`BatchError::Invalid`].
    pub fn delete_flow(
        &self,
        cube: &str,
        base: Option<Version>,
        name: &str,
    ) -> Result<CommitOutcome, BatchError> {
        self.define(cube, base, |store| {
            if store.delete_flow(name)? {
                Ok(())
            } else {
                Err(PersistError::Query(QueryError::Calc {
                    message: format!("no flow '{name}'"),
                }))
            }
        })
    }

    /// Define (create or replace) a scheduled job (ADR-0013) and publish a new
    /// version. The caller validates the job's step flows first.
    pub fn define_job(
        &self,
        cube: &str,
        base: Option<Version>,
        job: Job,
    ) -> Result<CommitOutcome, BatchError> {
        self.define(cube, base, |store| store.define_job(job))
    }

    /// Delete a job by name and publish a new version. A missing job returns
    /// [`BatchError::Invalid`].
    pub fn delete_job(
        &self,
        cube: &str,
        base: Option<Version>,
        name: &str,
    ) -> Result<CommitOutcome, BatchError> {
        self.define(cube, base, |store| {
            if store.delete_job(name)? {
                Ok(())
            } else {
                Err(PersistError::Query(QueryError::Calc {
                    message: format!("no job '{name}'"),
                }))
            }
        })
    }

    /// Define (create or replace) a flow unit test and publish a new version.
    pub fn define_flow_test(
        &self,
        cube: &str,
        base: Option<Version>,
        test: FlowTest,
    ) -> Result<CommitOutcome, BatchError> {
        self.define(cube, base, |store| store.define_flow_test(test))
    }

    /// Delete a flow test by name and publish a new version. A missing test
    /// returns [`BatchError::Invalid`].
    pub fn delete_flow_test(
        &self,
        cube: &str,
        base: Option<Version>,
        name: &str,
    ) -> Result<CommitOutcome, BatchError> {
        self.define(cube, base, |store| {
            if store.delete_flow_test(name)? {
                Ok(())
            } else {
                Err(PersistError::Query(QueryError::Calc {
                    message: format!("no flow test '{name}'"),
                }))
            }
        })
    }

    /// Define (create or replace) a data-source connection and publish a new
    /// version.
    pub fn define_connection(
        &self,
        cube: &str,
        base: Option<Version>,
        connection: Connection,
    ) -> Result<CommitOutcome, BatchError> {
        self.define(cube, base, |store| store.define_connection(connection))
    }

    /// Delete a connection by name and publish a new version. A missing
    /// connection returns [`BatchError::Invalid`].
    pub fn delete_connection(
        &self,
        cube: &str,
        base: Option<Version>,
        name: &str,
    ) -> Result<CommitOutcome, BatchError> {
        self.define(cube, base, |store| {
            if store.delete_connection(name)? {
                Ok(())
            } else {
                Err(PersistError::Query(QueryError::Calc {
                    message: format!("no connection '{name}'"),
                }))
            }
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
        self.define(cube, base, |store| {
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
    /// readers, and existing cubes are untouched.
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
        let _topo = self.topology.lock().expect("topology mutex poisoned");
        if self.cubes.load().contains_key(name) {
            return Err(BatchError::AlreadyExists(name.to_string()));
        }

        // Persist on disk in the boot layout so the cube reloads on restart.
        let store = Store::create(cubes_dir.join(name), cube).map_err(BatchError::Persist)?;
        let version = self.ids.next_id();
        let state = Arc::new(CubeState {
            published: ArcSwap::from_pointee(Published {
                version,
                model: store.model().clone(),
            }),
            writer: Mutex::new(Writer { store, version }),
        });

        // Copy-on-write swap: clone the map (Arc values are cheap to clone), add
        // the new cube, and publish atomically. In-flight reads keep their map.
        let mut next = (**self.cubes.load()).clone();
        next.insert(name.to_string(), state);
        self.cubes.store(Arc::new(next));
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
        self.define_with(cube, base, op)
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
        let state = self
            .state(cube)
            .ok_or_else(|| BatchError::UnknownCube(cube.to_string()))?;
        let mut writer = state.writer.lock().expect("writer mutex poisoned");

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
            Err(PersistError::BatchRejected { index, source }) => {
                return Err(BatchError::Rejected { index, source })
            }
            Err(PersistError::Query(e)) => return Err(BatchError::Invalid(e)),
            // A bare model rejection from a definition op (e.g. unknown dimension,
            // kind conflict, alias collision) is a client-correctable error, not a
            // durability failure, so it surfaces as Invalid (422), not Persist.
            Err(PersistError::Model(e)) => return Err(BatchError::Invalid(QueryError::Model(e))),
            Err(e) => return Err(BatchError::Persist(e)),
        };

        let version = self.ids.next_id();
        state.published.store(Arc::new(Published {
            version,
            model: writer.store.model().clone(),
        }));
        writer.version = version;
        Ok((CommitOutcome { version }, value))
    }

    /// Force a checkpoint (full-persist) of a cube.
    pub fn checkpoint(&self, cube: &str) -> Result<(), BatchError> {
        let state = self
            .state(cube)
            .ok_or_else(|| BatchError::UnknownCube(cube.to_string()))?;
        let mut writer = state.writer.lock().expect("writer mutex poisoned");
        writer.store.checkpoint().map_err(BatchError::Persist)
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
                    },
                    DimensionDef {
                        name: "Period".into(),
                        elements: vec![("Jan".into(), ElementKind::Leaf)],
                        edges: vec![],
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
        let id = f.engine.register_dimension(product);

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
        let id = engine.register_dimension(product);
        let product_def = engine
            .dimension_registry()
            .get(id)
            .unwrap()
            .to_dimension_def();

        let measure = || DimensionDef {
            name: "Measure".into(),
            elements: vec![("Amount".into(), ElementKind::Leaf)],
            edges: vec![],
        };

        // Two cubes each materialize a copy of Product and record the reference.
        for cube in ["CubeA", "CubeB"] {
            engine
                .create_cube(cube, &[product_def.clone(), measure()])
                .unwrap();
            engine.attach_dimension(id, cube);
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

            let id = engine.register_dimension(product);
            engine.attach_dimension(id, "Sales");
            engine.attach_dimension(id, "Budget");
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
        let new_id = engine.register_dimension(other);
        assert!(new_id.0 > id.0);
    }
}
