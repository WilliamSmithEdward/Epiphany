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
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock, RwLockReadGuard};
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use utoipa::ToSchema;

use crate::storage::{read_json, write_json};

const FILE: &str = "users.json";
/// How long a session lasts without being used, unless set otherwise.
const IDLE: Duration = Duration::from_secs(30 * 60);
/// How many wrong passwords running a name may be given before it is refused for a while.
const ATTEMPTS: u32 = 5;
/// How long such a name is refused, counted from the last wrong password.
const PAUSE: Duration = Duration::from_secs(60);
/// How many passwords are checked at once. Each check takes some 19 MiB and a processor.
const CHECKED_AT_ONCE: usize = 4;
/// How many sign-ins may wait for their turn; past that they are told to come back.
const WAITING: usize = 64;

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

/// A user as the server holds them while it runs.
#[derive(Debug, Clone)]
struct Account {
    /// The name as it was given.
    name: String,
    hash: String,
    grants: Arc<Grants>,
}

/// A user as an administrator sees them: never the password.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UserView {
    pub name: String,
    /// What the user may do.
    pub role: Role,
    /// The cubes on which the user has another role in place of that one.
    pub cubes: BTreeMap<String, Role>,
    /// How many sessions the user has open.
    pub sessions: usize,
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
    /// The user's name in lower case.
    user: String,
    grants: Arc<Grants>,
    used: Instant,
}

#[derive(Debug)]
pub struct Auth {
    /// Where `users.json` is kept, so that a change made while the server runs outlasts it.
    directory: PathBuf,
    /// The users, by name in lower case.
    users: RwLock<HashMap<String, Account>>,
    sessions: Mutex<HashMap<String, Session>>,
    /// Wrong passwords running and when the last was given, by name in lower case.
    failures: Mutex<HashMap<String, (u32, Instant)>>,
    idle: Duration,
    pause: Duration,
    /// Checking a password is slow and takes memory on purpose, and anybody may ask for it,
    /// so only so many are checked at once and only so many more may wait their turn.
    pub(crate) checking: Semaphore,
    pub(crate) waiting: Semaphore,
}

impl Auth {
    /// The users of a directory, or nothing if it has none, in which case the server asks
    /// for no sign-in.
    pub fn load(directory: &Path) -> Result<Option<Self>, Box<dyn Error>> {
        let users: Vec<User> = read_json(&directory.join(FILE))?.unwrap_or_default();
        let users: HashMap<String, Account> = users
            .into_iter()
            .map(|user| {
                let grants = Arc::new(Grants::new(user.role, user.cubes));
                let account = Account {
                    hash: user.password,
                    grants,
                    name: user.name,
                };
                (account.name.to_lowercase(), account)
            })
            .collect();
        Ok((!users.is_empty()).then(|| Self {
            directory: directory.to_owned(),
            users: RwLock::new(users),
            sessions: Mutex::default(),
            failures: Mutex::default(),
            idle: IDLE,
            pause: PAUSE,
            checking: Semaphore::new(CHECKED_AT_ONCE),
            waiting: Semaphore::new(WAITING),
        }))
    }

    /// Sets how long a session lasts without being used.
    pub fn set_idle(&mut self, idle: Duration) {
        self.idle = idle;
    }

    pub fn idle(&self) -> Duration {
        self.idle
    }

    /// Sets how many sign-ins may wait for their password to be checked.
    pub fn set_waiting(&mut self, waiting: usize) {
        self.waiting = Semaphore::new(waiting);
    }

    /// Sets how long a name is refused after too many wrong passwords.
    pub fn set_pause(&mut self, pause: Duration) {
        self.pause = pause;
    }

    fn users(&self) -> RwLockReadGuard<'_, HashMap<String, Account>> {
        self.users.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn sessions(&self) -> MutexGuard<'_, HashMap<String, Session>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
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
        let known = self
            .users()
            .get(&name)
            .map(|account| (account.hash.clone(), Arc::clone(&account.grants)));
        let hash = match &known {
            Some((hash, _)) => hash,
            None => NOBODY.get_or_init(|| hash("nobody").expect("hashing works")),
        };
        let right = PasswordHash::new(hash).is_ok_and(|hash| {
            Argon2::default()
                .verify_password(password.as_bytes(), &hash)
                .is_ok()
        });
        let grants = match known {
            Some((_, grants)) if right => grants,
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
        let mut sessions = self.sessions();
        sessions.retain(|_, session| now.duration_since(session.used) <= self.idle);
        let session = Session {
            user: name,
            grants: Arc::clone(&grants),
            used: now,
        };
        sessions.insert(token.clone(), session);
        Ok((token, grants))
    }

