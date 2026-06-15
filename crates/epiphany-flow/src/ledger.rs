//! The durable run ledger (ADR-0013 decision 2): an append-only, framed log of
//! scheduled and API-submitted flow runs.
//!
//! It is primary state, but deliberately neither the durability WAL (ADR-0010
//! forbids job history there) nor the audit stream. It reuses the WAL/audit
//! framing ([len u32][payload][crc u32] behind a magic+version header) so a tail
//! torn by a crash is detected and truncated, never silently re-firing or
//! dropping a run. Recovery is non-gating, like the audit log: a missing or
//! corrupt-header file is re-initialized rather than failing startup.
//!
//! The ledger is append-only; a run's state transitions (`Queued -> Running ->
//! Succeeded`/`Failed`) are recorded as successive records for the same run id,
//! and the latest record for an id wins. On open, any run still `Queued` or
//! `Running` was interrupted by a crash and is recorded `Interrupted`; the
//! convergent reconcile loop then re-derives its firing as due. `fire_millis`
//! is the injected clock value frozen at firing (ADR-0013 decision 0), so the
//! ledger carries no wall-clock reads of its own.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const RUN_MAGIC: [u8; 6] = *b"EPIRUN";
const RUN_VERSION: u16 = 1;
const HEADER_LEN: u64 = 8;

/// How many runs the ledger retains (ADR-0013, Phase 8). The ledger is bounded so
/// a frequently-firing job cannot grow it without limit: once it holds more than
/// twice `max_runs` distinct runs (or accumulates superseded state-transition
/// records), it compacts to the retained set -- the newest `max_runs` runs, plus
/// each job's latest *successful* run so `last_succeeded_fire` never regresses and
/// re-fires a job whose history was trimmed. Compaction rewrites the file via a
/// temp-then-rename, so a crash leaves the prior file intact.
#[derive(Debug, Clone, Copy)]
pub struct RunRetention {
    /// Keep at most this many runs (`0` = unbounded).
    pub max_runs: usize,
}

impl Default for RunRetention {
    /// Unbounded.
    fn default() -> Self {
        Self { max_runs: 0 }
    }
}

/// The lifecycle state of a run (ADR-0013 decision 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// Allocated, not yet picked up by a worker.
    Queued,
    /// A worker is executing it.
    Running,
    /// The outcome committed.
    Succeeded,
    /// A flow, strip, input, or connector error (terminal).
    Failed,
    /// In flight when the process stopped; recovered on restart.
    Interrupted,
    /// Coalesced because a prior run of the same job was still active.
    Skipped,
}

impl RunState {
    fn as_byte(self) -> u8 {
        match self {
            RunState::Queued => 1,
            RunState::Running => 2,
            RunState::Succeeded => 3,
            RunState::Failed => 4,
            RunState::Interrupted => 5,
            RunState::Skipped => 6,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            1 => RunState::Queued,
            2 => RunState::Running,
            3 => RunState::Succeeded,
            4 => RunState::Failed,
            5 => RunState::Interrupted,
            6 => RunState::Skipped,
            _ => return None,
        })
    }

    /// The canonical lowercase token (for the REST view).
    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Queued => "queued",
            RunState::Running => "running",
            RunState::Succeeded => "succeeded",
            RunState::Failed => "failed",
            RunState::Interrupted => "interrupted",
            RunState::Skipped => "skipped",
        }
    }

    /// Whether the run is still active (so a same-job firing must coalesce).
    pub fn is_active(self) -> bool {
        matches!(self, RunState::Queued | RunState::Running)
    }
}

/// One run, scheduled or API-submitted. The report counts are zero until the run
/// reaches a terminal state. `error` is empty unless the run failed. No secrets
/// or cell payloads cross this boundary (RG-13): only object identities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    /// The run id. For a scheduled run it is a deterministic function of the
    /// firing, so a re-derived firing reuses it and the ledger dedupes.
    pub id: String,
    /// The cube the run writes.
    pub cube: String,
    /// The job (when `is_job`) or flow (manual run) this run executes.
    pub target: String,
    /// Whether `target` names a job (vs a single flow).
    pub is_job: bool,
    /// The injected clock value frozen at firing (ADR-0013 decision 0).
    pub fire_millis: u64,
    /// The current lifecycle state.
    pub state: RunState,
    /// Rows read (aggregate across steps), at a terminal state.
    pub rows_read: u64,
    /// Cells written (aggregate across steps), at a terminal state.
    pub cells_written: u64,
    /// Elements added (aggregate across steps), at a terminal state.
    pub elements_added: u64,
    /// A human-readable failure message, or empty.
    pub error: String,
    /// The principal the run ran as (the scheduler service principal, or the
    /// submitting user).
    pub principal: String,
}

