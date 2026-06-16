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
//! cellset's values depend on five things, all of which the key captures
//! losslessly so the cache never relies on hash-collision resistance:
//!
//! - the cube and its MVCC version (every write bumps the version, so a stale
//!   entry can never be hit: it is self-invalidating, decision 6);
//! - the value-affecting view shape (rows, columns, context, suppress-zeros);
//! - the active what-if sandbox's scope id (per user, ADR-0014); and
//! - the caller's element deny mask (ADR-0015), as its exact denied set.
//!
//! The common case (no element denials, no sandbox) is a single entry shared by
//! every principal. A masked or sandboxed read is keyed on its precise context,
//! so it is never served to a principal whose context differs (fail-closed).
//!
//! The cache is split into two pools, a saved-view pool and a smaller ad-hoc
//! pool, so a client minting unbounded distinct ad-hoc shapes can only evict
//! ad-hoc entries, never the bounded saved-view entries.

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

/// The value-affecting shape of a view: only the fields that change cell values.
/// Name, owner, and visibility are excluded (they do not affect values).
#[derive(Clone, PartialEq, Eq, Hash)]
struct ViewShape {
    rows: Vec<AxisSpec>,
    columns: Vec<AxisSpec>,
    context: Vec<(String, String)>,
    suppress_zeros: bool,
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
        suppress_zeros: read.view.suppress_zeros,
    };
    // Same scope id the calc memo uses (ADR-0014), so two distinct sandboxes
    // never alias and a base read keys as None.
    let sandbox_scope = read.sandbox.map(|s| s.created.max(1));
    // Fail-closed: only an absent or empty mask keys as Unmasked (shared); any
    // denial keys on the exact denied set.
    let mask = match read.mask {
        Some(m) if !m.is_empty() => MaskKey::Masked(m.denied_pairs()),
        _ => MaskKey::Unmasked,
    };
    ViewCacheKey {
        cube: read.cube.to_string(),
        version: read.version,
        shape,
        sandbox_scope,
        mask,
    }
}

/// One bounded pool: a map of key to (cellset, last-access tick) plus a
/// monotonic counter. Eviction is approximate-LRU: on insert over the cap, the
/// lowest-tick entry is dropped. A doubly-linked-list LRU is not worth the
/// complexity at these sizes.
struct Pool {
    map: HashMap<ViewCacheKey, (Arc<Cellset>, u64)>,
    cap: usize,
    tick: u64,
}

impl Pool {
    fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            cap,
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
        entry.1 = tick;
        Some(entry.0.clone())
    }

    fn insert(&mut self, key: ViewCacheKey, value: Arc<Cellset>) {
        if self.cap == 0 {
            return;
        }
        self.tick += 1;
        let tick = self.tick;
        if self.map.len() >= self.cap && !self.map.contains_key(&key) {
            if let Some(victim) = self
                .map
                .iter()
                .min_by_key(|(_, (_, t))| *t)
                .map(|(k, _)| k.clone())
            {
                self.map.remove(&victim);
            }
        }
        self.map.insert(key, (value, tick));
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
    /// cache entirely). The ad-hoc pool is a fraction of that.
    pub fn new(entries: usize) -> Self {
        let subcap = if entries == 0 {
            0
        } else {
            (entries / ADHOC_SUBCAP_DIVISOR).max(1)
        };
        Self {
            saved: Mutex::new(Pool::new(entries)),
            adhoc: Mutex::new(Pool::new(subcap)),
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
            pool.lock()
                .expect("view cache poisoned")
                .insert(key, cellset.clone());
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
            suppress_zeros: false,
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

    /// A read context for the "Sales" cube, the only cube these tests use.
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
            view,
            sandbox,
            mask,
            is_adhoc,
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
}
