//! Durable persistence for the shared-dimension registry (ADR-0024, SD-2).
//!
//! Each shared dimension is stored as one canonical model-as-code file
//! `<dir>/<id>.model` (via [`Dimension::to_model_text`]); a sibling `index.toml`
//! records, per dimension, its id, generation, and the cubes that reference it.
//! The index is written last and is the authority on load, so a crash between
//! writing dimension bodies and the index leaves a consistent (older) registry.

use std::path::Path;

use epiphany_core::{Dimension, SaveError};
use serde::{Deserialize, Serialize};

use crate::store::PersistError;

/// One registry entry: a shared dimension plus its id, generation, and the cubes
/// that reference it.
#[derive(Debug)]
pub struct RegistryEntry {
    pub id: u64,
    pub generation: u64,
    pub references: Vec<String>,
    pub dimension: Dimension,
}

#[derive(Serialize, Deserialize, Default)]
struct IndexDoc {
    #[serde(default, rename = "dimension")]
    dimensions: Vec<IndexEntryDoc>,
}

#[derive(Serialize, Deserialize)]
struct IndexEntryDoc {
    id: u64,
    generation: u64,
    #[serde(default)]
    references: Vec<String>,
}

const INDEX_FILE: &str = "index.toml";

fn dim_path(dir: &Path, id: u64) -> std::path::PathBuf {
    dir.join(format!("{id}.model"))
}