/// The append-only run ledger: a durable file plus an in-memory index keyed by
/// run id (latest record wins).
#[derive(Debug)]
pub struct RunLedger {
    file: Option<File>,
    /// The backing path, kept so the ledger can be compacted in place (`None` for
    /// an in-memory ledger).
    path: Option<PathBuf>,
    policy: RunRetention,
    /// Full append history, in order.
    records: Vec<RunRecord>,
    /// Run id -> index of its latest record in `records`.
    latest: HashMap<String, usize>,
}

impl RunLedger {
    /// Open (or create) the ledger at `path`, recovering existing records and
    /// re-recording any interrupted (in-flight at crash) run as `Interrupted`.
    /// Non-gating: a torn tail is truncated and a missing/corrupt-header file is
    /// re-initialized, never erroring out of startup.
    pub fn open(path: PathBuf) -> io::Result<Self> {
        Self::open_with_policy(path, RunRetention::default())
    }

    /// Open the ledger with a retention `policy` (ADR-0013, Phase 8), enforced
    /// after recovery so reopening with a tighter bound compacts an oversized
    /// ledger on startup.
    pub fn open_with_policy(path: PathBuf, policy: RunRetention) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let existing = if path.exists() {
            replay(&fs::read(&path)?)
        } else {
            None
        };
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let records = match existing {
            Some((records, good_len)) => {
                file.set_len(good_len)?;
                file.seek(SeekFrom::End(0))?;
                records
            }
            None => {
                file.set_len(0)?;
                file.seek(SeekFrom::Start(0))?;
                file.write_all(&header())?;
                file.sync_data()?;
                Vec::new()
            }
        };
        let mut ledger = RunLedger {
            file: Some(file),
            path: Some(path),
            policy,
            records: Vec::new(),
            latest: HashMap::new(),
        };
        for record in records {
            ledger.index(record);
        }
        ledger.recover_interrupted()?;
        ledger.enforce_retention()?;
        Ok(ledger)
    }

    /// An in-memory ledger with no backing file: a hermetic test seam.
    pub fn in_memory() -> Self {
        RunLedger {
            file: None,
            path: None,
            policy: RunRetention::default(),
            records: Vec::new(),
            latest: HashMap::new(),
        }
    }

    /// Index a record into the in-memory maps (does not touch the file).
    fn index(&mut self, record: RunRecord) {
        let id = record.id.clone();
        self.records.push(record);
        self.latest.insert(id, self.records.len() - 1);
    }

    /// Append a run record (a new run or a state transition), fsync'd. Enforces
    /// the retention policy afterward (a no-op when unbounded or within bounds).
    pub fn append(&mut self, record: RunRecord) -> io::Result<()> {
        if let Some(file) = &mut self.file {
            file.write_all(&encode(&record))?;
            file.sync_data()?;
        }
        self.index(record);
        self.enforce_retention()?;
        Ok(())
    }

    /// Enforce the retention policy: compact to the retained set when the ledger
    /// grows past twice the cap in distinct runs, or accumulates many superseded
    /// state-transition records. Retains the newest `max_runs` runs plus each
    /// job's latest successful run (so `last_succeeded_fire` never regresses).
    fn enforce_retention(&mut self) -> io::Result<()> {
        let max = self.policy.max_runs;
        if max == 0 {
            return Ok(());
        }
        let distinct = self.latest.len();
        let over_distinct = distinct >= max.saturating_mul(2);
        // Many superseded records (each run keeps Queued/Running/terminal): compact
        // once the history is well past the distinct-run count.
        let over_history =
            self.records.len() >= distinct.saturating_mul(2).max(max.saturating_mul(2));
        if !over_distinct && !over_history {
            return Ok(());
        }

        // The latest record of every distinct run.
        let mut latest: Vec<RunRecord> = self
            .latest
            .values()
            .map(|&i| self.records[i].clone())
            .collect();

        // Always keep each job's latest *successful* run, so a trimmed history
        // never makes a job look unfired and re-derive prematurely.
        let mut keep: HashSet<String> = HashSet::new();
        let mut best_success: HashMap<(String, String), (u64, String)> = HashMap::new();
        for r in &latest {
            if r.is_job && r.state == RunState::Succeeded {
                let entry = best_success
                    .entry((r.cube.clone(), r.target.clone()))
                    .or_insert((0, String::new()));
                if r.fire_millis >= entry.0 {
                    *entry = (r.fire_millis, r.id.clone());
                }
            }
        }
        for (_, id) in best_success.values() {
            keep.insert(id.clone());
        }
        // Plus the newest `max_runs` runs (for the recent-runs query).
        latest.sort_by(|a, b| {
            b.fire_millis
                .cmp(&a.fire_millis)
                .then_with(|| a.id.cmp(&b.id))
        });
        for r in latest.iter().take(max) {
            keep.insert(r.id.clone());
        }

        // Nothing to drop (only superseded intermediates collapse): still worth a
        // rewrite if the file holds more than the distinct latest records.
        let mut retained: Vec<RunRecord> = latest
            .into_iter()
            .filter(|r| keep.contains(&r.id))
            .collect();
        if retained.len() == self.records.len() {
            return Ok(());
        }
        // Canonical order: oldest fire first, id breaking ties.
        retained.sort_by(|a, b| {
            a.fire_millis
                .cmp(&b.fire_millis)
                .then_with(|| a.id.cmp(&b.id))
        });
        if let Some(path) = self.path.clone() {
            self.file = Some(rewrite_file(&path, &retained)?);
        }
        self.records.clear();
        self.latest.clear();
        for record in retained {
            self.index(record);
        }
        Ok(())
    }

    /// On open, any run whose latest state is still active was interrupted by a
    /// crash; record it `Interrupted` so the convergent loop re-derives its
    /// firing as due (ADR-0013 decision 5).
    fn recover_interrupted(&mut self) -> io::Result<()> {
        let interrupted: Vec<RunRecord> = self
            .latest
            .values()
            .map(|&i| &self.records[i])
            .filter(|r| r.state.is_active())
            .map(|r| RunRecord {
                state: RunState::Interrupted,
                error: "interrupted by restart".to_string(),
                ..r.clone()
            })
            .collect();
        for record in interrupted {
            self.append(record)?;
        }
        Ok(())
    }

    /// The latest record for a run id, if any.
    pub fn get(&self, id: &str) -> Option<&RunRecord> {
        self.latest.get(id).map(|&i| &self.records[i])
    }

    /// Whether a run id is already known (dedup check for re-derived firings).
    pub fn contains(&self, id: &str) -> bool {
        self.latest.contains_key(id)
    }

    /// The fire time of the most recent *successful* run of a job, or `None`.
    /// This is the `last_fired` the reconcile loop computes `next_due` from;
    /// because it advances only on success, an interrupted firing re-derives.
    pub fn last_succeeded_fire(&self, cube: &str, job: &str) -> Option<u64> {
        self.records
            .iter()
            .filter(|r| {
                r.is_job && r.cube == cube && r.target == job && r.state == RunState::Succeeded
            })
            .map(|r| r.fire_millis)
            .max()
    }

    /// Whether a job currently has an active (queued or running) run, for the
    /// single-flight overlap policy (ADR-0013 decision 6).
    pub fn job_in_flight(&self, cube: &str, job: &str) -> bool {
        self.latest.values().any(|&i| {
            let r = &self.records[i];
            r.is_job && r.cube == cube && r.target == job && r.state.is_active()
        })
    }

    /// The latest record per run id (deduped), newest firing first. Test/REST view.
    fn latest_records(&self) -> Vec<&RunRecord> {
        let mut out: Vec<&RunRecord> = self.latest.values().map(|&i| &self.records[i]).collect();
        // Newest firing first; id breaks ties deterministically.
        out.sort_by(|a, b| {
            b.fire_millis
                .cmp(&a.fire_millis)
                .then_with(|| a.id.cmp(&b.id))
        });
        out
    }

    /// Recent runs for a cube (latest state per run), newest first, capped.
    pub fn recent(&self, cube: &str, limit: usize) -> Vec<RunRecord> {
        self.latest_records()
            .into_iter()
            .filter(|r| r.cube == cube)
            .take(limit)
            .cloned()
            .collect()
    }

    /// Recent runs of one job (latest state per run), newest first.
    pub fn runs_for_job(&self, cube: &str, job: &str) -> Vec<RunRecord> {
        self.latest_records()
            .into_iter()
            .filter(|r| r.is_job && r.cube == cube && r.target == job)
            .cloned()
            .collect()
    }

    /// Total distinct runs recorded (for tests and metrics).
    pub fn len(&self) -> usize {
        self.latest.len()
    }

    /// Whether the ledger holds no runs.
    pub fn is_empty(&self) -> bool {
        self.latest.is_empty()
    }
}

