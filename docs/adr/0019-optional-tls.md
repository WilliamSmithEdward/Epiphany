# ADR-0019: Optional TLS (HTTPS), simple to enable

- **Status:** Accepted (realized in m8.6)
- **Date:** 2026-06-15
- **Deciders:** Epiphany maintainers
- **Phase:** 8 hardening extension (m8.6)

## Context

The server speaks plain HTTP, bound to loopback by default. TLS was deferred
since Phase 2 and flagged by the security audit: any non-loopback deployment
sends session tokens and data in cleartext. The goal here is to add HTTPS that is
**optional** (the zero-config loopback HTTP experience must not change) and
**simple** (a single environment variable should get a working HTTPS endpoint,
with no certificate to obtain for local or internal use).

Forces:

- **No regression.** HTTP on loopback stays the default; nothing is required of
  an existing deployment.
- **Single static binary.** No system TLS library (OpenSSL) dependency: the
  crypto must be pure-Rust so the one-binary distribution holds on every target.
- **Permissive supply chain.** Adding crypto must stay within the project's
  permissive-only license policy, with any non-SPDX license (the crypto provider)
  cleared explicitly in `deny.toml`.
- **Lean core build.** The default `cargo build` and the determinism-focused test
  suite should not pull a crypto stack; TLS is a release-time feature.

## Decision

**1. Optional, off by default.** With no TLS configuration the server serves HTTP
exactly as before. TLS is purely opt-in.

**2. Two ways to enable, both simple; precedence is explicit.**

- **Self-signed (the easy path):** `EPIPHANY_TLS=on` makes the server generate a
  self-signed certificate (subject alternative names `localhost`, `127.0.0.1`,
  `::1`) into `{data_dir}/server/tls/` (owner-only) on first run and reuse it
  thereafter, then serve HTTPS. One variable, no certificate to obtain; browsers
  warn that it is self-signed, which is expected for local and internal use.
- **Bring-your-own (production):** `EPIPHANY_TLS_CERT` and `EPIPHANY_TLS_KEY`
  (paths to PEM files) serve HTTPS with the operator's real certificate. If both
  are set they take precedence over the self-signed path.

**3. Same bind address, scheme switches.** TLS serves on the existing
`EPIPHANY_BIND` (default `127.0.0.1:8080`); there is no second port and no
HTTP-to-HTTPS redirect. The startup log shows `https://` when TLS is on. This
keeps the surface to one knob.

**4. Pure-Rust implementation behind a `tls` cargo feature.** Serving uses
`axum-server` with `rustls` (the `ring` crypto provider); self-signed generation
uses `rcgen`. These are compiled only with the `tls` feature, which the release
binaries enable alongside `embed-ui`, so the default core build and CI gate stay
lean. A binary built without `tls` that is asked for TLS logs a warning and
serves HTTP rather than failing. Graceful shutdown is preserved via
`axum-server`'s handle.

**5. Supply chain.** `ring` ships a non-SPDX `LICENSE` (effectively ISC plus
permissive notices); it is cleared with a `deny.toml` clarification, consistent
with the permissive-only policy. The crypto stack is pure-Rust, so the single
static binary holds.

**6. Determinism and tests are unaffected.** TLS is transport-only and sits
outside the deterministic core; the integration tests drive `build_router`
directly with no transport, so they are unchanged. Configuration parsing and
self-signed certificate generation are unit-tested; a full TLS-handshake test is
out of scope (it would only re-test `rustls`).

## Alternatives considered

- **`native-tls` / OpenSSL.** Rejected: a system library dependency breaks the
  single static binary on some targets and complicates cross-compilation.
- **Always-compiled TLS (no feature gate).** Rejected: it would pull a crypto
  stack into every build and the determinism test suite; gating keeps the core
  lean while the shipped binaries still support TLS out of the box.
- **A dedicated HTTPS port and an HTTP-to-HTTPS redirect.** Rejected as
  unnecessary for "simple": one bind address and one scheme is easier to reason
  about; a redirector can be added later if asked.
- **Mandatory TLS.** Rejected: it would break the zero-config loopback default.

## Consequences

- New optional dependencies (`axum-server`, `rustls`/`ring`, `rcgen`) behind the
  `tls` feature, a `deny.toml` clarification for `ring`, and release packaging
  built with `embed-ui,tls`.
- `main.rs` chooses HTTP or HTTPS from configuration; a self-signed helper writes
  an owner-only cert and key; `Config` gains TLS fields (`EPIPHANY_TLS`,
  `EPIPHANY_TLS_CERT`, `EPIPHANY_TLS_KEY`).
- The HSTS header added in m8.5 becomes effective once TLS is on.
- A build without the `tls` feature ignores TLS configuration with a warning and
  serves HTTP.
