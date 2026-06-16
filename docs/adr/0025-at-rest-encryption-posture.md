# ADR-0025: At-rest encryption posture (operator-managed, not in-binary)

- **Status:** Accepted
- **Date:** 2026-06-15
- **Deciders:** Epiphany maintainers
- **Phase:** Post-roadmap (security hardening, Tier-2/3 backlog item T2-7)

## Context

The security audit's Tier-2/3 backlog includes "encryption at rest". Epiphany
persists several artifacts under the data directory: the cube model snapshots and
write-ahead logs, the security store (`security.model`, password hashes only),
the audit log, and the run ledger. On Unix the secret-bearing files are created
owner-only (`0600`, ADR-0017); elsewhere the data directory's inherited ACL
governs. None of the on-disk bytes are encrypted.

The threat is an actor who can read the data directory directly, bypassing the
server's authentication and authorization: another local user on a shared host,
a backup or snapshot system, a cloud volume snapshot, a lost or stolen disk, or a
misconfigured file share. File permissions raise the bar against an unprivileged
local user but do nothing against a root user, a backup, or a raw disk image.

The question is whether to build a cipher into the binary (encrypt the artifacts
with an operator-supplied key) or to rely on operator-managed encryption of the
storage beneath the data directory.

## Decision

**Do not build encryption at rest into the binary. Document operator-managed
encryption of the storage layer as the recommended posture, in
[`docs/DEPLOYMENT.md`](../DEPLOYMENT.md).**

Rationale:

- **Single-binary, zero-new-deps is a core property** (RG-10). A built-in cipher
  means a crypto dependency, a key-management surface (where does the key live,
  how is it rotated, how is it entered at boot without prompting), and a new class
  of "lost the key, lost the data" failure. That is a large, error-prone surface
  for a property the platform already solves better.
- **The platform does this well and transparently.** Full-disk / filesystem /
  volume encryption (BitLocker, LUKS/dm-crypt, FileVault; an encrypted cloud
  volume) protects *every* artifact at rest, including temp files and swap, with
  no application involvement and mature key management. An app-level cipher would
  protect only the files it knows about and would still leak via temp files and
  OS swap unless those are also encrypted.
- **It composes with secrets managers.** Where an organization already runs a
  vault/secrets manager, the recommended pattern is to mount the data directory
  on an encrypted volume keyed by that system, rather than re-implementing key
  handling in Epiphany.

The retained file-permission hardening (`0600` owner-only secret files on Unix,
ADR-0017) stays as defense in depth against an unprivileged local reader.

## Alternatives considered

- **Built-in transparent encryption with an operator key.** Rejected: adds a
  crypto dependency and a key-management/rotation/recovery surface, breaks the
  single-binary/zero-deps property, and still does not cover temp files or OS
  swap. The storage layer covers all of that, better.
- **Encrypt only the security store / audit log.** Rejected: partial coverage
  (the cube model can itself be sensitive business data), same key-management
  cost, and an inconsistent posture that invites a false sense of safety.
- **Do nothing and say nothing.** Rejected: silence reads as "encrypted" to some
  operators. An explicit, documented threat model plus a recommended baseline is
  the honest and useful outcome.

## Consequences

- `docs/DEPLOYMENT.md` gains an "Encryption at rest" section: the threat model
  (who can read the data directory), the recommendation (encrypted filesystem /
  volume as the self-hosted baseline; encrypted cloud volume for cloud deploys;
  a vault-keyed encrypted mount where one exists), and the note that file
  permissions are defense in depth, not encryption.
- No code change, no new dependency, no change to the single-binary property.
- If a future requirement genuinely needs application-layer encryption (for
  example, per-tenant keys in a managed multi-tenant offering), it gets its own
  ADR; this decision records that the default, self-hosted posture is
  operator-managed storage encryption.
