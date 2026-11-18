//! Users, their passwords, and the sessions they sign in to.
//!
//! Users are kept in `users.json` in the server's directory, each with a salted Argon2 hash of
//! the password; the password itself is never stored. Signing in gives a random token, which
//! the client sends with every request. A session ends when it is signed out of or has not
//! been used for a while. Sessions live in memory, so they end when the server stops.

use std::collections::HashMap;
use std::error::Error;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use serde::{Deserialize, Serialize};

use crate::storage::{read_json, write_json};

const FILE: &str = "users.json";
/// How long a session lasts without being used, unless set otherwise.
const IDLE: Duration = Duration::from_secs(30 * 60);

#[derive(Serialize, Deserialize)]
struct User {
    name: String,
    /// The Argon2 hash of the password, with its salt and settings.
    password: String,
}

#[derive(Debug)]
pub struct Auth {
    /// Password hashes by user name in lower case.
    users: HashMap<String, String>,
    /// When each session was last used, by token.
    sessions: Mutex<HashMap<String, Instant>>,
    idle: Duration,
}

impl Auth {
    /// The users of a directory, or nothing if it has none, in which case the server asks
    /// for no sign-in.
    pub fn load(directory: &Path) -> Result<Option<Self>, Box<dyn Error>> {
        let users: Vec<User> = read_json(&directory.join(FILE))?.unwrap_or_default();
        let users: HashMap<String, String> = users
            .into_iter()
            .map(|user| (user.name.to_lowercase(), user.password))
            .collect();
        Ok((!users.is_empty()).then(|| Self {
            users,
            sessions: Mutex::default(),
            idle: IDLE,
        }))
    }

    /// Sets how long a session lasts without being used.
    pub fn set_idle(&mut self, idle: Duration) {
        self.idle = idle;
    }

    pub fn idle(&self) -> Duration {
        self.idle
    }

    /// Starts a session for a user who gives the right password, and returns its token.
    pub fn sign_in(&self, name: &str, password: &str) -> Option<String> {
        // An unknown name is checked against a hash too, so that it takes as long to refuse
        // as a wrong password and the two cannot be told apart by timing.
        static NOBODY: OnceLock<String> = OnceLock::new();
        let known = self.users.get(&name.to_lowercase());
        let hash =
            known.unwrap_or_else(|| NOBODY.get_or_init(|| hash("nobody").expect("hashing works")));
        let right = PasswordHash::new(hash).is_ok_and(|hash| {
            Argon2::default()
                .verify_password(password.as_bytes(), &hash)
                .is_ok()
        });
        if known.is_none() || !right {
            return None;
        }
        let mut random = [0u8; 32];
        getrandom::fill(&mut random).expect("the operating system provides randomness");
        let token: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let mut sessions = self.sessions.lock().unwrap();
        let now = Instant::now();
        sessions.retain(|_, used| now.duration_since(*used) <= self.idle);
        sessions.insert(token.clone(), now);
        Some(token)
    }

    /// Whether a token belongs to a live session, which then counts as used just now.
    pub fn admits(&self, token: &str) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let now = Instant::now();
        let live = sessions
            .get(token)
            .is_some_and(|used| now.duration_since(*used) <= self.idle);
        match live {
            true => sessions.insert(token.to_owned(), now),
            false => sessions.remove(token),
        };
        live
    }

    pub fn sign_out(&self, token: &str) {
        self.sessions.lock().unwrap().remove(token);
    }
}

fn hash(password: &str) -> Result<String, Box<dyn Error>> {
    let hash = Argon2::default().hash_password(password.as_bytes());
    Ok(hash.map_err(|error| error.to_string())?.to_string())
}

/// Adds a user to a directory, or changes the password of one it already has.
pub fn set_user(directory: &Path, name: &str, password: &str) -> Result<(), Box<dyn Error>> {
    if name.trim().is_empty() || password.is_empty() {
        return Err("a user needs a name and a password".into());
    }
    let path = directory.join(FILE);
    let mut users: Vec<User> = read_json(&path)?.unwrap_or_default();
    users.retain(|user| !user.name.eq_ignore_ascii_case(name.trim()));
    users.push(User {
        name: name.trim().to_owned(),
        password: hash(password)?,
    });
    Ok(write_json(&path, &users)?)
}
