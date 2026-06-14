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
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use epiphany_core::{
    CellResolver, Connection, Cube, EdgeSpec, ElementSpec, Fixed, Flow, FlowTest, Model,
    ModelError, QueryError, RuleSet, RuleTest, Sandbox, Subset, View,
};
use epiphany_determinism::IdGen;
use epiphany_persist::{PersistError, Store};

pub use epiphany_persist::CellWrite;

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
            BatchError::Persist(e) => write!(f, "could not persist batch: {e}"),
        }
    }
}

impl std::error::Error for BatchError {}

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
#[derive(Clone)]
pub struct Engine {
    cubes: Arc<BTreeMap<String, CubeState>>,
    ids: Arc<IdGen>,
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
        let cubes = stores
            .into_iter()
            .map(|(name, store)| {
                let published = ArcSwap::from_pointee(Published {
                    version: 0,
                    model: store.model().clone(),
                });
                let state = CubeState {
                    writer: Mutex::new(Writer { store, version: 0 }),
                    published,
                };
                (name, state)
            })
            .collect();
        Engine {
            cubes: Arc::new(cubes),
            ids,
        }
    }

    /// The cube names, in deterministic sorted order.
    pub fn cube_names(&self) -> Vec<String> {
        self.cubes.keys().cloned().collect()
    }

    /// Whether a cube exists.
    pub fn has_cube(&self, cube: &str) -> bool {
        self.cubes.contains_key(cube)
    }

    /// Take a lock-free read snapshot of a cube. Never blocks and is never blocked
    /// by writers; the returned snapshot is a consistent whole-cube version.
    pub fn snapshot(&self, cube: &str) -> Option<ReadSnapshot> {
        let state = self.cubes.get(cube)?;
        Some(ReadSnapshot {
            inner: state.published.load_full(),
        })
    }

    /// The current committed version of a cube.
    pub fn version(&self, cube: &str) -> Option<Version> {
        self.cubes.get(cube).map(|s| s.published.load().version)
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
            .cubes
            .get(cube)
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
            .cubes
            .get(cube)
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
            .cubes
            .get(cube)
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
    /// Build a value resolver bound to a pinned snapshot.
    fn resolver(&self, snapshot: &ReadSnapshot) -> Box<dyn CellResolver>;
}

/// The default factory: a resolver reading stored cells, byte-identical to the
/// no-rules behavior. Stateless.
#[derive(Debug, Clone, Copy, Default)]
pub struct StoredCellsFactory;

impl CellResolverFactory for StoredCellsFactory {
    fn resolver(&self, snapshot: &ReadSnapshot) -> Box<dyn CellResolver> {
        Box::new(StoredResolver {
            snapshot: snapshot.clone(),
        })
    }
}

/// A [`CellResolver`] that owns a pinned snapshot and reads stored values.
#[derive(Debug)]
struct StoredResolver {
    snapshot: ReadSnapshot,
}

impl CellResolver for StoredResolver {
    fn value(&self, coord: &[u32]) -> Result<Fixed, QueryError> {
        Ok(self.snapshot.cube().get(coord)?)
    }

    fn string_value(&self, coord: &[u32]) -> Result<Option<String>, QueryError> {
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
}
