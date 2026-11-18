//! Keeps a database in a directory, so that it survives the process.
//!
//! ```text
//! <directory>/
//!   model/                 the current model
//!     model.json           its version
//!     dimensions/*.json
//!     cubes/*.json
//!   data/
//!     lock                 held by the one process serving the directory
//!     snapshot.json        every stored cell at some moment
//!     log.jsonl            one line per commit since the snapshot
//!     models/<version>/    every earlier model
//! ```
//!
//! A commit is appended to the log and flushed to disk before it is acknowledged. Opening the
//! directory loads the snapshot, replays the log, and replaces both with a fresh snapshot.

use std::borrow::Cow;
use std::collections::HashSet;
use std::error::Error;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use epiphany_core::database::{CellWrite, Database, Journal};
use epiphany_core::model::ModelDef;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// The log is replaced by a snapshot once it grows beyond this many bytes.
const LOG_LIMIT: u64 = 64 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct ModelFile {
    version: u64,
}

#[derive(Serialize, Deserialize)]
struct Snapshot<'a> {
    model_version: u64,
    cells: Cow<'a, [CellWrite]>,
}

/// Opens the database kept in a directory. The directory must hold a `model` folder; the
/// `data` folder is created on first use.
pub fn open(directory: &Path) -> Result<Database, Box<dyn Error>> {
    // Two servers writing one log would corrupt it, so the directory belongs to one process
    // at a time. The operating system releases the lock when that process ends, however it
    // ends.
    let data = directory.join("data");
    fs::create_dir_all(&data)?;
    let lock = File::create(data.join("lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            let directory = directory.display();
            return Err(format!("{directory} is already in use by another server").into());
        }
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }

    let (model, next) = (directory.join("model"), directory.join("model.next"));
    // A model update writes the new model beside the old one, then swaps them. Finish or
    // discard an update that was interrupted.
    if next.exists() {
        match model.exists() {
            true => fs::remove_dir_all(&next)?,
            false => fs::rename(&next, &model)?,
        }
    }
    let version = read_json::<ModelFile>(&model.join("model.json"))?.map_or(1, |file| file.version);
    let mut database = Database::restore(load_model(&model)?, version)?;

    let mut skipped = 0;
    if let Some(snapshot) = read_json::<Snapshot>(&data.join("snapshot.json"))? {
        skipped += database.load(&snapshot.cells);
    }
    let log = data.join("log.jsonl");
    let recorded = match fs::read_to_string(&log) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(at(&log, &error).into()),
    };
    for (number, line) in recorded.lines().enumerate() {
        // A commit that was being written when the process died is incomplete and was never
        // acknowledged. Nothing after it can be trusted.
        let Ok(commit) = serde_json::from_str::<Vec<CellWrite>>(line) else {
            eprintln!(
                "warning: {}: ignored from line {}",
                log.display(),
                number + 1
            );
            break;
        };
        skipped += database.load(&commit);
    }
    if skipped > 0 {
        eprintln!("warning: {skipped} stored cells no longer fit the model and were dropped");
    }

    let mut files = Files {
        directory: directory.to_owned(),
        log: OpenOptions::new().create(true).append(true).open(&log)?,
        log_bytes: recorded.len() as u64,
        _lock: lock,
    };
    if !recorded.is_empty() {
        files.snapshot(version, &database.cells())?;
    }
    database.set_journal(Box::new(files));
    Ok(database)
}

/// Loads every `dimensions/*.json` and `cubes/*.json` under a model directory, in name order.
pub fn load_model(directory: &Path) -> Result<ModelDef, Box<dyn Error>> {
    Ok(ModelDef {
        dimensions: load_all(&directory.join("dimensions"))?,
        cubes: load_all(&directory.join("cubes"))?,
    })
}

fn load_all<T: DeserializeOwned>(directory: &Path) -> Result<Vec<T>, Box<dyn Error>> {
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
    let load = |path: &PathBuf| Ok(read_json(path)?.expect("listed above"));
    paths.iter().map(load).collect()
}

