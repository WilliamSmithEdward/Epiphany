//! Per-username login lockout (ADR-0017): a deterministic, clock-injected guard
//! against password brute-forcing. After `max_failures` consecutive failed
//! logins a username is locked for `lockout_millis`; a success clears it. All
//! times come from the injected clock (the caller passes `now`), so the guard is
//! reproducible under `ManualClock` and reads no wall clock itself.

use std::collections::HashMap;

#[derive(Clone, Copy, Debug)]
struct Attempts {
    /// Consecutive failures in the current window.
    failures: u32,
    /// When the active lockout expires (injected-clock millis); 0 if not locked.
    locked_until: u64,
    /// Last activity, used to prune idle entries so memory stays bounded.
    last_seen: u64,
}

/// Tracks recent failed logins per username and enforces a temporary lockout.
#[derive(Debug)]
pub struct LoginGuard {
    attempts: HashMap<String, Attempts>,
    max_failures: u32,
    lockout_millis: u64,
}

impl LoginGuard {
    /// A guard that locks a username for `lockout_millis` after `max_failures`
    /// consecutive failures. The lockout is disabled when either is 0.
    pub fn new(max_failures: u32, lockout_millis: u64) -> Self {
        Self {
            attempts: HashMap::new(),
            max_failures,
            lockout_millis,
        }
    }

    fn disabled(&self) -> bool {
        self.max_failures == 0 || self.lockout_millis == 0
    }

    /// Whether `username` is currently locked out at `now`.
    pub fn is_locked(&self, username: &str, now: u64) -> bool {
        if self.disabled() {
            return false;
        }
        self.attempts
            .get(username)
            .is_some_and(|a| now < a.locked_until)
    }

    /// Record a failed login at `now`. Returns true if this failure locked (or
    /// keeps locked) the account. A served lockout starts a fresh window.
    pub fn record_failure(&mut self, username: &str, now: u64) -> bool {
        if self.disabled() {
            return false;
        }
        self.prune(now);
        let max = self.max_failures;
        let lockout = self.lockout_millis;
        let entry = self
            .attempts
            .entry(username.to_string())
            .or_insert(Attempts {
                failures: 0,
                locked_until: 0,
                last_seen: now,
            });
        // A lockout that has fully elapsed resets the counter, so a returning
        // user gets a fresh window of attempts rather than an instant re-lock.
        if entry.locked_until != 0 && now >= entry.locked_until {
            entry.failures = 0;
            entry.locked_until = 0;
        }
        entry.failures = entry.failures.saturating_add(1);
        entry.last_seen = now;
        if entry.failures >= max {
            entry.locked_until = now.saturating_add(lockout);
            true
        } else {
            false
        }
    }

    /// Clear a username's failure state after a successful login.
    pub fn record_success(&mut self, username: &str) {
        self.attempts.remove(username);
    }

    /// Drop entries that are neither locked nor recently active.
    fn prune(&mut self, now: u64) {
        let lockout = self.lockout_millis;
        self.attempts
            .retain(|_, a| now < a.locked_until || now < a.last_seen.saturating_add(lockout));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_after_max_failures_then_releases_after_cooldown() {
        let mut g = LoginGuard::new(3, 1_000);
        assert!(!g.is_locked("ann", 0));
        assert!(!g.record_failure("ann", 0));
        assert!(!g.record_failure("ann", 10));
        // The third consecutive failure locks the account.
        assert!(g.record_failure("ann", 20));
        assert!(g.is_locked("ann", 100));
        assert!(g.is_locked("ann", 1_019)); // still within the 1000ms cooldown
        assert!(!g.is_locked("ann", 1_020)); // cooldown elapsed (locked_until = 20+1000)
    }

    #[test]
    fn success_resets_the_counter() {
        let mut g = LoginGuard::new(3, 1_000);
        g.record_failure("ann", 0);
        g.record_failure("ann", 1);
        g.record_success("ann");
        // Two fresh failures do not lock (counter was reset by the success).
        assert!(!g.record_failure("ann", 2));
        assert!(!g.is_locked("ann", 3));
    }

    #[test]
    fn served_lockout_starts_a_fresh_window() {
        let mut g = LoginGuard::new(2, 1_000);
        g.record_failure("ann", 0);
        assert!(g.record_failure("ann", 1)); // locked until 1001
        assert!(!g.is_locked("ann", 1_001));
        // After the cooldown, one failure is just a fresh first failure, not a
        // re-lock.
        assert!(!g.record_failure("ann", 1_001));
        assert!(!g.is_locked("ann", 1_002));
    }

    #[test]
    fn zero_disables_the_lockout() {
        let mut g = LoginGuard::new(0, 1_000);
        for t in 0..100 {
            assert!(!g.record_failure("ann", t));
        }
        assert!(!g.is_locked("ann", 1_000));

        let mut g = LoginGuard::new(5, 0);
        assert!(!g.record_failure("ann", 0));
        assert!(!g.is_locked("ann", 0));
    }

    #[test]
    fn other_users_are_independent() {
        let mut g = LoginGuard::new(2, 1_000);
        g.record_failure("ann", 0);
        assert!(g.record_failure("ann", 1));
        assert!(g.is_locked("ann", 2));
        assert!(!g.is_locked("bob", 2));
    }
}
