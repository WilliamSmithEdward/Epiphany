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

use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use epiphany_core::database::{CellWrite, Database, Journal, NamedCells, NamedWrites};
use epiphany_core::model::ModelDef;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// The log is replaced by a snapshot once it grows beyond this many bytes.
const LOG_LIMIT: u64 = 64 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct ModelFile {
    version: u64,
}

#[derive(Serialize)]
struct Snapshot<'a> {
    model_version: u64,
    /// How many cells each cube holds, ahead of the cells, so that room is made for them
    /// at once as they are loaded.
    stored: BTreeMap<&'a str, usize>,
    cells: &'a NamedCells<'a>,
}

/// The fewest bytes a cell takes in a snapshot, `{"cube":"","at":[],"value":0}`: no more
/// room is made than the file could hold cells, whatever it says.
const LEAST_A_CELL: u64 = 30;

/// Loads the snapshot in a file into a database as it is read, so that a snapshot of
/// millions of cells is never held whole by name: that took some 300 bytes a cell beside
/// the cells themselves. Says how many cells no longer fit the model; nothing if there is
/// no snapshot.
fn load_snapshot(path: &Path, database: &mut Database) -> Result<Option<usize>, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(at(path, &error)),
    };
    let room = file.metadata().map_or(0, |file| file.len() / LEAST_A_CELL);
    let mut reader = serde_json::Deserializer::from_reader(io::BufReader::new(file));
    let mut loading = Loading {
        database,
        room: usize::try_from(room).unwrap_or(usize::MAX),
        skipped: 0,
    };
    let read = serde::de::DeserializeSeed::deserialize(&mut loading, &mut reader);
    read.and_then(|()| reader.end())
        .map_err(|error| at(path, &error))?;
    Ok(Some(loading.skipped))
}

struct Loading<'a> {
    database: &'a mut Database,
    /// How many more cells room may be made for.
    room: usize,
    skipped: usize,
}

// The snapshot, an object whose `cells` are stored as they are read and whose other fields
// are passed over.
impl<'de> serde::de::DeserializeSeed<'de> for &mut Loading<'_> {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> serde::de::Visitor<'de> for &mut Loading<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a snapshot, an object with its cells")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(field) = map.next_key::<String>()? {
            match field.as_str() {
                "stored" => {
                    for (cube, cells) in map.next_value::<BTreeMap<String, usize>>()? {
                        let cells = cells.min(self.room);
                        self.room -= cells;
                        self.database.reserve(&cube, cells);
                    }
                }
                "cells" => self.skipped += map.next_value_seed(Cells(&mut *self.database))?,
                _ => drop(map.next_value::<serde::de::IgnoredAny>()?),
            }
        }
        Ok(())
    }
}

/// The cells of a snapshot, stored as they come.
struct Cells<'a>(&'a mut Database);

impl<'de> serde::de::DeserializeSeed<'de> for Cells<'_> {
    type Value = usize;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<usize, D::Error> {
        self.0.load_from(deserializer)
    }
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
    skipped += load_snapshot(&data.join("snapshot.json"), &mut database)?.unwrap_or(0);
    let log = data.join("log.jsonl");
    let recorded = match fs::read_to_string(&log) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(at(&log, &error).into()),
    };
    let commits: Vec<&str> = recorded.lines().collect();
    for (number, line) in commits.iter().enumerate() {
        match serde_json::from_str::<Vec<CellWrite>>(line) {
            Ok(commit) => skipped += database.load(&commit),
            // A commit that was being written when the process died is incomplete and was
            // never acknowledged: the last line may be one, and is left out.
            Err(_) if number + 1 == commits.len() => eprintln!(
                "warning: {}: the last commit, on line {}, was cut short and is left out",
                log.display(),
                number + 1
            ),
            // Any other line was written whole and acknowledged. It is damaged, and to go
            // on without it would be to drop every commit after it and then, at the next
            // snapshot, to erase them for good.
            Err(error) => {
                return Err(format!(
                    "{}: line {} of {} cannot be read ({error}). The commits after it would \
                     be lost, so the directory is not opened; restore the file from a copy, \
                     or remove the damaged line to go on without that commit.",
                    log.display(),
                    number + 1,
                    commits.len()
                )
                .into());
            }
        }
    }
    if skipped > 0 {
        eprintln!("warning: {skipped} stored cells no longer fit the model and were dropped");
    }

    let created = !log.exists();
    let mut files = Files {
        directory: directory.to_owned(),
        log: OpenOptions::new().create(true).append(true).open(&log)?,
        log_bytes: recorded.len() as u64,
        _lock: lock,
    };
    // A log that is new must be known to its folder before a commit is trusted to it.
    if created {
        settle(&data)?;
    }
    if !recorded.is_empty() {
        files.snapshot(version, &database.named_cells())?;
    }
    database.set_journal(Box::new(files));
    Ok(database)
}

