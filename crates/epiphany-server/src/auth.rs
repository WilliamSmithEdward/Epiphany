//! Users, their passwords, what each may do, and the sessions they sign in to.
//!
//! Users are kept in `users.json` in the server's directory, each with a salted Argon2 hash of
//! the password; the password itself is never stored. Signing in gives a random token, which
//! the client sends with every request. A session ends when it is signed out of or has not
//! been used for a while. Sessions live in memory, so they end when the server stops.
//!
//! A name that has been given the wrong password several times running is refused for a
//! while whatever password comes with it, so that passwords cannot be tried by the thousand.

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fmt;
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::storage::{read_json, write_json};

const FILE: &str = "users.json";
/// How long a session lasts without being used, unless set otherwise.
const IDLE: Duration = Duration::from_secs(30 * 60);
/// How many wrong passwords running a name may be given before it is refused for a while.
const ATTEMPTS: u32 = 5;
/// How long such a name is refused, counted from the last wrong password.
const PAUSE: Duration = Duration::from_secs(60);

/// What a user may do. Each role may also do what the ones before it may.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Nothing.
    None,
    /// Read cells, the model and its dimensions, and run queries.
    Read,
    /// Also write cells, spread values and use transactions and locks.
    Write,
    /// Also change the model.
    #[default]
    Admin,
}

impl FromStr for Role {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        match text.to_lowercase().as_str() {
            "none" => Ok(Self::None),
            "read" => Ok(Self::Read),
            "write" => Ok(Self::Write),
            "admin" => Ok(Self::Admin),
            _ => Err(format!(
                "'{text}' is not a role; use none, read, write or admin"
            )),
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::Read => "read",
            Self::Write => "write",
            Self::Admin => "admin",
        })
    }
}

/// What a signed-in user may do: a role, and for some cubes another role in its place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grants {
    role: Role,
    /// The cubes with a role of their own, by name as given.
    cubes: BTreeMap<String, Role>,
    /// The same, by name in lower case without spaces.
    found: HashMap<String, Role>,
}

impl Grants {
    pub fn new(role: Role, cubes: BTreeMap<String, Role>) -> Self {
        let found = cubes
            .iter()
            .map(|(name, &role)| (key(name), role))
            .collect();
        Self { role, cubes, found }
    }

    /// The role that holds wherever a cube has none of its own. Changing the model takes
    /// this one to be admin, whatever any cube says.
    pub fn role(&self) -> Role {
        self.role
    }

    pub fn cubes(&self) -> &BTreeMap<String, Role> {
        &self.cubes
    }

    /// The role on one cube.
    pub fn on(&self, cube: &str) -> Role {
        self.found.get(&key(cube)).copied().unwrap_or(self.role)
    }

    /// The highest role held anywhere.
    pub fn most(&self) -> Role {
        self.cubes.values().copied().fold(self.role, Role::max)
    }
}

/// Cube names are compared as the engine compares them: ignoring case and spaces.
fn key(name: &str) -> String {
    name.chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

#[derive(Serialize, Deserialize)]
struct User {
    name: String,
    /// The Argon2 hash of the password, with its salt and settings.
    password: String,
    /// A user recorded before there were roles may do everything, as then.
    #[serde(default)]
    role: Role,
    /// Cubes on which the user has another role in place of that one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    cubes: BTreeMap<String, Role>,
}

/// Why a sign-in was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The name or the password is wrong.
    Wrong,
    /// The name has been given too many wrong passwords and is refused for this long yet.
    Paused(Duration),
}

#[derive(Debug)]
struct Session {
    grants: Arc<Grants>,
    used: Instant,
}

#[derive(Debug)]
pub struct Auth {
    /// Password hash and grants by user name in lower case.
    users: HashMap<String, (String, Arc<Grants>)>,
    sessions: Mutex<HashMap<String, Session>>,
    /// Wrong passwords running and when the last was given, by name in lower case.
    failures: Mutex<HashMap<String, (u32, Instant)>>,
    idle: Duration,
    pause: Duration,
}

