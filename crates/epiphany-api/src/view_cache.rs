//! A bounded, version-keyed view (cellset) cache (ADR-0028 Stage A).
//!
//! Executing a view recomputes its cellset from the snapshot every time. A
//! dashboard re-reading the same view between writes pays that cost repeatedly,
//! and the section-8 "cached or repeat view query" budget (p99 under about
//! 100 ms) has no implementation without a cache. This module is a read-through
//! cache of the core [`Cellset`] (the expensive, presentation-free result),
//! keyed so that a cached entry is only ever served for an identical read.
//!
//! Correctness and security live entirely in the key (ADR-0028 decision 3). A
//! cellset's values depend on six things, all of which the key captures
//! losslessly so the cache never relies on hash-collision resistance:
//!
//! - the cube and its MVCC version (every write bumps the version, so a stale
//!   entry can never be hit: it is self-invalidating, decision 6);
//! - the versions of every OTHER cube the target's rules read across cubes: a
//!   rule like `Sales.Revenue = Units * FX!Rate` reads cube `FX`, and a write to
//!   `FX` bumps `FX`'s version, not `Sales`'; keying only on the target's version
//!   would serve the pre-write `FX` value indefinitely (there is no TTL). ADR-0028
//!   decision 6 called version-keying "the entire invalidation story"; that holds
//!   only for single-cube dependencies, so this closes the cross-cube gap;
//! - the value-affecting view shape (rows, columns, context, suppress-zeros);
//! - the active what-if sandbox's scope id (per user, ADR-0014); and
//! - the caller's element deny mask (ADR-0015), as its exact denied set.
//!
//! The common case (no element denials, no sandbox, no cross-cube rules) is a
//! single entry shared by every principal. A masked or sandboxed read is keyed on
//! its precise context, so it is never served to a principal whose context differs
//! (fail-closed).
//!
//! The cache is split into two pools, a saved-view pool and a smaller ad-hoc
//! pool, so a client minting unbounded distinct ad-hoc shapes can only evict
//! ad-hoc entries, never the bounded saved-view entries. Each pool is bounded by
//! BOTH an entry count and an approximate byte budget (deterministic LRU eviction
//! by a monotonic counter), so version-keyed churn of large cellsets cannot pin
//! unbounded memory.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use epiphany_core::{AxisSpec, Cellset, ElementMask, Sandbox, View};

/// The default saved-view entry cap when none is configured.
pub const DEFAULT_ENTRIES: usize = 256;

/// The ad-hoc pool is this fraction of the saved-view cap (ad-hoc reads have
/// unbounded shape cardinality, so they get a smaller, isolated budget).
const ADHOC_SUBCAP_DIVISOR: usize = 4;

/// Cellsets larger than this are not cached (computed fresh, stored nothing), so
/// one very large view cannot dominate the cache's memory.
const MAX_CACHE_CELLS: usize = 1 << 20; // 1,048,576

/// Approximate resident bytes budget for the saved-view pool. Entry-count bounding
/// alone leaves the pool able to pin `entries` near-ceiling cellsets (256 x ~8 MB
/// = ~2 GB); this byte budget evicts by size too, so version-keyed churn of large
/// views cannot grow the cache without bound (ADR-0028 decision 5 bounds entries;
/// this bounds bytes). The ad-hoc pool gets a proportional fraction. Chosen so the
/// common small-cellset workload is unaffected while a hostile large-view churn is
/// capped well under the server's memory budget.
const DEFAULT_BYTE_BUDGET: usize = 64 * 1024 * 1024; // 64 MiB

/// Bytes charged for one cached cell: the `Fixed` value plus its slot in the
/// row-major grid AND the amortized per-cell cost of the axis-tuple member-name
/// vectors the cellset also retains (a deliberate over-estimate of the ~24-byte
/// cell floor, so the budget bounds true residency rather than under-counting the
/// String-heavy tuple vectors). Approximate by design: the budget is a bound, not
/// an allocator.
const APPROX_BYTES_PER_CELL: usize = 48;

