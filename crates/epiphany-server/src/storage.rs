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
//!     snapshot.json        every stored cell at some moment, and their supporting detail
//!     log.jsonl            one line per commit since the snapshot: its cells, with the
//!                          supporting detail it wrote or took away where there is any
//!     history.jsonl        one line per commit that changed a value: when, by whom, and
//!                          each cell's value before and after, kept through snapshots
//!     comments.jsonl       one line per comment added or taken away
//!     sandboxes/<id>.json  each sandbox, under its transaction id: its name, its owner and
//!                          the values it holds, written whole as it changes
//!     models/<version>/    every earlier model, its files that the model after it left
//!                          unchanged shared with that model rather than copied
//!     models/bytes.json    what each earlier model holds alone on disk
//! ```
//!
//! A commit is appended to the log and flushed to disk before it is acknowledged. Opening the
//! directory loads the snapshot, replays the log, and replaces both with a fresh snapshot.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use epiphany_core::Scalar;
use epiphany_core::database::{
    Asked, CellChange, CellDetail, CellWrite, Changes, CommentChange, Database, EarlierModel,
    History, Journal, KeptSandbox, NamedCells, NamedDetails, NamedWrites, OnDisk, TransactionId,
};
use epiphany_core::model::{CubeDef, DashboardDef, DimensionDef, ModelDef, Shared};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// The log is replaced by a snapshot once it grows beyond this many bytes.
const LOG_LIMIT: u64 = 64 * 1024 * 1024;

/// A dimension of more elements than this is written close rather than laid out. No one edits
/// one of a hundred thousand elements by hand, and laid out it is two and a half times the
/// size: a million took 131 MB rather than 53, slower to write, and kept again at every save.
const LAID_OUT_MOST: usize = 100_000;

#[derive(Serialize, Deserialize)]
struct ModelFile {
    version: u64,
}

/// Where, under `data/models`, what each earlier model holds alone on disk is kept.
const KEPT_BYTES: &str = "bytes.json";

/// A part of the model as it lies on disk: its file, within the model's folder, and the size
/// and time of change the file had when last written or read, to tell that no hand has
/// changed it since.
#[derive(Debug, Clone)]
struct OnFile {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}

impl OnFile {
    /// The file at `path` within `folder`, as it is now.
    fn at(folder: &Path, path: PathBuf) -> io::Result<Self> {
        let now = fs::metadata(folder.join(&path))?;
        Ok(Self {
            path,
            len: now.len(),
            modified: now.modified().ok(),
        })
    }

    /// Whether the file is still as it was left in `folder`.
    fn unchanged_in(&self, folder: &Path) -> bool {
        fs::metadata(folder.join(&self.path))
            .is_ok_and(|now| now.len() == self.len && now.modified().ok() == self.modified)
    }
}

/// The parts of one kind in the model on disk, by name, each beside its file.
type LaidParts<T> = HashMap<String, (T, OnFile)>;

/// The model in the `model` folder, part by part beside its file: for the next model saved to
/// tell the parts it has unchanged, and share their files with the model kept rather than
/// write them again.
#[derive(Debug, Default)]
struct Laid {
    dimensions: LaidParts<Shared<DimensionDef>>,
    cubes: LaidParts<CubeDef>,
    dashboards: LaidParts<DashboardDef>,
}

impl Laid {
    /// The bytes of the parts' files.
    fn bytes(&self) -> u64 {
        let sizes =
            |parts: &mut dyn Iterator<Item = &OnFile>| parts.map(|file| file.len).sum::<u64>();
        sizes(&mut self.dimensions.values().map(|(_, file)| file))
            + sizes(&mut self.cubes.values().map(|(_, file)| file))
            + sizes(&mut self.dashboards.values().map(|(_, file)| file))
    }
}

