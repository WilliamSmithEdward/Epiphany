//! Password strength policy (ADR-0017, Tier-2 hardening).
//!
//! A small, dependency-free check applied to user-set passwords (create user,
//! change password, admin reset). It is deliberately conservative: a minimum
//! length plus a reject-list of obviously weak passwords. It never echoes the
//! password; failures carry only a client-safe reason.

/// The configurable password-strength policy. `Default` is the secure baseline
/// (12-character minimum, reject the common-password list); the in-memory test
/// seam uses [`permissive`](PasswordPolicy::permissive).
#[derive(Debug, Clone)]
pub struct PasswordPolicy {
    /// Minimum length in Unicode scalar values.
    pub min_length: usize,
    /// Reject passwords on the embedded common-password list.
    pub reject_common: bool,
}

impl Default for PasswordPolicy {
    fn default() -> Self {
        Self {
            min_length: 12,
            reject_common: true,
        }
    }
}

impl PasswordPolicy {
    /// A policy that accepts any non-empty password. Used by the hermetic test
    /// seam so fixtures can use short passwords; never used by the real server.
    pub fn permissive() -> Self {
        Self {
            min_length: 1,
            reject_common: false,
        }
    }

    /// Check a candidate password. `Ok(())` if it satisfies the policy, else a
    /// client-safe reason (no password material).
    pub fn check(&self, password: &str) -> Result<(), String> {
        let len = password.chars().count();
        if len < self.min_length {
            return Err(format!(
                "password must be at least {} characters",
                self.min_length
            ));
        }
        if self.reject_common && is_common(password) {
            return Err("password is too common; choose a less guessable one".to_string());
        }
        Ok(())
    }
}

/// A small embedded list of the most-guessed passwords (and trivial variants).
/// Matched case-insensitively against the whole password. Not exhaustive: a
/// backstop against the worst choices, complementing the length minimum.
const COMMON: &[&str] = &[
    "password",
    "password1",
    "password12",
    "password123",
    "password1234",
    "passw0rd",
    "qwerty",
    "qwerty123",
    "qwertyuiop",
    "123456",
    "1234567",
    "12345678",
    "123456789",
    "1234567890",
    "111111",
    "000000",
    "abc123",
    "iloveyou",
    "admin",
    "administrator",
    "letmein",
    "welcome",
    "welcome1",
    "monkey",
    "dragon",
    "sunshine",
    "princess",
    "football",
    "baseball",
    "changeme",
    "changeme1",
    "secret",
    "epiphany",
    "epiphany1",
];

fn is_common(password: &str) -> bool {
    let lower = password.to_ascii_lowercase();
    COMMON.contains(&lower.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_enforces_length_and_rejects_common() {
        let p = PasswordPolicy::default();
        assert!(p.check("x").is_err()); // too short
        assert!(p.check("a1234567890").is_err()); // 11 chars
        assert!(p.check("aA1!234567890").is_ok()); // 13 chars, fine
                                                   // A 12+ char common password is still rejected.
        assert!(p.check("password1234").is_err());
        assert!(p.check("PassWord1234").is_err()); // case-insensitive
    }

    #[test]
    fn permissive_policy_accepts_short_nonempty() {
        let p = PasswordPolicy::permissive();
        assert!(p.check("pw").is_ok());
        assert!(p.check("").is_err()); // still rejects empty (min 1)
    }
}
