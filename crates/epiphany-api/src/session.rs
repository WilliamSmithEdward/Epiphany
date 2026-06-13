//! Opaque server-side session tokens, kept in memory. A restart logs everyone
//! out, by design for M2 (the acceptance suite asserts the store starts empty).
//! Tokens are 32 bytes of OS entropy, base64url-encoded; expiry is keyed on the
//! injected clock so it is deterministic in tests.

use std::collections::HashMap;

use base64::Engine as _;
use epiphany_security::Principal;
use rand_core::{OsRng, RngCore};

struct Session {
    principal: Principal,
    expires_at: u64,
}

/// An in-memory token-to-session map with TTL expiry.
pub struct SessionStore {
    sessions: HashMap<String, Session>,
    ttl_millis: u64,
}

impl std::fmt::Debug for SessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStore")
            .field("active", &self.sessions.len())
            .field("ttl_millis", &self.ttl_millis)
            .finish()
    }
}

impl SessionStore {
    /// A store whose sessions expire `ttl_millis` after they are issued.
    pub fn new(ttl_millis: u64) -> Self {
        Self {
            sessions: HashMap::new(),
            ttl_millis,
        }
    }

    /// Issue a fresh token for `principal` at time `now`.
    pub fn create(&mut self, principal: Principal, now: u64) -> String {
        let token = new_token();
        self.sessions.insert(
            token.clone(),
            Session {
                principal,
                expires_at: now.saturating_add(self.ttl_millis),
            },
        );
        token
    }

    /// Resolve a token to its principal at time `now`, pruning it if expired.
    pub fn lookup(&mut self, token: &str, now: u64) -> Option<Principal> {
        if self
            .sessions
            .get(token)
            .is_some_and(|s| now >= s.expires_at)
        {
            self.sessions.remove(token);
            return None;
        }
        self.sessions.get(token).map(|s| s.principal.clone())
    }

    /// Revoke a token (logout). A no-op if the token is unknown.
    pub fn revoke(&mut self, token: &str) {
        self.sessions.remove(token);
    }

    /// Number of active sessions.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Whether there are no active sessions.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

fn new_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}