/// Approximate the resident bytes of a cached cellset for the byte budget: the
/// dense value grid plus the two parallel per-cell channels (string/error) and the
/// axis-tuple name vectors, charged at [`APPROX_BYTES_PER_CELL`] per cell with a
/// small fixed floor so a zero-cell cellset still counts as one unit (never zero,
/// so the budget cannot be defeated by a flood of empty cellsets).
fn approx_bytes(cellset: &Cellset) -> usize {
    cellset
        .cells
        .len()
        .saturating_mul(APPROX_BYTES_PER_CELL)
        .max(APPROX_BYTES_PER_CELL)
}

/// The value-affecting shape of a view: only the fields that change cell values.
/// Name, owner, and visibility are excluded (they do not affect values).
#[derive(Clone, PartialEq, Eq, Hash)]
struct ViewShape {
    rows: Vec<AxisSpec>,
    columns: Vec<AxisSpec>,
    context: Vec<(String, String)>,
    suppress_zero_rows: bool,
    suppress_zero_columns: bool,
}

/// The element-security dimension of a key: an unmasked read (shared by all
/// principals) or a masked read keyed on its exact denied set.
#[derive(Clone, PartialEq, Eq, Hash)]
enum MaskKey {
    /// No element denials apply: the read is identical for every principal.
    Unmasked,
    /// The mask's exact denied `(dimension, index)` pairs (sorted, lossless).
    Masked(Vec<(u32, u32)>),
}

/// A complete, lossless view-cache key (ADR-0028 decision 3). Equality is exact,
/// so two distinct reads can never alias.
#[derive(Clone, PartialEq, Eq, Hash)]
struct ViewCacheKey {
    cube: String,
    version: u64,
    /// The `(cube name, version)` of every OTHER cube the target's rules read
    /// across cubes, sorted by name (lossless, deterministic). Empty when the
    /// target has no cross-cube rule references. A write to any of these cubes
    /// changes the key so the next read misses, closing the cross-cube staleness
    /// gap that per-cube version-keying alone leaves open.
    dep_versions: Vec<(String, u64)>,
    shape: ViewShape,
    sandbox_scope: Option<u64>,
    mask: MaskKey,
}

/// Everything that identifies a single view read: the cube and its version, the
/// view being executed, the active sandbox and element mask, and which pool the
/// read belongs to. Grouped so the cache entry point takes one context rather
/// than a long parameter list.
pub(crate) struct ViewRead<'a> {
    /// The cube name.
    pub cube: &'a str,
    /// The cube's MVCC version (the linearization point and invalidation key).
    pub version: u64,
    /// The `(cube name, version)` of every OTHER cube the target's rules read
    /// across cubes, so a write to a referenced cube invalidates this entry. The
    /// caller computes this from the target's rule source (its cross-cube
    /// references) before the lookup; empty when there are none. Order is
    /// normalized in [`build_key`], so the caller need not pre-sort.
    pub dep_versions: Vec<(String, u64)>,
    /// The view being executed (its value-affecting shape is keyed).
    pub view: &'a View,
    /// The active what-if sandbox, if any (ADR-0014).
    pub sandbox: Option<&'a Sandbox>,
    /// The caller's element deny mask, if any (ADR-0015).
    pub mask: Option<&'a ElementMask>,
    /// Whether this is an ad-hoc read (its own bounded pool) or a saved view.
    pub is_adhoc: bool,
}