#[derive(Serialize)]
struct Snapshot<'a> {
    model_version: u64,
    /// How many cells each cube holds, ahead of the cells, so that room is made for them
    /// at once as they are loaded.
    stored: BTreeMap<&'a str, usize>,
    cells: &'a NamedCells<'a>,
    /// The supporting detail of the cells that have one.
    details: NamedDetails<'a>,
}

/// A commit as the log holds it: its cells alone, a list as a commit always was, or with the
/// supporting detail written with them or taken away.
#[derive(Serialize, Deserialize)]
struct Detailed<C, D> {
    cells: C,
    details: D,
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
                "details" => {
                    let details: Vec<CellDetail> = map.next_value()?;
                    self.database.load_details(&details);
                }
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
    let (definition, laid) = read_model(&model)?;
    let mut database = Database::restore(definition, version)?;

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
        match replay(&mut database, line) {
            Ok(dropped) => skipped += dropped,
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
        earlier_bytes: Mutex::new(
            read_json(&data.join("models").join(KEPT_BYTES))?.unwrap_or_default(),
        ),
        laid,
        _lock: lock,
    };
    // A log that is new must be known to its folder before a commit is trusted to it.
    if created {
        settle(&data)?;
    }
    if !recorded.is_empty() {
        files.snapshot(version, &database.named_cells())?;
    }
    // The comments, as each was added or taken away. A line cut short by the process dying
    // as it was written was never acknowledged, and is left out.
    let comments = fs::read_to_string(data.join(COMMENTS)).or_else(|error| match error.kind() {
        io::ErrorKind::NotFound => Ok(String::new()),
        _ => Err(error),
    })?;
    let lines: Vec<&str> = comments.lines().collect();
    for (number, line) in lines.iter().enumerate() {
        match serde_json::from_str::<CommentChange>(line) {
            Ok(change) => database.load_comment(change),
            Err(_) if number + 1 == lines.len() => {
                eprintln!("warning: the last comment was cut short and is left out")
            }
            Err(error) => return Err(format!("{COMMENTS}, line {}: {error}", number + 1).into()),
        }
    }
    // The sandboxes, each whole in a file of its own. A file is written beside its place and
    // renamed into it, so one is never found half written; one that cannot be read is a
    // user's work, and the directory is not opened without it.
    let mut dropped = 0;
    for (id, path) in sandbox_files(&data.join(SANDBOXES))? {
        let kept: KeptFile = read_json(&path)?.expect("listed above");
        let KeptFile {
            name,
            owner,
            cells,
            details,
        } = &kept;
        dropped += database.restore_sandbox(id, owner, name, cells, details);
    }
    if dropped > 0 {
        eprintln!(
            "warning: {dropped} values in sandboxes no longer fit the model and were dropped"
        );
    }
    database.set_journal(Box::new(files));
    Ok(database)
}

/// Stores a commit as the log recorded it, with any supporting detail it brought or took
/// away. Says how many of its cells no longer fit the model.
fn replay(database: &mut Database, line: &str) -> serde_json::Result<usize> {
    if line.trim_start().starts_with('{') {
        let commit: Detailed<Vec<CellWrite>, Vec<CellDetail>> = serde_json::from_str(line)?;
        let dropped = database.load(&commit.cells);
        database.load_details(&commit.details);
        return Ok(dropped);
    }
    Ok(database.load(&serde_json::from_str::<Vec<CellWrite>>(line)?))
}

/// Where the comments are kept, in the data folder.
const COMMENTS: &str = "comments.jsonl";

/// Where the sandboxes are kept, in the data folder: one file each, named by its transaction.
const SANDBOXES: &str = "sandboxes";

/// A sandbox as its file holds it.
#[derive(Deserialize)]
struct KeptFile {
    name: String,
    owner: String,
    cells: Vec<CellWrite>,
    /// The supporting detail written with its values, or taken away; none in a file kept
    /// before there was any.
    #[serde(default)]
    details: Vec<CellDetail>,
}