/// Loads every `dimensions/*.json` and `cubes/*.json` under a model directory, in name order.
pub fn load_model(directory: &Path) -> Result<ModelDef, Box<dyn Error>> {
    let dashboards = directory.join("dashboards");
    Ok(ModelDef {
        dimensions: load_all(&directory.join("dimensions"))?,
        cubes: load_all(&directory.join("cubes"))?,
        // A model need have no dashboards, and no folder for them.
        dashboards: match dashboards.is_dir() {
            true => load_all(&dashboards)?,
            false => Vec::new(),
        },
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
    write_whole(path, value, true)
}

/// The same, laid out for a person to read or as closely as it will go. A model is read
/// and edited by hand; a snapshot of a million cells is not, and laid out it is twice the
/// size on disk.
fn write_whole(path: &Path, value: &impl Serialize, laid_out: bool) -> io::Result<()> {
    let temporary = path.with_extension("tmp");
    // JSON is written a few bytes at a time, and each piece handed straight to the file
    // would be a call to the operating system: a snapshot of a million cells took two
    // minutes that way. The pieces are gathered and handed over by the page.
    let mut file = BufWriter::new(File::create(&temporary)?);
    match laid_out {
        true => serde_json::to_writer_pretty(&mut file, value)?,
        false => serde_json::to_writer(&mut file, value)?,
    }
    file.write_all(b"\n")?;
    let file = file.into_inner().map_err(|error| error.into_error())?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    path.parent().map_or(Ok(()), settle)
}

/// Makes what has been created, renamed or removed in a folder last through a loss of power.
/// A file's contents are safe once the file is flushed, but that it is there under its name
/// is the folder's to record, and on Unix the folder must be flushed too. Windows records it
/// with the change itself and has no such call to make.
fn settle(folder: &Path) -> io::Result<()> {
    match cfg!(unix) {
        true => File::open(folder)?.sync_all(),
        false => Ok(()),
    }
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
    fn commit(&mut self, writes: &NamedWrites<'_>) -> io::Result<()> {
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
        if !definition.dashboards.is_empty() {
            write_each(&next.join("dashboards"), &definition.dashboards, |d| {
                &d.name
            })?;
        }
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
        fs::rename(&next, &model)?;
        settle(&history).and_then(|()| settle(&self.directory))
    }

    fn is_full(&self) -> bool {
        self.log_bytes > LOG_LIMIT
    }

    fn bytes(&self) -> Option<(u64, u64)> {
        let snapshot = fs::metadata(self.directory.join("data").join("snapshot.json"));
        Some((self.log_bytes, snapshot.map_or(0, |file| file.len())))
    }

    fn snapshot(&mut self, model_version: u64, cells: &NamedCells<'_>) -> io::Result<()> {
        let snapshot = Snapshot {
            model_version,
            stored: cells.counts(),
            cells,
        };
        let written = write_whole(
            &self.directory.join("data").join("snapshot.json"),
            &snapshot,
            false,
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
