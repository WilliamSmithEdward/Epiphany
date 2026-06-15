//! Access-control primitives: the access lattice, securable object kinds and
//! references, subjects (user or group), grant scope (ADR-0023), and the
//! per-subject grant list. Element-level access (ADR-0015) reuses the same
//! `AccessList`. Resolution is most-permissive-wins; admin bypass is composed at
//! the API boundary (this layer knows only grants).

use std::collections::BTreeMap;

/// The access lattice, totally ordered so effective access is a `max()` and a
/// requirement is `>= need`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum AccessLevel {
    /// No access.
    #[default]
    None,
    /// See and read.
    Read,
    /// Create, replace, delete, and run mutating operations.
    Write,
    /// Delete the object and manage its grants.
    Admin,
}

impl AccessLevel {
    /// The canonical lowercase token (for model-as-code and the REST surface).
    pub fn as_str(self) -> &'static str {
        match self {
            AccessLevel::None => "none",
            AccessLevel::Read => "read",
            AccessLevel::Write => "write",
            AccessLevel::Admin => "admin",
        }
    }

    /// Parse a token; `None` for an unrecognized string.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(AccessLevel::None),
            "read" => Some(AccessLevel::Read),
            "write" => Some(AccessLevel::Write),
            "admin" => Some(AccessLevel::Admin),
            _ => None,
        }
    }
}

/// The kind of a securable object (ADR-0015). `Job` is reserved for the Phase 8
/// scheduler (serialized now, enforced later).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ObjectKind {
    Cube,
    Dimension,
    Rule,
    Flow,
    View,
    Subset,
    Connection,
    Sandbox,
    Job,
    Group,
    User,
}

impl ObjectKind {
    /// The canonical lowercase token.
    pub fn as_str(self) -> &'static str {
        match self {
            ObjectKind::Cube => "cube",
            ObjectKind::Dimension => "dimension",
            ObjectKind::Rule => "rule",
            ObjectKind::Flow => "flow",
            ObjectKind::View => "view",
            ObjectKind::Subset => "subset",
            ObjectKind::Connection => "connection",
            ObjectKind::Sandbox => "sandbox",
            ObjectKind::Job => "job",
            ObjectKind::Group => "group",
            ObjectKind::User => "user",
        }
    }

    /// Parse a token; `None` for an unrecognized string.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "cube" => ObjectKind::Cube,
            "dimension" => ObjectKind::Dimension,
            "rule" => ObjectKind::Rule,
            "flow" => ObjectKind::Flow,
            "view" => ObjectKind::View,
            "subset" => ObjectKind::Subset,
            "connection" => ObjectKind::Connection,
            "sandbox" => ObjectKind::Sandbox,
            "job" => ObjectKind::Job,
            "group" => ObjectKind::Group,
            "user" => ObjectKind::User,
            _ => return None,
        })
    }
}

/// A reference to a securable object. Cube-scoped kinds carry their `cube`;
/// global kinds (a cube itself, a connection, a user, a group) leave it `None`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectRef {
    pub kind: ObjectKind,
    pub cube: Option<String>,
    pub name: String,
}

impl ObjectRef {
    /// A cube object (kind = Cube, name = the cube).
    pub fn cube(name: impl Into<String>) -> Self {
        Self {
            kind: ObjectKind::Cube,
            cube: None,
            name: name.into(),
        }
    }

    /// A cube-scoped object (rule, flow, view, subset, dimension, sandbox).
    pub fn in_cube(kind: ObjectKind, cube: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            kind,
            cube: Some(cube.into()),
            name: name.into(),
        }
    }

    /// A global object (connection, user, group).
    pub fn global(kind: ObjectKind, name: impl Into<String>) -> Self {
        Self {
            kind,
            cube: None,
            name: name.into(),
        }
    }
}

/// A grant subject: a single user or a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    User(String),
    Group(String),
}

/// The scope of a per-object-kind permission grant (ADR-0023): all cubes, or one
/// named cube. Ordered so it can key a `BTreeMap` deterministically.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    /// Every cube (and cube-creation, for the `Cube` kind at this scope).
    Global,
    /// One specific cube.
    Cube(String),
}

impl Scope {
    /// The cube this scope names, if specific.
    pub fn cube(&self) -> Option<&str> {
        match self {
            Scope::Global => None,
            Scope::Cube(name) => Some(name),
        }
    }
}

/// The grants on one object (or element): per-user and per-group access levels.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccessList {
    pub users: BTreeMap<String, AccessLevel>,
    pub groups: BTreeMap<String, AccessLevel>,
}

impl AccessList {
    /// The level a principal gets from these grants: the max over a direct user
    /// grant and any matching group grant. `None` if nothing matches.
    pub fn level_for(&self, username: &str, groups: &[String]) -> AccessLevel {
        let mut level = self
            .users
            .get(username)
            .copied()
            .unwrap_or(AccessLevel::None);
        for g in groups {
            if let Some(&gl) = self.groups.get(g) {
                level = level.max(gl);
            }
        }
        level
    }

    /// Set (or, with `AccessLevel::None`, remove) a subject's grant.
    pub fn set(&mut self, subject: &Subject, level: AccessLevel) {
        let (map, key) = match subject {
            Subject::User(u) => (&mut self.users, u),
            Subject::Group(g) => (&mut self.groups, g),
        };
        if level == AccessLevel::None {
            map.remove(key);
        } else {
            map.insert(key.clone(), level);
        }
    }

    /// Whether there are no grants.
    pub fn is_empty(&self) -> bool {
        self.users.is_empty() && self.groups.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lattice_orders_and_round_trips() {
        assert!(AccessLevel::None < AccessLevel::Read);
        assert!(AccessLevel::Read < AccessLevel::Write);
        assert!(AccessLevel::Write < AccessLevel::Admin);
        for l in [
            AccessLevel::None,
            AccessLevel::Read,
            AccessLevel::Write,
            AccessLevel::Admin,
        ] {
            assert_eq!(AccessLevel::parse(l.as_str()), Some(l));
        }
        assert_eq!(AccessLevel::parse("bogus"), None);
    }

    #[test]
    fn access_list_is_most_permissive_over_user_and_groups() {
        let mut list = AccessList::default();
        list.set(&Subject::User("ann".into()), AccessLevel::Read);
        list.set(&Subject::Group("editors".into()), AccessLevel::Write);
        // The group grant (Write) beats the direct user grant (Read).
        assert_eq!(
            list.level_for("ann", &["editors".to_string()]),
            AccessLevel::Write
        );
        // Without the group, only the user grant applies.
        assert_eq!(list.level_for("ann", &[]), AccessLevel::Read);
        // An unknown principal gets nothing.
        assert_eq!(list.level_for("bob", &[]), AccessLevel::None);
        // Revoking removes the grant.
        list.set(&Subject::User("ann".into()), AccessLevel::None);
        assert_eq!(list.level_for("ann", &[]), AccessLevel::None);
    }
}
