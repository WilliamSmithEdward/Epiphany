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

use crate::acl::{AccessLevel, AccessList, ObjectKind, Scope, Subject};
use crate::password::PasswordPolicy;

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
    /// A new password did not meet the strength policy (ADR-0017). The string is
    /// a client-safe reason (never echoes the password).
    WeakPassword(String),
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
            SecurityError::WeakPassword(why) => write!(f, "{why}"),
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
/// persistence. The modular per-object-kind grants and the element ACLs live in
/// the same artifact.
#[derive(Debug)]
pub struct SecurityStore {
    users: BTreeMap<String, User>,
    groups: BTreeSet<String>,
    element_acls: BTreeMap<ElementKey, AccessList>,
    /// Modular per-object-kind grants (ADR-0023): `(scope, kind) -> who has what`.
    /// The single authorization scheme; it superseded the per-object ACLs of
    /// ADR-0015 and the cube-grant model of ADR-0016 (fail-closed, no per-object
    /// grants, no open default posture). Element ACLs (below) are retained.
    grants: BTreeMap<(Scope, ObjectKind), AccessList>,
    path: Option<PathBuf>,
    fast_kdf: bool,
    /// A precomputed Argon2 hash of a fixed non-secret string, verified on the
    /// login path when a username does NOT exist so an unknown user costs the same
    /// one Argon2 verify as a known user with a wrong password — closing the
    /// user-enumeration timing channel (ADR-0017). Computed at the store's own KDF
    /// cost so the dummy verify matches the real verify in both test and prod.
    dummy_hash: String,
    /// Strength policy enforced on user-set passwords (create/change/reset).
    /// Secure default in production; permissive in the test seam so fixtures may
    /// use short passwords. Not enforced on bootstrap/`insert_user` (the generated
    /// admin password is strong by construction; an operator override is their own
    /// explicit choice).
    password_policy: PasswordPolicy,
}

