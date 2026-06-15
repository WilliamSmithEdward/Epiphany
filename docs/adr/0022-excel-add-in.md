# ADR-0022: Excel add-in (Excel-DNA + WebView2 configurator)

- **Status:** Accepted
- **Date:** 2026-06-15
- **Deciders:** Epiphany maintainers
- **Phase:** Post-roadmap (Excel client)

## Context

Planning and finance users live in Excel. They want to pull live cube values
into a worksheet, edit them, and write the changes back to the server, without
leaving Excel and without a second copy of the engine. The server already
exposes everything needed over the REST API (login, cube/cell read, transactional
batch write, sandboxes, security); the add-in must be a *thin client* over that
API, not a re-implementation of any engine logic. It must also be simple to set
up: a single file to install, and a friendly login/connection screen rather than
a config file of URLs and tokens.

Constraints and forces:

- Excel add-ins that can ship as one file and call managed code are best served
  by Excel-DNA, which packs a single `.xll`. The alternatives (VSTO, Office.js
  web add-ins) are heavier to deploy or sandboxed away from synchronous cell
  access.
- The login screen should reuse what already exists. The server serves a React
  login; embedding it in a WebView2 control avoids maintaining a second login UI
  and keeps auth logic on the server.
- Excel UDFs run on a calculation thread and must not block on network I/O, and
  a UDF may not write to cells other than its own.
- A bearer token is a secret; it must not sit in plaintext on disk or in the
  workbook.
- Excel is not installed in the build environment, so the add-in is built and
  statically verified here but load-tested by the user in Excel.

## Decision

Build a **.NET 8 (`net8.0-windows`) Excel-DNA add-in** in a new top-level
`excel-addin/` directory (a client, outside the Rust workspace), producing a
single packed `.xll`. It is a thin REST client only.

1. **Connection + login = a WebView2 configurator.** A ribbon "Connect" button
   opens a WinForms window hosting a `Microsoft.Web.WebView2` control pointed at
   the server's own login page (`<base>/`). The user signs in against the server
   as they would in a browser. On success the page hands the session token to the
   host via `window.chrome.webview.postMessage`; the host **validates the message
   origin** against the configured server base before accepting it. The base URL
   is the only thing the user types; it is remembered per user.

2. **Token at rest = DPAPI (CurrentUser).** The accepted token is encrypted with
   `ProtectedData.Protect(..., DataProtectionScope.CurrentUser)` and written to a
   per-user file under `%LOCALAPPDATA%\Epiphany`. It is never written to the
   workbook and never logged. Sign-out clears it. The token is also held in
   memory for the session.

3. **Reads = asynchronous `Task<object>` UDFs.** `EPIPHANY.READ(cube, dims...)`
   resolves a coordinate from name/value pairs and returns the value via an
   Excel-DNA async UDF, so the calc thread never blocks on the network. A small
   coalescing layer batches the cell reads issued in one recalc into one
   `cells/read` POST. Numeric values stay decimal strings end to end (ADR-0008)
   and are surfaced to Excel as numbers only at the boundary.

4. **Write-back = an explicit, range-based commit (not UDFs).** Because a UDF
   cannot write elsewhere, write-back is a deliberate action: the user selects a
   range whose cells were produced by `EPIPHANY.READ` (or a companion
   `EPIPHANY.POINT` marker), clicks "Commit", and the host reads the range,
   builds the coordinates, and POSTs one transactional `cells/batch`. Cell
   updates that the host pushes into the grid go through `ExcelAsyncUtil.QueueAsMacro`
   so they run on the main thread. The active sandbox (if any) is sent via the
   `X-Epiphany-Sandbox` header, mirroring the web client.

5. **One ribbon, spelled out.** Connect / Sign out, Refresh, Commit selection,
   and a sandbox indicator. Errors surface as plain-language dialogs, never raw
   stack traces.

## Alternatives considered

- **Office.js web add-in.** Cross-platform and store-distributable, but runs in a
  sandboxed iframe with async-only cell access and no single-file install; a
  poorer fit for a fast, synchronous-feeling grid and a desktop-first audience.
- **VSTO.** Full COM access but heavyweight deployment (ClickOnce/MSI),
  .NET-Framework-bound, and harder to ship as one file. Excel-DNA's single `.xll`
  is the simpler install the user asked for.
- **A second, native login UI in the add-in.** More work and a second place for
  auth bugs. Reusing the server's React login in WebView2 keeps one login.
- **Token in the workbook or a plaintext file.** Rejected: a token is a secret;
  DPAPI CurrentUser ties it to the OS user and keeps it off disk in the clear.
- **Synchronous reads.** Simpler code but blocks Excel's calc thread on network
  latency. Async `Task<object>` UDFs are the correct Excel-DNA pattern.

## Consequences

- Users install one `.xll`, click Connect, sign in on the familiar screen, and
  get `=EPIPHANY.READ(...)` plus a Commit button for write-back. No second engine,
  no token wrangling.
- The add-in depends only on the REST API; new server capabilities (sandboxes,
  security) apply automatically, and the add-in carries no model logic to drift.
- Because Excel is absent from the build environment, CI/static guarantees stop
  at "it builds and the REST contract matches"; **functional load-testing in Excel
  is the user's step**, documented in the add-in README. This boundary is called
  out so a green build is not mistaken for an in-Excel verification.
- Token handling is DPAPI-encrypted, origin-validated, and never persisted to the
  workbook, consistent with the project's no-secrets-at-rest stance (RG-13).
- Follow-ups (own ADRs if pursued): code-signing the `.xll` for distribution,
  an RTD push channel for live updates instead of manual Refresh, and richer
  range mapping (multi-dimension report templates).
