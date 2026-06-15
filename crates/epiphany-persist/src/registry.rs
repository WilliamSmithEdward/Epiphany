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

/// Write the registry to `dir`: one `<id>.model` per dimension, then `index.toml`
/// last. Stale `<id>.model` files for ids no longer present are removed so a
/// deleted dimension does not linger.
pub fn save_registry(dir: &Path, entries: &[RegistryEntry]) -> Result<(), PersistError> {
    std::fs::create_dir_all(dir)?;
    for entry in entries {
        let text = entry
            .dimension
            .to_model_text()
            .map_err(PersistError::Save)?;
        let path = dim_path(dir, entry.id);
        let tmp = path.with_extension("model.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)?;
    }
    // Remove orphaned dimension files (ids not in the current set).
    let keep: std::collections::BTreeSet<u64> = entries.iter().map(|e| e.id).collect();
    if let Ok(read) = std::fs::read_dir(dir) {
        for path in read.filter_map(Result::ok).map(|e| e.path()) {
            if path.extension().and_then(|e| e.to_str()) == Some("model") {
                if let Some(id) = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    if !keep.contains(&id) {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        }
    }
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
    std::fs::write(&tmp, index)?;
    std::fs::rename(&tmp, dir.join(INDEX_FILE))?;
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
