//! Epiphany calc — the rules + sparse-feeds calculation engine.
//!
//! Phase 0 skeleton. Phase 4 fills this in: the rules language, dependency
//! graph, sparse feeds with automatic feeder inference + validation,
//! calculation provenance ("explain"), and compiled on-demand evaluation.
//! See `docs/ROADMAP.md`.

/// Stable crate identifier, reported by the server's wiring banner.
pub const CRATE: &str = "epiphany-calc";

#[cfg(test)]
mod tests {
    #[test]
    fn crate_is_named() {
        assert_eq!(super::CRATE, "epiphany-calc");
    }

    #[test]
    fn links_core() {
        assert_eq!(epiphany_core::CRATE, "epiphany-core");
    }
}