/// Reads a JSON file, or nothing if the file does not exist.
pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(serde_json::from_str(&text).map_err(|e| at(path, &e))?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(at(path, &error)),
    }
}

/// An error message that names the file it is about.
fn at(path: &Path, error: &dyn Error) -> String {
    format!("{}: {error}", path.display())
}

/// Writes a JSON file so that it is either complete or absent: to a temporary file first,
/// flushed to disk, then renamed into place.
pub(crate) fn write_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let temporary = path.with_extension("tmp");
    let mut file = File::create(&temporary)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary, path)
}

/// Writes each item to its own JSON file in a new folder, named after the item.
fn write_each<T: Serialize>(
    folder: &Path,
    items: &[T],
    name: impl Fn(&T) -> &str,
) -> io::Result<()> {
    fs::create_dir_all(folder)?;
    let files = file_names(items.iter().map(name));
    let mut written = files.iter().zip(items);
    written.try_for_each(|(file, item)| write_json(&folder.join(file), item))
}

/// A file name for each name: lower case, with anything but letters and digits as a hyphen,
/// and a number appended where two names would otherwise share a file.
fn file_names<'a>(names: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut taken = HashSet::new();
    let unique = |name: &str| {
        let plain = |c: char| match c.is_alphanumeric() {
            true => c.to_ascii_lowercase(),
            false => '-',
        };
        let base: String = name.chars().map(plain).collect();
        let numbered = (2..).map(|number| format!("{base}-{number}"));
        let free = std::iter::once(base.clone())
            .chain(numbered)
            .find(|name| !taken.contains(name));
        let name = free.expect("the numbers never run out");
        taken.insert(name.clone());
        format!("{name}.json")
    };
    names.map(unique).collect()
}

#[derive(Debug)]
struct Files {
    directory: PathBuf,
    log: File,
    log_bytes: u64,
    /// Held for as long as the database is open.
    _lock: File,
}

impl Journal for Files {
    fn commit(&mut self, writes: &[CellWrite]) -> io::Result<()> {
        let mut line = serde_json::to_vec(writes)?;
        line.push(b'\n');
        self.log.write_all(&line)?;
        self.log.sync_data()?;
        self.log_bytes += line.len() as u64;
        Ok(())
    }

    fn model(&mut self, version: u64, definition: &ModelDef) -> io::Result<()> {
        let (model, next) = (
            self.directory.join("model"),
            self.directory.join("model.next"),
        );
        if next.exists() {
            fs::remove_dir_all(&next)?;
        }
        write_each(&next.join("dimensions"), &definition.dimensions, |d| {
            &d.name
        })?;
        write_each(&next.join("cubes"), &definition.cubes, |c| &c.name)?;
        write_json(&next.join("model.json"), &ModelFile { version })?;

        // Keep the model being replaced as a numbered copy, then put the new one in its place.
        // If the process dies between the two renames, opening the directory finishes the job.
        let history = self.directory.join("data").join("models");
        fs::create_dir_all(&history)?;
        let numbered = (2..).map(|copy| format!("{}-{copy}", version - 1));
        let mut names = std::iter::once((version - 1).to_string()).chain(numbered);
        let previous = names.find(|name| !history.join(name).exists());
        fs::rename(
            &model,
            history.join(previous.expect("the numbers never run out")),
        )?;
        fs::rename(&next, &model)
    }

    fn is_full(&self) -> bool {
        self.log_bytes > LOG_LIMIT
    }

    fn snapshot(&mut self, model_version: u64, cells: &[CellWrite]) -> io::Result<()> {
        let snapshot = Snapshot {
            model_version,
            cells: Cow::Borrowed(cells),
        };
        let written = write_json(
            &self.directory.join("data").join("snapshot.json"),
            &snapshot,
        )
        // The log is open for appending only, which does not allow truncating it; opening
        // it afresh does, and later appends then start from the beginning.
        .and_then(|()| File::create(self.directory.join("data").join("log.jsonl")))
        .and_then(|log| log.sync_all());
        match &written {
            Ok(()) => self.log_bytes = 0,
            Err(error) => eprintln!("warning: the snapshot could not be written: {error}"),
        }
        written
    }
}
