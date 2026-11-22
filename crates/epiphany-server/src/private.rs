//! What each user keeps for themself, which nobody else sees: views of cubes, beside the
//! model's views, and subsets of dimensions, beside the model's subsets.
//!
//! They belong to the users, not the model, so they are kept with them, outside the model's
//! versions, each kind in a file of its own in the server's directory, and they go when their
//! user does. Each user's views, and each user's subsets, take at most [`MOST_BYTES`], so
//! that nobody who may only read can fill the disk.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use epiphany_core::model::key;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::storage::{read_json, write_json};

/// The longest the name of something kept may be, in characters.
const LONGEST: usize = 120;
/// The most room, written out, that one user's things of one kind may take.
pub const MOST_BYTES: usize = 4 * 1024 * 1024;

/// A kind of thing users keep: its file, its name in messages, and the codes of refusals.
pub struct Kind {
    file: &'static str,
    what: &'static str,
    invalid: &'static str,
    unknown: &'static str,
}

/// Views, kept by cube.
pub const VIEWS: Kind = Kind {
    file: "views.json",
    what: "view",
    invalid: "invalid_view",
    unknown: "unknown_view",
};

/// Subsets, kept by dimension: lists of members.
pub const SUBSETS: Kind = Kind {
    file: "subsets.json",
    what: "subset",
    invalid: "invalid_subset",
    unknown: "unknown_subset",
};

/// One user's things: by what they belong to, a cube or a dimension, as the model named it
/// when the first was kept; then by their own names.
type Theirs<T> = BTreeMap<String, BTreeMap<String, T>>;

/// The things of one kind each user keeps, by user in lower case.
pub struct Private<T> {
    kind: Kind,
    path: PathBuf,
    kept: Mutex<BTreeMap<String, Theirs<T>>>,
}

impl<T> fmt::Debug for Private<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Private({})", self.kind.file)
    }
}

/// Why something was not kept or taken away.
#[derive(Debug)]
pub enum Refused {
    /// The name, or the room the things would take, is not allowed: a code, and why.
    Invalid(&'static str, String),
    /// There is no such thing: a code, and what was asked for.
    Missing(&'static str, String),
    /// They could not be written to disk.
    Unkept(io::Error),
}

impl fmt::Display for Refused {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(_, message) | Self::Missing(_, message) => formatter.write_str(message),
            Self::Unkept(error) => write!(formatter, "it could not be kept: {error}"),
        }
    }
}

impl<T: Clone + Serialize + DeserializeOwned> Private<T> {
    /// The things of a kind kept in a server's directory, or none yet.
    pub fn load(directory: &Path, kind: Kind) -> Result<Self, Box<dyn Error>> {
        let path = directory.join(kind.file);
        let kept = read_json(&path)?.unwrap_or_default();
        Ok(Self {
            kind,
            path,
            kept: Mutex::new(kept),
        })
    }

    fn kept(&self) -> MutexGuard<'_, BTreeMap<String, Theirs<T>>> {
        self.kept.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A user's things of a cube or a dimension, by name.
    pub fn of(&self, user: &str, under: &str) -> BTreeMap<String, T> {
        self.under(user, under, BTreeMap::clone).unwrap_or_default()
    }

    /// One of a user's things of a cube or a dimension, by its name, case and spaces aside.
    pub fn one(&self, user: &str, under: &str, name: &str) -> Option<T> {
        self.under(user, under, |things| {
            let found = things.iter().find(|(other, _)| key(other) == key(name));
            found.map(|(_, thing)| thing.clone())
        })?
    }

    /// What `take` makes of a user's things of a cube or a dimension, where they keep any.
    fn under<R>(
        &self,
        user: &str,
        under: &str,
        take: impl FnOnce(&BTreeMap<String, T>) -> R,
    ) -> Option<R> {
        let kept = self.kept();
        let theirs = kept.get(&user.to_lowercase())?;
        let (_, things) = theirs.iter().find(|(name, _)| key(name) == key(under))?;
        Some(take(things))
    }

    /// Keeps a thing of a cube or a dimension for a user, in place of any of theirs of the
    /// same name there, case and spaces aside. Written to disk before it is kept, or not kept
    /// at all.
    pub fn keep(&self, user: &str, under: &str, name: &str, thing: T) -> Result<(), Refused> {
        let Kind { what, invalid, .. } = self.kind;
        let name = name.trim();
        if name.is_empty() || name.chars().count() > LONGEST || name.chars().any(char::is_control) {
            let why = format!(
                "a {what}'s name is 1 to {LONGEST} characters, none of them control characters"
            );
            return Err(Refused::Invalid(invalid, why));
        }
        self.change(user, |theirs| {
            // Under the name the others there are kept by, or else as given.
            let named = theirs
                .keys()
                .find(|other| key(other) == key(under))
                .cloned();
            let things = theirs
                .entry(named.unwrap_or_else(|| under.to_owned()))
                .or_default();
            things.retain(|other, _| key(other) != key(name));
            things.insert(name.to_owned(), thing);
            let room = serde_json::to_vec(theirs).map_or(usize::MAX, |written| written.len());
            match room > MOST_BYTES {
                true => {
                    let why = format!(
                        "your {what}s would take more than {MOST_BYTES} bytes; take some away first"
                    );
                    Err(Refused::Invalid(invalid, why))
                }
                false => Ok(()),
            }
        })
    }

    /// Takes one of a user's things of a cube or a dimension away.
    pub fn forget(&self, user: &str, under: &str, name: &str) -> Result<(), Refused> {
        let Kind { what, unknown, .. } = self.kind;
        self.change(user, |theirs| {
            let things = theirs
                .iter_mut()
                .find(|(other, _)| key(other) == key(under));
            let missing = || {
                Refused::Missing(
                    unknown,
                    format!("you have no {what} of {under} called '{name}'"),
                )
            };
            let things = things.map(|(_, things)| things).ok_or_else(missing)?;
            let before = things.len();
            things.retain(|other, _| key(other) != key(name));
            match things.len() < before {
                true => Ok(()),
                false => Err(missing()),
            }
        })
    }

    /// Takes everything of a user's away, as the user goes.
    pub fn forget_user(&self, user: &str) -> io::Result<()> {
        let mut kept = self.kept();
        if kept.contains_key(&user.to_lowercase()) {
            let mut changed = kept.clone();
            changed.remove(&user.to_lowercase());
            write_json(&self.path, &changed)?;
            *kept = changed;
        }
        Ok(())
    }

    /// Changes a user's things as `change` says, then writes them all to disk; kept only once
    /// written, and not at all if `change` refuses.
    fn change(
        &self,
        user: &str,
        change: impl FnOnce(&mut Theirs<T>) -> Result<(), Refused>,
    ) -> Result<(), Refused> {
        let mut kept = self.kept();
        let mut changed = kept.clone();
        let theirs = changed.entry(user.to_lowercase()).or_default();
        change(theirs)?;
        // What holds nothing, and a user who keeps nothing, leave no trace.
        theirs.retain(|_, things| !things.is_empty());
        if theirs.is_empty() {
            changed.remove(&user.to_lowercase());
        }
        write_json(&self.path, &changed).map_err(Refused::Unkept)?;
        *kept = changed;
        Ok(())
    }
}
