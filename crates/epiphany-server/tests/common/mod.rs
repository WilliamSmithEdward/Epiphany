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

    /// Opens the directory. Another test starting a server process at this moment holds a
    /// copy of every open file of this one, a lock just let go among them, until the
    /// server's own program takes its place: so a directory whose database was just dropped
    /// may stay locked for an instant, and is tried again for up to a second.
    pub fn open(&self) -> Database {
        for _ in 0..50 {
            match open(&self.0) {
                Err(error)
                    if error
                        .to_string()
                        .ends_with("is already in use by another server") =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                opened => return opened.unwrap(),
            }
        }
        panic!(
            "{} stayed locked after its database was dropped",
            self.0.display()
        );
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
