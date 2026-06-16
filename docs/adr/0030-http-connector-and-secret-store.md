# ADR-0030: HTTP fetch connector and secret store

- **Status:** Accepted
- **Date:** 2026-06-16
- **Deciders:** Maintainer
- **Phase:** Post-roadmap (the connector follow-on flagged in ADR-0012)

## Context

ADR-0012 shipped the `command` connector (run an admin-defined program, read its
stdout as CSV/JSON) and named an HTTP connector plus a credential store as the
planned follow-on. Many internal data sources are HTTP(S) endpoints returning
CSV or JSON, and reaching them today means wrapping `curl` in a command
connection. A first-class HTTP connector is more direct, and it needs a place to
keep API credentials that is NOT the model file (the model is Git-tracked,
human-readable text; secrets must never live there, RG-13).

Forces and constraints:

- **Determinism is unaffected.** Fetching is the impure edge, already separated
  from the pure deterministic transform (ADR-0012): a connector produces the same
  `Vec<Row>` the flow consumes, so the engine, sandbox, and flow tests are
  unchanged. Flow tests pin inline rows; a live fetch is a documented input
  boundary.
- **Secrets stay out of the model, logs, and audit (RG-13).** The connection
  references a secret by NAME; the value lives only in an owner-only store.
- **Fail-closed and SSRF-aware.** A server-side fetcher can be steered at
  internal hosts. The capability is off by default and constrained by an explicit
  host allowlist.
- **Single static binary, dependency-light, GNU-toolchain-friendly.** No
  native-tls, no second crypto backend.

## Decision

1. **`ConnectionSpec::Http(HttpSpec)`** (core), alongside `Command`. `HttpSpec`:
   `url`, static non-secret `headers: Vec<(String, String)>`, optional
   `auth: HttpAuth`, `format` (CSV/JSON), `json_path`, `timeout_ms`. v1 is GET
   only (no request body); POST and other methods are a documented follow-on.

2. **`HttpAuth { kind: Bearer | Basic, secret: String }`** where `secret` is the
   NAME of an entry in the secret store, never the value. For Bearer the secret
   value is the token; for Basic it is `user:password`. The model text serializes
   only the name, so a connection round-trips through Git with no credential in
   it.

3. **A secret store** (`epiphany-security::SecretStore`): a name to value map
   persisted to an owner-only (0600) file, reusing the same write helper as the
   security model and admin-password file (ADR-0017). Admin operations: set a
   secret, delete a secret, and list secret NAMES. The value is write-only over
   the API: it is never returned, never logged, and never written to the audit
   stream (the audit target is the secret name only). At-rest protection is the
   operator-managed posture of ADR-0025 (encrypt the volume); the store adds no
   in-binary cipher.

4. **The connector never sees the store.** `epiphany-connect` depends on core and
   flow, not security. The API layer (which holds the `SecretStore`) resolves the
   connection's `auth.secret` name to a value, builds the concrete `Authorization`
   header, and passes it to `epiphany_connect::fetch_http`. So the connector stays
   security-free and the secret never crosses into the model or the connector's
   own config. A missing named secret fails the fetch (fail-closed).

5. **Two fail-closed gates.**
   - **Capability:** `EPIPHANY_ENABLE_HTTP_CONNECTORS` (off by default), mirroring
     the command-connector gate. Defining or running an HTTP connection requires
     it on.
   - **Host allowlist:** `EPIPHANY_HTTP_ALLOWED_HOSTS` (comma-separated). An HTTP
     connection's URL host must match an allowlisted host or it is rejected. An
     empty allowlist allows nothing (fail-closed), so enabling the capability is
     not enough on its own; the operator must also name the hosts. This bounds
     server-side request forgery to hosts the operator chose.

6. **Bounded fetch.** Connect/read timeout from `timeout_ms`, a response-size cap
   (the command connector's 16 MiB), a small redirect cap, and the same row cap as
   CSV/JSON parsing. Non-2xx is an error.

7. **Feature-gated end to end.** A new `http` cargo feature
   (`epiphany-connect/http` -> `epiphany-server`/`epiphany-api` re-export) builds
   the fetcher; release binaries enable it. A build without the feature rejects an
   HTTP connection at runtime with a clear "not built" error, exactly like the TLS
   feature. The HTTP client is **ureq** with rustls pinned to the **ring**
   provider and bundled webpki roots (no native-tls, no aws-lc-rs), matching the
   crypto stack the server TLS feature already uses.

8. **REST.** Admin secret management: `PUT /api/v1/secrets/{name}` (set, returns
   204, value never echoed), `DELETE /api/v1/secrets/{name}`, `GET /api/v1/secrets`
   (names only). The connection endpoints accept the Http kind (validated: URL
   parses, host is allowlisted, capability on, referenced secret exists) and the
   preview/flow-run paths resolve the secret and fetch. The web Data-sources panel
   gains an HTTP connection form and a secrets admin section that sets and lists
   secret names (never showing a value).

## Alternatives considered

- **Hand-rolled HTTPS over the in-tree rustls/ring.** Zero new dependency, but
  re-implementing HTTP/1.1 (redirects, chunked transfer, content-length, gzip) is
  error-prone for no real gain. `ureq` with rustls/ring is small, pure-Rust,
  GNU-toolchain-friendly, and already verified to add no second crypto backend.
- **Store secrets in the model with encryption.** Rejected: keeps ciphertext in
  Git, needs in-binary key management, and conflicts with model-as-code review.
  A name reference plus an owner-only out-of-band store is simpler and safer.
- **No host allowlist (rely on the capability gate alone).** Rejected: enabling
  HTTP fetch would then permit SSRF to any reachable host, including cloud
  metadata endpoints. The allowlist is the load-bearing SSRF control.
- **`reqwest`/`native-tls`.** Rejected: heavy (tokio/hyper) or a system-TLS
  dependency, against the single-static-binary and dependency-light goals.

## Consequences

- Flows can ingest from HTTP(S) APIs directly, with credentials kept out of the
  model and Git. The capability is off by default and SSRF-bounded by an explicit
  allowlist, so turning it on is a deliberate, scoped operator decision.
- New surface: `HttpSpec`/`HttpAuth`/`ConnectionSpec::Http` (core) and their
  serialization; `SecretStore` (security); `fetch_http` (connect, `http` feature);
  secret-admin REST + Http connection handling + `AppState` fields and gates
  (api); config/env + store loading + the `http` feature (server); web forms.
- Determinism, the flow sandbox, and existing connectors are unchanged. The HTTP
  client adds `ureq` (rustls/ring, gzip) behind the `http` feature; no aws-lc-rs,
  no native-tls; cargo-deny stays green.
- Validation: core serialization round-trip (the secret name, not a value);
  secret-store unit tests (set/list-names-only/delete, owner-only file);
  `fetch_http` against a localhost test server (CSV and JSON, size cap, non-2xx);
  API tests for the gates, the host allowlist rejection, secret write-only
  behavior, and that no secret reaches the model or audit. POST/method support,
  per-request body, and OAuth flows are documented follow-ons.