/// The fixed input hashed into [`SecurityStore::dummy_hash`]. Not a secret and
/// never a valid password (it is never inserted as a user's hash).
const DUMMY_HASH_INPUT: &str = "epiphany::no-such-user::timing-equalizer";

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
            element_acls: BTreeMap::new(),
            grants: BTreeMap::new(),
            path: Some(path),
            fast_kdf,
            dummy_hash: hash_password(DUMMY_HASH_INPUT, fast_kdf)?,
            password_policy: PasswordPolicy::default(),
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
            element_acls: BTreeMap::new(),
            grants: BTreeMap::new(),
            path: None,
            fast_kdf: true,
            dummy_hash: hash_password(DUMMY_HASH_INPUT, true).expect("dummy hash"),
            // Permissive in the hermetic test seam so fixtures may use short
            // passwords; the real server uses the secure default.
            password_policy: PasswordPolicy::permissive(),
        };
        store
            .insert_user(username, password, is_admin, false, &[])
            .expect("fresh store accepts the first user");
        store
    }

    /// Set the password-strength policy (the composition root applies it from
    /// configuration after opening the store).
    pub fn set_password_policy(&mut self, policy: PasswordPolicy) {
        self.password_policy = policy;
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
        self.check_password(password)?;
        self.insert_user(username, password, is_admin, false, &[])?;
        self.save()
    }

    /// Enforce the password-strength policy on a user-set password (ADR-0017).
    fn check_password(&self, password: &str) -> Result<(), SecurityError> {
        self.password_policy
            .check(password)
            .map_err(SecurityError::WeakPassword)
    }

    /// Verify credentials, returning the principal on success. When the username
    /// does not exist, a verify against [`dummy_hash`](Self::dummy_hash) still
    /// runs so an unknown user costs the same one Argon2 verify as a known user
    /// with a wrong password (no user-enumeration timing channel, ADR-0017).
    pub fn authenticate(&self, username: &str, password: &str) -> Option<Principal> {
        let Some(user) = self.users.get(username) else {
            // Equalize timing: run one verify on the not-found path, then deny.
            let _ = verify_password(password, &self.dummy_hash);
            return None;
        };
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
        self.check_password(new)?;
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
        self.check_password(password)?;
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
        self.check_password(new)?;
        let new_hash = hash_password(new, self.fast_kdf)?;
        let user = self
            .users
            .get_mut(username)
            .ok_or_else(|| SecurityError::UserNotFound(username.to_string()))?;
        user.password_hash = new_hash;
        self.save()
    }

    /// Reset a user to a freshly generated temporary password and require a
    /// change at next sign-in (admin operation), persisting. Returns the one-time
    /// temporary password for the admin to convey out of band; it is never stored
    /// in the clear, logged, or audited. The strength policy is not applied: a
    /// generated password is long and random by construction.
    pub fn reset_password_to_temp(
        &mut self,
        username: &str,
    ) -> Result<GeneratedAdminPassword, SecurityError> {
        // Fail fast on an unknown user before spending an Argon2 hash.
        if !self.users.contains_key(username) {
            return Err(SecurityError::UserNotFound(username.to_string()));
        }
        let temp = generate_password();
        let new_hash = hash_password(&temp, self.fast_kdf)?;
        let user = self.users.get_mut(username).expect("user present");
        user.password_hash = new_hash;
        user.must_change_password = true;
        self.save()?;
        Ok(GeneratedAdminPassword(temp))
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

    // ---- access resolution (ADR-0023) ----

    /// The current principal for a username (admin flag + groups) from the live
    /// store, for per-request re-resolution. `None` if the user no longer exists.
    pub fn principal(&self, username: &str) -> Option<Principal> {
        self.users.get(username).map(|u| Principal {
            username: username.to_string(),
            is_admin: u.is_admin,
            groups: u.groups.iter().cloned().collect(),
        })
    }

    /// Effective access to a cube's contents at the `Cube` kind (ADR-0023):
    /// `Cube:Read` to read, `Cube:Write` to write cell data. A thin convenience
    /// over [`effective`](Self::effective) keyed on a username: admin bypasses to
    /// `Admin`, an unknown user gets `None`, and otherwise it is the max of the
    /// caller's global and per-cube `Cube` grants. Fail-closed: no grant is `None`.
    pub fn cube_access(&self, username: &str, cube: &str) -> AccessLevel {
        match self.principal(username) {
            Some(p) => self.effective(&p, ObjectKind::Cube, Some(cube)),
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

    /// All element grants, for the admin listing surface.
    pub fn element_acls(&self) -> &BTreeMap<(String, String, String), AccessList> {
        &self.element_acls
    }

    // ---- modular per-object-kind grants (ADR-0023) ----

    /// All modular grants, keyed by `(scope, kind)`.
    pub fn grants(&self) -> &BTreeMap<(Scope, ObjectKind), AccessList> {
        &self.grants
    }

    /// Set (or, with `AccessLevel::None`, remove) a subject's grant on a
    /// `(scope, kind)`, then persist.
    pub fn set_grant(
        &mut self,
        subject: &Subject,
        scope: Scope,
        kind: ObjectKind,
        level: AccessLevel,
    ) -> Result<(), SecurityError> {
        let key = (scope, kind);
        let list = self.grants.entry(key.clone()).or_default();
        list.set(subject, level);
        if list.is_empty() {
            self.grants.remove(&key);
        }
        self.save()
    }

    /// The level a principal holds for `(scope, kind)` from the modular grants.
    fn grant_level(&self, principal: &Principal, scope: Scope, kind: ObjectKind) -> AccessLevel {
        self.grants
            .get(&(scope, kind))
            .map(|list| list.level_for(&principal.username, &principal.groups))
            .unwrap_or(AccessLevel::None)
    }

    /// The effective access a principal has to objects of `kind` in `cube`
    /// (ADR-0023): a server admin bypasses to `Admin`; otherwise the max over the
    /// principal's global and per-cube grants for that kind, with `Cube:Admin`
    /// over the cube conferring `Write` on its cube-scoped kinds. Fail-closed: no
    /// grant means `None`.
    pub fn effective(
        &self,
        principal: &Principal,
        kind: ObjectKind,
        cube: Option<&str>,
    ) -> AccessLevel {
        if principal.is_admin {
            return AccessLevel::Admin;
        }
        let mut level = self.grant_level(principal, Scope::Global, kind);
        if let Some(c) = cube {
            level = level.max(self.grant_level(principal, Scope::Cube(c.to_string()), kind));
        }
        if is_cube_scoped(kind) {
            if let Some(c) = cube {
                let cube_admin = self
                    .grant_level(principal, Scope::Global, ObjectKind::Cube)
                    .max(self.grant_level(principal, Scope::Cube(c.to_string()), ObjectKind::Cube));
                if cube_admin >= AccessLevel::Admin {
                    level = level.max(AccessLevel::Write);
                }
            }
        }
        level
    }

    /// Whether a principal may create or delete cubes (ADR-0023): a server admin,
    /// or the holder of a global `Cube:Admin` grant.
    pub fn can_manage_cubes(&self, principal: &Principal) -> bool {
        principal.is_admin
            || self.grant_level(principal, Scope::Global, ObjectKind::Cube) >= AccessLevel::Admin
    }

    /// Serialize to the canonical security model-as-code text (hashes only).
    /// Grants are flattened to one sorted row per (object/element, subject), so
    /// the output is byte-stable.
    pub fn to_model_text(&self) -> String {
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
        // Modular per-kind grants (ADR-0023), in sorted (scope, kind) then sorted
        // subject order so the output is byte-stable.
        let mut grants = Vec::new();
        for ((scope, kind), list) in &self.grants {
            let (scope_tag, cube) = match scope {
                Scope::Global => ("global", None),
                Scope::Cube(c) => ("cube", Some(c.clone())),
            };
            for (user, level) in &list.users {
                grants.push(GrantDoc {
                    subject_kind: "user".to_string(),
                    subject: user.clone(),
                    scope: scope_tag.to_string(),
                    cube: cube.clone(),
                    kind: kind.as_str().to_string(),
                    level: level.as_str().to_string(),
                });
            }
            for (group, level) in &list.groups {
                grants.push(GrantDoc {
                    subject_kind: "group".to_string(),
                    subject: group.clone(),
                    scope: scope_tag.to_string(),
                    cube: cube.clone(),
                    kind: kind.as_str().to_string(),
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
            element_acls,
            grants,
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
        // than failing the load (load is total; dangling/odd rows are simply
        // never consulted).
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
        // Modular per-kind grants (ADR-0023). Tolerant load: a row whose subject,
        // scope, kind, or level does not parse is skipped, never guessed at.
        let mut grants: BTreeMap<(Scope, ObjectKind), AccessList> = BTreeMap::new();
        for row in doc.grants {
            let Some(subject) = subject_from(&row.subject_kind, &row.subject) else {
                continue;
            };
            let Some(kind) = ObjectKind::parse(&row.kind) else {
                continue;
            };
            let Some(level) = AccessLevel::parse(&row.level) else {
                continue;
            };
            let scope = match row.scope.as_str() {
                "global" => Scope::Global,
                "cube" => match row.cube {
                    Some(cube) => Scope::Cube(cube),
                    None => continue,
                },
                _ => continue,
            };
            grants
                .entry((scope, kind))
                .or_default()
                .set(&subject, level);
        }
        Ok(SecurityStore {
            users,
            groups: doc.groups.into_iter().collect(),
            element_acls,
            grants,
            path: None,
            fast_kdf,
            dummy_hash: hash_password(DUMMY_HASH_INPUT, fast_kdf)?,
            password_policy: PasswordPolicy::default(),
        })
    }

    fn save(&self) -> Result<(), SecurityError> {
        if let Some(path) = &self.path {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = path.with_extension("model.tmp");
            // Owner-only from creation: the artifact holds password hashes, so it
            // must never be briefly world-readable (ADR-0017).
            crate::write_owner_only(&tmp, self.to_model_text().as_bytes())?;
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
    // Element ACLs (ADR-0015, retained) are additive and skipped when empty.
    #[serde(default, rename = "element_acl", skip_serializing_if = "Vec::is_empty")]
    element_acls: Vec<ElementAclDoc>,
    // Modular per-object-kind grants (ADR-0023); additive and skipped when empty.
    #[serde(default, rename = "grant", skip_serializing_if = "Vec::is_empty")]
    grants: Vec<GrantDoc>,
}

#[derive(Serialize, Deserialize)]
struct GrantDoc {
    subject_kind: String,
    subject: String,
    /// `global` or `cube`.
    scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cube: Option<String>,
    kind: String,
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

/// Whether a kind lives inside a cube, so a `Cube:Admin` grant over that cube
/// confers `Write` on it (ADR-0023). Cube, Connection, User, Group are not
/// cube-scoped.
fn is_cube_scoped(kind: ObjectKind) -> bool {
    matches!(
        kind,
        ObjectKind::Dimension
            | ObjectKind::Rule
            | ObjectKind::Flow
            | ObjectKind::View
            | ObjectKind::Subset
            | ObjectKind::Job
            | ObjectKind::Sandbox
    )
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
    fn password_policy_gates_create_change_and_reset() {
        // The test seam is permissive; apply the secure default to exercise it.
        let mut store = SecurityStore::with_admin("admin", "pw", true);
        store.set_password_policy(PasswordPolicy::default());

        // Too-short and common passwords are rejected on create.
        assert!(matches!(
            store.create_user("u", "short", false),
            Err(SecurityError::WeakPassword(_))
        ));
        assert!(matches!(
            store.create_user("u", "password1234", false),
            Err(SecurityError::WeakPassword(_))
        ));
        // A strong password is accepted.
        store.create_user("u", "a-strong-pass-1", false).unwrap();

        // change_password and reset_password enforce it too.
        assert!(matches!(
            store.change_password("u", "a-strong-pass-1", "weak"),
            Err(SecurityError::WeakPassword(_))
        ));
        store
            .change_password("u", "a-strong-pass-1", "another-strong-1")
            .unwrap();
        assert!(matches!(
            store.reset_password("u", "x"),
            Err(SecurityError::WeakPassword(_))
        ));
        store.reset_password("u", "reset-strong-pass-1").unwrap();
        assert!(store.authenticate("u", "reset-strong-pass-1").is_some());
    }

    #[test]
    fn reset_password_to_temp_generates_and_forces_change() {
        let mut store = SecurityStore::with_admin("admin", "pw", true);
        store
            .create_user_with_groups("u", "a-strong-pass-1", false, &[])
            .unwrap();
        assert!(!store.must_change_password("u"));

        let temp = store.reset_password_to_temp("u").unwrap().0;
        // The old password no longer works; the generated temp does.
        assert!(store.authenticate("u", "a-strong-pass-1").is_none());
        assert!(store.authenticate("u", &temp).is_some());
        // The user must now change it at next sign-in.
        assert!(store.must_change_password("u"));
        // Changing it with the temp clears the requirement.
        store
            .change_password("u", &temp, "brand-new-strong-1")
            .unwrap();
        assert!(!store.must_change_password("u"));
        // An unknown user is rejected (and spends no hash before the check).
        assert!(matches!(
            store.reset_password_to_temp("ghost"),
            Err(SecurityError::UserNotFound(_))
        ));
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
    fn cube_access_is_closed_by_default_and_governed_by_grants() {
        let mut store = SecurityStore::with_admin("admin", "pw", true);
        store.create_user("ann", "pw", false).unwrap();
        store.create_user("bob", "pw", false).unwrap();
        // Secure default (fail-closed, ADR-0023): an ungranted cube denies a
        // non-admin; the admin still bypasses.
        assert_eq!(store.cube_access("ann", "Sales"), AccessLevel::None);
        assert_eq!(store.cube_access("admin", "Sales"), AccessLevel::Admin);
        // A per-cube Cube:Read grant governs the cube exactly.
        store
            .set_grant(
                &Subject::User("ann".into()),
                Scope::Cube("Sales".into()),
                ObjectKind::Cube,
                AccessLevel::Read,
            )
            .unwrap();
        assert_eq!(store.cube_access("ann", "Sales"), AccessLevel::Read);
        assert_eq!(store.cube_access("bob", "Sales"), AccessLevel::None);
        assert_eq!(store.cube_access("admin", "Sales"), AccessLevel::Admin);
        // A global Cube:Write grant applies to every cube.
        store
            .set_grant(
                &Subject::User("bob".into()),
                Scope::Global,
                ObjectKind::Cube,
                AccessLevel::Write,
            )
            .unwrap();
        assert_eq!(store.cube_access("bob", "Sales"), AccessLevel::Write);
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
        store.create_user("ann", "pw", false).unwrap();
        store
            .set_grant(
                &Subject::User("ann".into()),
                Scope::Cube("Sales".into()),
                ObjectKind::Cube,
                AccessLevel::Write,
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
        assert_eq!(store2.cube_access("ann", "Sales"), AccessLevel::Write);
        let ann = principal("ann", false, &[]);
        let mgr = principal("m", false, &["managers"]);
        assert!(store2.element_readable(&mgr, "Sales", "Region", "North"));
        assert!(!store2.element_readable(&ann, "Sales", "Region", "North"));
    }

    #[test]
    fn file_without_acls_loads() {
        // An artifact with users only (no acl/grant sections) loads under the
        // format tag, with empty grant maps and a fail-closed posture.
        let text = format!(
            "format = \"{FORMAT_TAG}\"\n\n[[user]]\nusername = \"admin\"\nis_admin = true\nmust_change_password = false\npassword_hash = \"x\"\n"
        );
        let store = SecurityStore::from_model_text(&text, true).unwrap();
        assert_eq!(store.user_count(), 1);
        assert!(store.element_acls().is_empty());
        assert!(store.grants().is_empty());
    }

    // ---- modular per-object-kind grants (ADR-0023) ----

    #[test]
    fn modular_grants_resolve_per_kind() {
        let mut store = SecurityStore::with_admin("root", "pw", true);
        let entry = principal("entry", false, &[]);
        let flow = principal("fa", false, &["flow_authors"]);
        let modeler = principal("mod", false, &[]);
        let cadmin = principal("ca", false, &[]);
        let root = principal("root", true, &[]);

        // Data-entry user: write cells on Sales, nothing structural.
        store
            .set_grant(
                &Subject::User("entry".into()),
                Scope::Cube("Sales".into()),
                ObjectKind::Cube,
                AccessLevel::Write,
            )
            .unwrap();
        // Flow author role: a group with Flow:Write everywhere.
        store
            .set_grant(
                &Subject::Group("flow_authors".into()),
                Scope::Global,
                ObjectKind::Flow,
                AccessLevel::Write,
            )
            .unwrap();
        // Modeler: Dimension + Rule Write on Sales only.
        store
            .set_grant(
                &Subject::User("mod".into()),
                Scope::Cube("Sales".into()),
                ObjectKind::Dimension,
                AccessLevel::Write,
            )
            .unwrap();
        store
            .set_grant(
                &Subject::User("mod".into()),
                Scope::Cube("Sales".into()),
                ObjectKind::Rule,
                AccessLevel::Write,
            )
            .unwrap();
        // Cube admin of Sales.
        store
            .set_grant(
                &Subject::User("ca".into()),
                Scope::Cube("Sales".into()),
                ObjectKind::Cube,
                AccessLevel::Admin,
            )
            .unwrap();

        // Data-entry: cube Write (cells) but no model editing.
        assert_eq!(
            store.effective(&entry, ObjectKind::Cube, Some("Sales")),
            AccessLevel::Write
        );
        assert_eq!(
            store.effective(&entry, ObjectKind::Flow, Some("Sales")),
            AccessLevel::None
        );
        assert_eq!(
            store.effective(&entry, ObjectKind::Dimension, Some("Sales")),
            AccessLevel::None
        );

        // Flow author: Flow:Write on any cube; nothing else, no cube data write.
        assert_eq!(
            store.effective(&flow, ObjectKind::Flow, Some("Sales")),
            AccessLevel::Write
        );
        assert_eq!(
            store.effective(&flow, ObjectKind::Flow, Some("Budget")),
            AccessLevel::Write
        );
        assert_eq!(
            store.effective(&flow, ObjectKind::Cube, Some("Sales")),
            AccessLevel::None
        );
        assert_eq!(
            store.effective(&flow, ObjectKind::Dimension, Some("Sales")),
            AccessLevel::None
        );

        // Modeler: Dimension/Rule Write on Sales only.
        assert_eq!(
            store.effective(&modeler, ObjectKind::Dimension, Some("Sales")),
            AccessLevel::Write
        );
        assert_eq!(
            store.effective(&modeler, ObjectKind::Rule, Some("Sales")),
            AccessLevel::Write
        );
        assert_eq!(
            store.effective(&modeler, ObjectKind::Flow, Some("Sales")),
            AccessLevel::None
        );
        assert_eq!(
            store.effective(&modeler, ObjectKind::Dimension, Some("Budget")),
            AccessLevel::None
        );

        // Cube admin of Sales: Admin on the cube, Write on its cube-scoped kinds,
        // but only on Sales, and cannot create cubes (needs global Cube:Admin).
        assert_eq!(
            store.effective(&cadmin, ObjectKind::Cube, Some("Sales")),
            AccessLevel::Admin
        );
        assert_eq!(
            store.effective(&cadmin, ObjectKind::Flow, Some("Sales")),
            AccessLevel::Write
        );
        assert_eq!(
            store.effective(&cadmin, ObjectKind::Dimension, Some("Sales")),
            AccessLevel::Write
        );
        assert_eq!(
            store.effective(&cadmin, ObjectKind::Flow, Some("Budget")),
            AccessLevel::None
        );
        assert!(!store.can_manage_cubes(&cadmin));

        // Server admin bypasses everything.
        assert_eq!(
            store.effective(&root, ObjectKind::Dimension, Some("Anything")),
            AccessLevel::Admin
        );
        assert!(store.can_manage_cubes(&root));
    }

    #[test]
    fn global_cube_admin_manages_cubes_and_their_contents() {
        let mut store = SecurityStore::with_admin("root", "pw", true);
        let mgr = principal("mgr", false, &["cube_mgrs"]);
        store
            .set_grant(
                &Subject::Group("cube_mgrs".into()),
                Scope::Global,
                ObjectKind::Cube,
                AccessLevel::Admin,
            )
            .unwrap();
        assert!(store.can_manage_cubes(&mgr));
        // global Cube:Admin also confers Write on cube-scoped kinds in any cube.
        assert_eq!(
            store.effective(&mgr, ObjectKind::Flow, Some("Whatever")),
            AccessLevel::Write
        );
    }

    #[test]
    fn grants_round_trip_byte_identical() {
        let mut store = SecurityStore::with_admin("root", "pw", true);
        store
            .set_grant(
                &Subject::Group("flow_authors".into()),
                Scope::Global,
                ObjectKind::Flow,
                AccessLevel::Write,
            )
            .unwrap();
        store
            .set_grant(
                &Subject::User("mod".into()),
                Scope::Cube("Sales".into()),
                ObjectKind::Dimension,
                AccessLevel::Write,
            )
            .unwrap();
        let text = store.to_model_text();
        let reloaded = SecurityStore::from_model_text(&text, true).unwrap();
        assert_eq!(reloaded.to_model_text(), text);
        assert_eq!(
            reloaded.effective(
                &principal("mod", false, &[]),
                ObjectKind::Dimension,
                Some("Sales")
            ),
            AccessLevel::Write
        );
    }
}
