//! The element-security deny mask (ADR-0015 decision 5): a pure index structure
//! the calculation engine and query model consult to enforce element access
//! without depending on `epiphany-security`.
//!
//! The mask names, per dimension position, the element indices a principal may
//! not see. The security layer builds it once per request (resolving element
//! ACLs against the live store); core and calc only ever index into it. Denying a
//! leaf is the load-bearing case: because a consolidation pulls each contributing
//! leaf back through the resolver, a denied leaf taints every rollup that
//! includes it (deny-the-rollup), which is what closes the subtraction-inference
//! leak. A denied consolidated member is honored when directly addressed or
//! enumerated on an axis, but does not propagate (its leaves carry the rollup
//! protection). When no element ACLs apply the mask is absent entirely, so the
//! hot path pays nothing.

use crate::Cube;

/// A per-dimension-position set of denied element indices for one cube and one
/// principal. Built by the security layer; consulted by core and calc with O(1)
/// indexing. `denied[d]` is a dense allow/deny bitset over dimension `d`'s
/// elements (empty when that dimension has no denials).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ElementMask {
    denied: Vec<Vec<bool>>,
}

impl ElementMask {
    /// Build a mask from per-dimension denied-index lists (one entry per cube
    /// dimension, in coordinate order). `element_counts[d]` sizes dimension `d`'s
    /// bitset; indices outside it are ignored.
    pub fn from_denied(element_counts: &[u32], denied_indices: &[Vec<u32>]) -> Self {
        let denied = element_counts
            .iter()
            .enumerate()
            .map(|(d, &count)| {
                let mut bits = vec![false; count as usize];
                if let Some(list) = denied_indices.get(d) {
                    for &idx in list {
                        if let Some(slot) = bits.get_mut(idx as usize) {
                            *slot = true;
                        }
                    }
                }
                bits
            })
            .collect();
        Self { denied }
    }

    /// Whether the mask denies nothing (so callers can drop it and skip the check).
    pub fn is_empty(&self) -> bool {
        self.denied.iter().all(|s| !s.iter().any(|&b| b))
    }

    /// The mask's exact denied set as sorted `(dimension position, element
    /// index)` pairs. This is the canonical, lossless identity of a mask: two
    /// masks are interchangeable for a read iff their `denied_pairs` are equal.
    /// Used by the API view cache (ADR-0028) to key a masked entry on the precise
    /// denial set that produced it, so two principals with identical denials
    /// share one entry and any difference yields a distinct, non-aliasing key.
    /// Iteration is in dimension-then-index order, so the result is already
    /// sorted and deterministic.
    pub fn denied_pairs(&self) -> Vec<(u32, u32)> {
        let mut pairs = Vec::new();
        for (dim, bits) in self.denied.iter().enumerate() {
            for (idx, &denied) in bits.iter().enumerate() {
                if denied {
                    pairs.push((dim as u32, idx as u32));
                }
            }
        }
        pairs
    }

    fn dim_has_denials(&self, dim: usize) -> bool {
        self.denied.get(dim).is_some_and(|s| s.iter().any(|&b| b))
    }

    fn index_denied(&self, dim: usize, index: u32) -> bool {
        self.denied
            .get(dim)
            .and_then(|s| s.get(index as usize))
            .copied()
            .unwrap_or(false)
    }

    /// Whether a fully-leaf coordinate names any denied member. Cheap (no
    /// expansion): used at the calc leaf terminal, where every component is a
    /// leaf, so every read (direct, rollup contribution, or rule reference) is
    /// checked exactly once.
    pub fn denies_leaf(&self, coord: &[u32]) -> bool {
        coord
            .iter()
            .enumerate()
            .any(|(d, &idx)| self.index_denied(d, idx))
    }

    /// Whether a coordinate (leaf or consolidated) names, or rolls up, any denied
    /// member. Expands each consolidated component to its contributing leaves
    /// (`Dimension::leaf_weights`, zero-weight-excluding) and checks those.
    pub fn denies(&self, cube: &Cube, coord: &[u32]) -> bool {
        coord
            .iter()
            .enumerate()
            .any(|(d, &idx)| self.member_denied(cube, d, idx))
    }

