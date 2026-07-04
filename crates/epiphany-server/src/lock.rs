//! An exclusive advisory lock on the data directory, so a second server process
//! over the same directory fails cleanly instead of silently corrupting the WAL.
//!
//! The store documents a single-process assumption (`epiphany-persist`'s
//! `store.rs`: "one process owns a cube's data directory at a time … the store
//! does not take an OS file lock") but nothing enforced it. A double-started
//! service, or a foreground run beside the installed service, would open the same
//! `wal.log` successfully — on Windows `std` opens files with full share flags —
//! and both processes would append at overlapping offsets and truncate each
//! other's fsynced frames. The failure is silent: acknowledged writes vanish and
//! the interleaved tail is dropped as a "torn tail" on the next recovery.
//!
//! This guard takes an OS-level exclusive lock on `<data_dir>/lock` at boot and
//! holds it for the process lifetime (the [`DataDirLock`] must be kept alive):
//!
//! - **Windows:** the lock file is opened with an empty share mode, so a second
//!   process's open fails with a sharing violation. The handle is released by the
//!   OS when the process exits, even on a crash, so the lock is self-healing.
//! - **Unix:** the lock file is created with `O_EXCL` and carries the owner's pid.
//!   It is removed on a clean shutdown (via `Drop`). A hard crash can leave it
//!   behind; the error message names the file so an operator can remove a stale
//!   lock after confirming no server is running. (`std` exposes no `flock`, and
//!   this crate adds no new dependency to get one.)

use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};

const LOCK_FILE: &str = "lock";

/// A held exclusive lock on a data directory. Keep it alive for as long as the
/// process owns the directory; dropping it releases the lock.
#[derive(Debug)]
pub struct DataDirLock {
    path: PathBuf,
    // The open handle IS the lock on Windows (held open with no sharing). On Unix
    // the presence of the pid file is the lock; the handle is retained for
    // symmetry and to keep the file from being reused underneath us.
    _file: File,
}

/// Why acquiring the data-directory lock failed.
#[derive(Debug)]
pub enum LockError {
    /// Another process already holds the lock (or a stale lock file is in the
    /// way after a crash, on Unix).
    AlreadyHeld { path: PathBuf },
    /// The lock file could not be created/opened for an unrelated reason.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockError::AlreadyHeld { path } => write!(
                f,
                "the data directory is already in use by another Epiphany process \
                 (lock file {}); only one server may own a data directory at a time. \
                 If no server is running, a previous run may have crashed — remove the \
                 lock file and retry.",
                path.display()
            ),
            LockError::Io { path, source } => {
                write!(
                    f,
                    "could not acquire the data-directory lock {}: {source}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for LockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LockError::Io { source, .. } => Some(source),
            LockError::AlreadyHeld { .. } => None,
        }
    }
}

impl DataDirLock {
    /// Acquire the exclusive lock on `data_dir`, creating the directory (and the
    /// lock file) if needed. Fails with [`LockError::AlreadyHeld`] if another
    /// process owns it.
    pub fn acquire(data_dir: &Path) -> Result<Self, LockError> {
        std::fs::create_dir_all(data_dir).map_err(|source| LockError::Io {
            path: data_dir.to_path_buf(),
            source,
        })?;
        let path = data_dir.join(LOCK_FILE);
        let file = acquire_locked_file(&path)?;
        Ok(Self { path, _file: file })
    }
}

impl Drop for DataDirLock {
    fn drop(&mut self) {
        // On Unix the lock IS the file's existence, so remove it on a clean exit
        // to avoid a stale lock after shutdown. On Windows the OS releases the
        // handle when the file closes (as `_file` drops); removing the now-unshared
        // file is harmless best-effort tidy-up.
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(windows)]
fn acquire_locked_file(path: &Path) -> Result<File, LockError> {
    use std::os::windows::fs::OpenOptionsExt;
    // share_mode(0): no other handle may open the file while we hold it, so a
    // second server's open fails with a sharing violation (mapped below).
    // FILE_ATTRIBUTE_NORMAL keeps default semantics.
    // ERROR_SHARING_VIOLATION (32) / ERROR_LOCK_VIOLATION (33): another process
    // holds the exclusively-opened handle. Rust maps these to
    // `ErrorKind::Uncategorized`, so match on the raw OS code rather than the kind.
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_LOCK_VIOLATION: i32 = 33;
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // The file is only a lock handle; its contents are irrelevant, so do not
        // truncate (open-or-create, leaving any existing bytes untouched).
        .truncate(false)
        .share_mode(0)
        .open(path)
    {
        Ok(f) => Ok(f),
        Err(e)
            if matches!(
                e.raw_os_error(),
                Some(ERROR_SHARING_VIOLATION) | Some(ERROR_LOCK_VIOLATION)
            ) =>
        {
            Err(LockError::AlreadyHeld {
                path: path.to_path_buf(),
            })
        }
        Err(source) => Err(LockError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(not(windows))]
fn acquire_locked_file(path: &Path) -> Result<File, LockError> {
    use std::io::Write;
    // O_EXCL create: succeeds only if we create the file, so a second live process
    // (whose predecessor created and still holds it) fails with AlreadyExists. The
    // owner's pid is recorded to aid an operator diagnosing a stale lock.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut f) => {
            let _ = writeln!(f, "{}", std::process::id());
            Ok(f)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(LockError::AlreadyHeld {
            path: path.to_path_buf(),
        }),
        Err(source) => Err(LockError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("epiphany-server-lock-{}-{tag}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn second_acquire_is_rejected_while_the_first_is_held() {
        let dir = scratch("contended");
        let first = DataDirLock::acquire(&dir).expect("first lock");
        let second = DataDirLock::acquire(&dir);
        assert!(
            matches!(second, Err(LockError::AlreadyHeld { .. })),
            "a second lock on a held directory must be rejected, got {second:?}"
        );
        drop(first);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn releasing_the_lock_lets_a_new_process_acquire_it() {
        let dir = scratch("released");
        let first = DataDirLock::acquire(&dir).expect("first lock");
        drop(first);
        // After a clean release the directory is free again.
        let second = DataDirLock::acquire(&dir).expect("re-acquire after release");
        drop(second);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn acquire_creates_the_data_dir_if_absent() {
        let dir = scratch("creates").join("nested");
        assert!(!dir.exists());
        let lock = DataDirLock::acquire(&dir).expect("acquire creates the dir");
        assert!(dir.is_dir());
        drop(lock);
        std::fs::remove_dir_all(&dir).ok();
    }
}
