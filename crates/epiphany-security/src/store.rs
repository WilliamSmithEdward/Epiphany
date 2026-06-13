//! The security store: users, groups, and password hashes, persisted as a
//! separate model-as-code artifact. Only password *hashes* are stored, never
//! plaintext, and never inside the cube model.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine as _;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};

const FORMAT_TAG: &str = "epiphany-security/v0";
const ADMIN_USERNAME: &str = "admin";
const ADMINS_GROUP: &str = "admins";

/// An authenticated identity handed to request handlers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub username: String,
    pub is_admin: bool,
    pub groups: Vec<String>,
}

/// A randomly generated admin password, surfaced exactly once on first run.
/// Its `Debug` redacts the value so it never lands in logs (RG-13).
pub struct GeneratedAdminPassword(pub String);

impl std::fmt::Debug for GeneratedAdminPassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GeneratedAdminPassword(***)")
    }
}

/// What went wrong in a security operation.
#[derive(Debug)]
pub enum SecurityError {
    /// A user with this name already exists.
    UserExists(String),
    /// No user with this name.
    UserNotFound(String),
    /// The supplied current password did not verify.
    IncorrectPassword,
    /// Password hashing failed.
    Hashing(String),
    /// Reading or writing the security artifact failed.
    Io(std::io::Error),
    /// The security artifact could not be parsed.
    Format(String),
}

impl std::fmt::Display for SecurityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecurityError::UserExists(u) => write!(f, "user '{u}' already exists"),
            SecurityError::UserNotFound(u) => write!(f, "user '{u}' not found"),
            SecurityError::IncorrectPassword => write!(f, "incorrect password"),
            SecurityError::Hashing(e) => write!(f, "password hashing failed: {e}"),
            SecurityError::Io(e) => write!(f, "security artifact I/O error: {e}"),
            SecurityError::Format(m) => write!(f, "invalid security artifact: {m}"),
        }
    }
}

impl std::error::Error for SecurityError {}

impl From<std::io::Error> for SecurityError {
    fn from(e: std::io::Error) -> Self {
        SecurityError::Io(e)
    }
}

#[derive(Clone)]
struct User {
    is_admin: bool,
    password_hash: String,
    must_change_password: bool,
    groups: BTreeSet<String>,
}

impl std::fmt::Debug for User {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("User")
            .field("is_admin", &self.is_admin)
            .field("password_hash", &"<redacted>")
            .field("must_change_password", &self.must_change_password)
            .field("groups", &self.groups)
            .finish()
    }
}

/// In-memory users and groups with durable, hash-only persistence.
#[derive(Debug)]
pub struct SecurityStore {
    users: BTreeMap<String, User>,
    groups: BTreeSet<String>,
    path: Option<PathBuf>,
    fast_kdf: bool,
}

