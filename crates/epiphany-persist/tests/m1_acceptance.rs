//! M1 acceptance suite: "It stores and aggregates" (end of Phase 1).
//!
//! Proves the Phase 1 definition of done end to end, under deterministic mode
//! (fixed model, seeded RNG, fixed hash seed, exact numerics):
//!
//! 1. Load a multi-dimensional cube from model-as-code text.
//! 2. Read consolidated values correctly (weighted rollups and an alternate
//!    hierarchy).
//! 3. Export to canonical text that is a fixed point (export-load-export is the
//!    identity).
//! 4. Write leaves through a durable store, then restart and recover byte-
//!    identical state (both the WAL-replay and snapshot-checkpoint paths).
//! 5. Aggregate correctly and persist at scale on a benchmark model.
//!
//! The per-cell memory budget (ROADMAP section 8) is gated by the allocator
//! probe in `epiphany-core/tests/memory.rs`; read/write/aggregate latency is
//! tracked by `epiphany-core/benches/cube_ops.rs`. This suite covers the
//! functional DoD and aggregation correctness.

use std::fs;
use std::path::PathBuf;

use epiphany_core::{Cube, Dimension, Fixed};
use epiphany_determinism::DeterministicRng;
use epiphany_persist::Store;

/// A human-authored model-as-code document: a 2-D `Sales` cube over a `Region`
/// dimension (with an alternate `Coastal` rollup) and a `Version` dimension
/// (with a weighted `Variance = Actual - Budget`).
const MODEL: &str = r#"
format = "epiphany-model/v0"

[cube]
name = "Sales"
dimensions = ["Region", "Version"]

[[dimension]]
name = "Region"
elements = [
    { name = "North", kind = "leaf" },
    { name = "South", kind = "leaf" },
    { name = "East", kind = "leaf" },
    { name = "Total", kind = "consolidated" },
    { name = "Coastal", kind = "consolidated" },
]
edges = [
    { parent = "Total", child = "North", weight = 1 },
    { parent = "Total", child = "South", weight = 1 },
    { parent = "Total", child = "East", weight = 1 },
    { parent = "Coastal", child = "North", weight = 1 },
    { parent = "Coastal", child = "East", weight = 1 },
]

[[dimension]]
name = "Version"
elements = [
    { name = "Actual", kind = "leaf" },
    { name = "Budget", kind = "leaf" },
    { name = "Variance", kind = "consolidated" },
]
edges = [
    { parent = "Variance", child = "Actual", weight = 1 },
    { parent = "Variance", child = "Budget", weight = -1 },
]

[[cell]]
coord = ["North", "Actual"]
value = "100"

[[cell]]
coord = ["North", "Budget"]
value = "80"

[[cell]]
coord = ["South", "Actual"]
value = "50"

[[cell]]
coord = ["East", "Actual"]
value = "30"
"#;

/// A unique scratch directory for one test (cleaned up at the end).
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("epiphany-m1-{}-{name}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    dir
}

/// Resolve `(dimension_index, element_name)` to an element index in `cube`.
fn idx(cube: &Cube, dim: usize, name: &str) -> u32 {
    cube.dimension(dim)
        .index_of(name)
        .unwrap_or_else(|| panic!("missing element {name}"))
}

#[test]
fn loads_multidim_cube_and_reads_consolidations() {
    let cube = Cube::from_model_text(MODEL).expect("load model from text");
    assert_eq!(cube.rank(), 2);

    let total = idx(&cube, 0, "Total");
    let coastal = idx(&cube, 0, "Coastal");
    let north = idx(&cube, 0, "North");
    let actual = idx(&cube, 1, "Actual");
    let budget = idx(&cube, 1, "Budget");
    let variance = idx(&cube, 1, "Variance");

    // Simple rollups.
    assert_eq!(cube.get(&[total, actual]).unwrap(), Fixed::from(180)); // 100+50+30
    assert_eq!(cube.get(&[total, budget]).unwrap(), Fixed::from(80)); // only North
                                                                      // Alternate hierarchy (Coastal = North + East).
    assert_eq!(cube.get(&[coastal, actual]).unwrap(), Fixed::from(130));
    // Weighted consolidation (Variance = Actual - Budget).
    assert_eq!(cube.get(&[north, variance]).unwrap(), Fixed::from(20)); // 100-80
    assert_eq!(cube.get(&[total, variance]).unwrap(), Fixed::from(100)); // 180-80
    assert_eq!(cube.get(&[coastal, variance]).unwrap(), Fixed::from(50)); // 130-80
}

