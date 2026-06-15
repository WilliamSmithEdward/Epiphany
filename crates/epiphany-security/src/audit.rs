//! The audit stream (ADR-0010): an append-only, framed log of security-relevant
//! and model-changing actions, separate from the durability WAL.
//!
//! Framing mirrors the WAL ([len u32][payload][crc u32], little-endian, behind an
//! 8-byte magic+version header) so a tail torn by a crash is detected and
//! discarded. Unlike the WAL, recovery is **non-gating**: a missing, short, or
//! corrupt-header file is logged and re-initialized rather than failing startup,
//! because the audit stream must never block recovery of live state. Records
//! carry no secrets or PII (RG-13): only object identities, never credentials,
//! tokens, or cell payloads. Timestamps come from the injected clock (ADR-0009),
//! so audit output is deterministic in tests.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const AUDIT_MAGIC: [u8; 6] = *b"EPIAUD";
const AUDIT_VERSION: u16 = 1;
const HEADER_LEN: u64 = 8;

/// A security-relevant or model-changing action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditAction {
    Login,
    Logout,
    AccessDenied,
    UserChange,
    GroupChange,
    ObjectCreate,
    ObjectUpdate,
    ObjectDelete,
    FlowExec,
    JobExec,
    SandboxCommit,
    SandboxDiscard,
    Checkpoint,
}

impl AuditAction {
    fn as_byte(self) -> u8 {
        match self {
            AuditAction::Login => 1,
            AuditAction::Logout => 2,
            AuditAction::AccessDenied => 3,
            AuditAction::UserChange => 4,
            AuditAction::GroupChange => 5,
            AuditAction::ObjectCreate => 6,
            AuditAction::ObjectUpdate => 7,
            AuditAction::ObjectDelete => 8,
            AuditAction::FlowExec => 9,
            AuditAction::JobExec => 10,
            AuditAction::SandboxCommit => 11,
            AuditAction::SandboxDiscard => 12,
            AuditAction::Checkpoint => 13,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            1 => AuditAction::Login,
            2 => AuditAction::Logout,
            3 => AuditAction::AccessDenied,
            4 => AuditAction::UserChange,
            5 => AuditAction::GroupChange,
            6 => AuditAction::ObjectCreate,
            7 => AuditAction::ObjectUpdate,
            8 => AuditAction::ObjectDelete,
            9 => AuditAction::FlowExec,
            10 => AuditAction::JobExec,
            11 => AuditAction::SandboxCommit,
            12 => AuditAction::SandboxDiscard,
            13 => AuditAction::Checkpoint,
            _ => return None,
        })
    }

    /// The canonical lowercase token (for the REST filter and display).
    pub fn as_str(self) -> &'static str {
        match self {
            AuditAction::Login => "login",
            AuditAction::Logout => "logout",
            AuditAction::AccessDenied => "access_denied",
            AuditAction::UserChange => "user_change",
            AuditAction::GroupChange => "group_change",
            AuditAction::ObjectCreate => "object_create",
            AuditAction::ObjectUpdate => "object_update",
            AuditAction::ObjectDelete => "object_delete",
            AuditAction::FlowExec => "flow_exec",
            AuditAction::JobExec => "job_exec",
            AuditAction::SandboxCommit => "sandbox_commit",
            AuditAction::SandboxDiscard => "sandbox_discard",
            AuditAction::Checkpoint => "checkpoint",
        }
    }

    /// Parse a token; `None` if unrecognized.
    pub fn parse(s: &str) -> Option<Self> {
        (1..=13)
            .map(|b| AuditAction::from_byte(b).unwrap())
            .find(|a| a.as_str() == s)
    }
}

/// One audit entry. `object_kind`/`target` are empty when an action has no
/// object (login, logout). `allowed` is false only for a denial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRecord {
    pub seq: u64,
    pub timestamp_millis: u64,
    pub actor: String,
    pub action: AuditAction,
    pub object_kind: String,
    pub target: String,
    pub allowed: bool,
}