/// The file of each sandbox kept in a folder, with the transaction it is kept under. None
/// if there is no folder.
fn sandbox_files(folder: &Path) -> Result<Vec<(TransactionId, PathBuf)>, String> {
    let entries = match fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(at(folder, &error)),
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry.map_err(|error| at(folder, &error))?.path();
        let json = path
            .extension()
            .is_some_and(|extension| extension == "json");
        let id = path
            .file_stem()
            .and_then(|stem| stem.to_str()?.parse().ok());
        if let (true, Some(id)) = (json, id) {
            files.push((id, path));
        }
    }
    Ok(files)
}

/// Where the history of cells is kept, in the data folder: one line for each commit that
/// changed a value, never replaced by a snapshot.
const HISTORY: &str = "history.jsonl";

/// A commit as the history file holds it.
#[derive(Deserialize)]
struct Recorded {
    time: u64,
    author: Option<String>,
    cells: Vec<RecordedCell>,
}

#[derive(Deserialize)]
struct RecordedCell {
    cube: String,
    at: Vec<String>,
    before: Scalar,
    after: Scalar,
}

/// The history file, read apart from the database.
struct HistoryFile(PathBuf);

impl History for HistoryFile {
    /// Reads the file from its end, the newest commit first, until the changes asked for are
    /// found, so that the newest of a long history are found without reading the rest: read
    /// from its start, a history of two million commits took 1.2 s for each part asked for. A
    /// line newer than any asked for is passed over by the time it begins with; one is read as
    /// JSON only if it holds every name asked about, as JSON writes them, so that most lines
    /// are passed over at a glance; a user's name may be written in another case, and is
    /// looked for once the line is read. A line cut short, the last after a crash, is left out.
    fn find(&self, asked: &Asked, skip: usize, most: usize) -> io::Result<Vec<CellChange>> {
        if most == 0 {
            return Ok(Vec::new());
        }
        let file = match File::open(&self.0) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let names = asked.cube.iter().chain(asked.at.iter().flatten());
        let needles = names
            .map(serde_json::to_vec)
            .collect::<Result<Vec<_>, _>>()?;
        let holds =
            |line: &[u8], needle: &[u8]| line.windows(needle.len()).any(|part| part == needle);
        let newer = |line: &[u8]| {
            let time = time_of(line);
            asked
                .until
                .zip(time)
                .is_some_and(|(until, time)| time > until)
        };
        let mut passed = 0;
        let mut found = Vec::with_capacity(most.min(1024));
        for line in Backwards::new(file)? {
            let line = line?;
            if newer(&line) || !needles.iter().all(|needle| holds(&line, needle)) {
                continue;
            }
            let Ok(commit) = serde_json::from_slice::<Recorded>(&line) else {
                continue;
            };
            if !asked.takes_commit(commit.time, commit.author.as_deref()) {
                continue;
            }
            // A commit's cells, the last first, as the newest are given first.
            for cell in commit.cells.into_iter().rev() {
                if !asked.takes_cell(&cell.cube, &cell.at) {
                    continue;
                }
                if passed < skip {
                    passed += 1;
                    continue;
                }
                found.push(CellChange {
                    time: commit.time,
                    author: commit.author.clone(),
                    cube: cell.cube,
                    at: cell.at,
                    before: cell.before,
                    after: cell.after,
                });
                if found.len() == most {
                    return Ok(found);
                }
            }
        }
        Ok(found)
    }
}

/// The time a line of the history begins with, read without the rest of it: each is
/// written `{"time":…`, and one written otherwise is read whole.
fn time_of(line: &[u8]) -> Option<u64> {
    let digits = line.strip_prefix(b"{\"time\":")?;
    let end = digits.iter().position(|byte| !byte.is_ascii_digit())?;
    std::str::from_utf8(&digits[..end]).ok()?.parse().ok()
}

/// How much of a file is read at a time from its end.
const BLOCK: u64 = 64 * 1024;

