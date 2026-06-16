//! Data spreading (ADR-0029): expand a value entered at a (possibly
//! consolidated) coordinate into an exact, deterministic set of leaf writes.
//!
//! This is a pure model operation: it does no I/O, security, or commit. The
//! current-value reader used by [`SpreadMethod::Proportional`] is injected
//! (mirroring the [`crate::CellResolver`] seam), so the API can supply a resolver
//! that already honors the active sandbox and element mask, while the engine
//! stays testable in isolation.
//!
//! Exactness (ADR-0008): arithmetic is on the scaled `i64` directly; the only
//! rounding is a deterministic remainder allocation (one scaled unit at a time to
//! the leading leaves in coordinate order), so the contributing leaves always sum
//! back to the entered value. Determinism (ADR-0009): leaves are enumerated in a
//! fixed order and the remainder is allocated in that same order.

use crate::{Cube, Fixed, QueryError};

/// How an entered value is distributed across the contributing leaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpreadMethod {
    /// Split the value evenly across the leaves.
    Equal,
    /// Split the value in proportion to each leaf's current value; falls back to
    /// [`SpreadMethod::Equal`] when the current values sum to zero.
    Proportional,
    /// Set every leaf to the value.
    Repeat,
    /// Set every leaf to zero.
    Clear,
}

/// Why a spread could not be computed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpreadError {
    /// A contributing leaf has a consolidation weight other than +1, so no
    /// distribution preserves the entered total (ADR-0029 decision 4).
    WeightedConsolidation,
    /// The target expands to more leaf cells than [`MAX_SPREAD_LEAVES`].
    TooManyLeaves {
        /// The (possibly saturated) leaf count that tripped the cap.
        count: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
    /// Reading a current leaf value (for proportional spreading) failed.
    Read(QueryError),
}

/// The largest number of leaf writes one spread may produce.
pub const MAX_SPREAD_LEAVES: usize = 200_000;

/// Expand `target` (element indices in dimension order, possibly consolidated)
/// into leaf writes that distribute `value` by `method`. `read_leaf` reads a
/// leaf's current value and is consulted only by [`SpreadMethod::Proportional`].
pub fn spread_leaves(
    cube: &Cube,
    target: &[u32],
    value: Fixed,
    method: SpreadMethod,
    read_leaf: &dyn Fn(&[u32]) -> Result<Fixed, QueryError>,
) -> Result<Vec<(Vec<u32>, Fixed)>, SpreadError> {
    // Per-dimension contributing leaves (sorted by index via leaf_weights). A
    // non-unit weight anywhere means no distribution preserves the total.
    let mut per_dim: Vec<Vec<u32>> = Vec::with_capacity(target.len());
    for (d, &idx) in target.iter().enumerate() {
        let weights = cube
            .dimension(d)
            .leaf_weights(idx)
            .map_err(|e| SpreadError::Read(QueryError::Model(e)))?;
        let mut leaves = Vec::with_capacity(weights.len());
        for (leaf, weight) in weights {
            if weight != 1 {
                return Err(SpreadError::WeightedConsolidation);
            }
            leaves.push(leaf);
        }
        if leaves.is_empty() {
            // A consolidated member with no contributing leaves: nothing to write.
            return Ok(Vec::new());
        }
        per_dim.push(leaves);
    }

    // Cartesian-product size, capped (saturating so we never overflow).
    let mut count: usize = 1;
    for leaves in &per_dim {
        count = count.saturating_mul(leaves.len());
        if count > MAX_SPREAD_LEAVES {
            return Err(SpreadError::TooManyLeaves {
                count,
                cap: MAX_SPREAD_LEAVES,
            });
        }
    }

    // Enumerate leaf coordinates in a fixed order (last dimension varies fastest).
    let mut coords: Vec<Vec<u32>> = Vec::with_capacity(count);
    let mut combo = vec![0usize; per_dim.len()];
    for _ in 0..count {
        coords.push(per_dim.iter().zip(&combo).map(|(l, &i)| l[i]).collect());
        for d in (0..combo.len()).rev() {
            combo[d] += 1;
            if combo[d] < per_dim[d].len() {
                break;
            }
            combo[d] = 0;
        }
    }

    let total = value.to_scaled();
    let scaled: Vec<i64> = match method {
        SpreadMethod::Repeat => vec![total; coords.len()],
        SpreadMethod::Clear => vec![0; coords.len()],
        SpreadMethod::Equal => distribute_equal(total, coords.len()),
        SpreadMethod::Proportional => {
            let mut weights = Vec::with_capacity(coords.len());
            for coord in &coords {
                weights.push(read_leaf(coord).map_err(SpreadError::Read)?.to_scaled());
            }
            distribute_proportional(total, &weights)
        }
    };

    Ok(coords
        .into_iter()
        .zip(scaled)
        .map(|(coord, v)| (coord, Fixed::from_scaled(v)))
        .collect())
}