impl Auth {
    /// The users of a directory, or nothing if it has none, in which case the server asks
    /// for no sign-in.
    pub fn load(directory: &Path) -> Result<Option<Self>, Box<dyn Error>> {
        let users: Vec<User> = read_json(&directory.join(FILE))?.unwrap_or_default();
        let users: HashMap<String, (String, Arc<Grants>)> = users
            .into_iter()
            .map(|user| {
                let grants = Arc::new(Grants::new(user.role, user.cubes));
                (user.name.to_lowercase(), (user.password, grants))
            })
            .collect();
        Ok((!users.is_empty()).then(|| Self {
            users,
            sessions: Mutex::default(),
            failures: Mutex::default(),
            idle: IDLE,
            pause: PAUSE,
        }))
    }

    /// Sets how long a session lasts without being used.
    pub fn set_idle(&mut self, idle: Duration) {
        self.idle = idle;
    }

    pub fn idle(&self) -> Duration {
        self.idle
    }

    /// Sets how long a name is refused after too many wrong passwords.
    pub fn set_pause(&mut self, pause: Duration) {
        self.pause = pause;
    }

    /// Starts a session for a user who gives the right password, and returns its token and
    /// what the user may do.
    pub fn sign_in(&self, name: &str, password: &str) -> Result<(String, Arc<Grants>), Refusal> {
        let name = name.to_lowercase();
        let now = Instant::now();
        {
            // Names that are not users are counted like any other, so that a refusal does
            // not tell which names exist. Counts older than the pause are forgotten, which
            // also keeps the list short.
            let mut failures = self.failures.lock().unwrap();
            failures.retain(|_, (_, last)| now.duration_since(*last) < self.pause);
            if let Some((ATTEMPTS.., last)) = failures.get(&name) {
                return Err(Refusal::Paused(self.pause - now.duration_since(*last)));
            }
        }
        // An unknown name is checked against a hash too, so that it takes as long to refuse
        // as a wrong password and the two cannot be told apart by timing.
        static NOBODY: OnceLock<String> = OnceLock::new();
        let known = self.users.get(&name);
        let hash = match known {
            Some((hash, _)) => hash,
            None => NOBODY.get_or_init(|| hash("nobody").expect("hashing works")),
        };
        let right = PasswordHash::new(hash).is_ok_and(|hash| {
            Argon2::default()
                .verify_password(password.as_bytes(), &hash)
                .is_ok()
        });
        let grants = match known {
            Some((_, grants)) if right => Arc::clone(grants),
            _ => {
                let mut failures = self.failures.lock().unwrap();
                let (count, last) = failures.entry(name).or_insert((0, now));
                (*count, *last) = (*count + 1, now);
                return Err(Refusal::Wrong);
            }
        };
        self.failures.lock().unwrap().remove(&name);
        let mut random = [0u8; 32];
        getrandom::fill(&mut random).expect("the operating system provides randomness");
        let token: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|_, session| now.duration_since(session.used) <= self.idle);
        let session = Session {
            grants: Arc::clone(&grants),
            used: now,
        };
        sessions.insert(token.clone(), session);
        Ok((token, grants))
    }

    /// What the user of a live session may do; the session then counts as used just now.
    /// Nothing if the token belongs to no live session.
    pub fn admits(&self, token: &str) -> Option<Arc<Grants>> {
        let mut sessions = self.sessions.lock().unwrap();
        let now = Instant::now();
        match sessions.get_mut(token) {
            Some(session) if now.duration_since(session.used) <= self.idle => {
                session.used = now;
                Some(Arc::clone(&session.grants))
            }
            _ => {
                sessions.remove(token);
                None
            }
        }
    }

    pub fn sign_out(&self, token: &str) {
        self.sessions.lock().unwrap().remove(token);
    }
}

fn hash(password: &str) -> Result<String, Box<dyn Error>> {
    let hash = Argon2::default().hash_password(password.as_bytes());
    Ok(hash.map_err(|error| error.to_string())?.to_string())
}

/// Adds a user to a directory, or changes the password and grants of one it already has.
pub fn set_user(
    directory: &Path,
    name: &str,
    password: &str,
    grants: &Grants,
) -> Result<(), Box<dyn Error>> {
    if name.trim().is_empty() || password.is_empty() {
        return Err("a user needs a name and a password".into());
    }
    let path = directory.join(FILE);
    let mut users: Vec<User> = read_json(&path)?.unwrap_or_default();
    users.retain(|user| !user.name.eq_ignore_ascii_case(name.trim()));
    users.push(User {
        name: name.trim().to_owned(),
        password: hash(password)?,
        role: grants.role(),
        cubes: grants.cubes().clone(),
    });
    Ok(write_json(&path, &users)?)
}
