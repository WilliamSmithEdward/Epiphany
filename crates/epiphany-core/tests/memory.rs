//! Bytes-per-cell measurement against the ROADMAP section 8 budget
//! (about 24 bytes per numeric leaf cell, including index overhead).
//!
//! A counting global allocator (test-only; `unsafe` is allowed here because this
//! is measurement scaffolding, not shipped engine code) tracks live heap bytes.
//! We snapshot the counter around populating a cube's cell store, so the delta
//! is the store's growth alone: the per-cell cost the budget targets.
//!
//! The measurement is deterministic: a fixed insertion sequence drives the hash
//! table through the same growth, so the final allocation is identical run to
//! run.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use epiphany_core::{Cube, Dimension, Fixed};

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
struct Counting;

// Safety: every method forwards to the system allocator with the same arguments
// and only adjusts an atomic byte counter, so the allocator contract is upheld.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = System.realloc(ptr, layout, new_size);
        if !new_ptr.is_null() {
            LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
            LIVE_BYTES.fetch_add(new_size, Ordering::Relaxed);
        }
        new_ptr
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn bytes_per_leaf_cell_within_budget() {
    // A single leaf dimension so the cube is narrow (a u64 key) and every write
    // lands a distinct cell. The count is chosen to fill the hash table to a high
    // load factor, which is the representative steady state.
    const N: u32 = 110_000;

    let mut account = Dimension::new("Account");
    let leaves: Vec<u32> = (0..N).map(|i| account.add_leaf(format!("a{i}"))).collect();
    let mut cube = Cube::new("Big", vec![account]).unwrap();

    // Measure only the cell store's growth: snapshot after the cube exists but
    // before any cell is populated.
    let before = LIVE_BYTES.load(Ordering::Relaxed);
    for &leaf in &leaves {
        cube.set_leaf(&[leaf], Fixed::from(1)).unwrap();
    }
    let after = LIVE_BYTES.load(Ordering::Relaxed);

    assert_eq!(cube.cell_count(), N as usize);
    let bytes_per_cell = (after - before) as f64 / f64::from(N);
    println!(
        "cell store: {} bytes for {N} cells = {bytes_per_cell:.2} bytes/cell",
        after - before
    );

    // Steady-state payload is (u64 key + i64 value) = 16 bytes; the SwissTable
    // adds about one control byte per slot plus load-factor slack. The budget is
    // about 24 bytes including this overhead.
    assert!(
        bytes_per_cell <= 24.0,
        "bytes/cell {bytes_per_cell:.2} exceeds the 24-byte budget (ROADMAP section 8)"
    );
}
