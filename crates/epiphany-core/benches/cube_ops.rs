//! Throughput and latency benchmarks for the cube hot paths, checked against the
//! ROADMAP section 8 budgets (bulk-load about 1M cells/sec/core; point and
//! cold-view query latency). Run with `cargo bench -p epiphany-core`.
//!
//! A self-contained harness (no external bench framework) so it builds on the
//! GNU toolchain and adds no dependencies. It measures wall-clock time and is not
//! part of the deterministic test suite: it validates the performance mandate, it
//! does not gate correctness. Numbers are best-of-N to cut scheduler noise; they
//! are indicative, not promises.

use std::hint::black_box;
use std::time::{Duration, Instant};

use epiphany_core::{Cube, Dimension, Fixed};

/// An `Account` dimension with `n` leaves under one consolidated `Total`.
fn account_dim(n: u32) -> (Dimension, u32, Vec<u32>) {
    let mut d = Dimension::new("Account");
    let leaves: Vec<u32> = (0..n).map(|i| d.add_leaf(format!("a{i}"))).collect();
    let total = d.add_consolidated("Total");
    for &leaf in &leaves {
        d.add_child(total, leaf, 1).unwrap();
    }
    (d, total, leaves)
}

/// A one-dimensional cube with every leaf populated.
fn populated(n: u32) -> (Cube, u32, Vec<u32>) {
    let (d, total, leaves) = account_dim(n);
    let mut cube = Cube::new("Bench", vec![d]).unwrap();
    for &leaf in &leaves {
        cube.set_leaf(&[leaf], Fixed::from(1)).unwrap();
    }
    (cube, total, leaves)
}

/// Bulk-load: populate an empty cube, reported as cells/second (best of `reps`).
fn bulk_load() {
    const N: u32 = 100_000;
    const REPS: u32 = 12;
    let (dim, _total, leaves) = account_dim(N);

    let mut best = Duration::MAX;
    for _ in 0..REPS {
        let mut cube = Cube::new("Bench", vec![dim.clone()]).unwrap();
        let start = Instant::now();
        for &leaf in &leaves {
            cube.set_leaf(&[leaf], Fixed::from(1)).unwrap();
        }
        best = best.min(start.elapsed());
        black_box(cube.cell_count());
    }
    let per_sec = f64::from(N) / best.as_secs_f64();
    println!(
        "bulk_load          {N} cells in {:>7.3} ms  ->  {:>6.2} M cells/sec/core  (budget ~1.0)",
        best.as_secs_f64() * 1e3,
        per_sec / 1e6,
    );
}

/// Point read: single leaf lookups, reported as ns/op.
fn point_read() {
    const ITERS: usize = 5_000_000;
    let (cube, _total, leaves) = populated(100_000);

    let start = Instant::now();
    for i in 0..ITERS {
        let leaf = leaves[i % leaves.len()];
        black_box(cube.get_leaf(&[black_box(leaf)]).unwrap());
    }
    let elapsed = start.elapsed();
    println!(
        "get_leaf_point     {:>7.1} ns/op  over {ITERS} reads",
        elapsed.as_secs_f64() * 1e9 / ITERS as f64,
    );
}

/// Cold consolidated read: a `Total` that scans every populated cell.
fn consolidated_read() {
    const ITERS: u32 = 1_000;
    let (cube, total, _leaves) = populated(100_000);

    let start = Instant::now();
    for _ in 0..ITERS {
        black_box(cube.get(&[black_box(total)]).unwrap());
    }
    let per_call = start.elapsed().as_secs_f64() * 1e3 / f64::from(ITERS);
    println!(
        "get_consolidated   {:>7.3} ms/call  (scans 100k cells; budget p99 < ~1000 ms)",
        per_call,
    );
}

fn main() {
    println!("epiphany-core cube benchmarks (release):");
    bulk_load();
    point_read();
    consolidated_read();
}