/// The lines of a file, the last first, read from its end a block at a time.
struct Backwards {
    file: File,
    /// How much of the file, from its start, is left to read.
    left: u64,
    /// The end of a line whose start is in what is left to read.
    carry: Vec<u8>,
    /// The lines whole in what has been read, the last given first.
    lines: Vec<Vec<u8>>,
}

impl Backwards {
    fn new(file: File) -> io::Result<Self> {
        let left = file.metadata()?.len();
        Ok(Self {
            file,
            left,
            carry: Vec::new(),
            lines: Vec::new(),
        })
    }
}

impl Iterator for Backwards {
    type Item = io::Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(line) = self.lines.pop() {
                return Some(Ok(line));
            }
            if self.left == 0 {
                // The file's first line, whole once nothing is left before it.
                return (!self.carry.is_empty()).then(|| Ok(std::mem::take(&mut self.carry)));
            }
            let size = BLOCK.min(self.left);
            self.left -= size;
            let mut block = vec![0; size as usize];
            let read = self.file.seek(SeekFrom::Start(self.left));
            if let Err(error) = read.and_then(|_| self.file.read_exact(&mut block)) {
                self.left = 0;
                self.carry.clear();
                return Some(Err(error));
            }
            // The block, and after it the end of the line it ends in the middle of.
            block.append(&mut self.carry);
            let mut parts = block.split(|&byte| byte == b'\n');
            self.carry = parts.next().unwrap_or_default().to_vec();
            self.lines = parts
                .filter(|line| !line.is_empty())
                .map(<[u8]>::to_vec)
                .collect();
        }
    }
}

/// Loads every `dimensions/*.json`, `cubes/*.json` and `dashboards/*.json` under a model
/// directory, in name order.
pub fn load_model(directory: &Path) -> Result<ModelDef, Box<dyn Error>> {
    read_model(directory).map(|(definition, _)| definition)
}

/// The model in `directory`, and each of its parts beside the file it was read from.
fn read_model(directory: &Path) -> Result<(ModelDef, Laid), Box<dyn Error>> {
    let (dimensions, laid_dimensions) =
        load_all(directory, "dimensions", |d: &Shared<DimensionDef>| &d.name)?;
    let (cubes, laid_cubes) = load_all(directory, "cubes", |c: &CubeDef| &c.name)?;
    // A model need have no dashboards, and no folder for them.
    let (dashboards, laid_dashboards) = match directory.join("dashboards").is_dir() {
        true => load_all(directory, "dashboards", |d: &DashboardDef| &d.name)?,
        false => (Vec::new(), HashMap::new()),
    };
    let laid = Laid {
        dimensions: laid_dimensions,
        cubes: laid_cubes,
        dashboards: laid_dashboards,
    };
    let definition = ModelDef {
        dimensions,
        cubes,
        dashboards,
    };
    Ok((definition, laid))
}

/// The parts in `folder` of the model in `directory`, in the order of their files' names, and
/// each by its name beside its file.
fn load_all<T: DeserializeOwned + Clone + Send>(
    directory: &Path,
    folder: &str,
    name: impl Fn(&T) -> &str,
) -> Result<(Vec<T>, LaidParts<T>), Box<dyn Error>> {
    let within = directory.join(folder);
    let entries = fs::read_dir(&within).map_err(|e| at(&within, &e))?;
    let mut paths: Vec<PathBuf> = entries
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<_, _>>()
        .map_err(|e| at(&within, &e))?;
    paths.retain(|path| {
        path.extension()
            .is_some_and(|extension| extension == "json")
    });
    paths.sort();
    // A file's size and time are taken before it is read: changed as it is read, it is seen
    // as changed when the model is next saved.
    let load = |path: &PathBuf| -> Result<(T, OnFile), String> {
        let file = path.file_name().expect("a file listed in its folder");
        let on_file =
            OnFile::at(directory, Path::new(folder).join(file)).map_err(|e| at(path, &e))?;
        Ok((read_json(path)?.expect("listed above"), on_file))
    };
    let mut parts = Vec::with_capacity(paths.len());
    let mut laid = HashMap::with_capacity(paths.len());
    for loaded in at_once(&paths, load) {
        let (part, on_file) = loaded?;
        laid.insert(name(&part).to_owned(), (part.clone(), on_file));
        parts.push(part);
    }
    Ok((parts, laid))
}

