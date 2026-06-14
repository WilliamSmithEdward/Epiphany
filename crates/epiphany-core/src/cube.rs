//! Cubes: the sparse multidimensional cell store and consolidation.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use crate::{Dimension, ElementKind, Fixed, ModelError};

/// A cube coordinate: one element index per dimension, in dimension order.
pub type Coord = Box<[u32]>;

/// Number of bits needed to represent element indices `0..len` (at least 1).
/// Element counts are `u32`, so this never exceeds 32.
fn bits_for(len: u32) -> u32 {
    if len <= 1 {
        1
    } else {
        u32::BITS - (len - 1).leading_zeros()
    }
}

/// Per-dimension bit packing for narrow coordinates (ADR-0006).
///
/// Each dimension occupies a bit field, sized to its element count, at a fixed
/// offset. When the total width fits in 64 bits the cube packs each coordinate
/// into a single `u64` key; otherwise it falls back to a boxed slice.
#[derive(Clone, Debug)]
struct Layout {
    offsets: Vec<u32>,
    masks: Vec<u64>,
    narrow: bool,
}

impl Layout {
    fn new(dimensions: &[Dimension]) -> Self {
        let mut offsets = Vec::with_capacity(dimensions.len());
        let mut masks = Vec::with_capacity(dimensions.len());
        let mut offset: u32 = 0;
        for dim in dimensions {
            let width = bits_for(dim.len()); // width <= 32
            offsets.push(offset);
            masks.push((1u64 << width) - 1);
            offset = offset.saturating_add(width);
        }
        Layout {
            offsets,
            masks,
            narrow: offset <= 64,
        }
    }

    fn pack(&self, coord: &[u32]) -> u64 {
        let mut key: u64 = 0;
        for (d, &idx) in coord.iter().enumerate() {
            key |= u64::from(idx) << self.offsets[d];
        }
        key
    }

    fn component(&self, key: u64, d: usize) -> u32 {
        ((key >> self.offsets[d]) & self.masks[d]) as u32
    }

    fn unpack(&self, key: u64, rank: usize) -> Vec<u32> {
        (0..rank).map(|d| self.component(key, d)).collect()
    }
}

/// The sparse cell store, keyed for memory efficiency (ADR-0006), generic over
/// the value type so numeric (`Fixed`) and string (interned id) cells share one
/// keying scheme.
///
/// Splitting the key type at the map level (rather than a per-key enum) keeps a
/// narrow numeric entry a bare `(u64, Fixed)` with no discriminant or alignment
/// padding: 16 bytes plus about one control byte of table overhead, comfortably
/// within the per-cell budget (ROADMAP section 8). Very wide cubes use a boxed-
/// slice key, paying a heap allocation per cell.
#[derive(Clone, Debug)]
enum CellStore<V> {
    Narrow(HashMap<u64, V, FxBuildHasher>),
    Wide(HashMap<Box<[u32]>, V, FxBuildHasher>),
}

impl<V> CellStore<V> {
    fn new(narrow: bool) -> Self {
        if narrow {
            CellStore::Narrow(HashMap::default())
        } else {
            CellStore::Wide(HashMap::default())
        }
    }

    fn len(&self) -> usize {
        match self {
            CellStore::Narrow(m) => m.len(),
            CellStore::Wide(m) => m.len(),
        }
    }

    fn get(&self, layout: &Layout, coord: &[u32]) -> Option<&V> {
        match self {
            CellStore::Narrow(m) => m.get(&layout.pack(coord)),
            CellStore::Wide(m) => m.get(coord),
        }
    }

    fn put(&mut self, layout: &Layout, coord: &[u32], value: V) {
        match self {
            CellStore::Narrow(m) => {
                m.insert(layout.pack(coord), value);
            }
            CellStore::Wide(m) => {
                m.insert(coord.into(), value);
            }
        }
    }

    fn clear(&mut self, layout: &Layout, coord: &[u32]) {
        match self {
            CellStore::Narrow(m) => {
                m.remove(&layout.pack(coord));
            }
            CellStore::Wide(m) => {
                m.remove(coord);
            }
        }
    }

