# ADR-0018: HTTP-surface hardening (security headers and body-size limit)

- **Status:** Accepted (realized in m8.5)
- **Date:** 2026-06-14
- **Deciders:** Epiphany maintainers
- **Phase:** 8 hardening extension (m8.5)

## Context

The security audit (the source of the [ADR-0017](0017-authentication-and-credential-hardening.md)
bundle) also found the HTTP surface lacked two routine defenses: no security
response headers (so a browser is free to MIME-sniff, frame the app, or leak a
full referrer) and no explicit request body-size cap making the bound an
intentional, documented decision. Both are cheap, dependency-free wins (axum and
its `http` types are already present), so they are grouped here as the first
slice of the broader hardening bundle.

Forces:

- **No new dependency.** Use axum's built-in `DefaultBodyLimit` and a
  `map_response` middleware over the `http` crate's standard header constants.
- **No UI regression.** The headers, especially the Content-Security-Policy, must
  not break the embedded single-page UI, which loads its own bundle and talks
  only to its own origin (REST and a same-origin WebSocket).
- **Tested = served.** Apply the layers in `build_router`, which both the server
  binary and every integration test use, so the behavior is exercised by tests.

## Decision

**1. Security response headers on every response.** A `map_response` layer in
`build_router` sets, on all responses:

- `X-Content-Type-Options: nosniff` (no MIME sniffing),
- `X-Frame-Options: DENY` (anti-clickjacking; `frame-ancestors 'none'` in the CSP
  is the modern equivalent and is set too),
- `Referrer-Policy: no-referrer`,
- `Strict-Transport-Security: max-age=31536000` (honored only over HTTPS;
  harmless over plain HTTP, and correct once TLS is fronted),
- `Content-Security-Policy: default-src 'self'; script-src 'self'; style-src
  'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-
  ancestors 'none'; base-uri 'self'; form-action 'self'`.

The CSP is same-origin only, which fits the bundled SPA: it loads its own script
bundle (`script-src 'self'`), injects styles at runtime (`style-src
'unsafe-inline'`), and reaches only this origin for data and the WebSocket
(`connect-src 'self'`). It is a constant so an operator fronting the app
differently can relax it in one place.

**2. An explicit request body-size limit.** `build_router` applies
`DefaultBodyLimit::max(8 MiB)`. Axum already defaults to a 2 MiB cap; making it
explicit documents the intent and raises it enough for legitimate batch writes
and flow imports while still bounding per-request memory. An over-limit request
is rejected with 413 before the handler runs.

## Alternatives considered

- **A request timeout layer in this slice.** Deferred: a global timeout would
  also cut off legitimately long operations (a synchronous flow run or import)
  and, because flow work runs on a blocking task, would return 408 to the client
  while the work continued server-side. It needs route-scoping and is handled in
  a later slice, not bundled with these zero-risk changes.
- **A stricter CSP without `style-src 'unsafe-inline'`.** Rejected for now: the
  SPA injects inline styles at runtime, so a nonce/hash scheme would be needed,
  which is more moving parts than this slice warrants. The policy is a constant
  and can be tightened later.
- **A configurable body limit via `AppState`.** Rejected for this slice: it would
  ripple through every test's `AppState` literal for little gain; a documented
  constant is enough, and configurability can be threaded later if needed.

## Consequences

- A `map_response` security-headers layer and a `DefaultBodyLimit` in
  `build_router`, exercised by `epiphany-api/tests/http_hardening.rs` (headers
  present on a response; an over-limit body is rejected with 413).
- No new dependency; no new `AppState` field; the layers are static.
- TLS itself remains out of scope (it needs a dependency or a fronting proxy) and
  is its own later decision; the HSTS header is correct in advance of it.
- Remaining hardening-bundle items (session idle-timeout and revocation,
  password-strength policy, a dummy-hash for the login timing channel, and parser
  and input-size bounds) are separate follow-on slices.
