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

/// The sparse cell store, keyed for memory efficiency (ADR-0006).
///
/// Splitting the key type at the map level (rather than a per-key enum) keeps a
/// narrow entry a bare `(u64, Fixed)` with no discriminant or alignment padding:
/// 16 bytes plus about one control byte of table overhead, comfortably within
/// the per-cell budget (ROADMAP section 8). Very wide cubes use a boxed-slice
/// key, paying a heap allocation per cell.
#[derive(Clone, Debug)]
enum CellStore {
    Narrow(HashMap<u64, Fixed, FxBuildHasher>),
    Wide(HashMap<Box<[u32]>, Fixed, FxBuildHasher>),
}

impl CellStore {
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
/// packed into compact integer keys (ADR-0006). Consolidated values are computed
/// on demand by the sparse consolidation algorithm.
///
/// Note (later increments): consolidated reads currently scan populated cells,
/// which is correct; an index and a calculation cache come later.
#[derive(Clone, Debug)]
pub struct Cube {
    name: String,
    dimensions: Vec<Dimension>,
    layout: Layout,
    cells: CellStore,
}

/// Iterator over populated cells as `(coordinate, value)`, hiding which key
/// representation the cube uses.
enum EntryIter<'a> {
    Narrow {
        iter: std::collections::hash_map::Iter<'a, u64, Fixed>,
        layout: &'a Layout,
        rank: usize,
    },
    Wide(std::collections::hash_map::Iter<'a, Box<[u32]>, Fixed>),
}

impl Iterator for EntryIter<'_> {
    type Item = (Vec<u32>, Fixed);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            EntryIter::Narrow { iter, layout, rank } => iter
                .next()
                .map(|(&key, &value)| (layout.unpack(key, *rank), value)),
            EntryIter::Wide(iter) => iter.next().map(|(key, &value)| (key.to_vec(), value)),
        }
    }
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
        Ok(Self {
            name: name.into(),
            dimensions,
            layout,
            cells,
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

    /// Number of populated leaf cells.
    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }

    /// Iterate populated leaf cells as `(coordinate, value)`.
    pub fn cell_entries(&self) -> impl Iterator<Item = (Vec<u32>, Fixed)> + '_ {
        let rank = self.rank();
        match &self.cells {
            CellStore::Narrow(m) => EntryIter::Narrow {
                iter: m.iter(),
                layout: &self.layout,
                rank,
            },
            CellStore::Wide(m) => EntryIter::Wide(m.iter()),
        }
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

    /// Write a value to a leaf cell. Every coordinate element must be a leaf.
    /// Writing [`Fixed::ZERO`] clears the cell, keeping the store sparse.
    pub fn set_leaf(&mut self, coord: &[u32], value: Fixed) -> Result<(), ModelError> {
        self.check_coord(coord)?;
        for (d, &idx) in coord.iter().enumerate() {
            let element = self.dimensions[d].element(idx)?;
            if element.kind != ElementKind::Leaf {
                return Err(ModelError::WriteToNonLeaf {
                    dimension: self.dimensions[d].name().to_string(),
                    element: element.name.clone(),
                });
            }
        }
        let clear = value.is_zero();
        match &mut self.cells {
            CellStore::Narrow(m) => {
                let key = self.layout.pack(coord);
                if clear {
                    m.remove(&key);
                } else {
                    m.insert(key, value);
                }
            }
            CellStore::Wide(m) => {
                if clear {
                    m.remove(coord);
                } else {
                    m.insert(coord.into(), value);
                }
            }
        }
        Ok(())
    }

    /// Read a leaf cell directly (zero if unpopulated).
    pub fn get_leaf(&self, coord: &[u32]) -> Result<Fixed, ModelError> {
        self.check_coord(coord)?;
        Ok(self.leaf_value(coord))
    }

    /// Direct cell lookup, assuming `coord` is already validated and all-leaf.
    fn leaf_value(&self, coord: &[u32]) -> Fixed {
        match &self.cells {
            CellStore::Narrow(m) => m.get(&self.layout.pack(coord)).copied(),
            CellStore::Wide(m) => m.get(coord).copied(),
        }
        .unwrap_or(Fixed::ZERO)
    }

    /// Read a value at any coordinate, consolidating across consolidated elements.
    ///
    /// Exact and deterministic: contributions are summed in a 128-bit accumulator
    /// over exact integer values, so the result is independent of iteration order.
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

        // Fast path: a pure leaf cell is a direct lookup.
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
}