/// Does `work` on every item, many at once, and gives back what came of each, in order.
/// Reading or writing a model's files one after another waits on each in turn: a model of
/// ten thousand dimensions, its files new to a disk that scans each as it is first opened,
/// took 78 s to read one at a time and takes 4 s so (2026-11-24).
fn at_once<T: Sync, R: Send>(items: &[T], work: impl Fn(&T) -> R + Sync) -> Vec<R> {
    // Twice as many threads as run at once: much of the time goes in waiting on the disk.
    let threads = thread::available_parallelism().map_or(1, NonZero::get) * 2;
    let next = AtomicUsize::new(0);
    let work_through = || {
        let mut done = Vec::new();
        loop {
            let index = next.fetch_add(1, Ordering::Relaxed);
            let Some(item) = items.get(index) else {
                return done;
            };
            done.push((index, work(item)));
        }
    };
    let mut done: Vec<(usize, R)> = thread::scope(|scope| {
        let workers: Vec<_> = (0..threads.min(items.len()))
            .map(|_| scope.spawn(work_through))
            .collect();
        let joined = workers.into_iter().map(|worker| worker.join());
        joined
            .flat_map(|done| done.unwrap_or_else(|panic| std::panic::resume_unwind(panic)))
            .collect()
    });
    done.sort_unstable_by_key(|(index, _)| *index);
    done.into_iter().map(|(_, result)| result).collect()
}

/// Reads a JSON file, or nothing if the file does not exist.
pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(serde_json::from_str(&text).map_err(|e| at(path, &e))?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(at(path, &error)),
    }
}

/// The bytes of every file under a folder, its folders' too; none where there is no folder.
fn bytes_under(folder: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(folder) else {
        return 0;
    };
    let bytes = |entry: fs::DirEntry| match entry.file_type() {
        Ok(kind) if kind.is_dir() => bytes_under(&entry.path()),
        _ => entry.metadata().map_or(0, |file| file.len()),
    };
    entries.flatten().map(bytes).sum()
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
    write_file(path, value, laid_out)?;
    path.parent().map_or(Ok(()), settle)
}