/// Write the registry to `dir`: one `<id>.model` per dimension, then `index.toml`.
///
/// Ordering is durability-critical. The bodies are written and fsynced, then the
/// new `index.toml` is written and fsynced (it is the authority on load), and only
/// THEN are bodies for ids no longer present swept. That order means a crash can
/// never leave the old index pointing at a body this call removed: the deletion
/// happens strictly after the new index is durable, so recovery always sees an
/// index whose every listed body still exists.
///
/// A swept body is *quarantined* (renamed to `<id>.model.orphan`), not deleted.
/// The active set is defined solely by the durable `index.toml`, so a quarantined
/// body is invisible to [`load_registry`] and a "deleted dimension does not
/// linger" exactly as before - but an entry set that is empty by mistake (e.g. a
/// caller that substituted an empty registry after a failed load) cannot cause
/// permanent, unrecoverable loss of the dimension library: the bodies remain on
/// disk under their `.orphan` name and can be restored.
pub fn save_registry(dir: &Path, entries: &[RegistryEntry]) -> Result<(), PersistError> {
    std::fs::create_dir_all(dir)?;
    // 1. Write and fsync each dimension body before it is referenced by the index.
    for entry in entries {
        let text = entry
            .dimension
            .to_model_text()
            .map_err(PersistError::Save)?;
        let path = dim_path(dir, entry.id);
        let tmp = path.with_extension("model.tmp");
        write_durable(&tmp, text.as_bytes())?;
        std::fs::rename(&tmp, &path)?;
    }
    // 2. Write and fsync the new index; it is the authority on load. Doing this
    //    before any sweep means the index that survives a crash never lists a body
    //    a later step removed.
    let doc = IndexDoc {
        dimensions: entries
            .iter()
            .map(|e| IndexEntryDoc {
                id: e.id,
                generation: e.generation,
                references: e.references.clone(),
            })
            .collect(),
    };
    let index = toml::to_string(&doc).map_err(|e| PersistError::Save(SaveError::Toml(e)))?;
    let tmp = dir.join("index.toml.tmp");
    write_durable(&tmp, index.as_bytes())?;
    std::fs::rename(&tmp, dir.join(INDEX_FILE))?;
    // 3. Only now that the new index is durable, quarantine bodies whose ids it no
    //    longer lists (rename to `<id>.model.orphan` rather than delete, so a
    //    mistaken empty entry set cannot destroy the library irrecoverably). Ids in
    //    a BTreeSet so the sweep order is deterministic (ADR-0009).
    let keep: std::collections::BTreeSet<u64> = entries.iter().map(|e| e.id).collect();
    if let Ok(read) = std::fs::read_dir(dir) {
        let mut paths: Vec<std::path::PathBuf> =
            read.filter_map(Result::ok).map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            if path.extension().and_then(|e| e.to_str()) == Some("model") {
                if let Some(id) = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    if !keep.contains(&id) {
                        let quarantine = path.with_extension("model.orphan");
                        let _ = std::fs::rename(&path, &quarantine);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Write `bytes` to `path` and fsync the file's contents before returning, so a
/// following rename promotes a fully-durable file (matching the WAL/snapshot
/// durability contract in ADR-0002).
fn write_durable(path: &Path, bytes: &[u8]) -> Result<(), PersistError> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Load the registry from `dir`. An absent index means an empty registry (first
/// run). Each indexed dimension's body is read from its `<id>.model`.
pub fn load_registry(dir: &Path) -> Result<Vec<RegistryEntry>, PersistError> {
    let index_path = dir.join(INDEX_FILE);
    if !index_path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&index_path)?;
    let doc: IndexDoc =
        toml::from_str(&text).map_err(|e| PersistError::Corrupt(format!("registry index: {e}")))?;
    let mut entries = Vec::with_capacity(doc.dimensions.len());
    for ie in doc.dimensions {
        let body = std::fs::read_to_string(dim_path(dir, ie.id))?;
        let dimension = Dimension::from_model_text(&body).map_err(PersistError::Load)?;
        entries.push(RegistryEntry {
            id: ie.id,
            generation: ie.generation,
            references: ie.references,
            dimension,
        });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("epiphany-registry-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        dir
    }

    fn dim(name: &str) -> Dimension {
        let mut d = Dimension::new(name);
        let a = d.add_leaf("A");
        let b = d.add_leaf("B");
        let total = d.add_consolidated("Total");
        d.add_child(total, a, 1).unwrap();
        d.add_child(total, b, 1).unwrap();
        d
    }

    fn entry(id: u64, name: &str, references: &[&str]) -> RegistryEntry {
        RegistryEntry {
            id,
            generation: 1,
            references: references.iter().map(|s| s.to_string()).collect(),
            dimension: dim(name),
        }
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = scratch("round-trip");
        let entries = vec![entry(1, "Region", &["Sales"]), entry(2, "Period", &[])];
        save_registry(&dir, &entries).unwrap();
        let loaded = load_registry(&dir).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, 1);
        assert_eq!(loaded[0].dimension.name(), "Region");
        assert_eq!(loaded[0].references, vec!["Sales".to_string()]);
        assert_eq!(loaded[1].id, 2);
        assert_eq!(loaded[1].dimension.name(), "Period");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dropped_id_is_quarantined_not_deleted() {
        // A save with a smaller entry set removes the dropped id from the active set
        // (the index no longer lists it, so load ignores it) but preserves its body
        // on disk under `<id>.model.orphan` rather than destroying it.
        let dir = scratch("quarantine");
        save_registry(&dir, &[entry(1, "Region", &[]), entry(2, "Period", &[])]).unwrap();
        // A later save that omits id 2 (e.g. it was genuinely deleted).
        save_registry(&dir, &[entry(1, "Region", &[])]).unwrap();

        let loaded = load_registry(&dir).unwrap();
        assert_eq!(loaded.len(), 1, "the index drops the removed dimension");
        assert_eq!(loaded[0].id, 1);
        // The active body is gone from the model set, but recoverable on disk.
        assert!(!dim_path(&dir, 2).exists(), "the active body was swept");
        assert!(
            dir.join("2.model.orphan").exists(),
            "the swept body is quarantined, not destroyed"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_save_does_not_destroy_bodies() {
        // The critical case: a caller that mistakenly passes an empty entry set
        // (e.g. after a swallowed load failure) must NOT permanently lose the
        // library. The bodies are quarantined and can be recovered.
        let dir = scratch("empty-save");
        save_registry(&dir, &[entry(1, "Region", &[]), entry(2, "Period", &[])]).unwrap();
        // The destructive-looking call with an empty set.
        save_registry(&dir, &[]).unwrap();

        assert!(load_registry(&dir).unwrap().is_empty());
        assert!(
            dir.join("1.model.orphan").exists() && dir.join("2.model.orphan").exists(),
            "both bodies survive as quarantined files"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn index_is_written_after_bodies_and_load_matches_it() {
        // After a successful save the index lists exactly the current ids and every
        // listed body exists (the ordering invariant that keeps load consistent).
        let dir = scratch("index-authority");
        save_registry(&dir, &[entry(5, "Region", &[]), entry(9, "Period", &[])]).unwrap();
        for id in [5u64, 9] {
            assert!(dim_path(&dir, id).exists(), "listed body {id} exists");
        }
        let loaded = load_registry(&dir).unwrap();
        let ids: Vec<u64> = loaded.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![5, 9]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_index_is_empty_registry() {
        let dir = scratch("no-index");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_registry(&dir).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_index_fails_loudly() {
        // load_registry surfaces a corrupt index as an error rather than silently
        // yielding an empty registry (the empty-substitution is the caller's bug,
        // not this layer's).
        let dir = scratch("corrupt-index");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(INDEX_FILE), "this is not valid toml { [").unwrap();
        assert!(matches!(load_registry(&dir), Err(PersistError::Corrupt(_))));
        std::fs::remove_dir_all(&dir).ok();
    }
}
