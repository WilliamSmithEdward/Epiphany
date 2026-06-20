//! Load existing cubes from the data directory, or materialize the bundled demo
//! model on first run, and assemble the engine. Also opens the server-global
//! automation store (ADR-0035) and, on first boot under the new layout, lifts any
//! legacy per-cube flows/flow-tests/connections/jobs out of the cube models into
//! the global store.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use epiphany_core::{extract_legacy_automation, Automation};
use epiphany_determinism::IdGen;
use epiphany_engine::Engine;
use epiphany_persist::{slug, write_automation, AutomationStore, Store};

use crate::demo;

/// Open every cube under `<data_dir>/cubes/<slug>/`, or materialize the demo
/// model if there are none, and build the engine. Cube order is deterministic
/// (sorted by display name), so versions and listings are reproducible.
///
/// On-disk identity is decoupled from the display name (ADR-0037): a cube is
/// keyed in the engine by its loaded model's name (`Store::cube_name`), NOT by
/// its folder name, so it loads with its real name regardless of folder casing.
/// New cube folders are `slug(name)` (lowercase, filesystem-safe), and any
/// existing folder whose name differs from `slug(its real name)` is migrated by
/// rename on boot (see [`migrate_cube_dirs`]).
pub fn load_or_init(data_dir: &Path) -> Result<Engine, Box<dyn std::error::Error>> {
    let cubes_dir = data_dir.join("cubes");
    std::fs::create_dir_all(&cubes_dir)?;

    // Migrate legacy/display-name folders to lowercase slugs first (resilient:
    // never deletes or overwrites; logs and skips on any ambiguity), so the open
    // pass below reads from the canonical slug layout.
    migrate_cube_dirs(&cubes_dir);

    let mut dirs: Vec<_> = std::fs::read_dir(&cubes_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join("snapshot.model").is_file())
        .collect();
    dirs.sort();

    // Key each cube by its TRUE name from the loaded snapshot, not the folder
    // name. A folder named "sales" whose snapshot names the cube "Sales" loads as
    // "Sales".
    let mut stores = BTreeMap::new();
    for path in dirs {
        let store = Store::open(&path)?;
        stores.insert(store.cube_name().to_string(), store);
    }

    if stores.is_empty() {
        for (name, cube) in demo::demo_cubes() {
            let store = Store::create(cubes_dir.join(slug(&name)), cube)?;
            stores.insert(name, store);
        }
        tracing::info!("materialized the bundled demo model");
    }

    // Tell the engine where to create new cube stores so runtime cube creation
    // (ADR-0021) persists under the same layout it boots from, and where the
    // shared-dimension registry lives so dimension-library mutations are durable
    // and referencing cubes reconcile forward on restart (ADR-0024).
    Ok(Engine::from_stores(stores, Arc::new(IdGen::default()))
        .with_cubes_dir(cubes_dir)
        .with_dimensions_dir(data_dir.join("dimensions")))
}

