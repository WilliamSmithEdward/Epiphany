//! Shared by the tests that need a server directory on disk.

use std::fs;
use std::path::{Path, PathBuf};

use epiphany_core::database::Database;
use epiphany_server::open;

/// A server directory holding a copy of the example model, removed when dropped.
pub struct Home(pub PathBuf);

impl Home {
    pub fn new(name: &str) -> Self {
        let home = std::env::temp_dir().join(format!("epiphany-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../epiphany-core/tests/fixtures/model");
        copy(&fixture, &home.join("model"));
        Self(home)
    }

    pub fn open(&self) -> Database {
        open(&self.0).unwrap()
    }

    pub fn path(&self, relative: &str) -> PathBuf {
        self.0.join(relative)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn copy(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        match entry.file_type().unwrap().is_dir() {
            true => copy(&entry.path(), &target),
            false => drop(fs::copy(entry.path(), target).unwrap()),
        }
    }
}
