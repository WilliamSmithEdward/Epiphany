//! Epiphany core — the in-memory multidimensional model.
//!
//! Phase 0 skeleton. Phase 1 fills this in: dimensions, hierarchies with
//! alternate rollups, elements (N/C/S), attributes/aliases, a memory-tight
//! sparse cell store, subsets, views, sandboxes, and the canonical text
//! (model-as-code) serialization. See `docs/ROADMAP.md`.

/// Stable crate identifier, reported by the server's wiring banner.
pub const CRATE: &str = "epiphany-core";

#[cfg(test)]
mod tests {
    #[test]
    fn crate_is_named() {
        assert_eq!(super::CRATE, "epiphany-core");
    }

    #[test]
    fn links_determinism_harness() {
        use epiphany_determinism::Clock;
        let d = epiphany_determinism::Deterministic::with_seed(1);
        assert_eq!(
            d.clock.now_millis(),
            epiphany_determinism::Deterministic::EPOCH_2020_MILLIS
        );
    }
}
