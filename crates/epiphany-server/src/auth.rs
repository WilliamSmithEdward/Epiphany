//! Users, their passwords, what each may do, and the sessions they sign in to.
//!
//! Users are kept in `users.json` in the server's directory, each with a salted Argon2 hash of
//! the password; the password itself is never stored. Signing in gives a random token, which
//! the client sends with every request. A session ends when it is signed out of or has not
//! been used for a while. Sessions live in memory, so they end when the server stops.
//!
//! A name that has been given the wrong password several times running is refused for a
//! while whatever password comes with it, so that passwords cannot be tried by the thousand.
//!
//! An administrator may view the server as another user does: a session of that user's,
//! able only to read what they may read, which names the administrator viewing. Each is
//! recorded in `audit.jsonl` before it is given.
//!
//! A user may make API keys, for programs to use in their name: an administrator always,
//! anyone else once an administrator lets them. A key is sent as a session's token is, and
//! lasts until it is revoked or expires. Each is given what it may do in each part of the
//! server, cells, the model and administration, and may be kept to particular cubes; it
//! never does more than its user may at the time. Only a hash of each key is kept, with
//! its user in `users.json`, so the key itself is seen once, as it is made. A key is
//! recorded in the audit before it is made, and after it is revoked: revoking is never held
//! up for want of a line in the audit.
//!
//! What a user keeps for themself, their own views and subsets, is kept with them
//! ([`Private`]).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock, RwLockReadGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use blake2::{Blake2b256, Digest};
use epiphany_core::access::{Access, Permit};
use epiphany_core::model::{Model, ViewDef, key};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use utoipa::ToSchema;

use crate::private::{Private, SUBSETS, VIEWS};
use crate::storage::{read_json, write_json};

const FILE: &str = "users.json";
/// Where what administrators do as others is recorded, one line each, beside the users.
const AUDIT: &str = "audit.jsonl";
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
/// What every API key starts with, so that one is told from a session's token at a glance.
pub const KEY_START: &str = "epk_";
/// How much of a key after its start names it: enough to tell it from every other, far
/// too little to use it by.
const KEY_ID: usize = 12;
/// The most API keys a user may hold, each of which costs a rewrite of `users.json`.
const MOST_KEYS: usize = 100;
/// The longest name a key may be given, in characters.
const LONGEST_KEY_NAME: usize = 100;
/// How often, at most, the times keys were last used are saved. A use is noted in memory at
/// every request; written to disk each time, it would be a write for every read.
const SAVING_USES: Duration = Duration::from_secs(10 * 60);

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

/// Roles on elements, by dimension and then element, as given: `none`, `read` or `write`.
pub type ElementRoles = BTreeMap<String, BTreeMap<String, Role>>;

/// What a signed-in user may do: a role, for some cubes another role in its place, and for
/// some elements a role of their own, which holds beneath them too.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grants {
    role: Role,
    /// The cubes with a role of their own, by name as given.
    cubes: BTreeMap<String, Role>,
    /// The same, by name in lower case without spaces.
    found: HashMap<String, Role>,
    elements: ElementRoles,
    /// What the roles on elements come to for the model as it last was.
    worked: Worked,
}

/// The permits a user's roles on elements come to for one version of the model, kept until
/// the model changes. A copy of grants works them out again.
#[derive(Debug, Default)]
struct Worked(Mutex<Option<(u64, Arc<Access>)>>);

impl Clone for Worked {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for Worked {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Worked {}

impl Grants {
    pub fn new(role: Role, cubes: BTreeMap<String, Role>) -> Self {
        let found = cubes
            .iter()
            .map(|(name, &role)| (key(name), role))
            .collect();
        Self {
            role,
            cubes,
            found,
            ..Self::default()
        }
    }

    /// The same grants with these roles on elements.
    pub fn with_elements(mut self, elements: ElementRoles) -> Self {
        self.elements = elements;
        self
    }

    /// The role that holds wherever a cube has none of its own. Changing the model takes
    /// this one to be admin, whatever any cube says.
    pub fn role(&self) -> Role {
        self.role
    }

    pub fn cubes(&self) -> &BTreeMap<String, Role> {
        &self.cubes
    }

    /// The roles on elements, as given.
    pub fn elements(&self) -> &ElementRoles {
        &self.elements
    }

    /// The role on one cube.
    pub fn on(&self, cube: &str) -> Role {
        self.found.get(&key(cube)).copied().unwrap_or(self.role)
    }

    /// The highest role held anywhere.
    pub fn most(&self) -> Role {
        self.cubes.values().copied().fold(self.role, Role::max)
    }

