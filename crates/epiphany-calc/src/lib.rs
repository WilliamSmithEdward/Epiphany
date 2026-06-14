//! Epiphany calc: the rules + sparse-feeds calculation engine.
//!
//! Phase 4 fills this in: the rules language (the [`rules`] front end), a
//! dependency graph, sparse feeds with automatic feeder inference and
//! validation, calculation provenance ("explain"), and compiled on-demand
//! evaluation. See `docs/ROADMAP.md`.

pub mod rules;

mod compile;
mod compiled;
mod eval;
mod feeders;
mod provenance;
mod registry;
mod testing;

pub use compile::compile;
pub use compiled::{
    AddrSlot, CCell, CCond, CExpr, CompileError, CompiledArea, CompiledModel, CompiledRule,
    DimPredicate, RuleId,
};
pub use eval::{CalcEngine, CalcError, CalcView, EvalRegistry, SandboxOverlay};
pub use feeders::{
    infer_feeders, validate_feeders, FeederDiagnostics, FeederIndex, FeederInference, OpaqueRule,
};
pub use provenance::{explain, explain_with};
pub use registry::{CubeRegistry, SingleCube, VecRegistry};
pub use testing::{run_rule_tests, AssertionFailure, TestOutcome, TestRunError};

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
