//! A durable cube store: an in-memory cube backed by a snapshot plus a WAL.
//!
//! The snapshot is the canonical model-as-code text (ADR-0003) written by
//! `epiphany-core`; it is the latest checkpoint of the whole cube. The WAL
//! (`crate::wal`) is the append-only tail of leaf writes since that checkpoint.
//! Recovery loads the snapshot, then replays the WAL tail. A checkpoint (the
//! explicit full-persist command) rewrites the snapshot and clears the WAL.
//!
//! The store owns the cube and only mutates it through `set_leaf`, so the
//! dimension structure never changes underneath the WAL: the element indices a
//! record names stay valid against the snapshot they replay onto.

use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use epiphany_core::{Cube, Fixed, LoadError, ModelError, SaveError};

use crate::wal::{self, Record};

const SNAPSHOT_FILE: &str = "snapshot.model";
const SNAPSHOT_TMP: &str = "snapshot.model.tmp";
const WAL_FILE: &str = "wal.log";

/// An error from the durability layer.
#[derive(Debug)]
pub enum PersistError {
    /// A filesystem operation failed.
    Io(std::io::Error),
    /// Replaying the WAL produced a write the model rejected.
    Model(ModelError),
    /// The snapshot could not be loaded.
    Load(LoadError),
    /// The snapshot could not be written.
    Save(SaveError),
    /// The WAL header was missing or unrecognized.
    Corrupt(String),
}

impl std::fmt::Display for PersistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PersistError::Io(e) => write!(f, "persistence I/O error: {e}"),
            PersistError::Model(e) => write!(f, "WAL replay rejected by model: {e}"),
            PersistError::Load(e) => write!(f, "could not load snapshot: {e}"),
            PersistError::Save(e) => write!(f, "could not write snapshot: {e}"),
            PersistError::Corrupt(m) => write!(f, "corrupt persistence: {m}"),
        }
    }
}

impl std::error::Error for PersistError {}

impl From<std::io::Error> for PersistError {
    fn from(e: std::io::Error) -> Self {
        PersistError::Io(e)
    }
}
impl From<ModelError> for PersistError {
    fn from(e: ModelError) -> Self {
        PersistError::Model(e)
    }
}
impl From<LoadError> for PersistError {
    fn from(e: LoadError) -> Self {
        PersistError::Load(e)
    }
}
impl From<SaveError> for PersistError {
    fn from(e: SaveError) -> Self {
        PersistError::Save(e)
    }
}

/// A cube made durable by a snapshot plus a write-ahead log in a directory.
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    cube: Cube,
    wal: File,
    uncheckpointed: u64,
    sync_on_write: bool,
    checkpoint_after: u64,
}