    fn entries<'a>(&'a self, layout: &'a Layout, rank: usize) -> Entries<'a, V> {
        match self {
            CellStore::Narrow(m) => Entries::Narrow {
                iter: m.iter(),
                layout,
                rank,
            },
            CellStore::Wide(m) => Entries::Wide(m.iter()),
        }
    }
}

/// Iterator over populated cells as `(coordinate, &value)`, hiding which key
/// representation the store uses.
enum Entries<'a, V> {
    Narrow {
        iter: std::collections::hash_map::Iter<'a, u64, V>,
        layout: &'a Layout,
        rank: usize,
    },
    Wide(std::collections::hash_map::Iter<'a, Box<[u32]>, V>),
}

impl<'a, V> Iterator for Entries<'a, V> {
    type Item = (Vec<u32>, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Entries::Narrow { iter, layout, rank } => iter
                .next()
                .map(|(&key, value)| (layout.unpack(key, *rank), value)),
            Entries::Wide(iter) => iter.next().map(|(key, value)| (key.to_vec(), value)),
        }
    }
}

/// An interning pool for string cell values (ADR-0006).
///
/// Each distinct string is stored once and referenced by a compact id, so a cell
/// costs one `u32` id, not a heap allocation per cell, and repeated text (status
/// codes, labels) is shared. The pool grows monotonically; ids are assigned in
/// insertion order, which is deterministic.
#[derive(Clone, Debug, Default)]
struct StringPool {
    by_id: Vec<Box<str>>,
    ids: HashMap<Box<str>, u32, FxBuildHasher>,
}

impl StringPool {
    fn intern(&mut self, value: &str) -> u32 {
        if let Some(&id) = self.ids.get(value) {
            return id;
        }
        let id = self.by_id.len() as u32;
        let boxed: Box<str> = value.into();
        self.by_id.push(boxed.clone());
        self.ids.insert(boxed, id);
        id
    }

    fn get(&self, id: u32) -> &str {
        &self.by_id[id as usize]
    }
}

/// A small, fast, dependency-free hasher (FxHash) for the cell store.
///
/// Deterministic (fixed seed), and far cheaper than the default SipHash on the
/// integer keys that dominate the hot path.
#[derive(Default)]
struct FxHasher {
    hash: u64,
}

const FX_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FxHasher {
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(FX_SEED);
    }
}

impl Hasher for FxHasher {
    fn finish(&self) -> u64 {
        self.hash
    }

    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            self.add(u64::from_le_bytes(chunk.try_into().unwrap()));
        }
        let remainder = chunks.remainder();
        if !remainder.is_empty() {
            let mut buf = [0u8; 8];
            buf[..remainder.len()].copy_from_slice(remainder);
            self.add(u64::from_le_bytes(buf));
        }
    }
}

type FxBuildHasher = BuildHasherDefault<FxHasher>;

/// A cube: an ordered set of dimensions and a sparse store of populated leaf cells.
///
/// Only populated leaf cells are stored (writing zero clears a cell), so memory
/// scales with the data, not with the dense cartesian space. Coordinates are
/// packed into compact integer keys (ADR-0006). Numeric and string cells live in
/// separate stores with disjoint coordinate spaces (a string cell must address a
/// string element; a numeric cell is all numeric leaves). Consolidated values are
/// computed on demand by the sparse consolidation algorithm over numeric cells.
///
/// Note (later increments): consolidated reads currently scan populated cells,
/// which is correct; an index and a calculation cache come later.
#[derive(Clone, Debug)]
pub struct Cube {
    name: String,
    dimensions: Vec<Dimension>,
    layout: Layout,
    cells: CellStore<Fixed>,
    string_cells: CellStore<u32>,
    string_pool: StringPool,
}

/// Combined consolidation weight for a cell, or `None` if the cell does not
/// contribute to the query (some component is not a leaf-descendant of the
/// queried element). `component(d)` returns the cell's element index in dim `d`.
fn cell_weight(per_dim: &[HashMap<u32, i64>], component: impl Fn(usize) -> u32) -> Option<i128> {
    let mut weight: i128 = 1;
    for (d, weights) in per_dim.iter().enumerate() {
        weight *= i128::from(*weights.get(&component(d))?);
    }
    Some(weight)
}