fn header() -> [u8; 8] {
    let mut h = [0u8; 8];
    h[..6].copy_from_slice(&RUN_MAGIC);
    h[6..].copy_from_slice(&RUN_VERSION.to_le_bytes());
    h
}

/// Rewrite the ledger file to exactly `records` (a compaction), crash-safely:
/// write a sibling temp file (header + framed records), fsync, rename over the
/// path, and return a fresh append-positioned handle. A crash before the rename
/// leaves the original file intact.
fn rewrite_file(path: &Path, records: &[RunRecord]) -> io::Result<File> {
    let tmp = path.with_extension("compact");
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(&header())?;
        for record in records {
            f.write_all(&encode(record))?;
        }
        f.sync_data()?;
    }
    fs::rename(&tmp, path)?;
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    f.seek(SeekFrom::End(0))?;
    Ok(f)
}

fn write_str(payload: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    payload.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    payload.extend_from_slice(bytes);
}

fn read_str(rest: &[u8], pos: &mut usize) -> Option<String> {
    let len = u32::from_le_bytes(rest.get(*pos..*pos + 4)?.try_into().ok()?) as usize;
    *pos += 4;
    let s = std::str::from_utf8(rest.get(*pos..*pos + len)?)
        .ok()?
        .to_string();
    *pos += len;
    Some(s)
}

