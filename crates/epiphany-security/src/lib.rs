//! Epiphany security — users/groups and object & element authorization.
//!
//! Phase 0 skeleton. Phase 7 fills this in: native authentication, groups,
//! admin vs non-admin, and object + element security. See `docs/ROADMAP.md`.

/// Stable crate identifier, reported by the server's wiring banner.
pub const CRATE: &str = "epiphany-security";

#[cfg(test)]
mod tests {
    #[test]
    fn crate_is_named() {
        assert_eq!(super::CRATE, "epiphany-security");
    }

    #[test]
    fn links_core() {
        assert_eq!(epiphany_core::CRATE, "epiphany-core");
    }
}
