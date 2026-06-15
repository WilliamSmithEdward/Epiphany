//! Epiphany core: the in-memory multidimensional model.
//!
//! Phase 1 complete. This crate owns the model: dimensions, elements (numeric,
//! string, and consolidated), the consolidation hierarchy (alternate rollups and
//! weighted edges), attributes and aliases, cubes, the sparse cell store with the
//! packed-key memory layout (ADR-0006) and on-demand consolidation, and the
//! canonical model-as-code text serialization (ADR-0003). Numeric cell values are
//! exact fixed-point ([`Fixed`], ADR-0008) for deterministic, finance-correct
//! arithmetic; string cells hold interned text.
//!
//! Still to come (later phases): a calculation cache, rules, and views. See
//! `docs/ROADMAP.md`.

mod cube;
mod dimension;
mod element_mask;
mod error;
mod query;
mod text;
mod value;

pub use cube::{Coord, Cube, EdgeSpec, ElementSpec};
pub use dimension::{AttributeDef, AttributeKind, AttributeValue, Dimension, Element, ElementKind};
pub use element_mask::ElementMask;
pub use error::ModelError;
pub use query::{
    execute_view, resolve_subset, validate_subset, validate_view, Axis, AxisSpec, CellResolver,
    CellTrace, Cellset, CommandSpec, Connection, ConnectionSpec, ExplainDepth, Flow, FlowTest, Job,
    Model, NoSetEvaluator, QueryError, RuleSet, RuleTest, Sandbox, SetEvaluator, SourceFormat,
    StoredCells, Subset, SubsetKind, TestCell, TraceKind, Trigger, View, Visibility,
};
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