/// A query over the audit stream; every field is an optional narrowing filter.
#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    pub actor: Option<String>,
    pub action: Option<AuditAction>,
    pub target: Option<String>,
    pub allowed: Option<bool>,
    pub from: Option<u64>,
    pub to: Option<u64>,
    pub offset: usize,
    pub limit: Option<usize>,
}

/// How long the audit log is retained (ADR-0010, Phase 8). The log is bounded so
/// it cannot grow without limit; when it exceeds the bound it is compacted to the
/// retained tail (the file is rewritten via a temp-then-rename, so a crash during
/// compaction leaves the previous file intact). Sequence numbers stay monotonic
/// across a compaction; older records are dropped, not renumbered.
#[derive(Debug, Clone, Copy)]
pub struct RetentionPolicy {
    /// Keep at most this many records (`0` = unbounded). The log compacts to the
    /// newest `max_records` once it grows past twice that.
    pub max_records: usize,
    /// Drop records older than this many milliseconds before the newest record
    /// (`None` = no age limit). Ages use the injected timestamps, never the wall
    /// clock, so retention is deterministic in tests.
    pub max_age_millis: Option<u64>,
}

impl Default for RetentionPolicy {
    /// Unbounded: keep everything (the pre-Phase-8 behavior).
    fn default() -> Self {
        Self {
            max_records: 0,
            max_age_millis: None,
        }
    }
}

/// The append-only audit log: a durable file plus an in-memory index for query.
/// Holds its own file handle and is guarded by its own lock at the API layer, so
/// audit writes do not serialize behind the security mutex.
#[derive(Debug)]
pub struct AuditLog {
    file: Option<File>,
    /// The backing path, kept so the log can be compacted in place (`None` for an
    /// in-memory log).
    path: Option<PathBuf>,
    policy: RetentionPolicy,
    records: Vec<AuditRecord>,
    next_seq: u64,
}

impl AuditLog {
    /// Open (or create) the audit log at `path`, recovering existing records,
    /// with no retention bound (keeps everything). Non-gating: a torn tail is
    /// truncated and a missing/corrupt-header file is re-initialized.
    pub fn open(path: PathBuf) -> io::Result<Self> {
        Self::open_with_policy(path, RetentionPolicy::default())
    }

