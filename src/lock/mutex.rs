//! Per-lane and per-repo target mutexes via std OS advisory locks (§S2.8).
//!
//! The primitive is `std::fs::File::try_lock` (stable since Rust 1.89) — an exclusive
//! advisory lock on the open file description (`flock(LOCK_EX)`-style). Exactly one
//! holder per file; all other contenders get `WouldBlock`. The [`LaneMutex`] RAII guard
//! holds the locked `File`; **dropping it closes the fd, so the OS releases the lock —
//! including on process death / crash.** There is no stale lease, no PID body, no
//! timeout body, and no stale-recovery code. Mutex files are persistent lock *targets*
//! and are never unlinked.
//!
//! Lock order is always lane → target (§S2.4); the single target mutex is always the
//! second acquisition, so there is no lock-ordering cycle and no deadlock.

use std::fs::{File, TryLockError};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{LaneError, RefusedReason};
use crate::lock::paths;
use crate::lock::FsOps;

/// How long to retry a contended mutex before reporting `MutexBusy`.
const MAX_WAIT: Duration = Duration::from_millis(3000);
const MUTEX_MODE: u32 = 0o600;

/// RAII guard holding an exclusive advisory lock on a mutex file. Drop releases it
/// (the OS releases on fd close, including on crash).
pub struct LaneMutex {
    // Held only for its `Drop` (closes the fd → releases the advisory lock).
    _file: File,
}

impl LaneMutex {
    /// Open the persistent mutex file under the object guard and acquire an exclusive
    /// advisory lock, retrying with bounded jittered backoff. Times out as `MutexBusy`
    /// (exit 1); a non-`WouldBlock` lock error is an `Io` error (exit 2).
    pub fn acquire(path: &Path, fs: &dyn FsOps, expected_uid: u32) -> Result<Self, LaneError> {
        let file = paths::open_or_create_writer(path, MUTEX_MODE, fs, expected_uid)?;
        let start = SystemTime::now();
        let mut backoff = Duration::from_millis(5);
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(TryLockError::WouldBlock) => {
                    if start.elapsed().unwrap_or_default() >= MAX_WAIT {
                        return Err(LaneError::Refused(RefusedReason::MutexBusy));
                    }
                    std::thread::sleep(backoff + jitter());
                    backoff = (backoff * 2).min(Duration::from_millis(100));
                }
                Err(TryLockError::Error(e)) => return Err(LaneError::Io(e)),
            }
        }
    }
}

/// A few milliseconds of jitter, derived dependency-free from the clock subsec nanos
/// (avoids lock-step retries between contending processes).
fn jitter() -> Duration {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    Duration::from_millis(u64::from(n % 7))
}