    /// Whether a single member on dimension position `dim` is, or rolls up, a
    /// denied member. Used to suppress denied members from an axis enumeration.
    pub fn denies_member(&self, cube: &Cube, dim: usize, member: u32) -> bool {
        self.member_denied(cube, dim, member)
    }

    fn member_denied(&self, cube: &Cube, dim: usize, member: u32) -> bool {
        if !self.dim_has_denials(dim) {
            return false;
        }
        if self.index_denied(dim, member) {
            return true;
        }
        cube.dimensions()
            .get(dim)
            .and_then(|d| d.leaf_weights(member).ok())
            .is_some_and(|weights| {
                weights
                    .iter()
                    .any(|(leaf, _)| self.index_denied(dim, *leaf))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Dimension;

    /// Region(North,South,Total=N+S) x Measure(Sales) so a denied leaf can be
    /// checked directly and through a rollup.
    fn cube() -> Cube {
        let mut region = Dimension::new("Region");
        let n = region.add_leaf("North");
        let s = region.add_leaf("South");
        let t = region.add_consolidated("Total");
        region.add_child(t, n, 1).unwrap();
        region.add_child(t, s, 1).unwrap();
        let mut measure = Dimension::new("Measure");
        measure.add_leaf("Sales");
        Cube::new("Sales", vec![region, measure]).unwrap()
    }

    #[test]
    fn empty_mask_denies_nothing() {
        let cube = cube();
        let mask = ElementMask::from_denied(&[3, 1], &[Vec::new(), Vec::new()]);
        assert!(mask.is_empty());
        let total = cube.dimension(0).resolve("Total").unwrap();
        assert!(!mask.denies(&cube, &[total, 0]));
        assert!(!mask.denies_leaf(&[1, 0]));
    }

    #[test]
    fn denies_a_leaf_directly_and_through_its_rollup() {
        let cube = cube();
        let north = cube.dimension(0).resolve("North").unwrap();
        let south = cube.dimension(0).resolve("South").unwrap();
        let total = cube.dimension(0).resolve("Total").unwrap();
        // Deny South in dimension 0.
        let mask = ElementMask::from_denied(&[3, 1], &[vec![south], Vec::new()]);
        assert!(!mask.is_empty());

        // The denied leaf is denied directly; the allowed leaf is not.
        assert!(mask.denies_leaf(&[south, 0]));
        assert!(!mask.denies_leaf(&[north, 0]));
        // The rollup over the denied leaf is denied; a leaf check alone would miss
        // it (Total is not itself in the denied set).
        assert!(!mask.denies_leaf(&[total, 0]));
        assert!(mask.denies(&cube, &[total, 0]));
        assert!(!mask.denies(&cube, &[north, 0]));

        // Member-level suppression matches: South and Total are suppressed, North
        // is kept.
        assert!(mask.denies_member(&cube, 0, south));
        assert!(mask.denies_member(&cube, 0, total));
        assert!(!mask.denies_member(&cube, 0, north));
    }

    #[test]
    fn denied_pairs_is_canonical_and_lossless() {
        let cube = cube();
        let north = cube.dimension(0).resolve("North").unwrap();
        let south = cube.dimension(0).resolve("South").unwrap();

        // An empty mask has no pairs; two principals with the same denials yield
        // identical pairs (so they share a cache entry); different denials differ.
        let empty = ElementMask::from_denied(&[3, 1], &[Vec::new(), Vec::new()]);
        assert!(empty.denied_pairs().is_empty());

        let deny_south_a = ElementMask::from_denied(&[3, 1], &[vec![south], Vec::new()]);
        let deny_south_b = ElementMask::from_denied(&[3, 1], &[vec![south], Vec::new()]);
        assert_eq!(deny_south_a.denied_pairs(), deny_south_b.denied_pairs());
        assert_eq!(deny_south_a.denied_pairs(), vec![(0, south)]);

        let deny_north = ElementMask::from_denied(&[3, 1], &[vec![north], Vec::new()]);
        assert_ne!(deny_south_a.denied_pairs(), deny_north.denied_pairs());

        // Pairs are sorted by (dimension, index) regardless of input order.
        let multi = ElementMask::from_denied(&[3, 1], &[vec![south, north], Vec::new()]);
        let mut sorted = multi.denied_pairs();
        let observed = sorted.clone();
        sorted.sort_unstable();
        assert_eq!(observed, sorted);
    }
}
