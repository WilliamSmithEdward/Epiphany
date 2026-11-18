//! Reads a model definition from a directory of JSON files.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use epiphany_core::model::ModelDef;
use serde::de::DeserializeOwned;

/// Loads every `dimensions/*.json` and `cubes/*.json` under a model directory, in name order.
pub fn load_model(directory: &Path) -> Result<ModelDef, Box<dyn Error>> {
    Ok(ModelDef {
        dimensions: load_all(&directory.join("dimensions"))?,
        cubes: load_all(&directory.join("cubes"))?,
    })
}

fn load_all<T: DeserializeOwned>(directory: &Path) -> Result<Vec<T>, Box<dyn Error>> {
    let at = |path: &Path, error: &dyn Error| format!("{}: {error}", path.display());
    let entries = fs::read_dir(directory).map_err(|e| at(directory, &e))?;
    let mut paths: Vec<PathBuf> = entries
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<_, _>>()
        .map_err(|e| at(directory, &e))?;
    paths.retain(|path| {
        path.extension()
            .is_some_and(|extension| extension == "json")
    });
    paths.sort();
    let load = |path: &PathBuf| {
        let text = fs::read_to_string(path).map_err(|e| at(path, &e))?;
        Ok(serde_json::from_str(&text).map_err(|e| at(path, &e))?)
    };
    paths.iter().map(load).collect()
}