impl SecurityStore {
    /// Open the store at `path`, or create it on first run with an `admin` user.
    /// Without `admin_override`, a random admin password is generated and returned
    /// once (to be shown to the operator). `fast_kdf` lowers the Argon2 cost and
    /// must be used only in tests. Returns `(store, generated_password_or_none)`.
    pub fn open_or_bootstrap(
        path: PathBuf,
        fast_kdf: bool,
        admin_override: Option<&str>,
    ) -> Result<(Self, Option<GeneratedAdminPassword>), SecurityError> {
        if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            let mut store = Self::from_model_text(&text, fast_kdf)?;
            store.path = Some(path);
            return Ok((store, None));
        }
        let mut store = SecurityStore {
            users: BTreeMap::new(),
            groups: BTreeSet::new(),
            path: Some(path),
            fast_kdf,
        };
        store.groups.insert(ADMINS_GROUP.to_string());
        let password = admin_override
            .map(str::to_string)
            .unwrap_or_else(generate_password);
        store.insert_user(ADMIN_USERNAME, &password, true, true, &[ADMINS_GROUP])?;
        store.save()?;
        let generated = admin_override
            .is_none()
            .then_some(GeneratedAdminPassword(password));
        Ok((store, generated))
    }

    /// An in-memory store (no file) seeded with one user. A hermetic test seam.
    pub fn with_admin(username: &str, password: &str, is_admin: bool) -> Self {
        let mut store = SecurityStore {
            users: BTreeMap::new(),
            groups: BTreeSet::new(),
            path: None,
            fast_kdf: true,
        };
        store
            .insert_user(username, password, is_admin, false, &[])
            .expect("fresh store accepts the first user");
        store
    }

    fn insert_user(
        &mut self,
        username: &str,
        password: &str,
        is_admin: bool,
        must_change_password: bool,
        groups: &[&str],
    ) -> Result<(), SecurityError> {
        if self.users.contains_key(username) {
            return Err(SecurityError::UserExists(username.to_string()));
        }
        let password_hash = hash_password(password, self.fast_kdf)?;
        self.users.insert(
            username.to_string(),
            User {
                is_admin,
                password_hash,
                must_change_password,
                groups: groups.iter().map(|g| (*g).to_string()).collect(),
            },
        );
        Ok(())
    }

    /// Create a new user (admin operation), persisting the change.
    pub fn create_user(
        &mut self,
        username: &str,
        password: &str,
        is_admin: bool,
    ) -> Result<(), SecurityError> {
        self.insert_user(username, password, is_admin, false, &[])?;
        self.save()
    }

    /// Verify credentials, returning the principal on success.
    pub fn authenticate(&self, username: &str, password: &str) -> Option<Principal> {
        let user = self.users.get(username)?;
        verify_password(password, &user.password_hash).then(|| Principal {
            username: username.to_string(),
            is_admin: user.is_admin,
            groups: user.groups.iter().cloned().collect(),
        })
    }

    /// Whether a user must change their password before normal use.
    pub fn must_change_password(&self, username: &str) -> bool {
        self.users
            .get(username)
            .is_some_and(|u| u.must_change_password)
    }

    /// Change a user's password after verifying the current one, persisting it.
    pub fn change_password(
        &mut self,
        username: &str,
        current: &str,
        new: &str,
    ) -> Result<(), SecurityError> {
        let user = self
            .users
            .get(username)
            .ok_or_else(|| SecurityError::UserNotFound(username.to_string()))?;
        if !verify_password(current, &user.password_hash) {
            return Err(SecurityError::IncorrectPassword);
        }
        let new_hash = hash_password(new, self.fast_kdf)?;
        let user = self.users.get_mut(username).expect("user present");
        user.password_hash = new_hash;
        user.must_change_password = false;
        self.save()
    }

    /// Number of users.
    pub fn user_count(&self) -> usize {
        self.users.len()
    }

    /// Serialize to the canonical security model-as-code text (hashes only).
    pub fn to_model_text(&self) -> String {
        let doc = SecurityDoc {
            format: FORMAT_TAG.to_string(),
            users: self
                .users
                .iter()
                .map(|(username, u)| UserDoc {
                    username: username.clone(),
                    is_admin: u.is_admin,
                    must_change_password: u.must_change_password,
                    password_hash: u.password_hash.clone(),
                    groups: u.groups.iter().cloned().collect(),
                })
                .collect(),
            groups: self.groups.iter().cloned().collect(),
        };
        toml::to_string(&doc).expect("security document serializes")
    }

    fn from_model_text(text: &str, fast_kdf: bool) -> Result<Self, SecurityError> {
        let doc: SecurityDoc =
            toml::from_str(text).map_err(|e| SecurityError::Format(e.to_string()))?;
        if doc.format != FORMAT_TAG {
            return Err(SecurityError::Format(format!(
                "unknown security format '{}'",
                doc.format
            )));
        }
        let users = doc
            .users
            .into_iter()
            .map(|u| {
                (
                    u.username,
                    User {
                        is_admin: u.is_admin,
                        password_hash: u.password_hash,
                        must_change_password: u.must_change_password,
                        groups: u.groups.into_iter().collect(),
                    },
                )
            })
            .collect();
        Ok(SecurityStore {
            users,
            groups: doc.groups.into_iter().collect(),
            path: None,
            fast_kdf,
        })
    }

    fn save(&self) -> Result<(), SecurityError> {
        if let Some(path) = &self.path {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = path.with_extension("model.tmp");
            std::fs::write(&tmp, self.to_model_text())?;
            std::fs::rename(&tmp, path)?;
        }
        Ok(())
    }
}

