# ADR-0017: Authentication and credential hardening

- **Status:** Accepted (realized in m8.3)
- **Date:** 2026-06-14
- **Deciders:** Epiphany maintainers
- **Phase:** 8 hardening extension (m8.3)

## Context

A post-roadmap security audit (a find-then-verify review across authentication,
transport, authorization, untrusted execution, and secrets) confirmed a set of
authentication and credential-handling gaps. This ADR locks the highest-value,
lowest-risk subset (the "Tier 1 auth bundle") and closes it without new heavy
dependencies:

1. **No login rate-limiting or lockout.** A known username can be guessed at
   without limit.
2. **`must_change_password` is surfaced but not enforced.** A first-run admin
   (or a user after an admin reset) can read and write data before rotating the
   credential.
3. **The generated first-run admin password is printed to stdout.** It lands in
   terminal scrollback and any log capture.
4. **The on-disk secret artifacts are world-readable.** `security.model` (password
   hashes), `audit.log`, and the run ledger are written with default permissions,
   so any local user can read them.

Four forces shape the decision:

- **Determinism ([ADR-0009](0009-determinism-strategy.md)).** Lockout decisions
  are time-based and observable (they gate login and emit audit records), so they
  must read the injected `Clock`, never the wall clock directly, and reproduce
  under `ManualClock`.
- **No new heavy dependencies.** The bundle uses std plus the existing
  argon2/axum/clock seams: no rate-limit middleware crate, no async timers.
- **Fail-closed.** On doubt the safe state is to deny (lock, or require a change)
  rather than allow.
- **No regression / least surprise.** Enforcement must not strand a legitimate
  user: a locked account recovers after a cooldown, and a must-change user can
  still log in, see who they are, change the password, and log out.
- **Cross-platform.** Tightening file permissions is a Unix mode-bit concept; on
  Windows it is a documented no-op and the data directory's inherited ACL is the
  operator's responsibility.

## Decision

**1. Per-username login lockout, clock-injected.** A new in-memory `LoginGuard`
(in `epiphany-api`, on `AppState` behind a `Mutex`, a sibling of `SessionStore`)
tracks consecutive failed logins per username with timestamps from the injected
clock. After `max_failures` consecutive failures (default 5) the username is
locked for a cooldown (default 15 minutes). The login handler consults the guard
**before** verifying the password (so a locked account never even runs Argon2,
removing a CPU and timing lever), returns **429 Too Many Requests** while locked,
records a failure on an authentication miss, and clears the counter on success.
The guard prunes expired entries on access, so its memory is bounded by the
number of currently-failing users. It is keyed by **username, not IP**: it needs
no connect-info plumbing, is deterministic, and defends the realistic threat
(guessing a known account's password). The known cost is that an attacker can
lock a known user for the cooldown window; the cooldown is short and auto-expires,
the lockout is audited, and per-IP throttling remains a documented follow-on (it
needs `ConnectInfo` plumbing and a trusted-proxy-header policy to be meaningful
behind a load balancer).

**2. `must_change_password` is enforced at the request boundary.** The flag is
surfaced on the `Principal` and re-resolved per request. The `AuthPrincipal`
extractor, after resolving the session, denies (**403**, "password change
required") every authenticated route except the minimal recovery set:
change-password, logout, and me. Enforcing in the one extractor that every gated
handler already uses closes the "new handler forgets the check" footgun, the same
single-enforcement-point principle as [ADR-0015](0015-object-and-element-security.md).
Re-resolving from the live store (not the session-captured flag) lifts the gate
immediately when the password is changed, with no re-login.

**3. The generated first-run admin password is delivered by a restricted file,
not stdout.** On first run the server writes the generated password to
`{data_dir}/server/admin-password.txt` with owner-only permissions and logs only
the path plus an instruction to read it once and delete it. The secret never
enters stdout or the structured log (RG-13). An operator who sets
`EPIPHANY_ADMIN_PASSWORD` supplies their own and no file is written.

**4. Secret artifacts are created owner-only.** The writers for `security.model`,
`audit.log`, the run ledger, and the admin-password file create the file with
`0600` **from creation** on Unix (via `OpenOptions::mode`), so it is never
group- or world-readable even momentarily between a write and a later chmod (a
time-of-check window). Because `mode` applies only at creation, a pre-existing
file (for example a temp left by an interrupted save) is also normalized with an
explicit `set_permissions(0600)`. On non-Unix the mode and chmod calls are no-ops
and the **data directory's inherited ACL governs**, so the operator must protect
the data directory (full-disk or directory-level encryption and ACLs); this is
called out in the deployment guidance.

Policy values are configurable: `EPIPHANY_LOGIN_MAX_FAILURES` (default 5) and
`EPIPHANY_LOGIN_LOCKOUT_SECS` (default 900; `0` disables the lockout).

## Alternatives considered

- **Per-IP rate limiting / a token bucket (e.g. tower-governor).** Deferred: it
  needs `ConnectInfo` wiring and a trusted-proxy-header policy to be meaningful
  behind a load balancer, and adds a dependency; per-username lockout closes the
  primary threat now and per-IP can layer on later.
- **Exponential backoff (delaying the response) instead of a hard lockout.**
  Rejected: holding an async task open to sleep ties up a worker and is a softer
  control; a clock-checked lockout is cleaner and deterministic.
- **Enforcing `must_change_password` per handler.** Rejected: it repeats the
  check and invites omissions; the extractor is the single choke point.
- **Capturing `must_change_password` in the session.** Rejected: it goes stale on
  change; re-resolving from the store is correct and cheap (the authorization
  path already locks the store per request).
- **Encrypting the secret artifacts at rest.** Out of scope here: it needs a
  key-management decision. OS-level disk encryption is the documented mitigation;
  permission tightening is the cheap in-process win.

## Consequences

- A new `LoginGuard` type and `AppState` field, a `must_change_password` gate in
  the `AuthPrincipal` extractor, an owner-only file-permission helper applied at
  every secret writer, a first-run password file, two config knobs, and audit
  emission on lockout.
- Determinism holds: lockout reads only the injected clock and is driven under
  `ManualClock` in tests.
- Fail-closed: on doubt the request is denied (locked, or must-change) rather
  than allowed.
- Recovery paths stay open: a locked user waits out the cooldown; a must-change
  user logs in, changes the password, and proceeds.
- Out of scope, each its own future ADR if pursued: per-IP throttling,
  password-strength policy, session idle-timeout and revoke-on-change and
  rotate-on-login, security response headers and TLS (the transport bundle), and
  at-rest encryption.
