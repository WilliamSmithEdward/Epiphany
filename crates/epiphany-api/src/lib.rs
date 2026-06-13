//! Epiphany api: the REST + WebSocket surface.
//!
//! Phase 0 skeleton. Phase 2 fills this in: a clean JSON REST API (CRUD, cell
//! read/write), OpenAPI, native auth, and WebSocket change notifications, on
//! Axum. See `docs/ROADMAP.md`.

/// Stable crate identifier, reported by the server's wiring banner.
pub const CRATE: &str = "epiphany-api";

/// The engine subsystems this API exposes, by stable identifier.
pub fn wired_subsystems() -> [&'static str; 6] {
    [
        epiphany_core::CRATE,
        epiphany_calc::CRATE,
        epiphany_mdx::CRATE,
        epiphany_flow::CRATE,
        epiphany_security::CRATE,
        epiphany_persist::CRATE,
    ]
}

#[cfg(test)]
mod tests {
    #[test]
    fn crate_is_named() {
        assert_eq!(super::CRATE, "epiphany-api");
    }

    #[test]
    fn wires_all_engine_crates() {
        let subsystems = super::wired_subsystems();
        assert_eq!(subsystems.len(), 6);
        assert!(subsystems.contains(&"epiphany-core"));
        assert!(subsystems.contains(&"epiphany-persist"));
    }
}