// ---- password hashing (Argon2id) ----

fn argon2(fast: bool) -> Argon2<'static> {
    if fast {
        // Minimal cost for tests only; production hashes use the strong default.
        let params = Params::new(8, 1, 1, None).expect("valid argon2 params");
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
    } else {
        Argon2::default()
    }
}

fn hash_password(password: &str, fast: bool) -> Result<String, SecurityError> {
    let salt = SaltString::generate(&mut OsRng);
    argon2(fast)
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| SecurityError::Hashing(e.to_string()))
}

/// Constant-time verify. Parameters are read from the stored PHC string, so the
/// `fast` distinction only affects hashing, not verification.
fn verify_password(password: &str, phc: &str) -> bool {
    PasswordHash::new(phc)
        .map(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
        .unwrap_or(false)
}

/// A readable random password (~24 chars of url-safe base64 over 18 bytes).
fn generate_password() -> String {
    let mut bytes = [0u8; 18];
    OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

// ---- serialized document shape ----

#[derive(Serialize, Deserialize)]
struct SecurityDoc {
    format: String,
    #[serde(default, rename = "user")]
    users: Vec<UserDoc>,
    #[serde(default)]
    groups: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct UserDoc {
    username: String,
    is_admin: bool,
    must_change_password: bool,
    password_hash: String,
    #[serde(default)]
    groups: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("epiphany-sec-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        dir.join("security.model")
    }

    #[test]
    fn bootstrap_creates_admin_then_reopens() {
        let path = scratch("bootstrap");
        let (store, generated) =
            SecurityStore::open_or_bootstrap(path.clone(), true, None).unwrap();
        let password = generated.expect("first run generates a password").0;
        assert!(store.authenticate("admin", &password).unwrap().is_admin);
        assert!(store.must_change_password("admin"));

        // Reopening loads the persisted store and generates nothing new.
        let (reopened, none) = SecurityStore::open_or_bootstrap(path, true, None).unwrap();
        assert!(none.is_none());
        assert!(reopened.authenticate("admin", &password).is_some());
        assert!(reopened.authenticate("admin", "wrong").is_none());
    }

    #[test]
    fn with_admin_is_hermetic_and_authenticates() {
        let store = SecurityStore::with_admin("alice", "s3cret", true);
        let principal = store.authenticate("alice", "s3cret").unwrap();
        assert_eq!(principal.username, "alice");
        assert!(principal.is_admin);
        assert!(store.authenticate("alice", "nope").is_none());
        assert!(store.authenticate("bob", "s3cret").is_none());
    }

    #[test]
    fn change_password_requires_correct_current() {
        let mut store = SecurityStore::with_admin("alice", "old", false);
        assert!(matches!(
            store.change_password("alice", "wrong", "new"),
            Err(SecurityError::IncorrectPassword)
        ));
        store.change_password("alice", "old", "new").unwrap();
        assert!(store.authenticate("alice", "new").is_some());
        assert!(store.authenticate("alice", "old").is_none());
    }

    #[test]
    fn model_text_round_trips_and_still_verifies() {
        let path = scratch("roundtrip");
        let (store, generated) =
            SecurityStore::open_or_bootstrap(path, true, Some("known-pass")).unwrap();
        assert!(
            generated.is_none(),
            "override suppresses the generated password"
        );
        let text = store.to_model_text();
        let reloaded = SecurityStore::from_model_text(&text, true).unwrap();
        // The hash survived the round-trip and still verifies; no plaintext stored.
        assert!(reloaded.authenticate("admin", "known-pass").is_some());
        assert!(!text.contains("known-pass"));
    }

    #[test]
    fn unknown_format_is_rejected() {
        let err = SecurityStore::from_model_text("format = \"nope\"\n", true).unwrap_err();
        assert!(matches!(err, SecurityError::Format(_)));
    }
}