    /// What the roles on elements allow of each element of a model at a version: nothing to
    /// an administrator, who may do anything, or to a user with no role on any element.
    pub fn access(&self, model: &Model, version: u64) -> Option<Arc<Access>> {
        if self.role == Role::Admin || self.elements.is_empty() {
            return None;
        }
        let mut worked = self.worked.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((_, access)) = worked.as_ref().filter(|(at, _)| *at == version) {
            return Some(Arc::clone(access));
        }
        let permit = |role: &Role| match role {
            Role::None => Permit::Hidden,
            Role::Read => Permit::Read,
            Role::Write | Role::Admin => Permit::Write,
        };
        let given = self.elements.iter().map(|(dimension, elements)| {
            let elements = elements
                .iter()
                .map(|(name, role)| (name.clone(), permit(role)));
            (dimension.clone(), elements.collect())
        });
        let access = Arc::new(Access::new(model, &given.collect()));
        *worked = Some((version, Arc::clone(&access)));
        Some(access)
    }

    /// The same grants with none above reading: what another may see through them, changing
    /// nothing.
    pub fn read_only(&self) -> Self {
        let read = |role: Role| role.min(Role::Read);
        let cubes = self
            .cubes
            .iter()
            .map(|(cube, &role)| (cube.clone(), read(role)));
        let elements = self.elements.iter().map(|(dimension, elements)| {
            let elements = elements
                .iter()
                .map(|(name, &role)| (name.clone(), read(role)));
            (dimension.clone(), elements.collect())
        });
        Self::new(read(self.role), cubes.collect()).with_elements(elements.collect())
    }
}

/// A part of the server, as far as an API key may reach into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Cells,
    Model,
    Admin,
}

impl Part {
    /// What reaching this far into the part does, as a refusal says it.
    pub fn doing(self, reach: Role) -> &'static str {
        match (self, reach >= Role::Write) {
            (Self::Cells, false) => "read cells",
            (Self::Cells, true) => "write cells",
            (Self::Model, false) => "read the model",
            (Self::Model, true) => "change the model",
            (Self::Admin, false) => "see the server's administration",
            (Self::Admin, true) => "administer the server",
        }
    }
}

/// What an API key may do in each part of the server, each `none`, `read` or `write`, and on
/// which cubes. A key never does more than its user may: what it is given is held at every
/// request to what they may do then.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Permissions {
    /// Cells: reading them, with queries, views, history and comments; writing and spreading
    /// them, with transactions, locks, sandboxes and comments.
    pub cells: Role,
    /// The model: reading its dimensions, cubes and dashboards; changing them.
    pub model: Role,
    /// Everything under `/admin`: reading the server's state, users, audit and changes;
    /// managing users and their sessions.
    pub admin: Role,
    /// The cubes whose cells the key reaches, by name; every cube where none are named.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cubes: Vec<String>,
}

impl Permissions {
    /// How far into a part they reach.
    pub fn of(&self, part: Part) -> Role {
        match part {
            Part::Cells => self.cells,
            Part::Model => self.model,
            Part::Admin => self.admin,
        }
    }

    /// The most a user may give a key: into each part as far as their roles go. Changing the
    /// model and administration take an administrator, as they do of a user.
    pub fn most(user: &Grants) -> Self {
        let admin = user.role() == Role::Admin;
        let cells = user.most().min(Role::Write);
        let only_admin = |role: Role| if admin { role } else { Role::None };
        Self {
            cells,
            model: only_admin(Role::Write).max(cells.min(Role::Read)),
            admin: only_admin(Role::Write),
            cubes: Vec::new(),
        }
    }

    /// The same, held to what a user may do now.
    fn within(&self, user: &Grants) -> Self {
        let most = Self::most(user);
        Self {
            cells: self.cells.min(most.cells),
            model: self.model.min(most.model),
            admin: self.admin.min(most.admin),
            cubes: self.cubes.clone(),
        }
    }

    /// A user's grants as a key with these permissions has them: on no cube more than it
    /// reaches into cells, and on its cubes alone where it names any. An administrator is
    /// bound by no role on elements, and nor are their keys.
    fn on_cubes(&self, user: &Grants) -> Grants {
        let held = |role: Role| role.min(self.cells);
        let (role, cubes) = match self.cubes.is_empty() {
            true => {
                let cubes = user.cubes().iter();
                let cubes = cubes.map(|(cube, &role)| (cube.clone(), held(role)));
                (held(user.role()), cubes.collect())
            }
            false => {
                let cubes = self.cubes.iter();
                let cubes = cubes.map(|cube| (cube.clone(), held(user.on(cube))));
                (Role::None, cubes.collect())
            }
        };
        let elements = match user.role() {
            Role::Admin => ElementRoles::new(),
            _ => user.elements().clone(),
        };
        Grants::new(role, cubes).with_elements(elements)
    }