/// Split `total` (scaled) evenly across `n` leaves; the remainder (always smaller
/// than `n` in magnitude) is allocated one unit at a time to the leading leaves,
/// so the result sums to `total` exactly.
fn distribute_equal(total: i64, n: usize) -> Vec<i64> {
    if n == 0 {
        return Vec::new();
    }
    let n_i = n as i64;
    let base = total / n_i;
    let remainder = total - base * n_i; // |remainder| < n, same sign as total
    let mut out = vec![base; n];
    allocate_remainder(&mut out, remainder);
    out
}

/// Split `total` (scaled) in proportion to `weights`; floor each share, then
/// allocate the leftover deterministically so the result sums to `total` exactly.
/// Falls back to an even split when the weights sum to zero.
fn distribute_proportional(total: i64, weights: &[i64]) -> Vec<i64> {
    let sum: i128 = weights.iter().map(|&w| w as i128).sum();
    if sum == 0 {
        return distribute_equal(total, weights.len());
    }
    let t = total as i128;
    let mut out: Vec<i64> = weights
        .iter()
        .map(|&w| ((t * w as i128) / sum) as i64)
        .collect();
    let allocated: i128 = out.iter().map(|&x| x as i128).sum();
    // Each truncated share is within one unit of its exact value, so the leftover
    // magnitude is strictly less than the leaf count.
    let leftover = (t - allocated) as i64;
    allocate_remainder(&mut out, leftover);
    out
}

