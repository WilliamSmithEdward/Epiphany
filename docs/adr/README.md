# Architecture Decision Records

Significant decisions are recorded here (RG-16), one numbered file each. Use [`template.md`](template.md) for new ADRs. Supersede an earlier decision rather than silently contradicting it.

| ADR | Title | Status | Phase |
|---|---|---|---|
| [0001](0001-concurrency-model.md) | Concurrency model | Proposed | 0/1 |
| [0002](0002-runtime-persistence-format.md) | Runtime persistence format | Proposed | 0/1 |
| [0003](0003-model-as-code-serialization.md) | Model-as-code serialization format | Proposed | 1 |
| [0004](0004-embedded-javascript-engine.md) | Embedded JavaScript engine and TypeScript flows | Accepted | 5 |
| [0005](0005-automatic-feeder-inference.md) | Automatic feeder inference, validation, and under/over-feed detection | Accepted | 4 |
| [0006](0006-cell-storage-and-memory-layout.md) | Cell storage and memory layout | Accepted | 1 |
| [0007](0007-rule-evaluation-strategy.md) | Rule evaluation strategy (compiled rules, on-demand eval, memoization) | Accepted | 4 |
| [0008](0008-numeric-model-and-precision.md) | Numeric model and precision | Proposed | 1 |
| [0009](0009-determinism-strategy.md) | Determinism strategy | Accepted | 0 |
| [0010](0010-audit-logging.md) | Audit / user-action logging | Accepted | 7/8 |
| [0011](0011-mdx-seam-and-execution.md) | MDX evaluator seam, execute-time resolution, and zero-suppression | Accepted | 3 |
| [0012](0012-data-source-connectors.md) | Data-source connectors (command, HTTP) and the fetch/transform split | Accepted | 5+ |
| [0013](0013-flow-scheduling-and-orchestration.md) | Flow scheduling and orchestration | Accepted | 8 |
| [0014](0014-sandbox-overlay-representation.md) | Sandbox overlay representation | Accepted | 6 |
| [0015](0015-object-and-element-security.md) | Object and element security model | Element security accepted; object grants superseded by 0023 | 7 |
| [0016](0016-global-cube-grants-and-explicit-deny.md) | Global cube grants and explicit deny | Superseded by 0023 | 7 (m8.2) |
| [0017](0017-authentication-and-credential-hardening.md) | Authentication and credential hardening | Accepted | 8 (m8.3) |
| [0018](0018-http-surface-hardening.md) | HTTP-surface hardening (security headers, body-size limit) | Accepted | 8 (m8.5) |
| [0019](0019-optional-tls.md) | Optional TLS (HTTPS), simple to enable | Accepted | 8 (m8.6) |
| [0020](0020-web-ui-design-system-and-information-architecture.md) | Web UI design system and information architecture | Accepted | UI overhaul |
| [0021](0021-model-editing-api.md) | Model-editing API (create cubes, build dimensions, edit attributes) | Accepted | UI overhaul (W-API) |
| [0022](0022-excel-add-in.md) | Excel add-in (Excel-DNA + WebView2 configurator) | Accepted | Excel client |
| [0023](0023-modular-object-kind-permissions.md) | Modular per-object-kind permissions (roles for users and groups) | Accepted | Security model |
| [0024](0024-shared-independent-dimensions.md) | Shared, independent dimensions (a dimension registry) | Accepted | Model architecture |
| [0025](0025-at-rest-encryption-posture.md) | At-rest encryption posture (operator-managed, not in-binary) | Accepted | Security hardening |
| [0026](0026-web-syntax-highlighting.md) | Web syntax highlighting for the rules and flow editors (in-house) | Accepted | Web UI (W3) |
| [0027](0027-connection-preview-endpoint.md) | Connection preview endpoint (admin, gated, row-capped) | Accepted | Web UI (W4) |
| [0028](0028-view-cache-and-parallel-aggregation.md) | Persistent view cache and deterministic parallel aggregation | Accepted | Performance |
| [0029](0029-data-spreading.md) | Data spreading (equal, proportional, repeat, clear) | Accepted | Data entry |
| [0030](0030-http-connector-and-secret-store.md) | HTTP fetch connector and secret store | Accepted | Connectors |

> All originally-reserved ADR numbers are allocated; later decisions continue from 0020.