/// Migrate each existing cube folder to `slug(its real name)` (ADR-0037).
///
/// For every directory under `cubes_dir` that holds a `snapshot.model`, the
/// cube's TRUE name is read from the snapshot and slugged. If the folder name
/// already equals that slug, nothing happens. Otherwise the folder is renamed to
/// the slug so the on-disk layout is lowercase and filesystem-safe.
///
/// DATA SAFETY (paramount): this never deletes, merges, or overwrites a user's
/// data. On ANY ambiguity or error it logs a warning and leaves the existing
/// folder exactly as-is, then continues:
/// - if the target slug folder already exists AND is a DIFFERENT directory from
///   the source (a real slug/case collision between two distinct cubes, or a
///   half-finished prior migration), it skips the rename so it can never clobber
///   another cube's directory;
/// - if the snapshot cannot be read or the rename fails (permissions, a Windows
///   sharing violation, etc.), it logs and skips;
/// - it never panics, so a single bad folder can never block boot.
///
/// Case-only renames on a case-insensitive filesystem (Windows/macOS): a folder
/// `Sales` whose slug is `sales` resolves to the SAME directory, so a naive
/// "target exists -> skip" check would never migrate it. We detect the
/// same-directory case via canonicalization and perform the rename through a
/// temporary name (a two-step rename), which is safe on every platform and on a
/// case-sensitive FS too.
///
/// Mirrors the resilient style of [`load_automation`]'s migration: best-effort,
/// loss-proof, boot always proceeds.
fn migrate_cube_dirs(cubes_dir: &Path) {
    let entries = match std::fs::read_dir(cubes_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "could not scan cubes dir for slug migration; skipping");
            return;
        }
    };

    // Collect first so the rename does not perturb the directory iterator.
    let dirs: Vec<std::path::PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join("snapshot.model").is_file())
        .collect();

    for path in dirs {
        let folder = match path.file_name().and_then(|n| n.to_str()) {
            Some(f) => f,
            None => {
                tracing::warn!(path = %path.display(), "cube folder name is not valid UTF-8; leaving as-is");
                continue;
            }
        };

        // Read the cube's true name from its snapshot (no WAL replay needed: the
        // name lives in the snapshot text). Resilient: an unreadable snapshot
        // just means we cannot compute the target, so leave the folder alone.
        let store = match Store::open(&path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(folder = %folder, error = %e, "could not open a cube to compute its slug; leaving the folder as-is");
                continue;
            }
        };
        let target = slug(store.cube_name());
        drop(store);

        if folder == target {
            continue; // already at its canonical slug
        }

        let target_path = path.with_file_name(&target);
        if target_path.exists() {
            // The target name exists. Distinguish a REAL collision (a different
            // cube's directory) from a case-fold self-match (e.g. `Sales` and
            // `sales` are the same dir on a case-insensitive FS). Compare the
            // canonical paths: if they resolve to the same place it is a case-only
            // rename of THIS folder, which is a legitimate migration.
            let same_dir = match (path.canonicalize(), target_path.canonicalize()) {
                (Ok(a), Ok(b)) => a == b,
                // If we cannot canonicalize, treat it as a distinct dir and skip,
                // erring on the side of never clobbering.
                _ => false,
            };
            if !same_dir {
                tracing::warn!(
                    from = %folder,
                    to = %target,
                    "target slug folder already exists as a different cube; leaving the folder as-is to avoid data loss (resolve the name collision manually)"
                );
                continue;
            }
            // Case-only rename on a case-insensitive FS: go through a temp name so
            // the casing actually changes on disk (a direct same-dir rename can be
            // a no-op there).
            let tmp = path.with_file_name(format!(".migrating-{target}"));
            if tmp.exists() {
                tracing::warn!(from = %folder, to = %target, "a stale migration temp dir is in the way; leaving the folder as-is");
                continue;
            }
            match std::fs::rename(&path, &tmp).and_then(|()| std::fs::rename(&tmp, &target_path)) {
                Ok(()) => {
                    tracing::info!(from = %folder, to = %target, "migrated a cube folder to its lowercase slug (case-only rename, ADR-0037)")
                }
                Err(e) => {
                    // If the first rename succeeded but the second failed, the data
                    // is safe under the temp name; log loudly so the operator can
                    // recover it. We never delete.
                    tracing::warn!(from = %folder, to = %target, error = %e, "could not complete a case-only slug rename; data is intact (possibly under a '.migrating-*' temp folder)");
                }
            }
            continue;
        }

        match std::fs::rename(&path, &target_path) {
            Ok(()) => {
                tracing::info!(from = %folder, to = %target, "migrated a cube folder to its lowercase slug (ADR-0037)")
            }
            Err(e) => {
                tracing::warn!(from = %folder, to = %target, error = %e, "could not rename a cube folder to its slug; leaving it as-is")
            }
        }
    }
}