fn build_key(read: &ViewRead) -> ViewCacheKey {
    let shape = ViewShape {
        rows: read.view.rows.clone(),
        columns: read.view.columns.clone(),
        context: read.view.context.clone(),
        suppress_zero_rows: read.view.suppress_zero_rows,
        suppress_zero_columns: read.view.suppress_zero_columns,
    };
    // Same scope id the calc memo uses (ADR-0014), so two distinct sandboxes never
    // alias WITHIN a run and a base read keys as None. (The engine's IdGen restarts
    // at 1 each boot, so this `created` id is unique only within a run; the
    // cross-restart ABA is the engine-deferred durable-high-water item, not the
    // cache's -- a cache entry never outlives the process that made it.)
    let sandbox_scope = read.sandbox.map(|s| s.created.max(1));
    // Fail-closed: only an absent or empty mask keys as Unmasked (shared); any
    // denial keys on the exact denied set.
    let mask = match read.mask {
        Some(m) if !m.is_empty() => MaskKey::Masked(m.denied_pairs()),
        _ => MaskKey::Unmasked,
    };
    // Normalize the cross-cube dependency versions to a deterministic order (by cube
    // name) so the key is order-independent and two identical reads always match.
    let mut dep_versions = read.dep_versions.clone();
    dep_versions.sort();
    dep_versions.dedup();
    ViewCacheKey {
        cube: read.cube.to_string(),
        version: read.version,
        dep_versions,
        shape,
        sandbox_scope,
        mask,
    }
}

/// One cached entry: the cellset, its last-access tick (for LRU), and its charged
/// approximate byte size (cached so eviction never re-walks the cellset).
struct Entry {
    cellset: Arc<Cellset>,
    tick: u64,
    bytes: usize,
}

/// One bounded pool: a map of key to [`Entry`] plus a monotonic counter. Eviction
/// is deterministic approximate-LRU by the monotonic tick (never a wall clock, per
/// the determinism mandate): on insert that would exceed EITHER the entry cap OR
/// the byte budget, the lowest-tick entries are dropped until both bounds hold. A
/// doubly-linked-list LRU is not worth the complexity at these sizes.
struct Pool {
    map: HashMap<ViewCacheKey, Entry>,
    cap: usize,
    /// Approximate byte budget for this pool's resident cellsets.
    byte_budget: usize,
    /// Running sum of every resident entry's `bytes` (kept in step with `map`).
    bytes: usize,
    tick: u64,
}

impl Pool {
    fn new(cap: usize, byte_budget: usize) -> Self {
        Self {
            map: HashMap::new(),
            cap,
            byte_budget,
            bytes: 0,
            tick: 0,
        }
    }

    fn get(&mut self, key: &ViewCacheKey) -> Option<Arc<Cellset>> {
        if self.cap == 0 || !self.map.contains_key(key) {
            return None;
        }
        self.tick += 1;
        let tick = self.tick;
        let entry = self.map.get_mut(key).expect("present");
        entry.tick = tick;
        Some(entry.cellset.clone())
    }

    /// Remove a key, keeping the running byte sum in step.
    fn remove(&mut self, key: &ViewCacheKey) {
        if let Some(entry) = self.map.remove(key) {
            self.bytes -= entry.bytes;
        }
    }

    /// Evict the single lowest-tick (least-recently-used) entry. Deterministic
    /// without a key tie-break: every tick value is produced monotonically and
    /// assigned to exactly one entry (on insert or access) and never reused, so at
    /// any moment all resident entries have DISTINCT ticks and the minimum is
    /// unique -- independent of hashmap iteration order. Returns whether anything
    /// was evicted.
    fn evict_one(&mut self) -> bool {
        let victim = self
            .map
            .iter()
            .min_by_key(|(_, e)| e.tick)
            .map(|(k, _)| k.clone());
        match victim {
            Some(k) => {
                self.remove(&k);
                true
            }
            None => false,
        }
    }

    fn insert(&mut self, key: ViewCacheKey, value: Arc<Cellset>, bytes: usize) {
        if self.cap == 0 {
            return;
        }
        self.tick += 1;
        let tick = self.tick;
        // Replacing an existing key: drop its old byte charge first.
        self.remove(&key);
        // Evict lowest-tick entries until inserting this one keeps BOTH bounds:
        // at most `cap` entries, and at most `byte_budget` bytes. Guard on a
        // non-empty map so a single oversized entry (already gated by
        // MAX_CACHE_CELLS at the call site) still lands rather than looping.
        while (self.map.len() + 1 > self.cap || self.bytes.saturating_add(bytes) > self.byte_budget)
            && !self.map.is_empty()
        {
            if !self.evict_one() {
                break;
            }
        }
        self.bytes = self.bytes.saturating_add(bytes);
        self.map.insert(
            key,
            Entry {
                cellset: value,
                tick,
                bytes,
            },
        );
    }
}

