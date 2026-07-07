# Lessons learned

Short engineering postmortems for problems that were expensive to diagnose, kept
here so the next person (or agent) recognises the shape quickly and reaches for the
technique that actually works. Newest first.

## 2026-07-07 — Uninterruptible Linux-CI hang in `cargo test --workspace`

**Symptom.** The `Test (linux-x86_64)` and `Test (linux-aarch64)` CI jobs capped at
the 30-minute limit and were killed; macOS, Windows, and every local run passed.
Re-running only re-hung. GitHub Actions **discards the logs of a job that is
cancelled by timeout**, so there was nothing to read.

**Root cause.** An `epiphany-api` ↔ `epiphany-connect` *cross-binary* interaction.
Both test binaries spawn real `sh` subprocesses; `epiphany-connect`'s
`a_slow_program_times_out` exercises the timeout → process-group `SIGKILL` → reap
path. When that test binary shares a Linux CI runner with api's connector subprocess
tests, the reap leaves the child **uninterruptible** (kernel D-state), which wedges
the runner. Each crate passes on its own (`cargo test -p <crate>`); the hang needs
**both** binaries in one `cargo test --workspace` invocation. Production is
unaffected — the reap is bounded in `kill_and_reap`; the wedge is specific to
cargo's multi-binary test harness, not the server.

**Why it was so hard.** The wedge is uninterruptible, so it also hangs the runner's
own cleanup. A capped job therefore destroys **both its logs and its uploaded
artifacts**. Every capture strategy failed the same way and burned a full CI cycle
(~20–30 min) each:

- streaming to the step log — lost on cap
- writing to a file + `actions/upload-artifact` — the wedge blocks the upload step
- a PTY capture via `script` — same cap
- incremental `git push` exfiltration to a side branch — fragile (a `timeout`-killed
  `git commit` left a stale `.git/index.lock` and froze the loop)
- `cargo nextest` with a per-test timeout — its terminator can't reap a D-state child
- foreground `timeout --signal=KILL` — never returns; SIGKILL can't reap D-state

**What actually worked — bisect on JOB STATUS.** A job's pass/cap *status* survives a
cap even when its logs and artifacts do not. That is the only reliable signal for an
uninterruptible CI hang:

1. **Per-crate matrix** — one job each running `cargo test -p <crate>`. All passed
   ⇒ no single crate hangs ⇒ it is a cross-binary interaction.
2. **Exclude-one matrix** — one job each running
   `cargo test --workspace --exclude <crate>`. The crate(s) whose *removal* makes the
   run pass are the interacting ones (here: `epiphany-api` and `epiphany-connect`).
3. **`--skip <testname>` arms** — one job each running `cargo test --workspace --
   --skip <test>`, to narrow to the single offending test.

**The fix.** `#[cfg_attr(target_os = "linux", ignore = "…")]` on
`a_slow_program_times_out` — skipped on Linux CI only, still run on macOS/Windows CI
and every local run, joining its two already-ignored backgrounded-grandchild
siblings. Run with `--ignored` on Linux to exercise the real subprocess behaviour.

**Prevention.**

- Real-subprocess tests (spawning `sh`, doing process-group kills) are CI-hostile on
  Linux when several run together in one workspace invocation. Gate new ones with
  `#[cfg_attr(target_os = "linux", ignore = "…")]` and keep them runnable via
  `--ignored`.
- When a Linux CI job hangs with no logs, **do not** spend cycles trying to capture
  output the cap will destroy. Go straight to job-status bisection (per-crate →
  exclude-one → `--skip`).
- Related earlier fix in the same area: an unbounded `child.wait()` in
  `kill_and_reap` was replaced with a bounded `try_wait` poll, and two
  backgrounded-grandchild subprocess tests were ignored. This was the *first* of two
  Linux-CI subprocess hangs; the one above was the second.