    /// Why a user may not give a key these permissions, if they may not.
    fn refused(&self, user: &Grants) -> Option<String> {
        let parts = [Part::Cells, Part::Model, Part::Admin];
        if parts.iter().any(|&part| self.of(part) == Role::Admin) {
            return Some("each part takes none, read or write".to_owned());
        }
        if parts.iter().all(|&part| self.of(part) == Role::None) {
            return Some("a key needs to be able to do something".to_owned());
        }
        let most = Self::most(user);
        let beyond: Vec<&str> = parts
            .iter()
            .filter(|&&part| self.of(part) > most.of(part))
            .map(|&part| part.doing(self.of(part)))
            .collect();
        if !beyond.is_empty() {
            let beyond = beyond.join(" or ");
            return Some(format!(
                "a key does no more than its user may, who may not {beyond}"
            ));
        }
        if !self.cubes.is_empty() && (self.model == Role::Write || self.admin != Role::None) {
            let message = "a key kept to particular cubes may not change the model or \
                           administer the server, which reach every cube";
            return Some(message.to_owned());
        }
        None
    }
}

/// An API key as its user and administrators see it: never the key itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct KeyView {
    /// What follows `epk_` at the start of the key: it names the key in lists and to revoke
    /// it, and is not enough of it to use.
    pub id: String,
    /// What its user calls it.
    pub name: String,
    #[serde(flatten)]
    pub permissions: Permissions,
    /// When it was made, in seconds since 1970.
    pub created: u64,
    /// When it stops working, in seconds since 1970; it lasts until revoked where there is
    /// no such time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<u64>,
    /// When it was last used, in seconds since 1970; never, where there is no such time. It
    /// is noted at every use and saved every few minutes, so the last uses before the server
    /// stopped may be missed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<u64>,
}

impl KeyView {
    /// The key as the audit names it: by its name, and the start of it that names it too.
    fn label(&self) -> String {
        format!("{} ({KEY_START}{})", self.name, self.id)
    }
}

/// A key as `users.json` keeps it: by its hash, never as itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredKey {
    #[serde(flatten)]
    view: KeyView,
    /// The BLAKE2b hash of the key. A key is random and long, so a hash quick to work out
    /// is as safe as a slow one, and it is worked out at every request.
    hash: String,
}

/// A user's keys as they see them.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct KeyList {
    /// Whether they may make keys: an administrator always, anyone else once an
    /// administrator lets them.
    pub allowed: bool,
    /// The most a key of theirs may be given now.
    pub most: Permissions,
    /// Their keys, the newest first.
    pub keys: Vec<KeyView>,
}

/// Why a key was not made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyRefusal {
    /// The user may not make keys.
    NotAllowed,
    /// The key asked for cannot be made, for the reason given.
    Invalid(String),
    /// It could not be saved.
    Unsaved(String),
}

/// An API key as a request that carries it is let in by: whose it is, and what it may do.
#[derive(Debug)]
struct Usable {
    /// The user's name as it was given.
    user: String,
    /// What the key reaches, held to what its user may do.
    reach: Arc<Permissions>,
    /// The user's grants, held to the key's reach into cells and to its cubes.
    grants: Arc<Grants>,
    expires: Option<u64>,
    /// When it was last used, in seconds since 1970; zero if never.
    used: AtomicU64,
}

/// Whether a user may make API keys: an administrator always, anyone else if let.
fn makes_keys(role: Role, allowed: bool) -> bool {
    allowed || role == Role::Admin
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
    /// Elements with a role of their own, which holds beneath them too.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    elements: ElementRoles,
    /// Whether an administrator lets the user make API keys. Administrators always may.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    makes_keys: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    keys: Vec<StoredKey>,
}

impl User {
    /// The same user, their keys taken away if they may no longer make any.
    fn kept(mut self) -> Self {
        if !makes_keys(self.role, self.makes_keys) {
            self.keys.clear();
        }
        self
    }
}

/// A user as the server holds them while it runs.
#[derive(Debug, Clone)]
struct Account {
    /// The name as it was given.
    name: String,
    hash: String,
    grants: Arc<Grants>,
    /// Whether an administrator lets them make API keys.
    makes_keys: bool,
    /// Their keys, the oldest first.
    keys: Vec<StoredKey>,
}

impl Account {
    fn makes_keys(&self) -> bool {
        makes_keys(self.grants.role(), self.makes_keys)
    }
}

impl From<User> for Account {
    fn from(user: User) -> Self {
        let user = user.kept();
        let grants = Grants::new(user.role, user.cubes).with_elements(user.elements);
        Self {
            name: user.name,
            hash: user.password,
            grants: Arc::new(grants),
            makes_keys: user.makes_keys,
            keys: user.keys,
        }
    }
}