/// A bounded, version-keyed cache of executed cellsets (ADR-0028 Stage A).
/// Shared behind an `Arc` in `AppState`; cheap to clone (the `Arc`).
pub struct ViewCache {
    saved: Mutex<Pool>,
    adhoc: Mutex<Pool>,
    enabled: bool,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl ViewCache {
    /// Build a cache whose saved-view pool holds `entries` items (0 disables the
    /// cache entirely) within the default byte budget. The ad-hoc pool is a
    /// fraction of both bounds.
    pub fn new(entries: usize) -> Self {
        Self::with_budget(entries, DEFAULT_BYTE_BUDGET)
    }

    /// Build a cache with an explicit saved-view entry cap and byte budget. The
    /// ad-hoc pool gets a proportional fraction of each. Exposed so a test can pin a
    /// tiny byte budget and prove byte-eviction without allocating gigabytes.
    pub fn with_budget(entries: usize, byte_budget: usize) -> Self {
        let subcap = if entries == 0 {
            0
        } else {
            (entries / ADHOC_SUBCAP_DIVISOR).max(1)
        };
        let sub_bytes = if entries == 0 {
            0
        } else {
            (byte_budget / ADHOC_SUBCAP_DIVISOR).max(1)
        };
        Self {
            saved: Mutex::new(Pool::new(entries, byte_budget)),
            adhoc: Mutex::new(Pool::new(subcap, sub_bytes)),
            enabled: entries > 0,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Return the cached cellset for this exact read, or compute it with
    /// `compute`, cache it (subject to the per-entry ceiling and pool cap), and
    /// return it. `compute` runs without any lock held. On a miss two callers may
    /// both compute the same result; that is harmless (idempotent) and avoids
    /// holding a lock across the expensive calc.
    pub(crate) fn get_or_compute<F, E>(&self, read: ViewRead, compute: F) -> Result<Arc<Cellset>, E>
    where
        F: FnOnce() -> Result<Cellset, E>,
    {
        if !self.enabled {
            return Ok(Arc::new(compute()?));
        }
        let pool = if read.is_adhoc {
            &self.adhoc
        } else {
            &self.saved
        };
        let key = build_key(&read);
        if let Some(hit) = pool.lock().expect("view cache poisoned").get(&key) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(hit);
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let cellset = Arc::new(compute()?);
        if cellset.cells.len() <= MAX_CACHE_CELLS {
            let bytes = approx_bytes(&cellset);
            pool.lock()
                .expect("view cache poisoned")
                .insert(key, cellset.clone(), bytes);
        }
        Ok(cellset)
    }

    /// Whether the cache is on (a non-zero entry cap).
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Cumulative cache hits (for the operator dashboard and tests).
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Cumulative cache misses.
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// The number of resident entries across both pools.
    pub fn entries(&self) -> usize {
        let saved = self.saved.lock().expect("view cache poisoned").map.len();
        let adhoc = self.adhoc.lock().expect("view cache poisoned").map.len();
        saved + adhoc
    }
}

impl Default for ViewCache {
    fn default() -> Self {
        Self::new(DEFAULT_ENTRIES)
    }
}

impl std::fmt::Debug for ViewCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViewCache")
            .field("enabled", &self.enabled)
            .field("entries", &self.entries())
            .field("hits", &self.hits())
            .field("misses", &self.misses())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epiphany_core::Visibility;
    use std::cell::Cell;

    fn view_named(name: &str, member: &str) -> View {
        View {
            name: name.to_string(),
            cube: "Sales".to_string(),
            owner: None,
            visibility: Visibility::Public,
            rows: vec![AxisSpec::Members {
                dimension: "Region".to_string(),
                members: vec![member.to_string()],
            }],
            columns: vec![AxisSpec::Members {
                dimension: "Measure".to_string(),
                members: vec!["Sales".to_string()],
            }],
            context: Vec::new(),
            suppress_zero_rows: false,
            suppress_zero_columns: false,
        }
    }

    /// An empty cellset is enough for key/eviction tests: keys vary by shape,
    /// version, mask, and sandbox, not by cell content.
    fn empty_cellset() -> Cellset {
        Cellset {
            row_dimensions: Vec::new(),
            column_dimensions: Vec::new(),
            row_tuples: Vec::new(),
            column_tuples: Vec::new(),
            context: Vec::new(),
            cells: Vec::new(),
            cell_strings: Vec::new(),
            cell_errors: Vec::new(),
            suppressed_row_tuples: Vec::new(),
            suppressed_column_tuples: Vec::new(),
        }
    }

    fn compute_counting(counter: &Cell<u32>) -> impl FnOnce() -> Result<Cellset, ()> + '_ {
        move || {
            counter.set(counter.get() + 1);
            Ok(empty_cellset())
        }
    }

    fn mask_denying(pairs: &[(u32, u32)]) -> ElementMask {
        // Two dimensions sized generously; deny the requested indices.
        let mut by_dim: Vec<Vec<u32>> = vec![Vec::new(), Vec::new()];
        for &(d, i) in pairs {
            by_dim[d as usize].push(i);
        }
        ElementMask::from_denied(&[16, 16], &by_dim)
    }

    /// A read context for the "Sales" cube, the only cube these tests use. No
    /// cross-cube dependencies (empty `dep_versions`); [`read_with_deps`] covers
    /// the cross-cube case.
    fn read<'a>(
        version: u64,
        view: &'a View,
        sandbox: Option<&'a Sandbox>,
        mask: Option<&'a ElementMask>,
        is_adhoc: bool,
    ) -> ViewRead<'a> {
        ViewRead {
            cube: "Sales",
            version,
            dep_versions: Vec::new(),
            view,
            sandbox,
            mask,
            is_adhoc,
        }
    }

    /// A read whose target has a cross-cube dependency on the given `(cube,
    /// version)` pairs (unsorted is fine; `build_key` normalizes).
    fn read_with_deps<'a>(version: u64, deps: Vec<(String, u64)>, view: &'a View) -> ViewRead<'a> {
        ViewRead {
            cube: "Sales",
            version,
            dep_versions: deps,
            view,
            sandbox: None,
            mask: None,
            is_adhoc: false,
        }
    }

    #[test]
    fn caches_and_serves_a_repeat_read() {
        let cache = ViewCache::new(8);
        let view = view_named("v", "North");
        let calls = Cell::new(0u32);
        let r = || read(1, &view, None, None, false);
        cache.get_or_compute(r(), compute_counting(&calls)).unwrap();
        cache.get_or_compute(r(), compute_counting(&calls)).unwrap();
        assert_eq!(calls.get(), 1, "second read must hit the cache");
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 1);
    }

    #[test]
    fn version_bump_misses() {
        let cache = ViewCache::new(8);
        let view = view_named("v", "North");
        let calls = Cell::new(0u32);
        cache
            .get_or_compute(read(1, &view, None, None, false), compute_counting(&calls))
            .unwrap();
        cache
            .get_or_compute(read(2, &view, None, None, false), compute_counting(&calls))
            .unwrap();
        assert_eq!(calls.get(), 2, "a new version must recompute");
    }

    #[test]
    fn different_shape_does_not_alias() {
        let cache = ViewCache::new(8);
        let calls = Cell::new(0u32);
        let north = view_named("v", "North");
        let south = view_named("v", "South");
        cache
            .get_or_compute(read(1, &north, None, None, false), compute_counting(&calls))
            .unwrap();
        cache
            .get_or_compute(read(1, &south, None, None, false), compute_counting(&calls))
            .unwrap();
        assert_eq!(
            calls.get(),
            2,
            "a different member list is a different read"
        );
    }

    #[test]
    fn mask_difference_keeps_entries_distinct_but_same_denials_share() {
        let cache = ViewCache::new(8);
        let view = view_named("v", "North");
        let calls = Cell::new(0u32);
        let m_south = mask_denying(&[(0, 1)]);
        let m_south_again = mask_denying(&[(0, 1)]);
        let m_north = mask_denying(&[(0, 0)]);

        // Unmasked entry.
        cache
            .get_or_compute(read(1, &view, None, None, false), compute_counting(&calls))
            .unwrap();
        // Masked entry (distinct from unmasked).
        cache
            .get_or_compute(
                read(1, &view, None, Some(&m_south), false),
                compute_counting(&calls),
            )
            .unwrap();
        // Identical denials: shares the masked entry (no recompute).
        cache
            .get_or_compute(
                read(1, &view, None, Some(&m_south_again), false),
                compute_counting(&calls),
            )
            .unwrap();
        // Different denials: a distinct entry (recompute).
        cache
            .get_or_compute(
                read(1, &view, None, Some(&m_north), false),
                compute_counting(&calls),
            )
            .unwrap();
        assert_eq!(
            calls.get(),
            3,
            "unmasked + deny-south + deny-north = 3 entries; the repeat deny-south hits"
        );
    }

    #[test]
    fn sandbox_scope_keeps_entries_distinct() {
        let cache = ViewCache::new(8);
        let view = view_named("v", "North");
        let calls = Cell::new(0u32);
        let sb_a = Sandbox::new("a", "u", 7);
        let sb_b = Sandbox::new("b", "u", 9);
        cache
            .get_or_compute(read(1, &view, None, None, false), compute_counting(&calls))
            .unwrap();
        cache
            .get_or_compute(
                read(1, &view, Some(&sb_a), None, false),
                compute_counting(&calls),
            )
            .unwrap();
        cache
            .get_or_compute(
                read(1, &view, Some(&sb_b), None, false),
                compute_counting(&calls),
            )
            .unwrap();
        assert_eq!(calls.get(), 3, "base + two distinct sandboxes = 3 entries");
    }

    #[test]
    fn adhoc_and_saved_pools_are_separate() {
        let cache = ViewCache::new(8);
        let view = view_named("v", "North");
        let calls = Cell::new(0u32);
        cache
            .get_or_compute(read(1, &view, None, None, false), compute_counting(&calls))
            .unwrap();
        cache
            .get_or_compute(read(1, &view, None, None, true), compute_counting(&calls))
            .unwrap();
        assert_eq!(calls.get(), 2, "the same read in each pool is two entries");
    }

    #[test]
    fn eviction_bounds_the_pool() {
        let cache = ViewCache::new(4);
        let view = view_named("v", "North");
        let calls = Cell::new(0u32);
        for version in 1..=20u64 {
            cache
                .get_or_compute(
                    read(version, &view, None, None, false),
                    compute_counting(&calls),
                )
                .unwrap();
        }
        assert!(cache.entries() <= 4, "saved pool stays within its cap");
    }

    #[test]
    fn disabled_never_caches() {
        let cache = ViewCache::new(0);
        let view = view_named("v", "North");
        let calls = Cell::new(0u32);
        for _ in 0..3 {
            cache
                .get_or_compute(read(1, &view, None, None, false), compute_counting(&calls))
                .unwrap();
        }
        assert_eq!(calls.get(), 3, "a disabled cache recomputes every time");
        assert_eq!(cache.entries(), 0);
    }

    /// A cellset with `n` cells, so the byte budget charges it a real, non-zero
    /// size (`empty_cellset` has zero cells and would never trip the budget).
    fn sized_cellset(n: usize) -> Cellset {
        let mut cs = empty_cellset();
        cs.cells = vec![epiphany_core::Fixed::ZERO; n];
        cs.cell_strings = vec![None; n];
        cs.cell_errors = vec![None; n];
        cs
    }

    /// A write to a CROSS-CUBE dependency (not the target) must miss: the target's
    /// own version is unchanged, but the referenced cube's version moved, so a
    /// value a fresh read would recompute is never served stale (the item-1 bug).
    #[test]
    fn cross_cube_dependency_version_bump_misses() {
        let cache = ViewCache::new(8);
        let view = view_named("v", "North");
        let calls = Cell::new(0u32);
        // Target Sales@1 depends on FX@1.
        cache
            .get_or_compute(
                read_with_deps(1, vec![("FX".to_string(), 1)], &view),
                compute_counting(&calls),
            )
            .unwrap();
        // FX is written (FX@2) while Sales stays @1: must recompute.
        cache
            .get_or_compute(
                read_with_deps(1, vec![("FX".to_string(), 2)], &view),
                compute_counting(&calls),
            )
            .unwrap();
        assert_eq!(
            calls.get(),
            2,
            "a write to a referenced cube invalidates the cached cellset"
        );
        // Re-reading at FX@2 now hits (the dependency version is stable again).
        cache
            .get_or_compute(
                read_with_deps(1, vec![("FX".to_string(), 2)], &view),
                compute_counting(&calls),
            )
            .unwrap();
        assert_eq!(calls.get(), 2, "the stable dependency version hits");
    }

    /// The dependency list is order-independent: the same set of `(cube, version)`
    /// in a different order is the same read (the key normalizes order).
    #[test]
    fn cross_cube_dependency_order_does_not_matter() {
        let cache = ViewCache::new(8);
        let view = view_named("v", "North");
        let calls = Cell::new(0u32);
        cache
            .get_or_compute(
                read_with_deps(1, vec![("FX".into(), 3), ("Rates".into(), 5)], &view),
                compute_counting(&calls),
            )
            .unwrap();
        cache
            .get_or_compute(
                read_with_deps(1, vec![("Rates".into(), 5), ("FX".into(), 3)], &view),
                compute_counting(&calls),
            )
            .unwrap();
        assert_eq!(calls.get(), 1, "reordered dependencies are the same key");
    }

    /// The byte budget bounds residency independently of the entry cap: with a
    /// generous entry cap but a tiny byte budget, churning large cellsets evicts by
    /// size, so the pool never pins more than a couple of them.
    #[test]
    fn byte_budget_bounds_residency() {
        // Entry cap 100 (never the binding bound here); byte budget fits ~2 of
        // these 1000-cell cellsets (1000 * APPROX_BYTES_PER_CELL each).
        let budget = 2 * 1000 * APPROX_BYTES_PER_CELL + 1;
        let cache = ViewCache::with_budget(100, budget);
        let view = view_named("v", "North");
        for version in 1..=20u64 {
            cache
                .get_or_compute(read(version, &view, None, None, false), || {
                    Ok::<_, ()>(sized_cellset(1000))
                })
                .unwrap();
        }
        assert!(
            cache.entries() <= 2,
            "the byte budget evicts large cellsets by size, got {} entries",
            cache.entries()
        );
        assert!(cache.entries() >= 1, "at least the most recent entry stays");
    }

    /// A re-inserted key (same cube/version/shape) does not double-count bytes: its
    /// old charge is dropped first, so residency reflects distinct entries only.
    #[test]
    fn reinsert_does_not_leak_byte_accounting() {
        let budget = 3 * 500 * APPROX_BYTES_PER_CELL + 1;
        let cache = ViewCache::with_budget(100, budget);
        let view = view_named("v", "North");
        // Insert the SAME read (version 1) many times: each miss recomputes and
        // re-inserts the same key. If bytes leaked per re-insert, the running sum
        // would climb and evict; instead it stays at one entry's charge.
        for _ in 0..10 {
            // Force a miss each time by disabling reuse: a fresh compute returning a
            // sized cellset. The key is identical, so it is a replace, not growth.
            let mut pool = cache.saved.lock().unwrap();
            pool.map.clear();
            pool.bytes = 0;
            drop(pool);
            cache
                .get_or_compute(read(1, &view, None, None, false), || {
                    Ok::<_, ()>(sized_cellset(500))
                })
                .unwrap();
        }
        let pool = cache.saved.lock().unwrap();
        assert_eq!(pool.map.len(), 1, "one distinct entry");
        assert_eq!(
            pool.bytes,
            approx_bytes(&sized_cellset(500)),
            "byte sum matches the single resident entry, no leak"
        );
    }
}
