//! Epiphany persist: runtime durability (a fast-restart cache over the model).
//!
//! Phase 0 skeleton. Phase 1/8 fills this in: an append-only transaction log,
//! periodic binary snapshots, crash recovery, and startup load. The canonical
//! source of truth is the model-as-code text in `epiphany-core`; this layer is
//! a derived cache. See `docs/ROADMAP.md`.

/// Stable crate identifier, reported by the server's wiring banner.
pub const CRATE: &str = "epiphany-persist";

#[cfg(test)]
mod tests {
    #[test]
    fn crate_is_named() {
        assert_eq!(super::CRATE, "epiphany-persist");
    }

    #[test]
    fn links_dependencies() {
        assert_eq!(epiphany_core::CRATE, "epiphany-core");
        let ids = epiphany_determinism::IdGen::default();
        assert_eq!(ids.next_id(), 1);
    }
}