/// Open the server-global automation store (ADR-0035) at `<data_dir>/automation`,
/// migrating any legacy per-cube automation on first boot under the new layout.
///
/// Migration (ADR-0035 decision 8): for each cube, the cube's `snapshot.model`
/// text is scanned for legacy `[[flow]]`/`[[flow_test]]`/`[[connection]]`/`[[job]]`
/// blocks (the cube model parser already ignores them). Each is lifted into the
/// global model; on a name collision the cube-scoped name is prefixed with
/// `"{cube}_"` and a warning is logged. A migrated flow gets `default_cube =
/// Some(origin_cube)` if it had none, and `owner = first_admin` if it had none, so
/// its body keeps working and scheduled runs have a real run-as principal. After
/// merging, the global file is written and each migrated cube is re-checkpointed
/// (dropping the legacy blocks from its snapshot). Resilient: a migration error is
/// logged and boot continues with whatever merged cleanly (an empty store at
/// worst), never crashing.
pub fn load_automation(
    data_dir: &Path,
    engine: &Engine,
    first_admin: Option<&str>,
) -> Result<AutomationStore, Box<dyn std::error::Error>> {
    let automation_dir = data_dir.join("automation");
    let cubes_dir = data_dir.join("cubes");

    // If the global file already exists, the migration ran on a prior boot; just
    // open it (no re-scan of cube snapshots).
    if automation_dir.join("automation.model").is_file() {
        return Ok(AutomationStore::open(automation_dir)?);
    }

    // First boot under the new layout: scan every cube for legacy automation.
    let mut merged = Automation::new();
    let mut migrated_cubes: Vec<String> = Vec::new();
    for cube in engine.cube_names() {
        // The folder is the slug of the display name (ADR-0037), not the name.
        let snapshot_path = cubes_dir.join(slug(&cube)).join("snapshot.model");
        let text = match std::fs::read_to_string(&snapshot_path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let legacy = match extract_legacy_automation(&text) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(cube = %cube, error = %e, "could not read legacy automation; skipping");
                continue;
            }
        };
        if legacy.flows.is_empty()
            && legacy.flow_tests.is_empty()
            && legacy.connections.is_empty()
            && legacy.jobs.is_empty()
        {
            continue;
        }
        merge_legacy(&mut merged, legacy, &cube, first_admin);
        migrated_cubes.push(cube);
    }

    if migrated_cubes.is_empty() {
        // Nothing to lift; just open (creates) an empty global store.
        return Ok(AutomationStore::open(automation_dir)?);
    }

    // Persist the merged global model, then re-checkpoint each migrated cube so the
    // legacy blocks are dropped from its snapshot. A persist failure logs and falls
    // back to an empty store rather than blocking boot.
    if let Err(e) = write_automation(&automation_dir, &merged) {
        tracing::error!(error = %e, "failed to write the migrated automation; starting empty");
        return Ok(AutomationStore::open(automation_dir)?);
    }
    for cube in &migrated_cubes {
        if let Err(e) = engine.checkpoint(cube) {
            tracing::warn!(cube = %cube, error = ?e, "could not re-checkpoint a migrated cube; its snapshot still carries legacy automation blocks (harmless, ignored on load)");
        }
    }
    tracing::info!(
        cubes = migrated_cubes.len(),
        flows = merged.flows.len(),
        connections = merged.connections.len(),
        jobs = merged.jobs.len(),
        "migrated legacy per-cube automation into the global store (ADR-0035)"
    );

    Ok(AutomationStore::open(automation_dir)?)
}

/// Merge one cube's legacy automation into the global model, prefixing on a name
/// collision and stamping a migrated flow's `default_cube`/`owner` defaults.
fn merge_legacy(
    merged: &mut Automation,
    legacy: Automation,
    cube: &str,
    first_admin: Option<&str>,
) {
    // Lift flows first, recording how each was renamed by a cross-cube collision
    // so this cube's jobs and flow tests (which reference flows by name) can be
    // remapped to the new flow names, never dangling or binding to another cube's
    // same-named flow. Legacy flows predate `inputs`, so a renamed connection can
    // never strand a flow input here.
    let mut flow_rename: BTreeMap<String, String> = BTreeMap::new();
    for (name, mut flow) in legacy.flows {
        if flow.default_cube.is_none() {
            flow.default_cube = Some(cube.to_string());
        }
        if flow.owner.is_none() {
            flow.owner = first_admin.map(str::to_string);
        }
        let key = unique_key(&merged.flows, &name, cube);
        if key != name {
            flow_rename.insert(name.clone(), key.clone());
        }
        flow.name = key.clone();
        merged.flows.insert(key, flow);
    }
    for (name, mut test) in legacy.flow_tests {
        if let Some(renamed) = flow_rename.get(&test.flow) {
            test.flow = renamed.clone();
        }
        let key = unique_key(&merged.flow_tests, &name, cube);
        test.name = key.clone();
        merged.flow_tests.insert(key, test);
    }
    for (name, mut conn) in legacy.connections {
        let key = unique_key(&merged.connections, &name, cube);
        conn.name = key.clone();
        merged.connections.insert(key, conn);
    }
    for (name, mut job) in legacy.jobs {
        for step in &mut job.steps {
            if let Some(renamed) = flow_rename.get(step) {
                *step = renamed.clone();
            }
        }
        let key = unique_key(&merged.jobs, &name, cube);
        job.name = key.clone();
        merged.jobs.insert(key, job);
    }
}

