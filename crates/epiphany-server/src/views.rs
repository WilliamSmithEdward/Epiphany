//! Views a user keeps for themself: queries saved against a cube that nobody else sees,
//! beside the model's views, which everybody does.
//!
//! They belong to the users, so they are kept with them, in `views.json` in the server's
//! directory, and go when their user does. Each user's take at most [`MOST_BYTES`], so that
//! nobody who may only read can fill the disk.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use epiphany_core::model::{ViewDef, key};

use crate::storage::{read_json, write_json};

const FILE: &str = "views.json";
/// The longest a view's name may be, in characters.
const LONGEST: usize = 120;
/// The most room, written out, that one user's views may take.
pub const MOST_BYTES: usize = 4 * 1024 * 1024;

/// Each user's views: by user, in lower case; by cube, as the model named it when the view
/// was saved; and by the view's own name.
type Kept = BTreeMap<String, BTreeMap<String, BTreeMap<String, ViewDef>>>;

#[derive(Debug)]
pub struct PrivateViews {
    path: PathBuf,
    kept: Mutex<Kept>,
}

/// Why a view was not kept or taken away.
#[derive(Debug)]
pub enum Refused {
    /// The name, or the room the views would take, is not allowed; the text says why.
    Invalid(String),
    /// There is no such view.
    Missing(String),
    /// The views could not be written to disk.
    Unkept(io::Error),
}

impl fmt::Display for Refused {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) | Self::Missing(message) => formatter.write_str(message),
            Self::Unkept(error) => write!(formatter, "the views could not be kept: {error}"),
        }
    }
}

impl PrivateViews {
    /// The views kept in a server's directory, or none yet.
    pub fn load(directory: &Path) -> Result<Self, Box<dyn Error>> {
        let path = directory.join(FILE);
        let kept = read_json(&path)?.unwrap_or_default();
        Ok(Self {
            path,
            kept: Mutex::new(kept),
        })
    }

    fn kept(&self) -> MutexGuard<'_, Kept> {
        self.kept.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A user's views of a cube, by name.
    pub fn of(&self, user: &str, cube: &str) -> BTreeMap<String, ViewDef> {
        let kept = self.kept();
        let theirs = kept.get(&user.to_lowercase());
        theirs
            .and_then(|cubes| cubes.iter().find(|(name, _)| key(name) == key(cube)))
            .map(|(_, views)| views.clone())
            .unwrap_or_default()
    }

    /// Keeps a view of a cube for a user, in place of any of theirs of the same name there,
    /// case and spaces aside. Written to disk before it is kept, or not kept at all.
    pub fn keep(&self, user: &str, cube: &str, name: &str, view: ViewDef) -> Result<(), Refused> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > LONGEST || name.chars().any(char::is_control) {
            return Err(Refused::Invalid(format!(
                "a view's name is 1 to {LONGEST} characters, none of them control characters"
            )));
        }
        self.change(user, |theirs| {
            // Under the name the cube's other views are kept by, or else as given.
            let named = theirs.keys().find(|other| key(other) == key(cube)).cloned();
            let views = theirs
                .entry(named.unwrap_or_else(|| cube.to_owned()))
                .or_default();
            views.retain(|other, _| key(other) != key(name));
            views.insert(name.to_owned(), view);
            let room = serde_json::to_vec(theirs).map_or(usize::MAX, |written| written.len());
            match room > MOST_BYTES {
                true => Err(Refused::Invalid(format!(
                    "your views would take more than {MOST_BYTES} bytes; take some away first"
                ))),
                false => Ok(()),
            }
        })
    }

    /// Takes one of a user's views of a cube away.
    pub fn forget(&self, user: &str, cube: &str, name: &str) -> Result<(), Refused> {
        self.change(user, |theirs| {
            let views = theirs
                .iter_mut()
                .find(|(other, _)| key(other) == key(cube))
                .map(|(_, views)| views);
            let missing =
                || Refused::Missing(format!("you have no view of {cube} called '{name}'"));
            let views = views.ok_or_else(missing)?;
            let before = views.len();
            views.retain(|other, _| key(other) != key(name));
            match views.len() < before {
                true => Ok(()),
                false => Err(missing()),
            }
        })
    }

    /// Takes every view of a user away, as the user goes.
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

    /// Changes a user's views as `change` says, then writes them all to disk; kept only once
    /// written, and not at all if `change` refuses.
    fn change(
        &self,
        user: &str,
        change: impl FnOnce(&mut BTreeMap<String, BTreeMap<String, ViewDef>>) -> Result<(), Refused>,
    ) -> Result<(), Refused> {
        let mut kept = self.kept();
        let mut changed = kept.clone();
        let theirs = changed.entry(user.to_lowercase()).or_default();
        change(theirs)?;
        // A cube without views, and a user without any, leave no trace.
        theirs.retain(|_, views| !views.is_empty());
        if theirs.is_empty() {
            changed.remove(&user.to_lowercase());
        }
        write_json(&self.path, &changed).map_err(Refused::Unkept)?;
        *kept = changed;
        Ok(())
    }
}