fn encode(record: &RunRecord) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&record.fire_millis.to_le_bytes());
    payload.push(record.state.as_byte());
    payload.push(u8::from(record.is_job));
    payload.extend_from_slice(&record.rows_read.to_le_bytes());
    payload.extend_from_slice(&record.cells_written.to_le_bytes());
    payload.extend_from_slice(&record.elements_added.to_le_bytes());
    write_str(&mut payload, &record.id);
    write_str(&mut payload, &record.cube);
    write_str(&mut payload, &record.target);
    write_str(&mut payload, &record.error);
    write_str(&mut payload, &record.principal);
    let mut framed = Vec::with_capacity(payload.len() + 8);
    framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    framed.extend_from_slice(&payload);
    framed.extend_from_slice(&crc32(&payload).to_le_bytes());
    framed
}

fn decode(payload: &[u8]) -> Option<RunRecord> {
    let fire_millis = u64::from_le_bytes(payload.get(0..8)?.try_into().ok()?);
    let state = RunState::from_byte(*payload.get(8)?)?;
    let is_job = *payload.get(9)? != 0;
    let rows_read = u64::from_le_bytes(payload.get(10..18)?.try_into().ok()?);
    let cells_written = u64::from_le_bytes(payload.get(18..26)?.try_into().ok()?);
    let elements_added = u64::from_le_bytes(payload.get(26..34)?.try_into().ok()?);
    let rest = payload.get(34..)?;
    let mut pos = 0;
    let id = read_str(rest, &mut pos)?;
    let cube = read_str(rest, &mut pos)?;
    let target = read_str(rest, &mut pos)?;
    let error = read_str(rest, &mut pos)?;
    let principal = read_str(rest, &mut pos)?;
    Some(RunRecord {
        id,
        cube,
        target,
        is_job,
        fire_millis,
        state,
        rows_read,
        cells_written,
        elements_added,
        error,
        principal,
    })
}

/// Scan the file; return the intact records and the length of intact framing, or
/// `None` if the header is missing/unrecognized (the caller re-initializes).
fn replay(bytes: &[u8]) -> Option<(Vec<RunRecord>, u64)> {
    if bytes.len() < HEADER_LEN as usize
        || bytes[..6] != RUN_MAGIC
        || u16::from_le_bytes([bytes[6], bytes[7]]) != RUN_VERSION
    {
        return None;
    }
    let mut records = Vec::new();
    let mut pos = HEADER_LEN as usize;
    let mut good = HEADER_LEN;
    while pos + 4 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let frame_end = pos + 4 + len + 4;
        if frame_end > bytes.len() {
            break;
        }
        let payload = &bytes[pos + 4..pos + 4 + len];
        let crc = u32::from_le_bytes(bytes[pos + 4 + len..frame_end].try_into().unwrap());
        if crc32(payload) != crc {
            break;
        }
        match decode(payload) {
            Some(record) => {
                records.push(record);
                good = frame_end as u64;
            }
            None => break,
        }
        pos = frame_end;
    }
    Some((records, good))
}

