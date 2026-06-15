//! Epiphany flow: Flows: TypeScript ETL/automation on an embedded JS engine.
//!
//! A flow is TypeScript source ([`epiphany_core::Flow`]) that the runtime strips
//! to JavaScript ([`strip`]) and runs on an embedded engine (boa, ADR-0004)
//! against a deterministic host context, turning its staged outputs into
//! dimension-element and cell changes. The crate depends only on
//! `epiphany-core` and `epiphany-determinism`; the API layer applies a flow's
//! planned changes through the engine.

/// Stable crate identifier, reported by the server's wiring banner.
pub const CRATE: &str = "epiphany-flow";

pub mod csv;
pub mod ledger;
pub mod run;
pub mod scheduler;
pub mod strip;
pub mod testing;

pub use csv::{parse_csv, CsvError, Row};
pub use ledger::{RunLedger, RunRecord, RunRetention, RunState};
pub use run::{run_flow, validate_flow, FlowError, FlowOutcome, FlowReport, PlannedCell};
pub use scheduler::{due_firings, scheduled_run_id, Firing};
pub use strip::{strip_types, StripError};
pub use testing::{
    apply_outcome, run_flow_tests, AssertionFailure, FlowTestError, FlowTestOutcome,
};

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
