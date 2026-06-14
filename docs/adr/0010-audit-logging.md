# ADR-0010: Audit / user-action logging

- **Status:** Accepted
- **Date:** 2026-06-13 (proposed), 2026-06-14 (accepted for Phase 7)
- **Deciders:** Epiphany maintainers
- **Phase:** 7 (recording and query) / 8 (retention and rotation)

## Context
Audit / user-action logging records who did what: security-relevant actions and model or data changes, for compliance and after-the-fact investigation. It was deferred in ROADMAP section 13 and is now promoted into scope (RG-16 requires an ADR and a phase plan for any reintroduction).

Three forces shape the decision:
- **It is not durability.** The runtime transaction log (WAL, [ADR-0002](0002-runtime-persistence-format.md)) exists to recover live state after a crash. It is a fast-restart cache of cell and runtime state, is reconstructible from the model-as-code text ([ADR-0003](0003-model-as-code-serialization.md)), and is rotated and truncated against snapshots. An audit record answers a different question (which actor performed which action against which object) and must not be conflated with, or discarded alongside, WAL recovery.
- **It is coupled to identity and authorization.** An audit entry names an actor and a target object, so it only has meaning once users, groups, and object and element security exist (auth in Phase 2, object and element security in Phase 7). Phase 7 is therefore the home, and the record lives in `epiphany-security`.
- **Determinism and no-secrets.** Observability is first-class but carries a hard rule: no secrets in logs (RG-13). Audit records are observable output, so they must be deterministic in tests (injected clock, [ADR-0009](0009-determinism-strategy.md)) and must never contain credentials, tokens, or PII.

## Decision
A dedicated, **append-only audit stream** in `epiphany-security`, separate from the durability WAL.

1. **What is audited.** Security-relevant and model-changing actions: login and logout, permission grant and denial (including access-denied events), user and group changes, object create, update, and delete (cube, dimension, rule, flow, job, view, subset, security control object), flow and job execution, sandbox commit and discard, and the explicit full-persist command. Ordinary high-frequency reads and routine cell data entry are **not** audited, to keep the stream bounded and meaningful; cell writes are audited at the granularity of the operation, not per cell.
2. **Record shape.** Each entry carries an actor (user or group), an action, a target object reference, an outcome (allowed or denied), and an injected timestamp ([ADR-0009](0009-determinism-strategy.md)), plus a monotonic sequence number. No credentials, tokens, secrets, or PII (RG-13); values are referenced by object identity, not by copying sensitive payloads.
3. **Storage and relationship to the WAL.** The audit stream is its own append-only log, written and rotated independently of the WAL. It is **not** part of crash-recovery replay and never gates startup; a corrupt or truncated audit tail is detected and discarded (framed records as in ADR-0002) without blocking recovery of live state. It is distinct from durability and is not reconstructible from the model-as-code text, so it is treated as primary data, not a cache.
4. **Query surface.** Admins query and filter the audit log over REST (by actor, action, target, outcome, and time range) and through the Phase 7 security administration UI. Non-admins have no access.
5. **Retention and rotation.** A configurable retention window and size-bounded rotation, operationalized in Phase 8 alongside the other persistence and ops hardening work, with recovery testing of the audit log.
6. **Gating.** The feature is gated by the Phase 7 deterministic acceptance suite: audited actions produce correct, append-only, deterministic-timestamp records that survive a restart, contain no secrets or PII, and are queryable by an admin.

## Alternatives considered
- **Reuse the durability WAL as the audit trail.** Rejected: the WAL is a fast-restart cache that is rotated and truncated against snapshots and is reconstructible from the text model, so it cannot guarantee an actor-attributed, retention-controlled history. Conflating the two would either bloat recovery or silently drop audit history.
- **Emit audit events only into the structured tracing or metrics pipeline.** Rejected as the system of record: tracing is sampled and rotated for operations, not retained for compliance, and routing compliance data through general logs increases the risk of leaking secrets (RG-13). Tracing may still carry a non-authoritative breadcrumb.
- **An external audit or SIEM integration as the in-scope mechanism.** Rejected for now: signed or hash-chained off-box export and regulator-grade tamper-evidence stay deferred in section 13. The in-scope feature is a local, append-only, admin-queryable record; external export can layer on later without changing the recording API.

## Consequences
- A new append-only stream and an admin query surface in `epiphany-security`, plus an audit-log viewer in the Phase 7 web UI. A small per-action cost on the audited code paths, justified by the compliance and investigation value.
- The no-secrets and no-PII rule (RG-13) is enforced at the record boundary and checked by the Phase 7 acceptance suite; audit records are deterministic in tests via the injected clock (ADR-0009).
- Retention, rotation, and recovery testing of the audit stream are owned by Phase 8, keeping Phase 7 sized to the recording and query surface.
- The audit stream is primary data, not a cache: it is not reconstructible from the model-as-code text and must be considered in backup and operational planning separately from the WAL and snapshots.