impl From<&Account> for User {
    fn from(account: &Account) -> Self {
        Self {
            name: account.name.clone(),
            password: account.hash.clone(),
            role: account.grants.role(),
            cubes: account.grants.cubes().clone(),
            elements: account.grants.elements().clone(),
            makes_keys: account.makes_keys,
            keys: account.keys.clone(),
        }
    }
}

/// The keys of every user who may have them, by hash, as requests are let in by them.
fn usable(users: &HashMap<String, Account>) -> HashMap<String, Usable> {
    let accounts = users.values().filter(|account| account.makes_keys());
    let keys = accounts.flat_map(|account| {
        account.keys.iter().map(|key| {
            let usable = Usable {
                user: account.name.clone(),
                reach: Arc::new(key.view.permissions.within(&account.grants)),
                grants: Arc::new(key.view.permissions.on_cubes(&account.grants)),
                expires: key.view.expires,
                used: AtomicU64::new(key.view.used.unwrap_or(0)),
            };
            (key.hash.clone(), usable)
        })
    });
    keys.collect()
}

/// A user as an administrator sees them: never the password.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UserView {
    pub name: String,
    /// What the user may do.
    pub role: Role,
    /// The cubes on which the user has another role in place of that one.
    pub cubes: BTreeMap<String, Role>,
    /// The elements with a role of their own, by dimension and element: `none`, `read` or
    /// `write`, holding beneath them too where nearer elements have none.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub elements: ElementRoles,
    /// How many sessions the user has open.
    pub sessions: usize,
    /// Whether an administrator lets the user make API keys. Administrators always may.
    pub makes_keys: bool,
    /// The user's API keys, the newest first.
    pub keys: Vec<KeyView>,
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
    /// For a view of the server as the user has it, the administrator viewing, by name in
    /// lower case. The grants are then the user's, read only.
    viewer: Option<String>,
}

/// The user a live session or an API key is of, by the name as given, and what it may do;
/// for a view as that user, the administrator viewing; and for a key, how far it reaches.
#[derive(Debug, Clone)]
pub struct Admitted {
    pub name: String,
    pub grants: Arc<Grants>,
    pub viewer: Option<String>,
    pub key: Option<Arc<Permissions>>,
}

/// One thing kept on record: when, by whom, what, and as or of whom.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AuditEntry {
    /// When, in seconds since 1970.
    pub time: u64,
    /// Who did it.
    pub user: String,
    /// What they did: `view as`, `make key` or `revoke key`.
    pub action: String,
    /// Whom it was done as, or what to: a key by its name and the start of it, and whose it
    /// is where that is not the user's own.
    pub subject: String,
}

#[derive(Debug)]
pub struct Auth {
    /// Where `users.json` is kept, so that a change made while the server runs outlasts it.
    directory: PathBuf,
    /// The users, by name in lower case.
    users: RwLock<HashMap<String, Account>>,
    /// Their API keys, by hash. Put in place with the users, while they are held.
    keys: RwLock<HashMap<String, Usable>>,
    /// Whether a key has been used since the times of use were last saved.
    unsaved_uses: AtomicBool,
    /// When they were last saved, and how often, at most, they are.
    saved_uses: Mutex<Instant>,
    saving_uses: Duration,
    sessions: Mutex<HashMap<String, Session>>,
    /// Wrong passwords running and when the last was given, by name in lower case.
    failures: Mutex<HashMap<String, (u32, Instant)>>,
    idle: Duration,
    pause: Duration,
    /// Checking a password is slow and takes memory on purpose, and anybody may ask for it,
    /// so only so many are checked at once and only so many more may wait their turn.
    pub(crate) checking: Semaphore,
    pub(crate) waiting: Semaphore,
    /// Taken to add a line to the audit, one at a time.
    recording: Mutex<()>,
    /// The views each user keeps for themself, which go with them.
    pub(crate) views: Private<ViewDef>,
    /// The subsets each user keeps for themself, lists of members by name.
    pub(crate) subsets: Private<Vec<String>>,
}

