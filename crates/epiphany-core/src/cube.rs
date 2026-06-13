//! Cubes: the sparse multidimensional cell store and consolidation.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use crate::{Dimension, ElementKind, Fixed, ModelError};

/// A cube coordinate: one element index per dimension, in dimension order.
pub type Coord = Box<[u32]>;

/// Number of bits needed to represent element indices `0..len` (at least 1).
fn bits_for(len: u32) -> u32 {
    if len <= 1 {
        1
    } else {
        u32::BITS - (len - 1).leading_zeros()
    }
}

/// A packed cell key.
///
/// When a cube's coordinate space fits in 128 bits, a coordinate is bit-packed
/// into a single `u128` (8..16 bytes of key instead of a heap allocation per
/// cell). Very wide cubes fall back to a boxed slice. See ADR-0006.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum CellKey {
    Packed(u128),
    Wide(Box<[u32]>),
}

/// Per-dimension bit packing for cell coordinates.
#[derive(Clone, Debug)]
struct Layout {
    offsets: Vec<u32>,
    masks: Vec<u128>,
    packed: bool,
}

impl Layout {
    fn new(dimensions: &[Dimension]) -> Self {
        let mut offsets = Vec::with_capacity(dimensions.len());
        let mut masks = Vec::with_capacity(dimensions.len());
        let mut offset: u32 = 0;
        for dim in dimensions {
            let width = bits_for(dim.len());
            offsets.push(offset);
            masks.push((1u128 << width) - 1);
            offset = offset.saturating_add(width);
        }
        Layout {
            offsets,
            masks,
            packed: offset <= 128,
        }
    }

    fn key(&self, coord: &[u32]) -> CellKey {
        if self.packed {
            let mut packed: u128 = 0;
            for (d, &idx) in coord.iter().enumerate() {
                packed |= u128::from(idx) << self.offsets[d];
            }
            CellKey::Packed(packed)
        } else {
            CellKey::Wide(coord.into())
        }
    }

    fn component(&self, key: &CellKey, d: usize) -> u32 {
        match key {
            CellKey::Packed(packed) => ((packed >> self.offsets[d]) & self.masks[d]) as u32,
            CellKey::Wide(coord) => coord[d],
        }
    }

    fn unpack(&self, key: &CellKey, rank: usize) -> Vec<u32> {
        (0..rank).map(|d| self.component(key, d)).collect()
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
    cells: HashMap<CellKey, Fixed, FxBuildHasher>,
}

impl Cube {
    /// Create a cube from an ordered, non-empty list of dimensions.
    pub fn new(name: impl Into<String>, dimensions: Vec<Dimension>) -> Result<Self, ModelError> {
        if dimensions.is_empty() {
            return Err(ModelError::EmptyCube);
        }
        let layout = Layout::new(&dimensions);
        Ok(Self {
            name: name.into(),
            dimensions,
            layout,
            cells: HashMap::default(),
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
        self.cells
            .iter()
            .map(move |(key, &value)| (self.layout.unpack(key, rank), value))
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
        let key = self.layout.key(coord);
        if value.is_zero() {
            self.cells.remove(&key);
        } else {
            self.cells.insert(key, value);
        }
        Ok(())
    }

    /// Read a leaf cell directly (zero if unpopulated).
    pub fn get_leaf(&self, coord: &[u32]) -> Result<Fixed, ModelError> {
        self.check_coord(coord)?;
        Ok(self
            .cells
            .get(&self.layout.key(coord))
            .copied()
            .unwrap_or(Fixed::ZERO))
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
            return Ok(self
                .cells
                .get(&self.layout.key(coord))
                .copied()
                .unwrap_or(Fixed::ZERO));
        }

        // Sparse consolidation: include each populated cell whose every component
        // is a leaf-descendant of the corresponding query element.
        let mut acc: i128 = 0;
        for (key, value) in &self.cells {
            let mut weight: i128 = 1;
            let mut included = true;
            for (d, weights) in per_dim.iter().enumerate() {
                match weights.get(&self.layout.component(key, d)) {
                    Some(&w) => weight *= i128::from(w),
                    None => {
                        included = false;
                        break;
                    }
                }
            }
            if included {
                acc += weight * i128::from(value.to_scaled());
            }
        }
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
    fn small_cube_uses_packed_keys() {
        let (region, _t, _r) = sum_dim("Region", 5);
        let (period, _t2, _p) = sum_dim("Period", 12);
        let cube = Cube::new("C", vec![region, period]).unwrap();
        assert!(
            cube.layout.packed,
            "small cubes should pack coordinates into a u128 key"
        );
    }

    #[test]
    fn layout_round_trips_packed_and_wide() {
        let coord = [3u32, 9u32];

        let packed = Layout {
            offsets: vec![0, 4],
            masks: vec![0xF, 0xF],
            packed: true,
        };
        let pk = packed.key(&coord);
        assert!(matches!(pk, CellKey::Packed(_)));
        assert_eq!(packed.unpack(&pk, 2), vec![3, 9]);
        assert_eq!(packed.component(&pk, 0), 3);
        assert_eq!(packed.component(&pk, 1), 9);

        let wide = Layout {
            offsets: vec![0, 4],
            masks: vec![0xF, 0xF],
            packed: false,
        };
        let wk = wide.key(&coord);
        assert!(matches!(wk, CellKey::Wide(_)));
        assert_eq!(wide.unpack(&wk, 2), vec![3, 9]);
        assert_eq!(wide.component(&wk, 1), 9);
    }
}
