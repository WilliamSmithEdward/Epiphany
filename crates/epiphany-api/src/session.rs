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
    /// Absolute expiry: the session is invalid at/after this time regardless of
    /// activity (the hard cap, `now + ttl` at issue).
    expires_at: u64,
    /// Last time this token was seen on a request; an idle session (no activity
    /// for longer than the store's idle window) expires even before `expires_at`.
    last_seen: u64,
}

/// An in-memory token-to-session map with absolute-TTL and optional idle expiry.
pub struct SessionStore {
    sessions: HashMap<String, Session>,
    ttl_millis: u64,
    /// Idle-timeout window (ADR-0017). `None` disables idle expiry (the test seam
    /// and any deployment that opts out); the server sets it from configuration.
    idle_millis: Option<u64>,
}

impl std::fmt::Debug for SessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStore")
            .field("active", &self.sessions.len())
            .field("ttl_millis", &self.ttl_millis)
            .field("idle_millis", &self.idle_millis)
            .finish()
    }
}

impl SessionStore {
    /// A store whose sessions expire `ttl_millis` after they are issued, with no
    /// idle timeout. Add one with [`with_idle_timeout`](Self::with_idle_timeout).
    pub fn new(ttl_millis: u64) -> Self {
        Self {
            sessions: HashMap::new(),
            ttl_millis,
            idle_millis: None,
        }
    }

    /// Set the idle-timeout window (ADR-0017): a session with no activity for
    /// longer than `idle_millis` expires even before its absolute TTL. `None`
    /// disables idle expiry. Builder for the composition root.
    #[must_use]
    pub fn with_idle_timeout(mut self, idle_millis: Option<u64>) -> Self {
        self.idle_millis = idle_millis.filter(|&m| m > 0);
        self
    }

    /// Issue a fresh token for `principal` at time `now`. Each login mints a new
    /// random token (never a client-supplied one), so there is no session-fixation
    /// surface to rotate away (ADR-0017).
    pub fn create(&mut self, principal: Principal, now: u64) -> String {
        let token = new_token();
        self.sessions.insert(
            token.clone(),
            Session {
                principal,
                expires_at: now.saturating_add(self.ttl_millis),
                last_seen: now,
            },
        );
        token
    }

    /// Resolve a token to its principal at time `now`, pruning it if it has hit
    /// its absolute TTL or gone idle past the idle window. A live lookup slides
    /// the idle window forward (records the activity).
    pub fn lookup(&mut self, token: &str, now: u64) -> Option<Principal> {
        let expired = self.sessions.get(token).is_some_and(|s| {
            now >= s.expires_at
                || self
                    .idle_millis
                    .is_some_and(|idle| now.saturating_sub(s.last_seen) > idle)
        });
        if expired {
            self.sessions.remove(token);
            return None;
        }
        match self.sessions.get_mut(token) {
            Some(s) => {
                s.last_seen = now;
                Some(s.principal.clone())
            }
            None => None,
        }
    }

    /// Revoke a token (logout). A no-op if the token is unknown.
    pub fn revoke(&mut self, token: &str) {
        self.sessions.remove(token);
    }

    /// Revoke every session belonging to `username`. Returns how many were
    /// revoked.
    pub fn revoke_user(&mut self, username: &str) -> usize {
        let before = self.sessions.len();
        self.sessions
            .retain(|_, s| s.principal.username != username);
        before - self.sessions.len()
    }

    /// Revoke every session belonging to `username` EXCEPT `keep` (e.g. on a
    /// password change, ADR-0017): every token issued before the change stops
    /// working at once, while the caller's current session (which just proved the
    /// correct current password) stays alive so the change does not log them out.
    /// Returns how many were revoked.
    pub fn revoke_user_except(&mut self, username: &str, keep: &str) -> usize {
        let before = self.sessions.len();
        self.sessions
            .retain(|tok, s| s.principal.username != username || tok == keep);
        before - self.sessions.len()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn principal(name: &str) -> Principal {
        Principal {
            username: name.to_string(),
            is_admin: false,
            groups: Vec::new(),
        }
    }

    #[test]
    fn absolute_ttl_expires_regardless_of_activity() {
        let mut store = SessionStore::new(1000);
        let t = store.create(principal("ann"), 0);
        assert!(store.lookup(&t, 999).is_some());
        assert!(store.lookup(&t, 1000).is_none()); // hit the hard cap
    }

    #[test]
    fn idle_timeout_is_a_sliding_window() {
        let mut store = SessionStore::new(1_000_000).with_idle_timeout(Some(1000));
        let t = store.create(principal("ann"), 0);
        // Activity within the window keeps it alive and slides the window forward.
        assert!(store.lookup(&t, 500).is_some()); // last_seen -> 500
        assert!(store.lookup(&t, 1400).is_some()); // 900 since last seen -> ok, slides to 1400
                                                   // Now go idle past the window from the last activity.
        assert!(store.lookup(&t, 2500).is_none()); // 1100 > 1000 -> expired
    }

    #[test]
    fn no_idle_timeout_by_default() {
        let mut store = SessionStore::new(1_000_000);
        let t = store.create(principal("ann"), 0);
        // No activity for a long time, but no idle window configured: still valid.
        assert!(store.lookup(&t, 999_999).is_some());
    }

    #[test]
    fn revoke_user_drops_all_of_one_users_sessions() {
        let mut store = SessionStore::new(1_000_000);
        let a1 = store.create(principal("ann"), 0);
        let a2 = store.create(principal("ann"), 0);
        let b1 = store.create(principal("bob"), 0);
        assert_eq!(store.revoke_user("ann"), 2);
        assert!(store.lookup(&a1, 1).is_none());
        assert!(store.lookup(&a2, 1).is_none());
        assert!(store.lookup(&b1, 1).is_some()); // bob untouched
    }

    #[test]
    fn revoke_user_except_keeps_the_current_session() {
        let mut store = SessionStore::new(1_000_000);
        let current = store.create(principal("ann"), 0);
        let other = store.create(principal("ann"), 0);
        let bob = store.create(principal("bob"), 0);
        assert_eq!(store.revoke_user_except("ann", &current), 1);
        assert!(store.lookup(&current, 1).is_some()); // current kept
        assert!(store.lookup(&other, 1).is_none()); // other ann session revoked
        assert!(store.lookup(&bob, 1).is_some()); // bob untouched
    }
}