    /// Open the audit log with a retention `policy` (ADR-0010, Phase 8). After
    /// recovery the policy is enforced immediately, so reopening with a tighter
    /// bound compacts an oversized log on startup.
    pub fn open_with_policy(path: PathBuf, policy: RetentionPolicy) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let existing = if path.exists() {
            replay(&fs::read(&path)?)
        } else {
            None
        };
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        crate::set_owner_only(&mut opts); // owner-only from creation (ADR-0017)
        let mut file = opts.open(&path)?;
        crate::restrict_to_owner(&path)?; // normalize a pre-existing file too
        let (records, next_seq) = match existing {
            Some((records, good_len)) => {
                // Drop a torn tail, then position at the end for new appends.
                file.set_len(good_len)?;
                file.seek(SeekFrom::End(0))?;
                let next = records.iter().map(|r| r.seq).max().map_or(0, |m| m + 1);
                (records, next)
            }
            None => {
                // New, or a missing/corrupt header: reset to a clean header.
                file.set_len(0)?;
                file.seek(SeekFrom::Start(0))?;
                file.write_all(&header())?;
                file.sync_data()?;
                (Vec::new(), 0)
            }
        };
        let mut log = AuditLog {
            file: Some(file),
            path: Some(path),
            policy,
            records,
            next_seq,
        };
        log.enforce_retention()?;
        Ok(log)
    }

    /// An in-memory audit log with no backing file: records are queryable but not
    /// persisted. A hermetic test seam, mirroring `SecurityStore::with_admin`.
    pub fn in_memory() -> Self {
        AuditLog {
            file: None,
            path: None,
            policy: RetentionPolicy::default(),
            records: Vec::new(),
            next_seq: 0,
        }
    }

    /// Append one record (assigning the next sequence number) and fsync it.
    /// `timestamp_millis` is supplied by the caller from the injected clock.
    #[allow(clippy::too_many_arguments)]
    pub fn append(
        &mut self,
        timestamp_millis: u64,
        actor: impl Into<String>,
        action: AuditAction,
        object_kind: impl Into<String>,
        target: impl Into<String>,
        allowed: bool,
    ) -> io::Result<u64> {
        let record = AuditRecord {
            seq: self.next_seq,
            timestamp_millis,
            actor: actor.into(),
            action,
            object_kind: object_kind.into(),
            target: target.into(),
            allowed,
        };
        if let Some(file) = &mut self.file {
            file.write_all(&encode(&record))?;
            file.sync_data()?;
        }
        let seq = record.seq;
        self.records.push(record);
        self.next_seq += 1;
        self.enforce_retention()?;
        Ok(seq)
    }

    /// Enforce the retention policy (ADR-0010, Phase 8): when the log exceeds its
    /// bound, compact to the retained tail. Cheap when within bounds (a length and
    /// one-timestamp check); a compaction rewrites the file via temp-then-rename so
    /// a crash mid-rewrite leaves the prior file intact, and keeps in-memory and
    /// on-disk identical.
    fn enforce_retention(&mut self) -> io::Result<()> {
        let max = self.policy.max_records;
        if max == 0 && self.policy.max_age_millis.is_none() {
            return Ok(());
        }
        // Drop records older than `max_age` before the newest one.
        let age_cut = self.policy.max_age_millis.and_then(|age| {
            self.records
                .last()
                .map(|r| r.timestamp_millis.saturating_sub(age))
        });
        // Compact lazily: only once the log has grown to twice the cap (so a
        // compaction amortizes over many appends), or an aged-out record is present.
        let over_count = max != 0 && self.records.len() >= max.saturating_mul(2);
        let over_age = age_cut.is_some_and(|cut| {
            self.records
                .first()
                .is_some_and(|r| r.timestamp_millis < cut)
        });
        if !over_count && !over_age {
            return Ok(());
        }
        let start = if max == 0 {
            0
        } else {
            self.records.len().saturating_sub(max)
        };
        let mut retained: Vec<AuditRecord> = self.records[start..].to_vec();
        if let Some(cut) = age_cut {
            retained.retain(|r| r.timestamp_millis >= cut);
        }
        if let Some(path) = self.path.clone() {
            self.file = Some(rewrite_file(&path, &retained)?);
        }
        self.records = retained;
        Ok(())
    }

    /// Query the in-memory record index with a filter (newest-last order).
    pub fn query(&self, filter: &AuditFilter) -> Vec<AuditRecord> {
        self.records
            .iter()
            .filter(|r| filter.actor.as_ref().is_none_or(|a| &r.actor == a))
            .filter(|r| filter.action.is_none_or(|a| r.action == a))
            .filter(|r| filter.target.as_ref().is_none_or(|t| &r.target == t))
            .filter(|r| filter.allowed.is_none_or(|o| r.allowed == o))
            .filter(|r| filter.from.is_none_or(|f| r.timestamp_millis >= f))
            .filter(|r| filter.to.is_none_or(|t| r.timestamp_millis <= t))
            .skip(filter.offset)
            .take(filter.limit.unwrap_or(usize::MAX))
            .cloned()
            .collect()
    }

    /// Total records held (for tests and metrics).
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the log is empty.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

fn header() -> [u8; 8] {
    let mut h = [0u8; 8];
    h[..6].copy_from_slice(&AUDIT_MAGIC);
    h[6..].copy_from_slice(&AUDIT_VERSION.to_le_bytes());
    h
}

/// Rewrite the audit file to exactly `records` (a compaction), crash-safely: write
/// a sibling temp file (header + framed records), fsync, then rename over the
/// path, and return a fresh append-positioned handle. A crash before the rename
/// leaves the original file intact.
fn rewrite_file(path: &Path, records: &[AuditRecord]) -> io::Result<File> {
    let tmp = path.with_extension("compact");
    {
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        crate::set_owner_only(&mut opts); // owner-only from creation (ADR-0017)
        let mut f = opts.open(&tmp)?;
        f.write_all(&header())?;
        for record in records {
            f.write_all(&encode(record))?;
        }
        f.sync_data()?;
    }
    crate::restrict_to_owner(&tmp)?; // normalize a pre-existing temp too
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

fn encode(record: &AuditRecord) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&record.seq.to_le_bytes());
    payload.extend_from_slice(&record.timestamp_millis.to_le_bytes());
    payload.push(record.action.as_byte());
    payload.push(u8::from(record.allowed));
    write_str(&mut payload, &record.actor);
    write_str(&mut payload, &record.object_kind);
    write_str(&mut payload, &record.target);
    let mut framed = Vec::with_capacity(payload.len() + 8);
    framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    framed.extend_from_slice(&payload);
    framed.extend_from_slice(&crc32(&payload).to_le_bytes());
    framed
}

