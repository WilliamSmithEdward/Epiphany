//! Epiphany security: users, groups, password hashing, and (Phase 7) object and
//! element authorization.
//!
//! Phase 2 (M2) delivers native authentication: an Argon2id-backed user store
//! ([`SecurityStore`]) persisted as a separate, hash-only model-as-code artifact,
//! plus a generated first-run admin. Authorization in M2 is authenticated plus
//! admin-or-not; per-object and per-element authorization arrives in Phase 7.

mod acl;
mod store;

pub use acl::{AccessLevel, AccessList, ObjectKind, ObjectRef, Subject};
pub use store::{GeneratedAdminPassword, Principal, SecurityError, SecurityStore};

/// Stable crate identifier, reported by the server's wiring banner.
pub const CRATE: &str = "epiphany-security";

#[cfg(test)]
mod tests {
    #[test]
    fn crate_is_named() {
        assert_eq!(super::CRATE, "epiphany-security");
    }
}
