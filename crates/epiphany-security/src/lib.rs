//! Epiphany security: users, groups, password hashing, and (Phase 7) object and
//! element authorization.
//!
//! Phase 2 (M2) delivers native authentication: an Argon2id-backed user store
//! ([`SecurityStore`]) persisted as a separate, hash-only model-as-code artifact,
//! plus a generated first-run admin. Authorization in M2 is authenticated plus
//! admin-or-not; per-object and per-element authorization arrives in Phase 7.

mod acl;
mod audit;
mod password;
mod secret;
mod store;

pub use acl::{AccessLevel, AccessList, ObjectKind, ObjectRef, Scope, Subject};
pub use audit::{AuditAction, AuditFilter, AuditLog, AuditRecord, RetentionPolicy};
pub use password::PasswordPolicy;
pub use secret::SecretStore;
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

/// Configure an [`OpenOptions`](std::fs::OpenOptions) to create new files
/// owner-only (`0600`) on Unix (ADR-0017); a no-op elsewhere. `mode` applies
/// only at creation, so pair it with [`restrict_to_owner`] to also normalize a
/// pre-existing file.
pub(crate) fn set_owner_only(opts: &mut std::fs::OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    #[cfg(not(unix))]
    {
        let _ = opts;
    }
}

/// Write `contents` to `path`, owner-only (`0600`) **from creation** on Unix, so
/// the bytes are never momentarily world-readable in the window between a write
/// and a later chmod (ADR-0017). Elsewhere it is a plain write under the data
/// directory's inherited ACL.
pub(crate) fn write_owner_only(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(contents)?;
        // `mode` applies only on creation; normalize in case the file pre-existed
        // (e.g. a stale temp from an interrupted save) with looser bits.
        restrict_to_owner(path)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)?;
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
