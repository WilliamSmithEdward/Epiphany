//! Epiphany core: the in-memory multidimensional model.
//!
//! Phase 1 (in progress). This crate owns the model: dimensions, elements, the
//! consolidation hierarchy (with alternate rollups), cubes, and the sparse cell
//! store with on-demand consolidation. Cell values are exact fixed-point
//! ([`Fixed`], ADR-0008) for deterministic, finance-correct arithmetic.
//!
//! Still to come this phase: element attributes & aliases, string cells, the
//! packed-key memory layout (ADR-0006) and a calculation cache, model-as-code
//! text serialization (ADR-0003), and runtime persistence. See `docs/ROADMAP.md`.

mod cube;
mod dimension;
mod error;
mod text;
mod value;

pub use cube::{Coord, Cube};
pub use dimension::{Dimension, Element, ElementKind};
pub use error::ModelError;
pub use text::{LoadError, SaveError};
pub use value::{Fixed, SCALE, SCALE_DECIMALS};

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
