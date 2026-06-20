# ADR-0037: Lowercase, filesystem-safe on-disk layout decoupled from display names

Status: Accepted
Date: 2026-06-19

## Context

Until now an object's on-disk folder name WAS its identity. A cube lives at
`<data_dir>/cubes/<cubeName>/`, and boot keyed each cube in the engine by
`path.file_name()` -- the directory name. The directory was created from the
cube's display name verbatim (e.g. a cube "Sales" lived in a folder literally
named `Sales`).

This couples two concerns that should be separate:

- the **display name** ("Sales"), which is what the API, UI, MDX, and every
  reference use; and
- the **on-disk folder name**, which is a filesystem path and must obey
  filesystem rules.

The coupling causes real, cross-platform problems:

- **Case sensitivity differs by platform.** Windows and macOS default to a
  case-insensitive filesystem; Linux (ext4/xfs) is case-sensitive. A model
  authored on Windows with cubes "Sales" and "sales" cannot be restored on
  Linux deterministically, and the same data dir behaves differently per OS.
- **Display names allow characters that are illegal or awkward in a path** --
  spaces, `/`, `:`, `*`, `?`, and others -- so a cube whose name contains them
  could not be stored at all, or stored inconsistently.
- **Identity-from-folder is fragile.** Any tool, backup, or sync that touched
  the folder casing would silently rename the cube.

The cube's true name is already stored inside its snapshot (`CubeDoc.name`,
serialized from `Cube::name()`), so the folder name is redundant as an identity
source -- we can read the real name from the loaded model instead.

The other server directories are already fixed or id-derived, not
display-name-derived: `<data_dir>/server/*` are fixed file names;
`<data_dir>/automation/automation.model` is a fixed name;
`<data_dir>/dimensions/` stores each shared dimension as `<numeric-id>.model`
plus a fixed `index.toml`. Only the per-cube folder is display-name-derived.

## Decision

**1. The on-disk folder is a lowercase, filesystem-safe slug, decoupled from the
display name.** A cube's display name is preserved everywhere in the API and UI;
only its folder becomes `slug(name)` (e.g. "Sales" -> `sales`). The display name
remains the single source of truth for identity and references.

**2. The engine reads identity from the snapshot, not the folder.** After
`Store::open(dir)`, boot keys the cube by `Store::cube_name()` (which returns
`self.model.cube().name()`), NOT by `path.file_name()`. A cube loads with its
real name regardless of the folder's casing or slugging. A new
`Store::cube_name()` accessor exposes this.

**3. `slug(name)`** (in `epiphany-persist`, exported and unit-tested) maps a
display name to a lowercase, filesystem-safe, non-empty folder name:

1. Lowercase every ASCII letter.
2. Replace every character not in `[a-z0-9-_]` (after lowercasing) with `-`.
3. Collapse any run of consecutive `-` into a single `-` (underscores are kept
   verbatim, not collapsed).
4. Trim leading and trailing `-`.
5. If the result is empty, fall back to a stable non-empty token (`unnamed`).

The result is always non-empty, lowercase, and drawn only from `[a-z0-9-_]`. The
function is idempotent on an already-slugged name.

**4. New cube folders use `slug(name)`.** Both the boot-time demo materialization
and the engine's runtime cube-create path (`Engine::create_cube`) create the
store at `cubes_dir.join(slug(name))`.

**5. Existing folders migrate by rename on boot.** Before opening cubes, boot
scans `<data_dir>/cubes/`; for each folder holding a `snapshot.model`, it reads
the cube's true name from the snapshot and, if the folder name differs from
`slug(name)`, renames the folder to the slug. The migration is resilient and
loss-proof (see Consequences / data safety).

**6. Cube names are unique case-insensitively.** Because two names that differ
only by case (or by slug-equivalent characters) would map to the same folder,
`Engine::create_cube` rejects a name that collides case-insensitively with an
existing cube, returning `AlreadyExists` with a clear message, in addition to the
existing exact-duplicate check.

## Alternatives considered

- **Keep folder = display name, escape unsafe characters only.** Rejected: does
  not solve the cross-platform case-sensitivity divergence, and percent-escaping
  produces ugly, non-obvious folder names while still coupling identity to the
  path.
- **Store a content-hash or numeric id as the folder name** (like the dimension
  registry's `<id>.model`). Rejected for cubes: an opaque id folder is far less
  operable -- operators and backups benefit from a human-readable `sales` folder
  -- and a slug gives that for free while still being filesystem-safe. The
  dimension registry keeps its id-based scheme (it is internal and already
  decoupled).
- **Allow case-distinct cube names and append a disambiguating suffix to the
  slug** (e.g. `sales`, `sales-2`). Rejected: it lets two cubes that look
  identical in a case-insensitive context coexist, which is confusing, and it
  reintroduces folder-derived identity (the suffix). Case-insensitive uniqueness
  is the simpler, clearer contract.

## Consequences

**Positive.**

- The on-disk layout is consistent and portable across Windows, macOS, and
  Linux; a data dir restored on a case-sensitive filesystem behaves identically.
- Folder names are always valid paths regardless of the display name's
  characters.
- Identity is robust: renaming or re-casing a folder out-of-band no longer
  silently renames the cube, because the name comes from the snapshot.

**Negative / trade-off (the case-insensitive-uniqueness consequence).** Two
cubes whose names differ only by case (e.g. "Sales" and "sales"), or that slug
to the same token, can no longer coexist -- they would share one folder.
`create_cube` rejects the second such name. This is a deliberate, documented
narrowing of the namespace; it matches the operator's mental model on the
majority (case-insensitive) platforms and removes a latent cross-platform
corruption.

**Data safety (paramount). The boot migration never deletes, merges, or
overwrites a user's data.** It only ever *renames* a cube folder onto a slug name
that does not yet exist. On any ambiguity or error it logs a warning and leaves
the existing folder exactly as-is, then continues (mirroring the resilient style
of `load_automation`'s migration):

- If the target slug folder already exists (a slug/case collision between two
  folders, or a half-finished prior migration), the rename is **skipped** so it
  can never clobber another cube's directory. Both folders are left intact for
  the operator to resolve.
- If a snapshot cannot be read or the rename fails (permissions, a Windows
  sharing violation, etc.), it is **logged and skipped**.
- It never panics, so a single problematic folder can never block boot.

**Scope.** Only the per-cube folder is migrated, because it is the only
display-name-derived path component. The `server/`, `automation/`, and
`dimensions/` directories and their contents are fixed names or numeric-id
files and are unaffected.

**Validation.** Unit tests cover `slug()` (spaces, uppercase, unsafe characters,
empty/all-unsafe fallback, idempotence, non-ASCII). Integration-style boot tests
cover: a cube named "Sales" persists under `sales` and reloads as "Sales"; an
existing `Sales` folder is migrated to `sales` on boot with the cube still named
"Sales"; a slug/case collision leaves both folders intact with no data loss; and
`create_cube` rejects a case-insensitive name collision without creating a folder.
