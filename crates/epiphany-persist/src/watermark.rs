//! Durable commit high-water mark (E1): a tiny fsynced side file recording the
//! largest commit version ever handed out for a data directory.
//!
//! Commit versions are minted from an in-memory counter that restarts each boot
//! (ADR-0001 gives one linearization point per cube, but the counter itself is
//! not persisted with the cubes). Without a durable high-water, a boot would
//! restart versions at 0/1 and reissue numbers a previous run already used, so an
//! optimistic-CAS `base_version` or a sandbox/cache key could alias a *different*
//! commit across a restart (an ABA hazard). Persisting the high-water and seeding
//! the counter past it at boot makes versions strictly increase across restarts.
//!
//! The file is a single little-endian `u64` framed exactly like a one-record WAL
//! payload would be conceptually, but kept deliberately minimal: 8 bytes, no
//! header, written atomically (temp + fsync + rename) so a crash mid-write leaves
//! the previous value intact. Advancing is monotonic and idempotent: a value not
//! greater than the stored one is a no-op, so a lagging writer can never move the
//! mark backwards. Deterministic (size/value-driven, no wall clock; ADR-0009).

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::store::PersistError;

/// The side file name under a data directory holding the durable high-water
/// commit version.
const WATERMARK_FILE: &str = "commit-watermark";
const WATERMARK_TMP: &str = "commit-watermark.tmp";

fn watermark_path(dir: &Path) -> PathBuf {
    dir.join(WATERMARK_FILE)
}

/// Read the durable high-water commit version from `dir`, or `0` if none has been
/// recorded yet (first run) or the file is absent/short/unreadable. Reading is
/// fail-soft on a malformed file: the counter is only ever *seeded past* this
/// value, so a lost high-water at worst re-seeds lower, and the very next
/// [`write_commit_watermark`] re-establishes a correct, larger mark. A corrupt
/// mark never blocks boot (unlike the registry, this is a hint, not the authority
/// for any object's identity).
pub fn read_commit_watermark(dir: &Path) -> u64 {
    let path = watermark_path(dir);
    match std::fs::read(&path) {
        Ok(bytes) if bytes.len() >= 8 => {
            u64::from_le_bytes(bytes[..8].try_into().expect("checked length >= 8"))
        }
        // Absent, short (torn first write), or unreadable: treat as "no mark yet".
        _ => 0,
    }
}

/// Durably record `version` as the high-water commit version for `dir` if it is
/// greater than the current mark (monotonic, idempotent). Writes atomically: a
/// fresh temp file is fsynced then renamed over the live mark, and the rename is
/// made durable (directory fsync on Unix; reopen + `sync_all` on Windows, where a
/// directory handle cannot be flushed), matching the snapshot durability contract
/// in ADR-0002. A value not greater than the stored one is a no-op (returns
/// `Ok(())` without touching disk), so an out-of-order or duplicate advance is
/// harmless.
pub fn write_commit_watermark(dir: &Path, version: u64) -> Result<(), PersistError> {
    if version <= read_commit_watermark(dir) {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(WATERMARK_TMP);
    {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        file.write_all(&version.to_le_bytes())?;
        file.sync_all()?;
    }
    let path = watermark_path(dir);
    std::fs::rename(&tmp, &path)?;
    // Make the rename durable on every platform (mirrors `Store`'s snapshot path).
    #[cfg(unix)]
    {
        std::fs::File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)?
            .sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("epiphany-watermark-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn absent_mark_reads_as_zero() {
        let dir = scratch("absent");
        assert_eq!(read_commit_watermark(&dir), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_then_read_round_trips_and_survives_reopen() {
        let dir = scratch("round-trip");
        write_commit_watermark(&dir, 42).unwrap();
        assert_eq!(read_commit_watermark(&dir), 42);
        // A fresh read (as a "restart" would do) sees the durable value.
        assert_eq!(read_commit_watermark(&dir), 42);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn advance_is_monotonic_and_idempotent() {
        let dir = scratch("monotonic");
        write_commit_watermark(&dir, 100).unwrap();
        // A smaller or equal value never moves the mark backwards.
        write_commit_watermark(&dir, 50).unwrap();
        write_commit_watermark(&dir, 100).unwrap();
        assert_eq!(read_commit_watermark(&dir), 100);
        // A larger value advances it.
        write_commit_watermark(&dir, 101).unwrap();
        assert_eq!(read_commit_watermark(&dir), 101);
        std::fs::remove_dir_all(&dir).ok();
    }
}