impl Auth {
    /// The users of a directory, or nothing if it has none, in which case the server asks
    /// for no sign-in.
    pub fn load(directory: &Path) -> Result<Option<Self>, Box<dyn Error>> {
        let users: Vec<User> = read_json(&directory.join(FILE))?.unwrap_or_default();
        let users: HashMap<String, Account> = users
            .into_iter()
            .map(|user| {
                let account = Account::from(user);
                (account.name.to_lowercase(), account)
            })
            .collect();
        if users.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            directory: directory.to_owned(),
            keys: RwLock::new(usable(&users)),
            users: RwLock::new(users),
            unsaved_uses: AtomicBool::new(false),
            saved_uses: Mutex::new(Instant::now()),
            saving_uses: SAVING_USES,
            sessions: Mutex::default(),
            failures: Mutex::default(),
            idle: IDLE,
            pause: PAUSE,
            checking: Semaphore::new(CHECKED_AT_ONCE),
            waiting: Semaphore::new(WAITING),
            recording: Mutex::default(),
            views: Private::load(directory, VIEWS)?,
            subsets: Private::load(directory, SUBSETS)?,
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

    /// Sets how often, at most, the times keys were last used are saved.
    pub fn set_saving_uses(&mut self, every: Duration) {
        self.saving_uses = every;
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
        let token = self.start(name, Arc::clone(&grants), None);
        Ok((token, grants))
    }

    /// Starts a session and gives its token: a random one, which cannot be guessed.
    fn start(&self, user: String, grants: Arc<Grants>, viewer: Option<String>) -> String {
        let token = random();
        let now = Instant::now();
        let mut sessions = self.sessions();
        sessions.retain(|_, session| now.duration_since(session.used) <= self.idle);
        let session = Session {
            user,
            grants,
            used: now,
            viewer,
        };
        sessions.insert(token.clone(), session);
        token
    }

    /// Starts a view of the server as another user has it, for an administrator: a session
    /// of that user's that may only read what they may read. It is recorded in the audit
    /// first, and refused if it cannot be. Gives its token, its grants, and the user's name
    /// as it was given.
    pub fn view_as(
        &self,
        viewer: &str,
        user: &str,
    ) -> Result<(String, Arc<Grants>, String), String> {
        let key = user.trim().to_lowercase();
        let (name, grants) = self
            .users()
            .get(&key)
            .map(|account| (account.name.clone(), Arc::new(account.grants.read_only())))
            .ok_or_else(|| format!("there is no user called {user}"))?;
        let entry = AuditEntry {
            time: now(),
            user: viewer.to_owned(),
            action: "view as".to_owned(),
            subject: name.clone(),
        };
        self.record(&entry).map_err(|error| {
            format!("the view could not be recorded, so it is not given: {error}")
        })?;
        let token = self.start(key, Arc::clone(&grants), Some(viewer.to_lowercase()));
        Ok((token, grants, name))
    }

    /// Adds a line to the audit, flushed to disk before it is relied on.
    fn record(&self, entry: &AuditEntry) -> io::Result<()> {
        let _turn = self
            .recording
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut line = serde_json::to_vec(entry)?;
        line.push(b'\n');
        let path = self.directory.join(AUDIT);
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        file.write_all(&line)?;
        file.sync_data()
    }

    /// What the audit records, the newest first, at most `most` of it. A line cut short, the
    /// last after a crash, is left out.
    pub fn audit(&self, most: usize) -> io::Result<Vec<AuditEntry>> {
        let file = match File::open(self.directory.join(AUDIT)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut entries = std::collections::VecDeque::with_capacity(most.min(1024));
        for line in io::BufReader::new(file).lines() {
            let Ok(entry) = serde_json::from_str::<AuditEntry>(&line?) else {
                continue;
            };
            if entries.len() == most {
                entries.pop_front();
            }
            if most > 0 {
                entries.push_back(entry);
            }
        }
        Ok(entries.into_iter().rev().collect())
    }

    /// The user of a live session or an API key, by the name as it was given, what they may
    /// do, and who views as them; a session or a key then counts as used just now. Nothing if
    /// the token belongs to no live session and is no key in force.
    pub fn admits(&self, token: &str) -> Option<Admitted> {
        if token.starts_with(KEY_START) {
            let keys = self.keys.read().unwrap_or_else(PoisonError::into_inner);
            let key = keys.get(&digest(token))?;
            let now = now();
            if key.expires.is_some_and(|at| now >= at) {
                return None;
            }
            key.used.store(now, Ordering::Relaxed);
            self.unsaved_uses.store(true, Ordering::Relaxed);
            return Some(Admitted {
                name: key.user.clone(),
                grants: Arc::clone(&key.grants),
                viewer: None,
                key: Some(Arc::clone(&key.reach)),
            });
        }
        let (user, grants, viewer) = {
            let mut sessions = self.sessions();
            let now = Instant::now();
            match sessions.get_mut(token) {
                Some(session) if now.duration_since(session.used) <= self.idle => {
                    session.used = now;
                    let grants = Arc::clone(&session.grants);
                    (session.user.clone(), grants, session.viewer.clone())
                }
                _ => {
                    sessions.remove(token);
                    return None;
                }
            }
        };
        // The names as they were given, looked up once the sessions are let go, so that the
        // two locks are never held together.
        let users = self.users.read().unwrap_or_else(PoisonError::into_inner);
        let given = |key: &String| {
            users
                .get(key)
                .map_or(key.clone(), |account| account.name.clone())
        };
        Some(Admitted {
            name: given(&user),
            grants,
            viewer: viewer.as_ref().map(given),
            key: None,
        })
    }

    pub fn sign_out(&self, token: &str) {
        self.sessions().remove(token);
    }

    /// Every user, by name, with how many sessions each has open.
    pub fn list(&self) -> Vec<UserView> {
        let now = Instant::now();
        let mut open: HashMap<String, usize> = HashMap::new();
        for session in self.sessions().values() {
            // A view as another user is the session of the administrator viewing.
            if now.duration_since(session.used) <= self.idle {
                let whose = session.viewer.as_ref().unwrap_or(&session.user);
                *open.entry(whose.clone()).or_default() += 1;
            }
        }
        // The users before their keys, as they are taken whenever both are.
        let accounts = self.users();
        let usable = self.keys.read().unwrap_or_else(PoisonError::into_inner);
        let mut users: Vec<UserView> = accounts
            .iter()
            .map(|(key, account)| UserView {
                name: account.name.clone(),
                role: account.grants.role(),
                cubes: account.grants.cubes().clone(),
                elements: account.grants.elements().clone(),
                sessions: open.get(key).copied().unwrap_or(0),
                makes_keys: account.makes_keys,
                keys: newest_first(&account.keys, &usable),
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

    /// Adds a user, or changes one: their grants, their password if one is given, and whether
    /// they may make API keys if that is given. A new user needs a password, and may make no
    /// keys unless let. It is saved at once, and the user's open sessions and keys hold the
    /// new grants from their next request. A user who may no longer make keys loses theirs.
    /// `by` is the administrator who changes them, on record for any key they take away.
    pub fn set(
        &self,
        name: &str,
        password: Option<&str>,
        grants: Grants,
        makes_keys: Option<bool>,
        by: &str,
    ) -> Result<(), String> {
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
        let before = users.get(&key);
        let hash = match (hashed, before) {
            (Some(hash), _) => hash,
            (None, Some(account)) => account.hash.clone(),
            (None, None) => return Err(format!("the new user {name} needs a password")),
        };
        let user = User {
            name: name.to_owned(),
            password: hash,
            role: grants.role(),
            cubes: grants.cubes().clone(),
            elements: grants.elements().clone(),
            makes_keys: makes_keys
                .or(before.map(|account| account.makes_keys))
                .unwrap_or(false),
            keys: before
                .map(|account| account.keys.clone())
                .unwrap_or_default(),
        };
        let had: Vec<String> = user.keys.iter().map(|key| key.view.label()).collect();
        let account = Account::from(user);
        // Keys the user may no longer have, taken away with the leave to make them.
        let taken = match account.keys.is_empty() {
            true => had,
            false => Vec::new(),
        };
        let mut changed = users.clone();
        changed.insert(key.clone(), account);
        keeps_an_admin(&changed)?;
        self.commit(&mut users, changed)?;
        drop(users);
        for label in taken {
            self.note_revoked(by, label, name);
        }
        let mut sessions = self.sessions();
        // Views by someone no longer an administrator end.
        let admin = grants.role() == Role::Admin;
        sessions.retain(|_, session| admin || session.viewer.as_ref() != Some(&key));
        for session in sessions.values_mut().filter(|session| session.user == key) {
            // A view as the user stays one that only reads.
            session.grants = match session.viewer {
                Some(_) => Arc::new(grants.read_only()),
                None => Arc::clone(&grants),
            };
        }
        Ok(())
    }

    /// Removes a user, ends their sessions and takes their views and subsets away. The last
    /// administrator cannot be removed.
    pub fn remove(&self, name: &str) -> Result<(), String> {
        let key = name.trim().to_lowercase();
        let mut users = self.users.write().unwrap_or_else(PoisonError::into_inner);
        if !users.contains_key(&key) {
            return Err(format!("there is no user called {name}"));
        }
        let mut changed = users.clone();
        changed.remove(&key);
        keeps_an_admin(&changed)?;
        self.commit(&mut users, changed)?;
        drop(users);
        self.end_sessions(&key);
        self.views
            .forget_user(&key)
            .and_then(|()| self.subsets.forget_user(&key))
            .map_err(|error| format!("{name} is removed, but not all they kept: {error}"))
    }

    /// Ends every session of a user, their views as others with them; says how many there
    /// were.
    pub fn end_sessions(&self, name: &str) -> usize {
        let key = name.trim().to_lowercase();
        let mut sessions = self.sessions();
        let before = sessions.len();
        sessions.retain(|_, session| session.user != key && session.viewer.as_ref() != Some(&key));
        before - sessions.len()
    }

    /// Whether there is a user of that name.
    pub fn has(&self, name: &str) -> bool {
        self.users().contains_key(&name.trim().to_lowercase())
    }

    /// Saves a changed set of users and puts it in place, the keys it holds with it, so that
    /// a key revoked or a user's grants changed hold from the next request. The times keys
    /// were last used are saved with it.
    fn commit(
        &self,
        users: &mut HashMap<String, Account>,
        mut changed: HashMap<String, Account>,
    ) -> Result<(), String> {
        self.note_uses(&mut changed);
        save(&self.directory, &changed).map_err(|error| error.to_string())?;
        *self.keys.write().unwrap_or_else(PoisonError::into_inner) = usable(&changed);
        *users = changed;
        Ok(())
    }

    /// Puts the times keys were last used, as noted at each use, with the users' keys, for
    /// them to be saved.
    fn note_uses(&self, users: &mut HashMap<String, Account>) {
        // A use from now on is one to save next time.
        self.unsaved_uses.store(false, Ordering::Relaxed);
        let usable = self.keys.read().unwrap_or_else(PoisonError::into_inner);
        for key in users
            .values_mut()
            .flat_map(|account| account.keys.iter_mut())
        {
            let used = usable
                .get(&key.hash)
                .map_or(0, |held| held.used.load(Ordering::Relaxed));
            if used > 0 {
                key.view.used = Some(used);
            }
        }
    }

    /// Whether the times keys were last used are to be saved now: a key has been used since
    /// they were last saved, long enough ago. True once each time, for the caller to save
    /// them, away from the request that asked.
    pub fn uses_due(&self) -> bool {
        if !self.unsaved_uses.load(Ordering::Relaxed) {
            return false;
        }
        let mut saved = self
            .saved_uses
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if saved.elapsed() < self.saving_uses {
            return false;
        }
        *saved = Instant::now();
        true
    }

    /// Saves the times keys were last used, with the users. If they cannot be saved, they
    /// are tried again with the next save, and nothing else is lost.
    pub fn save_uses(&self) {
        let mut users = self.users.write().unwrap_or_else(PoisonError::into_inner);
        let mut changed = users.clone();
        self.note_uses(&mut changed);
        match save(&self.directory, &changed) {
            Ok(()) => *users = changed,
            Err(error) => {
                self.unsaved_uses.store(true, Ordering::Relaxed);
                eprintln!("The times API keys were last used could not be saved: {error}");
            }
        }
    }

    /// A user's API keys, the newest first, whether they may make more, and the most one may
    /// be given. Nothing if there is no such user.
    pub fn keys(&self, user: &str) -> Option<KeyList> {
        let users = self.users();
        let account = users.get(&user.trim().to_lowercase())?;
        let usable = self.keys.read().unwrap_or_else(PoisonError::into_inner);
        Some(KeyList {
            allowed: account.makes_keys(),
            most: Permissions::most(&account.grants),
            keys: newest_first(&account.keys, &usable),
        })
    }

    /// Makes an API key for a user who may make keys, with permissions they may give it, to
    /// expire at a time if one is given. Gives the key as its user sees it, and the key
    /// itself: the one time it is given, for only its hash is kept.
    pub fn make_key(
        &self,
        user: &str,
        name: &str,
        permissions: Permissions,
        expires: Option<u64>,
    ) -> Result<(KeyView, String), KeyRefusal> {
        let user = user.trim().to_lowercase();
        let name = name.trim();
        let invalid = |message: String| Err(KeyRefusal::Invalid(message));
        let mut users = self.users.write().unwrap_or_else(PoisonError::into_inner);
        let account = users.get(&user).filter(|account| account.makes_keys());
        let Some(account) = account else {
            return Err(KeyRefusal::NotAllowed);
        };
        if name.is_empty() {
            return invalid("a key needs a name".to_owned());
        }
        if name.chars().count() > LONGEST_KEY_NAME {
            return invalid(format!(
                "a key's name takes at most {LONGEST_KEY_NAME} characters"
            ));
        }
        let mut mine = account.keys.iter().map(|key| &key.view.name);
        if mine.any(|other| other.to_lowercase() == name.to_lowercase()) {
            return invalid(format!("there is a key called {name} already"));
        }
        if account.keys.len() >= MOST_KEYS {
            return invalid(format!(
                "a user holds at most {MOST_KEYS} keys; revoke one first"
            ));
        }
        if expires.is_some_and(|at| at <= now()) {
            return invalid("a key cannot expire before it is made".to_owned());
        }
        if let Some(why) = permissions.refused(&account.grants) {
            return invalid(why);
        }
        let owner = account.name.clone();
        // The start of each key names it, so it is unlike any other key's.
        let taken: HashSet<&str> = users
            .values()
            .flat_map(|account| account.keys.iter().map(|key| key.view.id.as_str()))
            .collect();
        let key = loop {
            let key = format!("{KEY_START}{}", random());
            if !taken.contains(&key[KEY_START.len()..][..KEY_ID]) {
                break key;
            }
        };
        let view = KeyView {
            id: key[KEY_START.len()..][..KEY_ID].to_owned(),
            name: name.to_owned(),
            permissions,
            created: now(),
            expires,
            used: None,
        };
        let stored = StoredKey {
            view: view.clone(),
            hash: digest(&key),
        };
        // Kept on record before it is made, as a view as another user is.
        let entry = AuditEntry {
            time: view.created,
            user: owner,
            action: "make key".to_owned(),
            subject: view.label(),
        };
        self.record(&entry).map_err(|error| {
            KeyRefusal::Unsaved(format!(
                "the key could not be recorded, so it is not made: {error}"
            ))
        })?;
        let mut changed = users.clone();
        let account = changed.get_mut(&user).expect("the user was found");
        account.keys.push(stored);
        self.commit(&mut users, changed)
            .map_err(KeyRefusal::Unsaved)?;
        Ok((view, key))
    }

    /// Revokes one of a user's keys, which stops working at once, and records who revoked
    /// it. False if they hold no key of that id.
    pub fn revoke_key(&self, user: &str, id: &str, by: &str) -> Result<bool, String> {
        let user = user.trim().to_lowercase();
        let mut users = self.users.write().unwrap_or_else(PoisonError::into_inner);
        let found = users.get(&user).and_then(|account| {
            let key = account.keys.iter().find(|key| key.view.id == id)?;
            Some((account.name.clone(), key.view.label()))
        });
        let Some((owner, label)) = found else {
            return Ok(false);
        };
        let mut changed = users.clone();
        let account = changed.get_mut(&user).expect("the user was found");
        account.keys.retain(|key| key.view.id != id);
        self.commit(&mut users, changed)?;
        drop(users);
        self.note_revoked(by, label, &owner);
        Ok(true)
    }

    /// Records a key revoked, after it is: revoking a key is never held up for want of a
    /// line in the audit, which is said instead.
    fn note_revoked(&self, by: &str, label: String, owner: &str) {
        let subject = match owner.eq_ignore_ascii_case(by) {
            true => label,
            false => format!("{label} of {owner}"),
        };
        let entry = AuditEntry {
            time: now(),
            user: by.to_owned(),
            action: "revoke key".to_owned(),
            subject,
        };
        if let Err(error) = self.record(&entry) {
            eprintln!(
                "The key {} was revoked, but could not be recorded in the audit: {error}",
                entry.subject
            );
        }
    }
}

/// Keys as their user sees them, the newest first, each last used when it was, as noted at
/// each use whether saved yet or not.
fn newest_first(keys: &[StoredKey], usable: &HashMap<String, Usable>) -> Vec<KeyView> {
    let view = |key: &StoredKey| {
        let noted = usable
            .get(&key.hash)
            .map_or(0, |held| held.used.load(Ordering::Relaxed));
        KeyView {
            used: Some(noted).filter(|&used| used > 0).or(key.view.used),
            ..key.view.clone()
        }
    };
    keys.iter().rev().map(view).collect()
}

/// The time now, in seconds since 1970.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Thirty-two random bytes, in hexadecimal: a token or a key nobody can guess.
fn random() -> String {
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).expect("the operating system provides randomness");
    hex(&random)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The hash an API key is kept by.
fn digest(key: &str) -> String {
    hex(&Blake2b256::digest(key.as_bytes()))
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
    let mut stored: Vec<User> = users.values().map(User::from).collect();
    stored.sort_by_key(|user| user.name.to_lowercase());
    Ok(write_json(&directory.join(FILE), &stored)?)
}

fn hash(password: &str) -> Result<String, Box<dyn Error>> {
    let hash = Argon2::default().hash_password(password.as_bytes());
    Ok(hash.map_err(|error| error.to_string())?.to_string())
}

/// Adds a user to a directory, or changes the password and grants of one it already has,
/// who keeps their API keys while they may still make them.
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
    let same = |user: &User| user.name.eq_ignore_ascii_case(name.trim());
    let before = users.iter().find(|user| same(user));
    let (makes_keys, keys) = before.map_or((false, Vec::new()), |user| {
        (user.makes_keys, user.keys.clone())
    });
    users.retain(|user| !same(user));
    let user = User {
        name: name.trim().to_owned(),
        password: hash(password)?,
        role: grants.role(),
        cubes: grants.cubes().clone(),
        elements: grants.elements().clone(),
        makes_keys,
        keys,
    };
    users.push(user.kept());
    Ok(write_json(&path, &users)?)
}