/// The same, its folder left for the caller to settle: once, for a folder of many files.
fn write_file(path: &Path, value: &impl Serialize, laid_out: bool) -> io::Result<()> {
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
    fs::rename(&temporary, path)
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

/// Writes each part to its own JSON file in `sub` of the new model's folder, named after the
/// part and laid out where `laid_out` says, many at once and the folder settled once they all
/// are; or, where the model being replaced has the part as it is, by `before`, and its file
/// as it was left, shares that file instead, the model kept and the new one then holding it
/// alike: a save that changes a dashboard writes that dashboard, not ten thousand dimensions
/// again. A link refused, as by a file system that has none or a file linked as often as it
/// may be (1,023 times on Windows), is a file written. Gives the parts as they now lie, and
/// the bytes of the files shared.
fn lay_each<T: Serialize + PartialEq + Clone + Sync>(
    (model, next): (&Path, &Path),
    sub: &str,
    parts: &[T],
    name: impl Fn(&T) -> &str,
    laid_out: impl Fn(&T) -> bool + Sync,
    before: &LaidParts<T>,
) -> io::Result<(LaidParts<T>, u64)> {
    let folder = next.join(sub);
    fs::create_dir_all(&folder)?;
    let mut laid = HashMap::with_capacity(parts.len());
    let mut shared = 0;
    let mut written = Vec::new();
    // Linked one after another: many at once, links were slower.
    for (file, part) in file_names(parts.iter().map(&name)).into_iter().zip(parts) {
        let path = Path::new(sub).join(file);
        let kept = before.get(name(part));
        if let Some((was, on_file)) = kept
            && was == part
            && on_file.unchanged_in(model)
            && fs::hard_link(model.join(&on_file.path), next.join(&path)).is_ok()
        {
            shared += on_file.len;
            let on_file = OnFile {
                path,
                ..on_file.clone()
            };
            laid.insert(name(part).to_owned(), (part.clone(), on_file));
        } else {
            written.push((path, part));
        }
    }
    let write = |(path, part): &(PathBuf, &T)| {
        write_file(&next.join(path), part, laid_out(part))?;
        OnFile::at(next, path.clone())
    };
    for ((_, part), on_file) in written.iter().zip(at_once(&written, write)) {
        laid.insert(name(part).to_owned(), ((*part).clone(), on_file?));
    }
    settle(&folder)?;
    Ok((laid, shared))
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
    /// What each earlier model takes on disk, by the name of its folder, counted once: a
    /// model kept is never changed again, and a hundred kept of 10,000 dimensions are a
    /// million files to go over at every look at the server's state. One kept since its files
    /// are shared counts only those it holds alone, as it was saved, in `models/bytes.json`.
    earlier_bytes: Mutex<HashMap<String, u64>>,
    /// The model in the `model` folder, part by part beside its file.
    laid: Laid,
    /// Held for as long as the database is open.
    _lock: File,
}

impl Files {
    /// What the earlier models under `folder` take on disk, each counted the first time it is asked for.
    fn earlier_bytes(&self, folder: &Path) -> u64 {
        let Ok(entries) = fs::read_dir(folder) else {
            return 0;
        };
        let mut counted = self
            .earlier_bytes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let bytes = |entry: fs::DirEntry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => *counted
                .entry(entry.file_name().to_string_lossy().into_owned())
                .or_insert_with(|| bytes_under(&entry.path())),
            _ => entry.metadata().map_or(0, |file| file.len()),
        };
        entries.flatten().map(bytes).sum()
    }
}

impl Journal for Files {
    /// Appends the commit to the log: a list of its cells, or, where it brings supporting
    /// detail or takes some away, the cells with the detail, in one line either way.
    fn commit(&mut self, writes: &NamedWrites<'_>) -> io::Result<()> {
        let details = writes.details();
        let mut line = match details.is_empty() {
            true => serde_json::to_vec(writes)?,
            false => serde_json::to_vec(&Detailed {
                cells: writes,
                details,
            })?,
        };
        line.push(b'\n');
        self.log.write_all(&line)?;
        self.log.sync_data()?;
        self.log_bytes += line.len() as u64;
        Ok(())
    }

    /// Appends the change to the comments file, flushed to disk before it is acknowledged.
    fn comment(&mut self, change: &CommentChange) -> io::Result<()> {
        let path = self.directory.join("data").join(COMMENTS);
        let created = !path.exists();
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let mut line = serde_json::to_vec(change)?;
        line.push(b'\n');
        file.write_all(&line)?;
        file.sync_data()?;
        // A file that is new must be known to its folder before the comment is trusted to it.
        if created {
            settle(&self.directory.join("data"))?;
        }
        Ok(())
    }