/// The key to store a migrated object under: its original name, or `"{cube}_{name}"`
/// when that name already exists (a cross-cube collision), logging a warning.
fn unique_key<V>(map: &BTreeMap<String, V>, name: &str, cube: &str) -> String {
    if map.contains_key(name) {
        let prefixed = format!("{cube}_{name}");
        tracing::warn!(
            name = %name,
            cube = %cube,
            renamed = %prefixed,
            "legacy automation name collides across cubes; prefixed with the cube name"
        );
        prefixed
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique scratch data dir for one boot test (cleaned up by the test).
    fn boot_scratch(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("epiphany-server-boot-{}-{tag}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        dir
    }

    /// The exact, case-preserving names of the immediate sub-directories of
    /// `dir`. Needed because `Path::exists` is case-INSENSITIVE on Windows/macOS,
    /// so it cannot tell `Sales` from `sales`; the real entry name can.
    fn child_dir_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .filter(|e| e.path().is_dir())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn materializes_demo_then_reopens_without_rebuilding() {
        let dir = boot_scratch("materialize");

        let first = load_or_init(&dir).unwrap();
        assert!(first.has_cube("Sales"));
        let names = first.cube_names();
        drop(first);

        // The demo cube "Sales" persists under the lowercase slug "sales"
        // (ADR-0037), not under the display name. Check the real entry name
        // (case-preserving), since `exists()` is case-insensitive on Windows.
        let cubes_dir = dir.join("cubes");
        assert!(
            cubes_dir.join("sales").join("snapshot.model").is_file(),
            "demo cube persists under its slug 'sales'"
        );
        assert_eq!(
            child_dir_names(&cubes_dir),
            vec!["sales".to_string()],
            "the on-disk folder is lowercase 'sales', not 'Sales'"
        );

        // Reopening finds the persisted cube and does not re-materialize, still
        // keyed by the real display name.
        let second = load_or_init(&dir).unwrap();
        assert_eq!(second.cube_names(), names);
        assert!(second.has_cube("Sales"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn create_persists_under_slug_and_reloads_with_display_name() {
        // A cube named "Sales" lives in folder "sales" yet loads as "Sales".
        let dir = boot_scratch("slug-roundtrip");
        let cubes_dir = dir.join("cubes");
        std::fs::create_dir_all(&cubes_dir).unwrap();

        let region = {
            let mut d = epiphany_core::Dimension::new("Region");
            d.add_leaf("R0");
            d
        };
        let cube = epiphany_core::Cube::new("Sales", vec![region]).unwrap();
        Store::create(cubes_dir.join(slug("Sales")), cube).unwrap();
        assert!(cubes_dir.join("sales").join("snapshot.model").is_file());

        let engine = load_or_init(&dir).unwrap();
        assert!(engine.has_cube("Sales"), "loads with its real display name");
        assert!(!engine.has_cube("sales"), "not keyed by the folder slug");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn boot_migrates_a_display_name_folder_to_its_slug_preserving_the_name() {
        // Simulate a pre-ADR-0037 data dir: the folder is the display name
        // "Sales" and the snapshot names the cube "Sales".
        let dir = boot_scratch("migrate");
        let cubes_dir = dir.join("cubes");
        std::fs::create_dir_all(&cubes_dir).unwrap();

        let region = {
            let mut d = epiphany_core::Dimension::new("Region");
            d.add_leaf("R0");
            d
        };
        let cube = epiphany_core::Cube::new("Sales", vec![region]).unwrap();
        Store::create(cubes_dir.join("Sales"), cube).unwrap();
        assert!(cubes_dir.join("Sales").join("snapshot.model").is_file());

        // Boot migrates the folder to the slug; no data is lost and the cube
        // still loads as "Sales".
        let engine = load_or_init(&dir).unwrap();
        assert!(engine.has_cube("Sales"));
        assert!(
            cubes_dir.join("sales").join("snapshot.model").is_file(),
            "folder was renamed to the slug 'sales'"
        );
        // The real (case-preserving) entry is now exactly "sales": the old
        // "Sales" folder was renamed, not copied, and no data was lost.
        assert_eq!(
            child_dir_names(&cubes_dir),
            vec!["sales".to_string()],
            "the display-name folder was migrated to its lowercase slug"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn boot_migration_skips_a_slug_collision_without_data_loss() {
        // Two DISTINCT folders that slug to the same name: "my-cube" (already
        // canonical) and "My Cube!" (a leftover display-name folder that also
        // slugs to "my-cube"). These are genuinely different directories on every
        // platform (the names differ beyond case), so both can hold their own
        // snapshot. The migration must NOT rename "My Cube!" onto "my-cube" (that
        // would overwrite/lose data); it leaves both folders intact and logs.
        let dir = boot_scratch("collision");
        let cubes_dir = dir.join("cubes");
        std::fs::create_dir_all(&cubes_dir).unwrap();

        let make_cube = |name: &str| {
            let mut d = epiphany_core::Dimension::new("Region");
            d.add_leaf("R0");
            epiphany_core::Cube::new(name, vec![d]).unwrap()
        };
        // Canonical slug folder, cube named "My Cube".
        Store::create(cubes_dir.join("my-cube"), make_cube("My Cube")).unwrap();
        // A distinct folder "My Cube!" whose cube also slugs to "my-cube".
        Store::create(cubes_dir.join("My Cube!"), make_cube("My Cube")).unwrap();
        assert_ne!(slug("My Cube"), "My Cube!");
        assert_eq!(slug("My Cube"), "my-cube");

        // Migration runs during load and must be loss-proof.
        let _engine = load_or_init(&dir).unwrap();

        // BOTH folders still exist with their snapshots intact: nothing deleted,
        // nothing overwritten.
        assert!(
            cubes_dir.join("my-cube").join("snapshot.model").is_file(),
            "the canonical folder is untouched"
        );
        assert!(
            cubes_dir.join("My Cube!").join("snapshot.model").is_file(),
            "the colliding folder is left as-is (not renamed onto 'my-cube'), so no data is lost"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn migration_remaps_references_when_a_colliding_flow_is_renamed() {
        use epiphany_core::{Flow, Job, Trigger};

        fn flow(name: &str) -> Flow {
            Flow {
                name: name.to_string(),
                source: "function rows(ctx) {}".to_string(),
                owner: None,
                default_cube: None,
                inputs: Vec::new(),
            }
        }

        // Cube A already lifted a flow "load".
        let mut merged = Automation::new();
        merged.flows.insert("load".to_string(), flow("load"));

        // Cube B also has a flow "load", plus a job and a test that reference it
        // by name (the legacy same-cube convention).
        let mut legacy = Automation::new();
        legacy.flows.insert("load".to_string(), flow("load"));
        legacy.jobs.insert(
            "nightly".to_string(),
            Job {
                name: "nightly".to_string(),
                steps: vec!["load".to_string()],
                trigger: Trigger::Interval { every_millis: 1000 },
                enabled: true,
            },
        );
        legacy.flow_tests.insert(
            "t".to_string(),
            epiphany_core::FlowTest {
                name: "t".to_string(),
                flow: "load".to_string(),
                ..Default::default()
            },
        );

        merge_legacy(&mut merged, legacy, "B", Some("admin"));

        // Both flows survive under distinct names.
        assert!(merged.flows.contains_key("load"));
        assert!(merged.flows.contains_key("B_load"));
        // B's job step and flow test follow the rename (not cube A's "load").
        assert_eq!(merged.jobs["nightly"].steps, vec!["B_load".to_string()]);
        assert_eq!(merged.flow_tests["t"].flow, "B_load");
        // The migrated flow got its default cube and owner stamped.
        assert_eq!(merged.flows["B_load"].default_cube.as_deref(), Some("B"));
        assert_eq!(merged.flows["B_load"].owner.as_deref(), Some("admin"));
    }
}