#[test]
fn exports_to_canonical_fixed_point_text() {
    let cube = Cube::from_model_text(MODEL).expect("load model from text");
    let once = cube.to_model_text().unwrap();
    // Export -> load -> export must reproduce byte-identical text.
    let twice = Cube::from_model_text(&once)
        .unwrap()
        .to_model_text()
        .unwrap();
    assert_eq!(
        once, twice,
        "model-as-code export must be a canonical fixed point"
    );

    // Deterministic: loading the same source again yields the same canonical text.
    let again = Cube::from_model_text(MODEL)
        .unwrap()
        .to_model_text()
        .unwrap();
    assert_eq!(once, again);
}

#[test]
fn recovers_identical_state_after_restart() {
    let dir = scratch("recover");
    let cube = Cube::from_model_text(MODEL).expect("load model from text");

    let total = idx(&cube, 0, "Total");
    let budget = idx(&cube, 1, "Budget");
    let variance = idx(&cube, 1, "Variance");
    let south = idx(&cube, 0, "South");
    let east = idx(&cube, 0, "East");

    let before = {
        let mut store = Store::create(&dir, cube).unwrap();
        // Write additional leaves (fsync on by default -> crash-durable).
        store.set_leaf(&[south, budget], Fixed::from(40)).unwrap();
        store.set_leaf(&[east, budget], Fixed::from(10)).unwrap();
        let text = store.cube().to_model_text().unwrap();
        // Drop without an explicit checkpoint: recovery must replay the WAL.
        text
    };

    let store = Store::open(&dir).unwrap();
    let after = store.cube().to_model_text().unwrap();
    assert_eq!(
        before, after,
        "recovered state must be byte-identical to pre-restart"
    );

    // Consolidations recompute correctly over the recovered cube.
    // Total Budget = 80 + 40 + 10 = 130; Total Variance = 180 - 130 = 50.
    assert_eq!(
        store.cube().get(&[total, budget]).unwrap(),
        Fixed::from(130)
    );
    assert_eq!(
        store.cube().get(&[total, variance]).unwrap(),
        Fixed::from(50)
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn benchmark_model_aggregates_and_persists_at_scale() {
    const N: u32 = 20_000;
    let dir = scratch("scale");

    // A benchmark model: one large leaf dimension under a single Total.
    let mut account = Dimension::new("Account");
    let leaves: Vec<u32> = (0..N).map(|i| account.add_leaf(format!("a{i}"))).collect();
    let total = account.add_consolidated("Total");
    for &leaf in &leaves {
        account.add_child(total, leaf, 1).unwrap();
    }
    let cube = Cube::new("Bench", vec![account]).unwrap();

    // Populate with seeded pseudo-random values (deterministic), tracking the
    // expected sum. Batch the WAL (sync off) and checkpoint once at the end.
    let mut rng = DeterministicRng::new(20_240_612);
    let expected: i64 = {
        let mut store = Store::create(&dir, cube).unwrap();
        store.set_sync(false);
        let mut sum: i64 = 0;
        for &leaf in &leaves {
            let v = 1 + (rng.next_u64() % 999) as i32; // 1..=999, never clears
            store.set_leaf(&[leaf], Fixed::from(v)).unwrap();
            sum += i64::from(v);
        }
        store.checkpoint().unwrap(); // full-persist: snapshot + clear WAL
        sum
    };

    // Restart and recover from the snapshot; aggregation is exact and matches.
    let store = Store::open(&dir).unwrap();
    assert_eq!(store.cube().cell_count(), N as usize);
    assert_eq!(
        store.cube().get(&[total]).unwrap(),
        Fixed::from_int(expected).unwrap()
    );

    fs::remove_dir_all(&dir).ok();
}