fn decode(payload: &[u8]) -> Option<AuditRecord> {
    let seq = u64::from_le_bytes(payload.get(0..8)?.try_into().ok()?);
    let timestamp_millis = u64::from_le_bytes(payload.get(8..16)?.try_into().ok()?);
    let action = AuditAction::from_byte(*payload.get(16)?)?;
    let allowed = *payload.get(17)? != 0;
    let rest = payload.get(18..)?;
    let mut pos = 0;
    let actor = read_str(rest, &mut pos)?;
    let object_kind = read_str(rest, &mut pos)?;
    let target = read_str(rest, &mut pos)?;
    Some(AuditRecord {
        seq,
        timestamp_millis,
        actor,
        action,
        object_kind,
        target,
        allowed,
    })
}

/// Scan the file; return the intact records and the length of intact framing, or
/// `None` if the header is missing/unrecognized (the caller re-initializes).
fn replay(bytes: &[u8]) -> Option<(Vec<AuditRecord>, u64)> {
    if bytes.len() < HEADER_LEN as usize
        || bytes[..6] != AUDIT_MAGIC
        || u16::from_le_bytes([bytes[6], bytes[7]]) != AUDIT_VERSION
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

/// CRC32 (IEEE 802.3), table generated at compile time. Mirrors the WAL's, so the
/// audit stream carries no extra dependency.
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
            std::env::temp_dir().join(format!("epiphany-audit-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        dir.join("audit.log")
    }

    fn append_sample(log: &mut AuditLog, ts: u64, actor: &str, action: AuditAction) {
        log.append(ts, actor, action, "cube", "Sales", true)
            .unwrap();
    }

    #[test]
    fn appends_survive_reopen_in_order_with_monotonic_seq() {
        let path = scratch("reopen");
        {
            let mut log = AuditLog::open(path.clone()).unwrap();
            append_sample(&mut log, 100, "ann", AuditAction::Login);
            append_sample(&mut log, 200, "ann", AuditAction::ObjectCreate);
            append_sample(&mut log, 300, "bob", AuditAction::AccessDenied);
        }
        let log = AuditLog::open(path.clone()).unwrap();
        assert_eq!(log.len(), 3);
        let all = log.query(&AuditFilter::default());
        assert_eq!(all[0].seq, 0);
        assert_eq!(all[1].seq, 1);
        assert_eq!(all[2].seq, 2);
        assert_eq!(all[0].actor, "ann");
        assert_eq!(all[2].action, AuditAction::AccessDenied);

        // A further append continues the sequence across the reopen.
        let mut log = AuditLog::open(path).unwrap();
        let seq = log
            .append(400, "ann", AuditAction::Logout, "", "", true)
            .unwrap();
        assert_eq!(seq, 3);
    }

    #[test]
    fn torn_tail_is_discarded_and_startup_succeeds() {
        let path = scratch("torn");
        {
            let mut log = AuditLog::open(path.clone()).unwrap();
            append_sample(&mut log, 1, "ann", AuditAction::Login);
        }
        // Simulate a half-written next record.
        let mut bytes = std::fs::read(&path).unwrap();
        let intact = bytes.len();
        bytes.extend_from_slice(&9u32.to_le_bytes());
        bytes.extend_from_slice(&[1, 2, 0]);
        std::fs::write(&path, &bytes).unwrap();

        let log = AuditLog::open(path.clone()).unwrap();
        assert_eq!(log.len(), 1, "the torn tail is dropped");
        assert_eq!(std::fs::metadata(&path).unwrap().len() as usize, intact);
    }

    #[test]
    fn corrupt_header_reinitializes_without_error() {
        let path = scratch("corrupt-header");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not an audit file at all").unwrap();
        // Recovery must not fail; it re-initializes a clean, empty log.
        let mut log = AuditLog::open(path.clone()).unwrap();
        assert!(log.is_empty());
        log.append(1, "ann", AuditAction::Login, "", "", true)
            .unwrap();
        let log = AuditLog::open(path).unwrap();
        assert_eq!(log.len(), 1);
    }

    #[test]
    fn query_filters_by_actor_action_outcome_and_time() {
        let path = scratch("query");
        let mut log = AuditLog::open(path).unwrap();
        log.append(100, "ann", AuditAction::Login, "", "", true)
            .unwrap();
        log.append(
            150,
            "ann",
            AuditAction::AccessDenied,
            "cube",
            "Sales",
            false,
        )
        .unwrap();
        log.append(200, "bob", AuditAction::Login, "", "", true)
            .unwrap();

        let by_actor = log.query(&AuditFilter {
            actor: Some("ann".into()),
            ..Default::default()
        });
        assert_eq!(by_actor.len(), 2);

        let denials = log.query(&AuditFilter {
            allowed: Some(false),
            ..Default::default()
        });
        assert_eq!(denials.len(), 1);
        assert_eq!(denials[0].action, AuditAction::AccessDenied);

        let window = log.query(&AuditFilter {
            from: Some(120),
            to: Some(180),
            ..Default::default()
        });
        assert_eq!(window.len(), 1);
        assert_eq!(window[0].actor, "ann");

        let logins = log.query(&AuditFilter {
            action: Some(AuditAction::Login),
            ..Default::default()
        });
        assert_eq!(logins.len(), 2);

        // Paging.
        let page = log.query(&AuditFilter {
            offset: 1,
            limit: Some(1),
            ..Default::default()
        });
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].seq, 1);
    }

    #[test]
    fn action_tokens_round_trip() {
        for b in 1..=13u8 {
            let a = AuditAction::from_byte(b).unwrap();
            assert_eq!(AuditAction::parse(a.as_str()), Some(a));
        }
        assert_eq!(AuditAction::parse("nope"), None);
    }

    #[test]
    fn record_cap_compacts_to_the_tail_and_survives_reopen() {
        let path = scratch("retain-count");
        let policy = RetentionPolicy {
            max_records: 3,
            max_age_millis: None,
        };
        {
            let mut log = AuditLog::open_with_policy(path.clone(), policy).unwrap();
            // The sixth append reaches 2x the cap and compacts to the newest 3.
            for i in 0..6 {
                log.append(i, "ann", AuditAction::Login, "", "", true)
                    .unwrap();
            }
            assert_eq!(log.len(), 3);
            let all = log.query(&AuditFilter::default());
            // The newest three (seqs 3,4,5) survived; sequence numbers stay
            // monotonic across the compaction.
            assert_eq!(all.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![3, 4, 5]);
        }
        // Reopen with the same policy: the compacted file recovers to the tail and
        // a further append continues the sequence.
        let mut log = AuditLog::open_with_policy(path, policy).unwrap();
        assert_eq!(log.len(), 3);
        let seq = log
            .append(11, "ann", AuditAction::Logout, "", "", true)
            .unwrap();
        assert_eq!(seq, 6, "sequence continues across compaction and reopen");
    }

    #[test]
    fn age_retention_drops_records_older_than_the_window() {
        let path = scratch("retain-age");
        let policy = RetentionPolicy {
            max_records: 0,
            max_age_millis: Some(100),
        };
        let mut log = AuditLog::open_with_policy(path, policy).unwrap();
        log.append(1000, "ann", AuditAction::Login, "", "", true)
            .unwrap();
        log.append(1050, "ann", AuditAction::Login, "", "", true)
            .unwrap();
        // This record is the newest at 1200; the window is [1100, 1200], so the
        // 1000 and 1050 records age out.
        log.append(1200, "bob", AuditAction::Login, "", "", true)
            .unwrap();
        let all = log.query(&AuditFilter::default());
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].timestamp_millis, 1200);
    }
}