/// Add `remainder` to `out`, one scaled unit per leaf (in order) until exhausted,
/// matching the sign of the remainder. Assumes `|remainder| <= out.len()`.
fn allocate_remainder(out: &mut [i64], remainder: i64) {
    let step = if remainder >= 0 { 1 } else { -1 };
    let mut left = remainder.abs();
    let mut i = 0;
    while left > 0 && i < out.len() {
        out[i] += step;
        left -= 1;
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CellResolver, Cube, Dimension, StoredCells};
    use epiphany_determinism::DeterministicRng;

    /// Region(North,South,East,Total=N+S+E) x Measure(Sales,Cost,Margin=Sales-Cost).
    fn cube() -> Cube {
        let mut region = Dimension::new("Region");
        let n = region.add_leaf("North");
        let s = region.add_leaf("South");
        let e = region.add_leaf("East");
        let t = region.add_consolidated("Total");
        region.add_child(t, n, 1).unwrap();
        region.add_child(t, s, 1).unwrap();
        region.add_child(t, e, 1).unwrap();
        let mut measure = Dimension::new("Measure");
        let sales = measure.add_leaf("Sales");
        let cost = measure.add_leaf("Cost");
        let margin = measure.add_consolidated("Margin");
        measure.add_child(margin, sales, 1).unwrap();
        measure.add_child(margin, cost, -1).unwrap();
        Cube::new("Sales", vec![region, measure]).unwrap()
    }

    fn idx(cube: &Cube, dim: usize, member: &str) -> u32 {
        cube.dimension(dim).resolve(member).unwrap()
    }

    fn never_read(_: &[u32]) -> Result<Fixed, QueryError> {
        panic!("read_leaf must not be called for this method")
    }

    fn sum(writes: &[(Vec<u32>, Fixed)]) -> i64 {
        writes.iter().map(|(_, v)| v.to_scaled()).sum()
    }

    #[test]
    fn equal_preserves_the_total_exactly() {
        let c = cube();
        // Total/Sales -> North/Sales, South/Sales, East/Sales (3 leaves, weight 1).
        let target = vec![idx(&c, 0, "Total"), idx(&c, 1, "Sales")];
        let writes = spread_leaves(
            &c,
            &target,
            Fixed::from(100),
            SpreadMethod::Equal,
            &never_read,
        )
        .unwrap();
        assert_eq!(writes.len(), 3);
        assert_eq!(sum(&writes), Fixed::from(100).to_scaled());
        // 100 / 3 = 33.3334, 33.3333, 33.3333 (remainder to the first leaf).
        assert_eq!(writes[0].1, Fixed::from_scaled(333_334));
        assert_eq!(writes[1].1, Fixed::from_scaled(333_333));
        assert_eq!(writes[2].1, Fixed::from_scaled(333_333));
    }

    #[test]
    fn repeat_and_clear() {
        let c = cube();
        let target = vec![idx(&c, 0, "Total"), idx(&c, 1, "Sales")];
        let rep = spread_leaves(
            &c,
            &target,
            Fixed::from(7),
            SpreadMethod::Repeat,
            &never_read,
        )
        .unwrap();
        assert!(rep.iter().all(|(_, v)| *v == Fixed::from(7)));
        let clr = spread_leaves(
            &c,
            &target,
            Fixed::from(7),
            SpreadMethod::Clear,
            &never_read,
        )
        .unwrap();
        assert!(clr.iter().all(|(_, v)| v.is_zero()));
    }

    #[test]
    fn proportional_weighs_by_current_values_and_is_exact() {
        let mut c = cube();
        let n = idx(&c, 0, "North");
        let s = idx(&c, 0, "South");
        let sales = idx(&c, 1, "Sales");
        c.set_leaf(&[n, sales], Fixed::from(10)).unwrap();
        c.set_leaf(&[s, sales], Fixed::from(30)).unwrap();
        // East/Sales left at 0.
        let cells = StoredCells(&c);
        let target = vec![idx(&c, 0, "Total"), sales];
        let writes = spread_leaves(
            &c,
            &target,
            Fixed::from(100),
            SpreadMethod::Proportional,
            &|x| cells.value(x),
        )
        .unwrap();
        assert_eq!(sum(&writes), Fixed::from(100).to_scaled());
        // 10:30:0 of 100 -> 25, 75, 0.
        assert_eq!(writes[0].1, Fixed::from(25));
        assert_eq!(writes[1].1, Fixed::from(75));
        assert_eq!(writes[2].1, Fixed::ZERO);
    }

    #[test]
    fn proportional_falls_back_to_equal_when_basis_is_zero() {
        let c = cube();
        let cells = StoredCells(&c); // all leaves zero
        let target = vec![idx(&c, 0, "Total"), idx(&c, 1, "Sales")];
        let writes = spread_leaves(
            &c,
            &target,
            Fixed::from(100),
            SpreadMethod::Proportional,
            &|x| cells.value(x),
        )
        .unwrap();
        assert_eq!(sum(&writes), Fixed::from(100).to_scaled());
        assert_eq!(writes[0].1, Fixed::from_scaled(333_334));
    }

    #[test]
    fn weighted_consolidation_is_refused() {
        let c = cube();
        // North/Margin: Margin rolls up Cost with weight -1.
        let target = vec![idx(&c, 0, "North"), idx(&c, 1, "Margin")];
        let err = spread_leaves(
            &c,
            &target,
            Fixed::from(100),
            SpreadMethod::Equal,
            &never_read,
        )
        .unwrap_err();
        assert_eq!(err, SpreadError::WeightedConsolidation);
    }

    #[test]
    fn leaf_target_degenerates_to_a_single_write() {
        let c = cube();
        let target = vec![idx(&c, 0, "North"), idx(&c, 1, "Sales")];
        let writes = spread_leaves(
            &c,
            &target,
            Fixed::from(42),
            SpreadMethod::Equal,
            &never_read,
        )
        .unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].1, Fixed::from(42));
    }

    #[test]
    fn equal_is_exact_for_many_totals_and_counts() {
        // Property: an even split always sums back to the total, deterministically.
        let mut rng = DeterministicRng::new(42);
        for _ in 0..2000 {
            let total = (rng.next_u64() % 2_000_001) as i64 - 1_000_000;
            let n = (rng.next_u64() % 64) as usize + 1;
            let out = distribute_equal(total, n);
            assert_eq!(out.iter().sum::<i64>(), total);
        }
    }

    #[test]
    fn proportional_is_exact_for_random_weights() {
        let mut rng = DeterministicRng::new(7);
        for _ in 0..2000 {
            let total = (rng.next_u64() % 2_000_001) as i64 - 1_000_000;
            let n = (rng.next_u64() % 32) as usize + 1;
            let weights: Vec<i64> = (0..n)
                .map(|_| (rng.next_u64() % 201) as i64 - 100)
                .collect();
            let out = distribute_proportional(total, &weights);
            assert_eq!(out.iter().sum::<i64>(), total);
        }
    }
}
