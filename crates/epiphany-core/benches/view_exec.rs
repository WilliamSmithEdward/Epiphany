//! View-execution benchmarks (ADR-0028): cold consolidated view latency against
//! the ROADMAP section 8 budget (p99 under about 1 s), and the measurement that
//! gates Stage B (parallel aggregation). Run with `cargo bench -p epiphany-core`.
//!
//! A self-contained harness (no external bench framework). It measures wall-clock
//! time over a deliberately consolidation-heavy fixture: a fully crossjoined grid
//! whose corner cell (all dimensions at their Total) scans the dense leaf product,
//! which is the worst case for aggregation and the only case where parallelism
//! across output cells could pay off.

use std::hint::black_box;
use std::time::{Duration, Instant};

use epiphany_core::{
    execute_view_with, AxisSpec, CellResolver, Cube, Dimension, Fixed, NoSetEvaluator, Parallelism,
    QueryError, Subset, View, Visibility,
};

/// A resolver that reads consolidation-aware values straight from the cube (no
/// rules), so the benchmark measures the aggregation work itself, not rules.
struct CubeCells<'a>(&'a Cube);

impl CellResolver for CubeCells<'_> {
    fn value(&self, coord: &[u32]) -> Result<Fixed, QueryError> {
        self.0.get(coord).map_err(|e| QueryError::Calc {
            message: e.to_string(),
        })
    }

    fn string_value(&self, coord: &[u32]) -> Result<Option<String>, QueryError> {
        Ok(self.0.get_string(coord).ok().flatten().map(str::to_string))
    }
}

/// A cube of `dims` dimensions, each with `leaves` leaves under a `Total`. A
/// sparse diagonal of leaf cells is populated so totals are non-zero while memory
/// stays tiny; the consolidation still enumerates the dense leaf product.
fn make_cube(dims: usize, leaves: u32) -> Cube {
    let mut ds = Vec::with_capacity(dims);
    for d in 0..dims {
        let mut dim = Dimension::new(format!("D{d}"));
        let ls: Vec<u32> = (0..leaves)
            .map(|i| dim.add_leaf(format!("d{d}_{i}")))
            .collect();
        let total = dim.add_consolidated("Total");
        for &l in &ls {
            dim.add_child(total, l, 1).unwrap();
        }
        ds.push(dim);
    }
    let mut cube = Cube::new("Bench", ds).unwrap();
    for i in 0..leaves {
        let coord: Vec<u32> = vec![i; dims];
        cube.set_leaf(&coord, Fixed::from(1)).unwrap();
    }
    cube
}

/// Members of dimension `d`: every leaf plus the `Total`.
fn all_members(d: usize, leaves: u32) -> Vec<String> {
    let mut m: Vec<String> = (0..leaves).map(|i| format!("d{d}_{i}")).collect();
    m.push("Total".to_string());
    m
}

/// A fully crossjoined grid: dim 0 on rows, dim 1 on columns (both leaves+Total),
/// every other dimension fixed at its Total in the context. So every cell is a
/// consolidation over at least the context dimensions, and the (Total, Total)
/// corner scans the full dense leaf product.
fn grid_view(dims: usize, leaves: u32) -> View {
    let rows = vec![AxisSpec::Members {
        dimension: "D0".to_string(),
        members: all_members(0, leaves),
    }];
    let columns = vec![AxisSpec::Members {
        dimension: "D1".to_string(),
        members: all_members(1, leaves),
    }];
    let context: Vec<(String, String)> = (2..dims)
        .map(|d| (format!("D{d}"), "Total".to_string()))
        .collect();
    View {
        name: "bench".to_string(),
        cube: "Bench".to_string(),
        owner: None,
        visibility: Visibility::Public,
        rows,
        columns,
        context,
        suppress_zeros: false,
    }
}

/// Best-of-`reps` ms/call for executing the fixture under a parallelism policy.
fn time(cube: &Cube, view: &View, par: Parallelism, reps: u32) -> (f64, usize) {
    let cells = CubeCells(cube);
    let eval = NoSetEvaluator;
    let lookup = |_d: &str, _n: &str| -> Option<&Subset> { None };
    let mut best = Duration::MAX;
    let mut ncells = 0usize;
    for _ in 0..reps {
        let start = Instant::now();
        let cs = execute_view_with(cube, view, &cells, &lookup, &eval, None, par).unwrap();
        best = best.min(start.elapsed());
        ncells = cs.cells.len();
        black_box(&cs);
    }
    (best.as_secs_f64() * 1e3, ncells)
}

/// Cold serial latency against the section-8 budget.
fn run(label: &str, dims: usize, leaves: u32, reps: u32) {
    let cube = make_cube(dims, leaves);
    let view = grid_view(dims, leaves);
    let (ms, ncells) = time(&cube, &view, Parallelism::serial(), reps);
    println!("{label:28} dims={dims} leaves={leaves:>4} cells={ncells:>7}  ->  {ms:>9.2} ms/call");
}

/// Serial vs parallel (auto) for ADR-0028 Stage B: prints the speedup.
fn compare(label: &str, dims: usize, leaves: u32, reps: u32) {
    let cube = make_cube(dims, leaves);
    let view = grid_view(dims, leaves);
    let (serial, ncells) = time(&cube, &view, Parallelism::serial(), reps);
    let (parallel, _) = time(&cube, &view, Parallelism::auto(), reps);
    println!(
        "{label:24} cells={ncells:>7}  serial {serial:>8.2} ms  parallel {parallel:>8.2} ms  ->  {:>4.2}x",
        serial / parallel,
    );
}

fn main() {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    println!("epiphany-core view-execution benchmarks (release, {cores} cores):");
    println!("-- serial execute_view, cold (budget p99 < ~1000 ms) --");
    // Representative report-sized views: comfortably within budget.
    run("representative_small", 3, 30, 8);
    run("representative_medium", 3, 64, 5);
    // Larger crossjoined consolidations where aggregation work dominates.
    run("large_3d", 3, 128, 3);
    run("large_4d", 4, 40, 3);
    run("stress_3d", 3, 200, 2);

    println!("-- serial vs parallel (ADR-0028 Stage B) --");
    compare("small (stays serial)", 3, 30, 8);
    compare("large_3d", 3, 128, 3);
    compare("large_4d", 4, 40, 3);
    compare("stress_3d", 3, 200, 2);
}