impl Cube {
    /// Create a cube from an ordered, non-empty list of dimensions.
    pub fn new(name: impl Into<String>, dimensions: Vec<Dimension>) -> Result<Self, ModelError> {
        if dimensions.is_empty() {
            return Err(ModelError::EmptyCube);
        }
        let layout = Layout::new(&dimensions);
        let cells = CellStore::new(layout.narrow);
        let string_cells = CellStore::new(layout.narrow);
        Ok(Self {
            name: name.into(),
            dimensions,
            layout,
            cells,
            string_cells,
            string_pool: StringPool::default(),
        })
    }

    /// The cube name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Number of dimensions.
    pub fn rank(&self) -> usize {
        self.dimensions.len()
    }

    /// The dimension at position `i`.
    pub fn dimension(&self, i: usize) -> &Dimension {
        &self.dimensions[i]
    }

    /// All dimensions, in order.
    pub fn dimensions(&self) -> &[Dimension] {
        &self.dimensions
    }

    /// Number of populated numeric leaf cells.
    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }

    /// Number of populated string cells.
    pub fn string_cell_count(&self) -> usize {
        self.string_cells.len()
    }

    /// Iterate populated numeric leaf cells as `(coordinate, value)`.
    pub fn cell_entries(&self) -> impl Iterator<Item = (Vec<u32>, Fixed)> + '_ {
        self.cells
            .entries(&self.layout, self.rank())
            .map(|(coord, &value)| (coord, value))
    }

    /// Iterate populated string cells as `(coordinate, value)`.
    pub fn string_cell_entries(&self) -> impl Iterator<Item = (Vec<u32>, &str)> + '_ {
        let pool = &self.string_pool;
        self.string_cells
            .entries(&self.layout, self.rank())
            .map(move |(coord, &id)| (coord, pool.get(id)))
    }

    fn check_coord(&self, coord: &[u32]) -> Result<(), ModelError> {
        if coord.len() != self.rank() {
            return Err(ModelError::RankMismatch {
                expected: self.rank(),
                got: coord.len(),
            });
        }
        for (d, &idx) in coord.iter().enumerate() {
            self.dimensions[d].element(idx)?;
        }
        Ok(())
    }

    /// Write a numeric value to a leaf cell. Every coordinate element must be a
    /// numeric leaf. Writing [`Fixed::ZERO`] clears the cell, keeping the store
    /// sparse.
    pub fn set_leaf(&mut self, coord: &[u32], value: Fixed) -> Result<(), ModelError> {
        self.check_coord(coord)?;
        for (d, &idx) in coord.iter().enumerate() {
            let element = self.dimensions[d].element(idx)?;
            match element.kind {
                ElementKind::Leaf => {}
                ElementKind::String => {
                    return Err(ModelError::CellTypeMismatch {
                        dimension: self.dimensions[d].name().to_string(),
                        element: element.name.clone(),
                    })
                }
                ElementKind::Consolidated => {
                    return Err(ModelError::WriteToNonLeaf {
                        dimension: self.dimensions[d].name().to_string(),
                        element: element.name.clone(),
                    })
                }
            }
        }
        if value.is_zero() {
            self.cells.clear(&self.layout, coord);
        } else {
            self.cells.put(&self.layout, coord, value);
        }
        Ok(())
    }

    /// Read a numeric leaf cell directly (zero if unpopulated).
    pub fn get_leaf(&self, coord: &[u32]) -> Result<Fixed, ModelError> {
        self.check_coord(coord)?;
        Ok(self.leaf_value(coord))
    }

    /// Direct numeric cell lookup, assuming `coord` is already validated.
    fn leaf_value(&self, coord: &[u32]) -> Fixed {
        self.cells
            .get(&self.layout, coord)
            .copied()
            .unwrap_or(Fixed::ZERO)
    }

    /// Write a string value to a string cell. Every coordinate element must be a
    /// leaf, and at least one must be a string element. Writing an empty string
    /// clears the cell.
    pub fn set_string(&mut self, coord: &[u32], value: &str) -> Result<(), ModelError> {
        self.check_coord(coord)?;
        let mut addresses_string = false;
        for (d, &idx) in coord.iter().enumerate() {
            let element = self.dimensions[d].element(idx)?;
            match element.kind {
                ElementKind::String => addresses_string = true,
                ElementKind::Leaf => {}
                ElementKind::Consolidated => {
                    return Err(ModelError::WriteToNonLeaf {
                        dimension: self.dimensions[d].name().to_string(),
                        element: element.name.clone(),
                    })
                }
            }
        }
        if !addresses_string {
            return Err(ModelError::StringCellRequiresStringElement {
                cube: self.name.clone(),
            });
        }
        if value.is_empty() {
            self.string_cells.clear(&self.layout, coord);
        } else {
            let id = self.string_pool.intern(value);
            self.string_cells.put(&self.layout, coord, id);
        }
        Ok(())
    }

    /// Read a string cell, if populated.
    pub fn get_string(&self, coord: &[u32]) -> Result<Option<&str>, ModelError> {
        self.check_coord(coord)?;
        Ok(self
            .string_cells
            .get(&self.layout, coord)
            .map(|&id| self.string_pool.get(id)))
    }

    /// Read a value at any coordinate, consolidating across consolidated elements.
    ///
    /// Exact and deterministic: contributions are summed in a 128-bit accumulator
    /// over exact integer values, so the result is independent of iteration order.
    /// Only numeric cells contribute; string cells never aggregate.
    pub fn get(&self, coord: &[u32]) -> Result<Fixed, ModelError> {
        self.check_coord(coord)?;

        let mut per_dim: Vec<HashMap<u32, i64>> = Vec::with_capacity(self.rank());
        let mut all_leaf = true;
        for (d, &idx) in coord.iter().enumerate() {
            if self.dimensions[d].element(idx)?.kind != ElementKind::Leaf {
                all_leaf = false;
            }
            per_dim.push(self.dimensions[d].leaf_weights(idx)?.into_iter().collect());
        }

        // Fast path: a pure numeric-leaf cell is a direct lookup.
        if all_leaf {
            return Ok(self.leaf_value(coord));
        }

        // Sparse consolidation: include each populated cell whose every component
        // is a leaf-descendant of the corresponding query element. Summation is
        // exact and order-independent.
        let acc: i128 = match &self.cells {
            CellStore::Narrow(m) => m
                .iter()
                .filter_map(|(&key, value)| {
                    cell_weight(&per_dim, |d| self.layout.component(key, d))
                        .map(|w| w * i128::from(value.to_scaled()))
                })
                .sum(),
            CellStore::Wide(m) => m
                .iter()
                .filter_map(|(key, value)| {
                    cell_weight(&per_dim, |d| key[d]).map(|w| w * i128::from(value.to_scaled()))
                })
                .sum(),
        };
        i64::try_from(acc)
            .map(Fixed::from_scaled)
            .map_err(|_| ModelError::Overflow)
    }

    /// Consolidate a coordinate, sourcing each contributing leaf's value through
    /// `leaf_value` instead of reading the stored cell store directly.
    ///
    /// This is the rule-aware consolidation entry point (the seam the calc engine
    /// uses): with a stored-cell source it equals [`get`](Self::get) exactly; with
    /// a rule overlay it folds rule-derived leaf values into the rollup through
    /// the same exact weighted `i128` algebra. An all-leaf coordinate
    /// short-circuits to a single `leaf_value` call (no enumeration). A
    /// consolidated coordinate enumerates its contributing leaf coordinates (the
    /// cartesian product of each dimension's net-weighted leaves) and sums
    /// `weight * value` exactly. The error type is generic so a caller can thread
    /// either a `ModelError` or a richer calc error through the closure.
    ///
    /// The enumeration is dense over the consolidation's leaf space, so callers
    /// that can be sparse (feeder-driven) should restrict the queried region; the
    /// no-rules read path stays on the sparse [`get`](Self::get).
    pub fn consolidate_with<E, F>(&self, coord: &[u32], leaf_value: F) -> Result<Fixed, E>
    where
        E: From<ModelError>,
        F: Fn(&[u32]) -> Result<Fixed, E>,
    {
        self.check_coord(coord).map_err(E::from)?;

        let mut per_dim: Vec<Vec<(u32, i64)>> = Vec::with_capacity(self.rank());
        let mut all_leaf = true;
        for (d, &idx) in coord.iter().enumerate() {
            if self.dimensions[d].element(idx).map_err(E::from)?.kind != ElementKind::Leaf {
                all_leaf = false;
            }
            per_dim.push(self.dimensions[d].leaf_weights(idx).map_err(E::from)?);
        }

        // Fast path: a pure leaf coordinate is a single source lookup.
        if all_leaf {
            return leaf_value(coord);
        }

        // Dense enumeration of the contributing leaf coordinates: a mixed-radix
        // walk over the per-dimension weighted-leaf lists. Any empty list (a
        // net-zero rollup) means no contributing leaves.
        let total: usize = per_dim.iter().map(|w| w.len()).product();
        if total == 0 {
            return Ok(Fixed::ZERO);
        }
        let rank = self.rank();
        let mut combo = vec![0u32; rank];
        let mut acc: i128 = 0;
        for n in 0..total {
            let mut rem = n;
            let mut weight: i128 = 1;
            for d in 0..rank {
                let len = per_dim[d].len();
                let (leaf, w) = per_dim[d][rem % len];
                rem /= len;
                combo[d] = leaf;
                weight *= i128::from(w);
            }
            acc += weight * i128::from(leaf_value(&combo)?.to_scaled());
        }
        let scaled = i64::try_from(acc).map_err(|_| E::from(ModelError::Overflow))?;
        Ok(Fixed::from_scaled(scaled))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Dimension;
    use epiphany_determinism::DeterministicRng;

    fn fix(n: i32) -> Fixed {
        Fixed::from(n)
    }

    /// A dimension with `n` leaves and one consolidated "Total" summing them.
    /// Returns `(dimension, total_index, leaf_indices)`.
    fn sum_dim(name: &str, n: u32) -> (Dimension, u32, Vec<u32>) {
        let mut d = Dimension::new(name);
        let leaves: Vec<u32> = (0..n).map(|i| d.add_leaf(format!("{name}_l{i}"))).collect();
        let total = d.add_consolidated(format!("{name}_Total"));
        for &leaf in &leaves {
            d.add_child(total, leaf, 1).unwrap();
        }
        (d, total, leaves)
    }

    #[test]
    fn leaf_write_read_and_sparsity() {
        let (region, _total, r) = sum_dim("Region", 3);
        let mut cube = Cube::new("Sales", vec![region]).unwrap();
        assert_eq!(cube.cell_count(), 0);

        cube.set_leaf(&[r[0]], fix(10)).unwrap();
        cube.set_leaf(&[r[1]], fix(20)).unwrap();
        assert_eq!(cube.get_leaf(&[r[0]]).unwrap(), fix(10));
        assert_eq!(cube.get_leaf(&[r[2]]).unwrap(), Fixed::ZERO);
        assert_eq!(cube.cell_count(), 2);

        // Writing zero clears the cell.
        cube.set_leaf(&[r[0]], Fixed::ZERO).unwrap();
        assert_eq!(cube.cell_count(), 1);
        assert_eq!(cube.get_leaf(&[r[0]]).unwrap(), Fixed::ZERO);
    }

    #[test]
    fn consolidation_two_dimensions() {
        let (region, region_total, r) = sum_dim("Region", 2);
        let (period, period_total, p) = sum_dim("Period", 2);
        let mut cube = Cube::new("Sales", vec![region, period]).unwrap();
        cube.set_leaf(&[r[0], p[0]], fix(10)).unwrap();
        cube.set_leaf(&[r[1], p[0]], fix(20)).unwrap();
        cube.set_leaf(&[r[0], p[1]], fix(30)).unwrap();
        cube.set_leaf(&[r[1], p[1]], fix(40)).unwrap();

        assert_eq!(cube.get(&[region_total, p[0]]).unwrap(), fix(30));
        assert_eq!(cube.get(&[r[0], period_total]).unwrap(), fix(40));
        assert_eq!(cube.get(&[region_total, period_total]).unwrap(), fix(100));
        assert_eq!(cube.get(&[r[0], p[0]]).unwrap(), fix(10));
    }

    #[test]
    fn weighted_consolidation_variance() {
        let mut version = Dimension::new("Version");
        let actual = version.add_leaf("Actual");
        let budget = version.add_leaf("Budget");
        let variance = version.add_consolidated("Variance");
        version.add_child(variance, actual, 1).unwrap();
        version.add_child(variance, budget, -1).unwrap();

        let mut cube = Cube::new("PnL", vec![version]).unwrap();
        cube.set_leaf(&[actual], fix(100)).unwrap();
        cube.set_leaf(&[budget], fix(80)).unwrap();
        assert_eq!(cube.get(&[variance]).unwrap(), fix(20));
    }

    #[test]
    fn alternate_rollups() {
        let mut d = Dimension::new("Region");
        let north = d.add_leaf("North");
        let south = d.add_leaf("South");
        let east = d.add_leaf("East");
        let total = d.add_consolidated("Total");
        let coastal = d.add_consolidated("Coastal");
        for leaf in [north, south, east] {
            d.add_child(total, leaf, 1).unwrap();
        }
        d.add_child(coastal, north, 1).unwrap();
        d.add_child(coastal, east, 1).unwrap();

        let mut cube = Cube::new("Sales", vec![d]).unwrap();
        cube.set_leaf(&[north], fix(1)).unwrap();
        cube.set_leaf(&[south], fix(2)).unwrap();
        cube.set_leaf(&[east], fix(3)).unwrap();
        assert_eq!(cube.get(&[total]).unwrap(), fix(6));
        assert_eq!(cube.get(&[coastal]).unwrap(), fix(4));
    }

    #[test]
    fn write_to_consolidated_is_rejected() {
        let (region, region_total, _r) = sum_dim("Region", 2);
        let mut cube = Cube::new("Sales", vec![region]).unwrap();
        assert!(matches!(
            cube.set_leaf(&[region_total], fix(5)).unwrap_err(),
            ModelError::WriteToNonLeaf { .. }
        ));
    }

    #[test]
    fn rank_mismatch_is_rejected() {
        let (region, _total, _r) = sum_dim("Region", 2);
        let cube = Cube::new("Sales", vec![region]).unwrap();
        assert!(matches!(
            cube.get(&[0, 0]).unwrap_err(),
            ModelError::RankMismatch { .. }
        ));
    }

    #[test]
    fn cycles_are_rejected() {
        let mut d = Dimension::new("D");
        let a = d.add_consolidated("A");
        let b = d.add_consolidated("B");
        let leaf = d.add_leaf("L");
        d.add_child(a, b, 1).unwrap();
        d.add_child(b, leaf, 1).unwrap();
        // a -> b -> L; adding b -> a would close a cycle.
        assert!(matches!(
            d.add_child(b, a, 1).unwrap_err(),
            ModelError::CycleDetected { .. }
        ));
    }

    #[test]
    fn sparse_consolidation_matches_bruteforce_randomized() {
        let mut rng = DeterministicRng::new(2024);
        for _ in 0..200 {
            let nr = 1 + (rng.next_u64() % 5) as u32;
            let np = 1 + (rng.next_u64() % 5) as u32;
            let (region, region_total, r) = sum_dim("Region", nr);
            let (period, period_total, p) = sum_dim("Period", np);
            let mut cube = Cube::new("C", vec![region, period]).unwrap();

            let mut expected_total: i64 = 0;
            for &ri in &r {
                for &pj in &p {
                    if rng.next_u64().is_multiple_of(2) {
                        let v = (rng.next_u64() % 1000) as i32;
                        cube.set_leaf(&[ri, pj], fix(v)).unwrap();
                        expected_total += i64::from(v);
                    }
                }
            }
            assert_eq!(
                cube.get(&[region_total, period_total]).unwrap(),
                Fixed::from_int(expected_total).unwrap()
            );
        }
    }

    #[test]
    fn consolidation_is_deterministic_across_runs() {
        let build = || {
            let (region, region_total, r) = sum_dim("Region", 3);
            let mut cube = Cube::new("C", vec![region]).unwrap();
            cube.set_leaf(&[r[0]], fix(7)).unwrap();
            cube.set_leaf(&[r[2]], fix(5)).unwrap();
            cube.get(&[region_total]).unwrap()
        };
        assert_eq!(build(), build());
        assert_eq!(build(), fix(12));
    }

    #[test]
    fn small_cube_uses_a_narrow_key() {
        let (region, _t, _r) = sum_dim("Region", 5);
        let (period, _t2, _p) = sum_dim("Period", 12);
        let cube = Cube::new("C", vec![region, period]).unwrap();
        assert!(cube.layout.narrow, "small cubes should pack into a u64 key");
        assert!(matches!(cube.cells, CellStore::Narrow(_)));
    }

    #[test]
    fn wide_cube_falls_back_to_boxed_keys() {
        // 65 single-leaf dimensions need 65 bits, past the 64-bit narrow ceiling.
        let dims: Vec<Dimension> = (0..65)
            .map(|i| {
                let mut d = Dimension::new(format!("D{i}"));
                d.add_leaf("only");
                d
            })
            .collect();
        let mut cube = Cube::new("Wide", dims).unwrap();
        assert!(!cube.layout.narrow);
        assert!(matches!(cube.cells, CellStore::Wide(_)));

        let coord = vec![0u32; 65];
        cube.set_leaf(&coord, fix(7)).unwrap();
        assert_eq!(cube.get_leaf(&coord).unwrap(), fix(7));
        assert_eq!(cube.cell_count(), 1);
        // Round-trips through the entry iterator too.
        let entries: Vec<_> = cube.cell_entries().collect();
        assert_eq!(entries, vec![(coord, fix(7))]);
    }

    #[test]
    fn layout_packs_and_unpacks() {
        let coord = [3u32, 9u32];
        let layout = Layout {
            offsets: vec![0, 4],
            masks: vec![0xF, 0xF],
            narrow: true,
        };
        let key = layout.pack(&coord);
        assert_eq!(layout.component(key, 0), 3);
        assert_eq!(layout.component(key, 1), 9);
        assert_eq!(layout.unpack(key, 2), vec![3, 9]);
    }

    /// A Region x Measure cube where Measure has a numeric leaf, a string leaf,
    /// and a numeric Total over the numerics.
    fn string_cube() -> (Cube, u32, u32, u32, u32) {
        let mut region = Dimension::new("Region");
        let north = region.add_leaf("North");
        let south = region.add_leaf("South");
        region.add_consolidated("Total");
        region.add_child(2, north, 1).unwrap();
        region.add_child(2, south, 1).unwrap();

        let mut measure = Dimension::new("Measure");
        let sales = measure.add_leaf("Sales");
        let comment = measure.add_string("Comment");

        let cube = Cube::new("Sales", vec![region, measure]).unwrap();
        (cube, north, south, sales, comment)
    }

    #[test]
    fn string_cells_write_read_and_intern() {
        let (mut cube, north, south, _sales, comment) = string_cube();
        cube.set_string(&[north, comment], "ok").unwrap();
        cube.set_string(&[south, comment], "ok").unwrap(); // same text -> interned once

        assert_eq!(cube.get_string(&[north, comment]).unwrap(), Some("ok"));
        assert_eq!(cube.get_string(&[south, comment]).unwrap(), Some("ok"));
        assert_eq!(cube.string_cell_count(), 2);
        assert_eq!(
            cube.string_pool.by_id.len(),
            1,
            "equal strings share one id"
        );

        // An empty write clears the string cell.
        cube.set_string(&[north, comment], "").unwrap();
        assert_eq!(cube.get_string(&[north, comment]).unwrap(), None);
        assert_eq!(cube.string_cell_count(), 1);
    }

    #[test]
    fn numeric_write_to_string_leaf_is_rejected() {
        let (mut cube, north, _south, _sales, comment) = string_cube();
        assert!(matches!(
            cube.set_leaf(&[north, comment], fix(1)).unwrap_err(),
            ModelError::CellTypeMismatch { .. }
        ));
    }

    #[test]
    fn string_write_to_numeric_coordinate_is_rejected() {
        let (mut cube, north, _south, sales, _comment) = string_cube();
        assert!(matches!(
            cube.set_string(&[north, sales], "x").unwrap_err(),
            ModelError::StringCellRequiresStringElement { .. }
        ));
    }

    #[test]
    fn string_cells_do_not_affect_consolidation() {
        let (mut cube, north, south, sales, comment) = string_cube();
        cube.set_leaf(&[north, sales], fix(100)).unwrap();
        cube.set_leaf(&[south, sales], fix(50)).unwrap();
        cube.set_string(&[north, comment], "note").unwrap();

        let region_total = cube.dimension(0).index_of("Total").unwrap();
        // Total Sales = 150; the string cell contributes nothing.
        assert_eq!(cube.get(&[region_total, sales]).unwrap(), fix(150));
        // A numeric read over the string measure is zero (no numeric cells there).
        assert_eq!(cube.get(&[region_total, comment]).unwrap(), Fixed::ZERO);
    }

    #[test]
    fn consolidate_with_stored_source_equals_get() {
        let (region, region_total, r) = sum_dim("Region", 3);
        let (period, period_total, p) = sum_dim("Period", 2);
        let mut cube = Cube::new("Sales", vec![region, period]).unwrap();
        cube.set_leaf(&[r[0], p[0]], fix(10)).unwrap();
        cube.set_leaf(&[r[1], p[0]], fix(20)).unwrap();
        cube.set_leaf(&[r[0], p[1]], fix(30)).unwrap();
        let coords = [
            [r[0], p[0]],
            [r[2], p[1]],
            [region_total, p[0]],
            [r[0], period_total],
            [region_total, period_total],
        ];
        for c in coords {
            let via = cube
                .consolidate_with::<ModelError, _>(&c, |lc| Ok(cube.leaf_value(lc)))
                .unwrap();
            assert_eq!(via, cube.get(&c).unwrap(), "mismatch at {c:?}");
        }
    }

    #[test]
    fn consolidate_with_fast_path_does_not_enumerate() {
        let (region, region_total, r) = sum_dim("Region", 3);
        let cube = Cube::new("Sales", vec![region]).unwrap();
        // A pure-leaf coordinate calls the source exactly once.
        let calls = std::cell::Cell::new(0usize);
        cube.consolidate_with::<ModelError, _>(&[r[0]], |_| {
            calls.set(calls.get() + 1);
            Ok(Fixed::ZERO)
        })
        .unwrap();
        assert_eq!(calls.get(), 1, "a leaf read must not enumerate");
        // A consolidated coordinate enumerates its three contributing leaves.
        let calls = std::cell::Cell::new(0usize);
        cube.consolidate_with::<ModelError, _>(&[region_total], |_| {
            calls.set(calls.get() + 1);
            Ok(Fixed::ZERO)
        })
        .unwrap();
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn consolidate_with_equals_get_randomized() {
        let (region, _rt, r) = sum_dim("Region", 4);
        let (period, _pt, p) = sum_dim("Period", 3);
        let mut cube = Cube::new("Sales", vec![region, period]).unwrap();
        let mut rng = DeterministicRng::new(7);
        for &ri in &r {
            for &pi in &p {
                if rng.next_u64().is_multiple_of(2) {
                    let v = Fixed::from_scaled((rng.next_u64() % 1000) as i64);
                    cube.set_leaf(&[ri, pi], v).unwrap();
                }
            }
        }
        let rlen = cube.dimension(0).len();
        let plen = cube.dimension(1).len();
        for ri in 0..rlen {
            for pi in 0..plen {
                let c = [ri, pi];
                let via = cube
                    .consolidate_with::<ModelError, _>(&c, |lc| Ok(cube.leaf_value(lc)))
                    .unwrap();
                assert_eq!(via, cube.get(&c).unwrap(), "mismatch at {c:?}");
            }
        }
    }
}