impl Store {
    /// Create a fresh store for `cube` in `dir`, writing the initial snapshot and
    /// an empty WAL. Any existing WAL in `dir` is replaced.
    pub fn create(dir: impl Into<PathBuf>, cube: Cube) -> Result<Self, PersistError> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        write_snapshot(&dir, &cube)?;
        let wal = open_fresh_wal(&dir)?;
        Ok(Self {
            dir,
            cube,
            wal,
            uncheckpointed: 0,
            sync_on_write: true,
            checkpoint_after: 0,
        })
    }

    /// Open an existing store in `dir`: load the snapshot, then replay the WAL
    /// tail. A trailing record torn by a crash is discarded and the WAL is
    /// truncated to its last intact write.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, PersistError> {
        let dir = dir.into();
        let mut cube = Cube::load_from_path(dir.join(SNAPSHOT_FILE))?;

        let wal_path = dir.join(WAL_FILE);
        let wal = if wal_path.exists() {
            let bytes = fs::read(&wal_path)?;
            let replay = wal::replay(&bytes).map_err(|e| PersistError::Corrupt(e.to_string()))?;
            for record in &replay.records {
                match record {
                    Record::SetLeaf { coord, value } => cube.set_leaf(coord, *value)?,
                    Record::SetString { coord, value } => cube.set_string(coord, value)?,
                }
            }
            // Drop any torn tail, then position at the end for new appends.
            let mut file = OpenOptions::new().write(true).open(&wal_path)?;
            file.set_len(replay.good_len)?;
            file.seek(SeekFrom::End(0))?;
            file
        } else {
            open_fresh_wal(&dir)?
        };

        Ok(Self {
            dir,
            cube,
            wal,
            uncheckpointed: 0,
            sync_on_write: true,
            checkpoint_after: 0,
        })
    }

    /// Open the store in `dir` if it exists, otherwise create it from `cube`.
    /// `cube` is only built (and only consumed) when creating.
    pub fn open_or_create(
        dir: impl Into<PathBuf>,
        cube: impl FnOnce() -> Cube,
    ) -> Result<Self, PersistError> {
        let dir = dir.into();
        if dir.join(SNAPSHOT_FILE).exists() {
            Self::open(dir)
        } else {
            Self::create(dir, cube())
        }
    }

    /// Whether each write is flushed to disk (`fsync`) before returning. On by
    /// default: every acknowledged write survives a crash. Turning it off trades
    /// durability for throughput (the WAL still frames every record).
    pub fn set_sync(&mut self, on: bool) {
        self.sync_on_write = on;
    }

    /// Automatically checkpoint after this many writes (0 disables it; the
    /// default). A checkpoint bounds WAL growth and recovery time.
    pub fn set_auto_checkpoint(&mut self, after: u64) {
        self.checkpoint_after = after;
    }

    /// The cube, for reads.
    pub fn cube(&self) -> &Cube {
        &self.cube
    }

    /// Write a leaf cell: apply it to the in-memory cube and append it to the
    /// WAL. The model validates the coordinate first, so a rejected write is
    /// never logged. May trigger an automatic checkpoint (see
    /// [`set_auto_checkpoint`](Self::set_auto_checkpoint)).
    pub fn set_leaf(&mut self, coord: &[u32], value: Fixed) -> Result<(), PersistError> {
        self.cube.set_leaf(coord, value)?;
        let framed = wal::encode(&Record::SetLeaf {
            coord: coord.to_vec(),
            value,
        });
        self.wal.write_all(&framed)?;
        if self.sync_on_write {
            self.wal.sync_data()?;
        }
        self.uncheckpointed += 1;
        if self.checkpoint_after != 0 && self.uncheckpointed >= self.checkpoint_after {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// Write a string cell: apply it to the in-memory cube and append it to the
    /// WAL. Like [`set_leaf`](Self::set_leaf), the model validates first and may
    /// trigger an automatic checkpoint.
    pub fn set_string(&mut self, coord: &[u32], value: &str) -> Result<(), PersistError> {
        self.cube.set_string(coord, value)?;
        let framed = wal::encode(&Record::SetString {
            coord: coord.to_vec(),
            value: value.to_string(),
        });
        self.wal.write_all(&framed)?;
        if self.sync_on_write {
            self.wal.sync_data()?;
        }
        self.uncheckpointed += 1;
        if self.checkpoint_after != 0 && self.uncheckpointed >= self.checkpoint_after {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// Full-persist: rewrite the snapshot from the current in-memory cube and
    /// clear the WAL. After this, recovery needs only the snapshot.
    pub fn checkpoint(&mut self) -> Result<(), PersistError> {
        write_snapshot(&self.dir, &self.cube)?;
        self.wal.set_len(0)?;
        self.wal.seek(SeekFrom::Start(0))?;
        self.wal.write_all(&wal::header())?;
        self.wal.sync_data()?;
        self.uncheckpointed = 0;
        Ok(())
    }

    /// Flush and fsync the WAL. Useful when [`set_sync`](Self::set_sync) is off.
    pub fn flush(&mut self) -> Result<(), PersistError> {
        self.wal.flush()?;
        self.wal.sync_data()?;
        Ok(())
    }
}

/// Write the snapshot atomically: serialize to a temp file, then rename over the
/// live snapshot (rename replaces the destination on all supported platforms).
fn write_snapshot(dir: &Path, cube: &Cube) -> Result<(), PersistError> {
    let tmp = dir.join(SNAPSHOT_TMP);
    let text = cube.to_model_text()?;
    fs::write(&tmp, text)?;
    fs::rename(&tmp, dir.join(SNAPSHOT_FILE))?;
    Ok(())
}

/// Create or truncate the WAL and write its header, leaving the cursor at the end
/// (ready to append).
fn open_fresh_wal(dir: &Path) -> Result<File, PersistError> {
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(dir.join(WAL_FILE))?;
    file.write_all(&wal::header())?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use epiphany_core::Dimension;

    /// A 2-D cube: Region (3 leaves under Total) x Period (2 leaves under Total).
    /// Returns the cube and the leaf/consolidated indices needed by tests.
    struct Fixture {
        cube: Cube,
        r: Vec<u32>,
        region_total: u32,
        p: Vec<u32>,
        period_total: u32,
    }

    fn fixture() -> Fixture {
        let mut region = Dimension::new("Region");
        let r: Vec<u32> = (0..3).map(|i| region.add_leaf(format!("R{i}"))).collect();
        let region_total = region.add_consolidated("Total");
        for &leaf in &r {
            region.add_child(region_total, leaf, 1).unwrap();
        }
        let mut period = Dimension::new("Period");
        let p: Vec<u32> = (0..2).map(|i| period.add_leaf(format!("P{i}"))).collect();
        let period_total = period.add_consolidated("Total");
        for &leaf in &p {
            period.add_child(period_total, leaf, 1).unwrap();
        }
        let cube = Cube::new("Sales", vec![region, period]).unwrap();
        Fixture {
            cube,
            r,
            region_total,
            p,
            period_total,
        }
    }

    /// A unique scratch directory for one test (cleaned up at the end).
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("epiphany-persist-{}-{name}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn create_then_open_round_trips_writes() {
        let dir = scratch("round-trip");
        let f = fixture();
        let (r, p, region_total, period_total) = (f.r, f.p, f.region_total, f.period_total);
        {
            let mut store = Store::create(&dir, f.cube).unwrap();
            store.set_leaf(&[r[0], p[0]], Fixed::from(10)).unwrap();
            store.set_leaf(&[r[1], p[0]], Fixed::from(20)).unwrap();
            store.set_leaf(&[r[0], p[1]], Fixed::from(30)).unwrap();
        }
        let store = Store::open(&dir).unwrap();
        assert_eq!(
            store.cube().get_leaf(&[r[0], p[0]]).unwrap(),
            Fixed::from(10)
        );
        assert_eq!(
            store.cube().get(&[region_total, period_total]).unwrap(),
            Fixed::from(60)
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn recovers_without_an_explicit_checkpoint() {
        // No checkpoint after the writes: recovery must come entirely from the
        // WAL replayed onto the empty initial snapshot (the crash case).
        let dir = scratch("crash-no-checkpoint");
        let f = fixture();
        let (r, p) = (f.r.clone(), f.p.clone());
        {
            let mut store = Store::create(&dir, f.cube).unwrap();
            for (i, &leaf) in r.iter().enumerate() {
                store
                    .set_leaf(&[leaf, p[0]], Fixed::from((i as i32 + 1) * 100))
                    .unwrap();
            }
            // Drop without checkpoint: simulates a crash with a populated WAL.
        }
        let store = Store::open(&dir).unwrap();
        assert_eq!(store.cube().cell_count(), r.len());
        assert_eq!(
            store.cube().get_leaf(&[r[2], p[0]]).unwrap(),
            Fixed::from(300)
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn checkpoint_clears_wal_and_preserves_state() {
        let dir = scratch("checkpoint");
        let f = fixture();
        let (r, p, region_total, period_total) = (f.r, f.p, f.region_total, f.period_total);
        {
            let mut store = Store::create(&dir, f.cube).unwrap();
            store.set_leaf(&[r[0], p[0]], Fixed::from(10)).unwrap();
            store.set_leaf(&[r[1], p[1]], Fixed::from(40)).unwrap();
            store.checkpoint().unwrap();
            // After a checkpoint the WAL holds only its header.
            let wal_len = fs::metadata(dir.join(WAL_FILE)).unwrap().len();
            assert_eq!(wal_len, wal::WAL_HEADER_LEN);
            // A further write lands in the (now-empty) WAL.
            store.set_leaf(&[r[2], p[0]], Fixed::from(5)).unwrap();
        }
        let store = Store::open(&dir).unwrap();
        // Pre-checkpoint writes come from the snapshot; the last from the WAL.
        assert_eq!(
            store.cube().get(&[region_total, period_total]).unwrap(),
            Fixed::from(55)
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discards_a_torn_trailing_record() {
        let dir = scratch("torn-tail");
        let f = fixture();
        let (r, p) = (f.r, f.p);
        {
            let mut store = Store::create(&dir, f.cube).unwrap();
            store.set_leaf(&[r[0], p[0]], Fixed::from(10)).unwrap();
            store.set_leaf(&[r[1], p[0]], Fixed::from(20)).unwrap();
        }
        // Simulate a crash mid-write: append a half-written record to the WAL.
        let wal_path = dir.join(WAL_FILE);
        let mut bytes = fs::read(&wal_path).unwrap();
        let intact = bytes.len();
        bytes.extend_from_slice(&9u32.to_le_bytes()); // claims a 9-byte payload
        bytes.extend_from_slice(&[1, 2, 0]); // but only 3 bytes follow
        fs::write(&wal_path, &bytes).unwrap();

        let store = Store::open(&dir).unwrap();
        assert_eq!(store.cube().cell_count(), 2);
        assert_eq!(
            store.cube().get_leaf(&[r[1], p[0]]).unwrap(),
            Fixed::from(20)
        );
        // Recovery truncated the torn tail back to the last intact write.
        assert_eq!(fs::metadata(&wal_path).unwrap().len() as usize, intact);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn recovers_string_writes_via_wal() {
        let dir = scratch("string-wal");
        let mut measure = Dimension::new("Measure");
        let sales = measure.add_leaf("Sales");
        let note = measure.add_string("Note");
        let cube = Cube::new("M", vec![measure]).unwrap();
        {
            let mut store = Store::create(&dir, cube).unwrap();
            store.set_leaf(&[sales], Fixed::from(5)).unwrap();
            store.set_string(&[note], "checked").unwrap();
            // Drop without checkpoint: both writes must replay from the WAL.
        }
        let store = Store::open(&dir).unwrap();
        assert_eq!(store.cube().get_leaf(&[sales]).unwrap(), Fixed::from(5));
        assert_eq!(store.cube().get_string(&[note]).unwrap(), Some("checked"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn open_or_create_builds_then_reopens() {
        let dir = scratch("open-or-create");
        let f = fixture();
        let (r, p) = (f.r.clone(), f.p.clone());
        let cube = f.cube;
        let built = std::cell::Cell::new(false);
        {
            let mut store = Store::open_or_create(&dir, || {
                built.set(true);
                cube
            })
            .unwrap();
            assert!(built.get(), "first call must build the cube");
            store.set_leaf(&[r[0], p[0]], Fixed::from(7)).unwrap();
        }
        // Second call finds the snapshot and must not build.
        let store = Store::open_or_create(&dir, || panic!("must not rebuild")).unwrap();
        assert_eq!(
            store.cube().get_leaf(&[r[0], p[0]]).unwrap(),
            Fixed::from(7)
        );
        fs::remove_dir_all(&dir).ok();
    }
}
