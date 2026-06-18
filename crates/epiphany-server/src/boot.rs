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
use epiphany_persist::{write_automation, AutomationStore, Store};

use crate::demo;

/// Open every cube under `<data_dir>/cubes/<name>/`, or materialize the demo
/// model if there are none, and build the engine. Cube order is deterministic
/// (sorted), so versions and listings are reproducible.
pub fn load_or_init(data_dir: &Path) -> Result<Engine, Box<dyn std::error::Error>> {
    let cubes_dir = data_dir.join("cubes");
    std::fs::create_dir_all(&cubes_dir)?;

    let mut dirs: Vec<_> = std::fs::read_dir(&cubes_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join("snapshot.model").is_file())
        .collect();
    dirs.sort();

    let mut stores = BTreeMap::new();
    for path in dirs {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            stores.insert(name.to_string(), Store::open(&path)?);
        }
    }

    if stores.is_empty() {
        for (name, cube) in demo::demo_cubes() {
            let store = Store::create(cubes_dir.join(&name), cube)?;
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
        let snapshot_path = cubes_dir.join(&cube).join("snapshot.model");
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

    #[test]
    fn materializes_demo_then_reopens_without_rebuilding() {
        let dir = std::env::temp_dir().join(format!("epiphany-server-boot-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();

        let first = load_or_init(&dir).unwrap();
        assert!(first.has_cube("Sales"));
        let names = first.cube_names();
        drop(first);

        // Reopening finds the persisted cube and does not re-materialize.
        let second = load_or_init(&dir).unwrap();
        assert_eq!(second.cube_names(), names);

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
