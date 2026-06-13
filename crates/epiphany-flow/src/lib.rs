//! Epiphany flow: Flows: TypeScript ETL/automation on an embedded JS engine.
//!
//! Phase 0 skeleton. Phase 5 fills this in: a TypeScript flow interpreter
//! (Init/Schema/Rows/Finalize), data-source connectors (CSV/SQL/view),
//! vectorized host functions, the job scheduler, and flow sandboxing with
//! determinism guards (virtual clock, seeded RNG). See `docs/ROADMAP.md`.

/// Stable crate identifier, reported by the server's wiring banner.
pub const CRATE: &str = "epiphany-flow";

#[cfg(test)]
mod tests {
    #[test]
    fn crate_is_named() {
        assert_eq!(super::CRATE, "epiphany-flow");
    }

    #[test]
    fn links_dependencies() {
        assert_eq!(epiphany_core::CRATE, "epiphany-core");
        let _ = epiphany_determinism::DeterministicRng::new(0);
    }
}
