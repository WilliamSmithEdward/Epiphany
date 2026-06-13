//! Cubes: the sparse multidimensional cell store and consolidation.

use std::collections::HashMap;

use crate::{Dimension, ElementKind, Fixed, ModelError};

/// A cube coordinate: one element index per dimension, in dimension order.
pub type Coord = Box<[u32]>;

/// A cube: an ordered set of dimensions and a sparse store of populated leaf cells.
///
/// Only populated leaf cells are stored (writing zero clears a cell), so memory
/// scales with the data, not with the dense cartesian space. Consolidated values
/// are computed on demand by the sparse consolidation algorithm.
///
/// Note (Phase 1 follow-ups): the coordinate key is a boxed slice for now; the
/// packed-integer memory layout (ADR-0006) and a calculation cache are later
/// increments. Consolidated reads currently scan populated cells — correct, and
/// indexed/cached later.
#[derive(Clone, Debug)]
pub struct Cube {
    name: String,
    dimensions: Vec<Dimension>,
    cells: HashMap<Coord, Fixed>,
}

impl Cube {
    /// Create a cube from an ordered, non-empty list of dimensions.
    pub fn new(name: impl Into<String>, dimensions: Vec<Dimension>) -> Result<Self, ModelError> {
        if dimensions.is_empty() {
            return Err(ModelError::EmptyCube);
        }
        Ok(Self {
            name: name.into(),
            dimensions,
            cells: HashMap::new(),
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
        if value.is_zero() {
            self.cells.remove(coord);
        } else {
            self.cells.insert(coord.into(), value);
        }
        Ok(())
    }

    /// Read a leaf cell directly (zero if unpopulated).
    pub fn get_leaf(&self, coord: &[u32]) -> Result<Fixed, ModelError> {
        self.check_coord(coord)?;
        Ok(self.cells.get(coord).copied().unwrap_or(Fixed::ZERO))
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
            return Ok(self.cells.get(coord).copied().unwrap_or(Fixed::ZERO));
        }

        // Sparse consolidation: include each populated cell whose every component
        // is a leaf-descendant of the corresponding query element.
        let mut acc: i128 = 0;
        for (cell_coord, value) in &self.cells {
            let mut weight: i128 = 1;
            let mut included = true;
            for (d, weights) in per_dim.iter().enumerate() {
                match weights.get(&cell_coord[d]) {
                    Some(&w) => weight *= w as i128,
                    None => {
                        included = false;
                        break;
                    }
                }
            }
            if included {
                acc += weight * value.to_scaled() as i128;
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
        // a → b → L; adding b → a would close a cycle.
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
}
