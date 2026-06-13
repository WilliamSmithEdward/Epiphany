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
use epiphany_core::{Cube, ModelError};
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
            BatchError::Persist(e) => write!(f, "could not persist batch: {e}"),
        }
    }
}

impl std::error::Error for BatchError {}

/// One published, immutable cube version.
#[derive(Debug)]
struct Published {
    version: Version,
    cube: Cube,
}

/// A lock-free, immutable read snapshot of one cube. Holding it pins a single
/// committed version for the life of a query; concurrent commits never mutate it.
#[derive(Debug, Clone)]
pub struct ReadSnapshot {
    inner: Arc<Published>,
}

impl ReadSnapshot {
    /// The pinned cube version.
    pub fn cube(&self) -> &Cube {
        &self.inner.cube
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
                    cube: store.cube().clone(),
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
            cube: writer.store.cube().clone(),
        }));
        writer.version = version;

        Ok(CommitOutcome { version })
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

#[cfg(test)]
mod tests {
    use super::*;
    use epiphany_core::{Dimension, Fixed};

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
}
