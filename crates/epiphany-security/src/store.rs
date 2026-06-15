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

use crate::acl::{AccessLevel, AccessList, ObjectKind, ObjectRef, Subject};

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

/// A user summary for the admin listing surface (never includes the hash).
#[derive(Debug, Clone)]
pub struct UserView {
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

/// The key for an element grant: (cube, dimension, element name).
type ElementKey = (String, String, String);

/// In-memory users, groups, and access grants with durable, hash-only
/// persistence. Object and element ACLs (ADR-0015) live in the same artifact.
#[derive(Debug)]
pub struct SecurityStore {
    users: BTreeMap<String, User>,
    groups: BTreeSet<String>,
    object_acls: BTreeMap<ObjectRef, AccessList>,
    element_acls: BTreeMap<ElementKey, AccessList>,
    path: Option<PathBuf>,
    fast_kdf: bool,
    /// Deployment posture for an ungranted cube (ADR-0015 decision 2a): when
    /// `false` (the secure default), an ungranted cube is closed to non-admins;
    /// when `true`, it is open to any authenticated user at `Write` (the
    /// trusted-single-org convenience). Not persisted in the artifact; the
    /// composition root sets it from configuration.
    default_cube_open: bool,
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
            object_acls: BTreeMap::new(),
            element_acls: BTreeMap::new(),
            path: Some(path),
            fast_kdf,
            default_cube_open: false,
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
            object_acls: BTreeMap::new(),
            element_acls: BTreeMap::new(),
            path: None,
            fast_kdf: true,
            default_cube_open: false,
        };
        store
            .insert_user(username, password, is_admin, false, &[])
            .expect("fresh store accepts the first user");
        store
    }

    /// Set the ungranted-cube posture (ADR-0015 decision 2a). The composition
    /// root calls this from configuration; the secure default is closed.
    pub fn set_default_cube_open(&mut self, open: bool) {
        self.default_cube_open = open;
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

    // ---- admin management (Phase 7) ----

    /// All users (without hashes), for the admin listing surface.
    pub fn list_users(&self) -> Vec<UserView> {
        self.users
            .iter()
            .map(|(username, u)| UserView {
                username: username.clone(),
                is_admin: u.is_admin,
                groups: u.groups.iter().cloned().collect(),
            })
            .collect()
    }

    /// Create a user with an initial group set (admin operation), persisting.
    pub fn create_user_with_groups(
        &mut self,
        username: &str,
        password: &str,
        is_admin: bool,
        groups: &[String],
    ) -> Result<(), SecurityError> {
        let refs: Vec<&str> = groups.iter().map(String::as_str).collect();
        self.insert_user(username, password, is_admin, false, &refs)?;
        for g in groups {
            self.groups.insert(g.clone());
        }
        self.save()
    }

    /// Delete a user. Returns whether one was removed; persists on change.
    pub fn delete_user(&mut self, username: &str) -> Result<bool, SecurityError> {
        let removed = self.users.remove(username).is_some();
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// Set a user's admin flag, persisting.
    pub fn set_user_admin(&mut self, username: &str, is_admin: bool) -> Result<(), SecurityError> {
        let user = self
            .users
            .get_mut(username)
            .ok_or_else(|| SecurityError::UserNotFound(username.to_string()))?;
        user.is_admin = is_admin;
        self.save()
    }

    /// Replace a user's group membership, persisting (and registering any new
    /// groups in the global set).
    pub fn set_user_groups(
        &mut self,
        username: &str,
        groups: &[String],
    ) -> Result<(), SecurityError> {
        let user = self
            .users
            .get_mut(username)
            .ok_or_else(|| SecurityError::UserNotFound(username.to_string()))?;
        user.groups = groups.iter().cloned().collect();
        for g in groups {
            self.groups.insert(g.clone());
        }
        self.save()
    }

    /// Reset a user's password (admin operation), persisting.
    pub fn reset_password(&mut self, username: &str, new: &str) -> Result<(), SecurityError> {
        let new_hash = hash_password(new, self.fast_kdf)?;
        let user = self
            .users
            .get_mut(username)
            .ok_or_else(|| SecurityError::UserNotFound(username.to_string()))?;
        user.password_hash = new_hash;
        self.save()
    }

    /// All group names.
    pub fn list_groups(&self) -> Vec<String> {
        self.groups.iter().cloned().collect()
    }

    /// Create a group (idempotent), persisting.
    pub fn create_group(&mut self, name: &str) -> Result<(), SecurityError> {
        self.groups.insert(name.to_string());
        self.save()
    }

    /// Delete a group and remove it from every user's membership. Returns whether
    /// it existed; persists on change. (Any grants naming it become dangling and
    /// are simply never consulted, per ADR-0015.)
    pub fn delete_group(&mut self, name: &str) -> Result<bool, SecurityError> {
        let removed = self.groups.remove(name);
        if removed {
            for user in self.users.values_mut() {
                user.groups.remove(name);
            }
            self.save()?;
        }
        Ok(removed)
    }

    // ---- access resolution (ADR-0015) ----

    /// The grant-based access a principal has to an object: `Admin` if the
    /// principal is an admin (bypass), else the most-permissive of the object's
    /// user and group grants (`None` if none). The owner and public fallbacks
    /// are composed at the API boundary, which knows the object's owner and
    /// visibility from the model snapshot.
    pub fn object_access(&self, principal: &Principal, obj: &ObjectRef) -> AccessLevel {
        if principal.is_admin {
            return AccessLevel::Admin;
        }
        self.object_acls
            .get(obj)
            .map(|list| list.level_for(&principal.username, &principal.groups))
            .unwrap_or(AccessLevel::None)
    }

    /// The current principal for a username (admin flag + groups) from the live
    /// store, for per-request re-resolution. `None` if the user no longer exists.
    pub fn principal(&self, username: &str) -> Option<Principal> {
        self.users.get(username).map(|u| Principal {
            username: username.to_string(),
            is_admin: u.is_admin,
            groups: u.groups.iter().cloned().collect(),
        })
    }

    /// The effective access a `username` has to an object, re-resolved against the
    /// live store and composed with the owner (-> Write) and public (-> Read)
    /// fallbacks the API supplies from the model snapshot (ADR-0015). An unknown
    /// user gets `None`. This is the single entry point the API gates on.
    pub fn resolve_access(
        &self,
        username: &str,
        obj: &ObjectRef,
        owner: Option<&str>,
        public: bool,
    ) -> AccessLevel {
        let Some(p) = self.principal(username) else {
            return AccessLevel::None;
        };
        if p.is_admin {
            return AccessLevel::Admin;
        }
        let mut level = self
            .object_acls
            .get(obj)
            .map(|list| list.level_for(&p.username, &p.groups))
            .unwrap_or(AccessLevel::None);
        if owner == Some(username) {
            level = level.max(AccessLevel::Write);
        }
        if public && level < AccessLevel::Read {
            level = AccessLevel::Read;
        }
        level
    }

    /// Effective access to a cube for object-level gating (ADR-0015, decision
    /// 2a). Once an admin adds any grant the cube is "managed" and access is
    /// exactly the grants (plus admin bypass). A cube with no grants is
    /// "unmanaged": closed to non-admins by default (secure, fail-closed), or
    /// open to any authenticated user at `Write` when the deployment opts into the
    /// trusted-single-org posture via [`set_default_cube_open`]. An unmanaged cube
    /// is never `Admin`; an unknown user always gets `None`.
    pub fn cube_access(&self, username: &str, cube: &str) -> AccessLevel {
        let Some(p) = self.principal(username) else {
            return AccessLevel::None;
        };
        if p.is_admin {
            return AccessLevel::Admin;
        }
        match self.object_acls.get(&ObjectRef::cube(cube)) {
            Some(list) => list.level_for(&p.username, &p.groups),
            None if self.default_cube_open => AccessLevel::Write,
            None => AccessLevel::None,
        }
    }

    /// The element restriction for a `(cube, dimension, element)`: `None` means no
    /// element ACL applies, so the member is unrestricted (an admin is always
    /// unrestricted); `Some(level)` means the member is restricted to `level`.
    pub fn element_access(
        &self,
        principal: &Principal,
        cube: &str,
        dim: &str,
        element: &str,
    ) -> Option<AccessLevel> {
        if principal.is_admin {
            return None;
        }
        self.element_acls
            .get(&(cube.to_string(), dim.to_string(), element.to_string()))
            .map(|list| list.level_for(&principal.username, &principal.groups))
    }

    /// Whether a principal may read an element (unrestricted, or restricted at
    /// least to Read).
    pub fn element_readable(
        &self,
        principal: &Principal,
        cube: &str,
        dim: &str,
        element: &str,
    ) -> bool {
        self.element_access(principal, cube, dim, element)
            .map(|l| l >= AccessLevel::Read)
            .unwrap_or(true)
    }

    /// Whether a principal may write an element.
    pub fn element_writable(
        &self,
        principal: &Principal,
        cube: &str,
        dim: &str,
        element: &str,
    ) -> bool {
        self.element_access(principal, cube, dim, element)
            .map(|l| l >= AccessLevel::Write)
            .unwrap_or(true)
    }

    /// Whether any element ACL exists for a `(cube, dimension)` - lets the hot
    /// path skip building a mask when there are none.
    pub fn has_element_acls(&self, cube: &str, dim: &str) -> bool {
        self.element_acls
            .keys()
            .any(|(c, d, _)| c == cube && d == dim)
    }

    /// Set (or, with `AccessLevel::None`, remove) an object grant for a subject,
    /// persisting the change.
    pub fn set_object_access(
        &mut self,
        obj: ObjectRef,
        subject: &Subject,
        level: AccessLevel,
    ) -> Result<(), SecurityError> {
        let list = self.object_acls.entry(obj.clone()).or_default();
        list.set(subject, level);
        if list.is_empty() {
            self.object_acls.remove(&obj);
        }
        self.save()
    }

    /// Set (or remove) an element grant for a subject, persisting the change.
    pub fn set_element_access(
        &mut self,
        cube: &str,
        dim: &str,
        element: &str,
        subject: &Subject,
        level: AccessLevel,
    ) -> Result<(), SecurityError> {
        let key = (cube.to_string(), dim.to_string(), element.to_string());
        let list = self.element_acls.entry(key.clone()).or_default();
        list.set(subject, level);
        if list.is_empty() {
            self.element_acls.remove(&key);
        }
        self.save()
    }

    /// All object grants, for the admin listing surface.
    pub fn object_acls(&self) -> &BTreeMap<ObjectRef, AccessList> {
        &self.object_acls
    }

    /// All element grants, for the admin listing surface.
    pub fn element_acls(&self) -> &BTreeMap<(String, String, String), AccessList> {
        &self.element_acls
    }

    /// Serialize to the canonical security model-as-code text (hashes only).
    /// Grants are flattened to one sorted row per (object/element, subject), so
    /// the output is byte-stable.
    pub fn to_model_text(&self) -> String {
        let mut object_acls = Vec::new();
        for (obj, list) in &self.object_acls {
            for (user, level) in &list.users {
                object_acls.push(ObjectAclDoc {
                    kind: obj.kind.as_str().to_string(),
                    cube: obj.cube.clone(),
                    name: obj.name.clone(),
                    subject_kind: "user".to_string(),
                    subject: user.clone(),
                    level: level.as_str().to_string(),
                });
            }
            for (group, level) in &list.groups {
                object_acls.push(ObjectAclDoc {
                    kind: obj.kind.as_str().to_string(),
                    cube: obj.cube.clone(),
                    name: obj.name.clone(),
                    subject_kind: "group".to_string(),
                    subject: group.clone(),
                    level: level.as_str().to_string(),
                });
            }
        }
        let mut element_acls = Vec::new();
        for ((cube, dim, element), list) in &self.element_acls {
            for (user, level) in &list.users {
                element_acls.push(ElementAclDoc {
                    cube: cube.clone(),
                    dimension: dim.clone(),
                    element: element.clone(),
                    subject_kind: "user".to_string(),
                    subject: user.clone(),
                    level: level.as_str().to_string(),
                });
            }
            for (group, level) in &list.groups {
                element_acls.push(ElementAclDoc {
                    cube: cube.clone(),
                    dimension: dim.clone(),
                    element: element.clone(),
                    subject_kind: "group".to_string(),
                    subject: group.clone(),
                    level: level.as_str().to_string(),
                });
            }
        }
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
            object_acls,
            element_acls,
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
        // Rebuild the grant maps. Tolerate unknown kinds/levels/subjects rather
        // than failing the load (ADR-0015: load is total; dangling/odd rows are
        // simply never consulted).
        let mut object_acls: BTreeMap<ObjectRef, AccessList> = BTreeMap::new();
        for row in doc.object_acls {
            if let (Some(kind), Some(level), Some(subject)) = (
                ObjectKind::parse(&row.kind),
                AccessLevel::parse(&row.level),
                subject_from(&row.subject_kind, &row.subject),
            ) {
                object_acls
                    .entry(ObjectRef {
                        kind,
                        cube: row.cube,
                        name: row.name,
                    })
                    .or_default()
                    .set(&subject, level);
            }
        }
        let mut element_acls: BTreeMap<ElementKey, AccessList> = BTreeMap::new();
        for row in doc.element_acls {
            if let (Some(level), Some(subject)) = (
                AccessLevel::parse(&row.level),
                subject_from(&row.subject_kind, &row.subject),
            ) {
                element_acls
                    .entry((row.cube, row.dimension, row.element))
                    .or_default()
                    .set(&subject, level);
            }
        }
        Ok(SecurityStore {
            users,
            groups: doc.groups.into_iter().collect(),
            object_acls,
            element_acls,
            path: None,
            fast_kdf,
            default_cube_open: false,
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
    // ACLs are additive and skipped when empty, so pre-Phase-7 (v0) files load
    // and re-serialize byte-identically (ADR-0015); the format tag is unchanged.
    #[serde(default, rename = "object_acl", skip_serializing_if = "Vec::is_empty")]
    object_acls: Vec<ObjectAclDoc>,
    #[serde(default, rename = "element_acl", skip_serializing_if = "Vec::is_empty")]
    element_acls: Vec<ElementAclDoc>,
}

#[derive(Serialize, Deserialize)]
struct ObjectAclDoc {
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cube: Option<String>,
    name: String,
    subject_kind: String,
    subject: String,
    level: String,
}

#[derive(Serialize, Deserialize)]
struct ElementAclDoc {
    cube: String,
    dimension: String,
    element: String,
    subject_kind: String,
    subject: String,
    level: String,
}

/// Build a `Subject` from a serialized `(subject_kind, subject)`; `None` for an
/// unrecognized kind (tolerated on load).
fn subject_from(kind: &str, name: &str) -> Option<Subject> {
    match kind {
        "user" => Some(Subject::User(name.to_string())),
        "group" => Some(Subject::Group(name.to_string())),
        _ => None,
    }
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

    // ---- ACLs (Phase 7, ADR-0015) ----

    fn principal(name: &str, admin: bool, groups: &[&str]) -> Principal {
        Principal {
            username: name.to_string(),
            is_admin: admin,
            groups: groups.iter().map(|g| g.to_string()).collect(),
        }
    }

    #[test]
    fn object_access_resolves_grants_with_admin_bypass() {
        let mut store = SecurityStore::with_admin("admin", "pw", true);
        let cube = ObjectRef::cube("Sales");
        let admin = principal("admin", true, &[]);
        let ann = principal("ann", false, &["editors"]);
        let bob = principal("bob", false, &[]);

        // No grants: admin bypasses to Admin; others get None.
        assert_eq!(store.object_access(&admin, &cube), AccessLevel::Admin);
        assert_eq!(store.object_access(&ann, &cube), AccessLevel::None);

        // Direct Read for ann, Write for her group: most-permissive (Write) wins.
        store
            .set_object_access(
                cube.clone(),
                &Subject::User("ann".into()),
                AccessLevel::Read,
            )
            .unwrap();
        store
            .set_object_access(
                cube.clone(),
                &Subject::Group("editors".into()),
                AccessLevel::Write,
            )
            .unwrap();
        assert_eq!(store.object_access(&ann, &cube), AccessLevel::Write);
        assert_eq!(store.object_access(&bob, &cube), AccessLevel::None);
        assert_eq!(store.object_access(&admin, &cube), AccessLevel::Admin);

        // Revoke the group grant: ann falls back to her direct Read.
        store
            .set_object_access(
                cube.clone(),
                &Subject::Group("editors".into()),
                AccessLevel::None,
            )
            .unwrap();
        assert_eq!(store.object_access(&ann, &cube), AccessLevel::Read);
    }

    #[test]
    fn resolve_access_composes_grants_owner_and_public() {
        let mut store = SecurityStore::with_admin("admin", "pw", true);
        // ann is a real (authenticated) user; "ghost" below never exists, so it
        // is denied even on a public object (ADR-0015 decision 3).
        store.create_user("ann", "pw", false).unwrap();
        let view = ObjectRef::in_cube(ObjectKind::View, "Sales", "Grid");

        // A non-owner non-grantee on a private object: no access.
        assert_eq!(
            store.resolve_access("ann", &view, None, false),
            AccessLevel::None
        );
        // Public objects are readable by anyone.
        assert_eq!(
            store.resolve_access("ann", &view, None, true),
            AccessLevel::Read
        );
        // The owner gets Write even on a private object.
        assert_eq!(
            store.resolve_access("ann", &view, Some("ann"), false),
            AccessLevel::Write
        );
        // A grant raises a non-owner above the public floor.
        store
            .set_object_access(
                view.clone(),
                &Subject::User("ann".into()),
                AccessLevel::Admin,
            )
            .unwrap();
        assert_eq!(
            store.resolve_access("ann", &view, None, false),
            AccessLevel::Admin
        );
        // Admin bypasses regardless; an unknown user gets nothing even if public.
        assert_eq!(
            store.resolve_access("admin", &view, None, false),
            AccessLevel::Admin
        );
        assert_eq!(
            store.resolve_access("ghost", &view, None, true),
            AccessLevel::None
        );
    }

    #[test]
    fn cube_access_is_closed_by_default_and_governed_by_grants() {
        let mut store = SecurityStore::with_admin("admin", "pw", true);
        store.create_user("ann", "pw", false).unwrap();
        store.create_user("bob", "pw", false).unwrap();
        // Secure default (fail-closed): an ungranted cube denies a non-admin; the
        // admin still bypasses.
        assert_eq!(store.cube_access("ann", "Sales"), AccessLevel::None);
        assert_eq!(store.cube_access("admin", "Sales"), AccessLevel::Admin);
        // Granting one user governs the cube exactly by its grants.
        store
            .set_object_access(
                ObjectRef::cube("Sales"),
                &Subject::User("ann".into()),
                AccessLevel::Read,
            )
            .unwrap();
        assert_eq!(store.cube_access("ann", "Sales"), AccessLevel::Read);
        assert_eq!(store.cube_access("bob", "Sales"), AccessLevel::None);
        assert_eq!(store.cube_access("admin", "Sales"), AccessLevel::Admin);
    }

    #[test]
    fn cube_access_open_posture_is_opt_in() {
        let mut store = SecurityStore::with_admin("admin", "pw", true);
        store.create_user("ann", "pw", false).unwrap();
        // Opt into the trusted-single-org posture: an ungranted cube is open at
        // Write to any authenticated user, but never Admin.
        store.set_default_cube_open(true);
        assert_eq!(store.cube_access("ann", "Sales"), AccessLevel::Write);
        // The first grant still makes the cube managed (grants govern exactly).
        store
            .set_object_access(
                ObjectRef::cube("Sales"),
                &Subject::User("ann".into()),
                AccessLevel::Read,
            )
            .unwrap();
        store.create_user("bob", "pw", false).unwrap();
        assert_eq!(store.cube_access("ann", "Sales"), AccessLevel::Read);
        assert_eq!(store.cube_access("bob", "Sales"), AccessLevel::None);
        // A different, still-ungranted cube remains open under this posture.
        assert_eq!(store.cube_access("bob", "Other"), AccessLevel::Write);
    }

    #[test]
    fn element_acl_restricts_a_member_to_granted_subjects() {
        let mut store = SecurityStore::with_admin("admin", "pw", true);
        let ann = principal("ann", false, &[]);
        let admin = principal("admin", true, &[]);
        let mgr = principal("mary", false, &["managers"]);

        // No element ACLs: every member readable.
        assert!(store.element_readable(&ann, "Sales", "Region", "North"));
        assert!(!store.has_element_acls("Sales", "Region"));

        // Restrict North to the managers group (Read): ann is now denied, a
        // manager may read but not write, and South (no ACL) stays open.
        store
            .set_element_access(
                "Sales",
                "Region",
                "North",
                &Subject::Group("managers".into()),
                AccessLevel::Read,
            )
            .unwrap();
        assert!(store.has_element_acls("Sales", "Region"));
        assert!(!store.element_readable(&ann, "Sales", "Region", "North"));
        assert!(store.element_readable(&mgr, "Sales", "Region", "North"));
        assert!(!store.element_writable(&mgr, "Sales", "Region", "North"));
        assert!(store.element_readable(&ann, "Sales", "Region", "South"));
        // Admin bypasses element security entirely.
        assert!(store.element_readable(&admin, "Sales", "Region", "North"));
        assert!(store.element_writable(&admin, "Sales", "Region", "North"));
    }

    #[test]
    fn acls_round_trip_byte_identical() {
        let mut store = SecurityStore::with_admin("admin", "pw", true);
        store
            .set_object_access(
                ObjectRef::cube("Sales"),
                &Subject::User("ann".into()),
                AccessLevel::Write,
            )
            .unwrap();
        store
            .set_object_access(
                ObjectRef::in_cube(ObjectKind::Rule, "Sales", "margin"),
                &Subject::Group("editors".into()),
                AccessLevel::Read,
            )
            .unwrap();
        store
            .set_element_access(
                "Sales",
                "Region",
                "North",
                &Subject::Group("managers".into()),
                AccessLevel::Read,
            )
            .unwrap();

        let text1 = store.to_model_text();
        let store2 = SecurityStore::from_model_text(&text1, true).unwrap();
        let text2 = store2.to_model_text();
        assert_eq!(text1, text2, "ACLs must round-trip byte-identically");

        // The grants survived the round-trip.
        let ann = principal("ann", false, &[]);
        assert_eq!(
            store2.object_access(&ann, &ObjectRef::cube("Sales")),
            AccessLevel::Write
        );
        let mgr = principal("m", false, &["managers"]);
        assert!(store2.element_readable(&mgr, "Sales", "Region", "North"));
        assert!(!store2.element_readable(&ann, "Sales", "Region", "North"));
    }

    #[test]
    fn pre_phase7_file_without_acls_loads() {
        // A v0 artifact (users only, no acl sections) loads under the unchanged
        // format tag, with empty grant maps.
        let text = format!(
            "format = \"{FORMAT_TAG}\"\n\n[[user]]\nusername = \"admin\"\nis_admin = true\nmust_change_password = false\npassword_hash = \"x\"\n"
        );
        let store = SecurityStore::from_model_text(&text, true).unwrap();
        assert_eq!(store.user_count(), 1);
        assert!(store.object_acls().is_empty());
        assert!(store.element_acls().is_empty());
    }
}