    /// The user of a live session, by the name as it was given, and what they may do; the
    /// session then counts as used just now. Nothing if the token belongs to no live session.
    pub fn admits(&self, token: &str) -> Option<(String, Arc<Grants>)> {
        let (user, grants) = {
            let mut sessions = self.sessions();
            let now = Instant::now();
            match sessions.get_mut(token) {
                Some(session) if now.duration_since(session.used) <= self.idle => {
                    session.used = now;
                    (session.user.clone(), Arc::clone(&session.grants))
                }
                _ => {
                    sessions.remove(token);
                    return None;
                }
            }
        };
        // The name as it was given, looked up once the sessions are let go, so that the two
        // locks are never held together.
        let users = self.users.read().unwrap_or_else(PoisonError::into_inner);
        let name = users
            .get(&user)
            .map_or(user.clone(), |account| account.name.clone());
        Some((name, grants))
    }

    pub fn sign_out(&self, token: &str) {
        self.sessions().remove(token);
    }

    /// Every user, by name, with how many sessions each has open.
    pub fn list(&self) -> Vec<UserView> {
        let now = Instant::now();
        let mut open: HashMap<String, usize> = HashMap::new();
        for session in self.sessions().values() {
            if now.duration_since(session.used) <= self.idle {
                *open.entry(session.user.clone()).or_default() += 1;
            }
        }
        let mut users: Vec<UserView> = self
            .users()
            .iter()
            .map(|(key, account)| UserView {
                name: account.name.clone(),
                role: account.grants.role(),
                cubes: account.grants.cubes().clone(),
                sessions: open.get(key).copied().unwrap_or(0),
            })
            .collect();
        users.sort_by_key(|user| user.name.to_lowercase());
        users
    }

    /// How many sessions are open.
    pub fn open_sessions(&self) -> usize {
        let now = Instant::now();
        let sessions = self.sessions();
        sessions
            .values()
            .filter(|session| now.duration_since(session.used) <= self.idle)
            .count()
    }

    /// Adds a user, or changes one: their grants, and their password if one is given. A new
    /// user needs a password. It is saved at once, and the user's open sessions hold the new
    /// grants from their next request.
    pub fn set(&self, name: &str, password: Option<&str>, grants: Grants) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("a user needs a name".to_owned());
        }
        if password.is_some_and(str::is_empty) {
            return Err("a password cannot be empty".to_owned());
        }
        let hashed = password
            .map(hash)
            .transpose()
            .map_err(|error| error.to_string())?;
        let key = name.to_lowercase();
        let grants = Arc::new(grants);
        let mut users = self.users.write().unwrap_or_else(PoisonError::into_inner);
        let hash = match (hashed, users.get(&key)) {
            (Some(hash), _) => hash,
            (None, Some(account)) => account.hash.clone(),
            (None, None) => return Err(format!("the new user {name} needs a password")),
        };
        let mut changed = users.clone();
        changed.insert(
            key.clone(),
            Account {
                name: name.to_owned(),
                hash,
                grants: Arc::clone(&grants),
            },
        );
        keeps_an_admin(&changed)?;
        save(&self.directory, &changed).map_err(|error| error.to_string())?;
        *users = changed;
        drop(users);
        for session in self.sessions().values_mut() {
            if session.user == key {
                session.grants = Arc::clone(&grants);
            }
        }
        Ok(())
    }

    /// Removes a user and ends their sessions. The last administrator cannot be removed.
    pub fn remove(&self, name: &str) -> Result<(), String> {
        let key = name.trim().to_lowercase();
        let mut users = self.users.write().unwrap_or_else(PoisonError::into_inner);
        if !users.contains_key(&key) {
            return Err(format!("there is no user called {name}"));
        }
        let mut changed = users.clone();
        changed.remove(&key);
        keeps_an_admin(&changed)?;
        save(&self.directory, &changed).map_err(|error| error.to_string())?;
        *users = changed;
        drop(users);
        self.end_sessions(&key);
        Ok(())
    }

    /// Ends every session of a user; says how many there were.
    pub fn end_sessions(&self, name: &str) -> usize {
        let key = name.trim().to_lowercase();
        let mut sessions = self.sessions();
        let before = sessions.len();
        sessions.retain(|_, session| session.user != key);
        before - sessions.len()
    }

    /// Whether there is a user of that name.
    pub fn has(&self, name: &str) -> bool {
        self.users().contains_key(&name.trim().to_lowercase())
    }
}

/// Refuses a set of users with no administrator, who could change no user again.
fn keeps_an_admin(users: &HashMap<String, Account>) -> Result<(), String> {
    match users
        .values()
        .any(|account| account.grants.role() == Role::Admin)
    {
        true => Ok(()),
        false => {
            Err("that would leave no administrator, and nobody could manage users again".to_owned())
        }
    }
}

/// Writes the users to a directory's `users.json`.
fn save(directory: &Path, users: &HashMap<String, Account>) -> Result<(), Box<dyn Error>> {
    let mut stored: Vec<User> = users
        .values()
        .map(|account| User {
            name: account.name.clone(),
            password: account.hash.clone(),
            role: account.grants.role(),
            cubes: account.grants.cubes().clone(),
        })
        .collect();
    stored.sort_by_key(|user| user.name.to_lowercase());
    Ok(write_json(&directory.join(FILE), &stored)?)
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
