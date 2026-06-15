//! Epiphany security: users, groups, password hashing, and (Phase 7) object and
//! element authorization.
//!
//! Phase 2 (M2) delivers native authentication: an Argon2id-backed user store
//! ([`SecurityStore`]) persisted as a separate, hash-only model-as-code artifact,
//! plus a generated first-run admin. Authorization in M2 is authenticated plus
//! admin-or-not; per-object and per-element authorization arrives in Phase 7.

mod acl;
mod audit;
mod store;

pub use acl::{
    AccessLevel, AccessList, CubeGrant, DenyList, GrantEffect, ObjectKind, ObjectRef, Subject,
};
pub use audit::{AuditAction, AuditFilter, AuditLog, AuditRecord, RetentionPolicy};
pub use store::{GeneratedAdminPassword, Principal, SecurityError, SecurityStore, UserView};

/// Stable crate identifier, reported by the server's wiring banner.
pub const CRATE: &str = "epiphany-security";

/// Restrict a secret file to owner-only access (`0600`) on Unix (ADR-0017); a
/// no-op elsewhere, where the data directory's inherited ACL governs and the
/// operator is responsible for protecting it.
pub(crate) fn restrict_to_owner(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn crate_is_named() {
        assert_eq!(super::CRATE, "epiphany-security");
    }
}