/// CRC32 (IEEE 802.3), table generated at compile time. Mirrors the WAL's and the
/// audit stream's, so the ledger carries no extra dependency.
const fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static CRC32_TABLE: [u32; 256] = crc32_table();

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc = (crc >> 8) ^ CRC32_TABLE[((crc ^ u32::from(b)) & 0xFF) as usize];
    }
    crc ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("epiphany-ledger-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        dir.join("runs.log")
    }

    fn run(id: &str, job: &str, fire: u64, state: RunState) -> RunRecord {
        RunRecord {
            id: id.to_string(),
            cube: "Sales".to_string(),
            target: job.to_string(),
            is_job: true,
            fire_millis: fire,
            state,
            rows_read: 0,
            cells_written: 0,
            elements_added: 0,
            error: String::new(),
            principal: "scheduler".to_string(),
        }
    }

    #[test]
    fn append_and_latest_state_wins_across_reopen() {
        let path = scratch("reopen");
        {
            let mut l = RunLedger::open(path.clone()).unwrap();
            l.append(run("r1", "nightly", 1000, RunState::Queued))
                .unwrap();
            l.append(run("r1", "nightly", 1000, RunState::Running))
                .unwrap();
            l.append(run("r1", "nightly", 1000, RunState::Succeeded))
                .unwrap();
        }
        let l = RunLedger::open(path).unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l.get("r1").unwrap().state, RunState::Succeeded);
        assert_eq!(l.last_succeeded_fire("Sales", "nightly"), Some(1000));
    }

    #[test]
    fn in_flight_run_recovers_as_interrupted_and_does_not_count_as_fired() {
        let path = scratch("interrupted");
        {
            let mut l = RunLedger::open(path.clone()).unwrap();
            l.append(run("r1", "nightly", 1000, RunState::Running))
                .unwrap();
        }
        // Reopen: the in-flight run is recovered as Interrupted, so the job has no
        // successful fire and the loop will re-derive it as due.
        let l = RunLedger::open(path).unwrap();
        assert_eq!(l.get("r1").unwrap().state, RunState::Interrupted);
        assert_eq!(l.last_succeeded_fire("Sales", "nightly"), None);
        assert!(!l.job_in_flight("Sales", "nightly"));
    }

    #[test]
    fn torn_tail_is_truncated_without_dropping_intact_runs() {
        let path = scratch("torn");
        {
            let mut l = RunLedger::open(path.clone()).unwrap();
            l.append(run("r1", "nightly", 1000, RunState::Succeeded))
                .unwrap();
        }
        let mut bytes = std::fs::read(&path).unwrap();
        let intact = bytes.len();
        // A half-written next frame.
        bytes.extend_from_slice(&9u32.to_le_bytes());
        bytes.extend_from_slice(&[1, 2, 3]);
        std::fs::write(&path, &bytes).unwrap();
        let l = RunLedger::open(path.clone()).unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l.get("r1").unwrap().state, RunState::Succeeded);
        assert_eq!(std::fs::metadata(&path).unwrap().len() as usize, intact);
    }

    #[test]
    fn retention_bounds_growth_but_keeps_each_jobs_latest_success() {
        let path = scratch("retain");
        let policy = RunRetention { max_runs: 4 };
        let mut l = RunLedger::open_with_policy(path, policy).unwrap();
        // One early success for job "a", then many runs of job "b" that push past
        // the cap. Job "a"'s success must be retained so it does not re-fire.
        l.append(run("a@1", "a", 1, RunState::Succeeded)).unwrap();
        for i in 0..20 {
            let id = format!("b@{i}");
            l.append(run(&id, "b", 100 + i, RunState::Succeeded))
                .unwrap();
        }
        // Bounded near the cap (compaction is amortized at 2x), well under the
        // unbounded count of 21.
        assert!(l.len() <= 2 * 4 + 1, "ledger stays bounded: {}", l.len());
        // Job "a"'s last success is preserved despite the flood of "b" runs.
        assert_eq!(l.last_succeeded_fire("Sales", "a"), Some(1));
        // Job "b"'s latest success is the newest.
        assert_eq!(l.last_succeeded_fire("Sales", "b"), Some(119));
    }

    #[test]
    fn dedup_and_single_flight_helpers() {
        let mut l = RunLedger::in_memory();
        l.append(run("r1", "nightly", 1000, RunState::Running))
            .unwrap();
        assert!(l.contains("r1"));
        assert!(l.job_in_flight("Sales", "nightly"));
        l.append(run("r1", "nightly", 1000, RunState::Succeeded))
            .unwrap();
        assert!(!l.job_in_flight("Sales", "nightly"));
        assert_eq!(l.recent("Sales", 10).len(), 1);
    }
}
