//! Load existing cubes from the data directory, or materialize the bundled demo
//! model on first run, and assemble the engine.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use epiphany_determinism::IdGen;
use epiphany_engine::Engine;
use epiphany_persist::Store;

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
}