    /// Appends what a commit changed to the history file. The commit itself is in the log,
    /// flushed to disk; its history is written without waiting on the disk again, and a
    /// failure is reported rather than undoing the commit.
    fn changes(&mut self, changes: &Changes<'_>) {
        let path = self.directory.join("data").join(HISTORY);
        let written = serde_json::to_vec(changes)
            .map_err(io::Error::from)
            .and_then(|mut line| {
                line.push(b'\n');
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)?
                    .write_all(&line)
            });
        if let Err(error) = written {
            eprintln!("warning: the history of cells could not be written: {error}");
        }
    }

    fn history(&self) -> Option<Box<dyn History>> {
        Some(Box::new(HistoryFile(
            self.directory.join("data").join(HISTORY),
        )))
    }

    /// Writes a sandbox whole to its file, flushed to disk before it is acknowledged, or
    /// removes the file once the sandbox is deleted.
    fn sandbox(&mut self, id: TransactionId, sandbox: Option<&KeptSandbox<'_>>) -> io::Result<()> {
        let data = self.directory.join("data");
        let folder = data.join(SANDBOXES);
        let path = folder.join(format!("{id}.json"));
        let Some(sandbox) = sandbox else {
            return match fs::remove_file(&path) {
                Ok(()) => settle(&folder),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            };
        };
        // A folder that is new must be known to its own before a sandbox is trusted to it.
        if !folder.exists() {
            fs::create_dir_all(&folder)?;
            settle(&data)?;
        }
        write_whole(&path, sandbox, false)
    }

    fn model(&mut self, version: u64, definition: &ModelDef) -> io::Result<()> {
        let (model, next) = (
            self.directory.join("model"),
            self.directory.join("model.next"),
        );
        if next.exists() {
            fs::remove_dir_all(&next)?;
        }
        // Only what changed is written; the rest is shared with the model being replaced.
        let folders = (model.as_path(), next.as_path());
        let before = &self.laid;
        let small = |d: &Shared<DimensionDef>| d.elements.len() <= LAID_OUT_MOST;
        let (dimensions, dimensions_shared) = lay_each(
            folders,
            "dimensions",
            &definition.dimensions,
            |d| &d.name,
            small,
            &before.dimensions,
        )?;
        let (cubes, cubes_shared) = lay_each(
            folders,
            "cubes",
            &definition.cubes,
            |c| &c.name,
            |_| true,
            &before.cubes,
        )?;
        let (dashboards, dashboards_shared) = match definition.dashboards.is_empty() {
            true => (HashMap::new(), 0),
            false => lay_each(
                folders,
                "dashboards",
                &definition.dashboards,
                |d| &d.name,
                |_| true,
                &before.dashboards,
            )?,
        };
        write_json(&next.join("model.json"), &ModelFile { version })?;
        // What the model kept will hold alone: its files the new one does not share.
        let version_file = fs::metadata(model.join("model.json")).map_or(0, |file| file.len());
        let alone = (before.bytes() + version_file)
            .saturating_sub(dimensions_shared + cubes_shared + dashboards_shared);

        // Keep the model being replaced as a numbered copy, then put the new one in its place.
        // If the process dies between the two renames, opening the directory finishes the job.
        let history = self.directory.join("data").join("models");
        fs::create_dir_all(&history)?;
        let numbered = (2..).map(|copy| format!("{}-{copy}", version - 1));
        let mut names = std::iter::once((version - 1).to_string()).chain(numbered);
        let previous = names.find(|name| !history.join(name).exists());
        let previous = previous.expect("the numbers never run out");
        fs::rename(&model, history.join(&previous))?;
        fs::rename(&next, &model)?;
        self.laid = Laid {
            dimensions,
            cubes,
            dashboards,
        };
        settle(&history).and_then(|()| settle(&self.directory))?;
        // Counted now, as only now is it known which files it shares; and kept, for when the
        // directory is opened again. Kept for the server's state alone, it fails no save:
        // lost, a model kept is counted whole.
        let mut counted = self
            .earlier_bytes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        counted.insert(previous, alone);
        let kept = history.join(KEPT_BYTES);
        if let Err(error) = write_json(&kept, &*counted) {
            eprintln!("warning: {}: {error}", kept.display());
        }
        Ok(())
    }

    fn is_full(&self) -> bool {
        self.log_bytes > LOG_LIMIT
    }

    fn bytes(&self) -> Option<OnDisk> {
        let data = self.directory.join("data");
        let snapshot = fs::metadata(data.join("snapshot.json"));
        Some(OnDisk {
            log: self.log_bytes,
            snapshot: snapshot.map_or(0, |file| file.len()),
            model: bytes_under(&self.directory.join("model")),
            earlier_models: self.earlier_bytes(&data.join("models")),
        })
    }

    /// The numbered copies under `data/models`, each a model's folder, newest first.
    fn earlier_models(&self) -> Vec<EarlierModel> {
        let Ok(entries) = fs::read_dir(self.directory.join("data").join("models")) else {
            return Vec::new();
        };
        let kept = |entry: fs::DirEntry| {
            let name = entry.file_name().into_string().ok()?;
            // A copy is named for its version, with "-2" and on where a version came twice.
            let version = name.split('-').next()?.parse().ok()?;
            let metadata = entry.metadata().ok().filter(fs::Metadata::is_dir)?;
            let saved = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok();
            Some(EarlierModel {
                name,
                version,
                saved: saved.map(|since| since.as_secs()),
            })
        };
        let mut models: Vec<EarlierModel> =
            entries.filter_map(Result::ok).filter_map(kept).collect();
        models.sort_by(|a, b| (b.version, &b.name).cmp(&(a.version, &a.name)));
        models
    }

    fn earlier_model(&self, name: &str) -> io::Result<Option<ModelDef>> {
        // Only a copy the history lists is read, so that no other folder can be asked for.
        if !self.earlier_models().iter().any(|model| model.name == name) {
            return Ok(None);
        }
        let folder = self.directory.join("data").join("models").join(name);
        load_model(&folder)
            .map(Some)
            .map_err(|error| io::Error::other(error.to_string()))
    }

    fn snapshot(&mut self, model_version: u64, cells: &NamedCells<'_>) -> io::Result<()> {
        let snapshot = Snapshot {
            model_version,
            stored: cells.counts(),
            cells,
            details: cells.details(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The lines a file of this text gives read from its end, the last first.
    fn backwards(text: &[u8]) -> Vec<Vec<u8>> {
        let path = std::env::temp_dir().join(format!(
            "epiphany-backwards-{}-{}",
            std::process::id(),
            text.len()
        ));
        fs::write(&path, text).unwrap();
        let lines = Backwards::new(File::open(&path).unwrap()).unwrap();
        let lines = lines.collect::<io::Result<Vec<_>>>().unwrap();
        fs::remove_file(&path).unwrap();
        lines
    }

    #[test]
    fn a_file_is_read_from_its_end_a_line_at_a_time_whatever_its_lines_lengths() {
        // Lines of every length about a block's, so that a block ends before, at and after a
        // line's end, and one line longer than three blocks.
        let block = BLOCK as usize;
        let lengths = [1, 7, block - 1, block, block + 1, 3 * block + 5, 2, 0, 9];
        let lines: Vec<Vec<u8>> = lengths
            .iter()
            .enumerate()
            .filter(|&(_, &length)| length > 0)
            .map(|(index, &length)| vec![b'a' + index as u8; length])
            .collect();
        let mut text = lengths
            .iter()
            .enumerate()
            .map(|(index, &length)| vec![b'a' + index as u8; length])
            .collect::<Vec<_>>()
            .join(&b'\n');
        text.push(b'\n');
        let mut last_first = lines.clone();
        last_first.reverse();
        assert_eq!(backwards(&text), last_first);
        // A last line cut short is given as it is, to be passed over as no JSON; nothing is
        // given of an empty file.
        text.extend_from_slice(b"{\"time\":12");
        assert_eq!(backwards(&text)[0], b"{\"time\":12");
        assert!(backwards(b"").is_empty());
        assert_eq!(backwards(b"only"), [b"only".to_vec()]);
    }

    #[test]
    fn a_lines_time_is_read_from_its_start_alone() {
        assert_eq!(
            time_of(b"{\"time\":1779999985,\"author\":null,\"cells\":[]}"),
            Some(1779999985)
        );
        assert_eq!(time_of(b"{\"author\":null,\"time\":5}"), None);
        assert_eq!(time_of(b"{\"time\":12"), None);
    }
}
