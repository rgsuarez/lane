//! The adapter-owned per-lane PUBLISH lock serializing concurrent
//! `close --post-closeout` runs (Slice 4).
//!
//! Deliberately NOT the core lane mutex: core mutexes are never held across a
//! spawn/network call, and this lock exists precisely to bracket the whole
//! secret-resolve → renew → worktree-remove → POST → release critical path of a
//! gated publish. It serializes ONLY concurrent post-closeouts of the same lane —
//! core verbs never touch it.
//!
//! The lock file is a SAFETY CONTROL, so its acquisition is object-guarded like all
//! lane state: `validate_name`d path segments, guarded directory creation/validation
//! (symlink / non-directory / foreign owner ⇒ fail closed, exit 2), and the shared
//! [`LaneMutex`] acquisition (object-guarded file open/create + bounded try_lock ⇒
//! `mutex_busy` refusal on contention). Callers acquire BEFORE resolving any secret
//! or mutating anything, so a poisoned lock object or a lost race provably leaves
//! claim, worktree, secrets, and Linear untouched.

use crate::error::LaneError;
use crate::lock::mutex::LaneMutex;
use crate::lock::paths::{ensure_dir_guarded, LaneRoot};
use crate::lock::{validate_name, FsOps};

/// Directory under the lane root holding publish-lock files.
pub const PUBLISH_DIR: &str = "linear-publish";

/// RAII guard: dropping releases the advisory lock (including on crash).
pub struct PublishGuard {
    _mutex: LaneMutex,
}

impl std::fmt::Debug for PublishGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PublishGuard")
    }
}

/// Acquire the per-lane publish lock. Busy past the bounded window ⇒
/// `Refused(MutexBusy)` (exit 1); a poisoned dir/lock object ⇒ exit 2 fail-closed.
pub fn acquire(
    root: &LaneRoot,
    repo: &str,
    lane: &str,
    fs: &dyn FsOps,
) -> Result<PublishGuard, LaneError> {
    validate_name("repo", repo)?;
    validate_name("lane", lane)?;
    let dir = root.path().join(PUBLISH_DIR);
    ensure_dir_guarded(&dir, fs, root.expected_uid())?;
    let path = dir.join(format!("{repo}--{lane}.lock"));
    let mutex = LaneMutex::acquire(&path, fs, root.expected_uid())?;
    Ok(PublishGuard { _mutex: mutex })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::RefusedReason;
    use crate::lock::paths::LaneRoot;
    use crate::lock::StdFs;
    use std::os::unix::fs::symlink;

    fn root_at(dir: &std::path::Path) -> LaneRoot {
        let home = std::env::var("HOME").expect("HOME");
        LaneRoot::resolve(dir, Some(&home), &StdFs).expect("resolve root")
    }

    fn home_tempdir() -> tempfile::TempDir {
        let home = std::env::var("HOME").expect("HOME");
        tempfile::Builder::new()
            .prefix("lane-pub-")
            .tempdir_in(home)
            .expect("tempdir under HOME")
    }

    #[test]
    fn acquire_is_exclusive_and_released_on_drop() {
        let dir = home_tempdir();
        let root = root_at(dir.path());
        let g1 = acquire(&root, "ops", "demo", &StdFs).expect("first acquire");
        // A second contender times out busy (bounded ~3s window).
        let busy = acquire(&root, "ops", "demo", &StdFs).expect_err("must be busy");
        assert!(matches!(busy, LaneError::Refused(RefusedReason::MutexBusy)));
        drop(g1);
        let _g2 = acquire(&root, "ops", "demo", &StdFs).expect("acquire after drop");
    }

    #[test]
    fn distinct_lanes_do_not_contend() {
        let dir = home_tempdir();
        let root = root_at(dir.path());
        let _a = acquire(&root, "ops", "one", &StdFs).expect("a");
        let _b = acquire(&root, "ops", "two", &StdFs).expect("b");
    }

    #[test]
    fn symlinked_publish_dir_fails_closed() {
        let dir = home_tempdir();
        let root = root_at(dir.path());
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        symlink(&elsewhere, dir.path().join(PUBLISH_DIR)).unwrap();
        let err = acquire(&root, "ops", "demo", &StdFs).expect_err("must fail closed");
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn symlinked_lock_file_fails_closed() {
        let dir = home_tempdir();
        let root = root_at(dir.path());
        let pubdir = dir.path().join(PUBLISH_DIR);
        std::fs::create_dir_all(&pubdir).unwrap();
        std::fs::write(dir.path().join("target"), "x").unwrap();
        symlink(dir.path().join("target"), pubdir.join("ops--demo.lock")).unwrap();
        let err = acquire(&root, "ops", "demo", &StdFs).expect_err("must fail closed");
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn invalid_names_never_touch_disk() {
        let dir = home_tempdir();
        let root = root_at(dir.path());
        let err = acquire(&root, "../evil", "demo", &StdFs).expect_err("bad repo");
        assert_eq!(err.exit_code(), 2);
        assert!(
            !dir.path().join(PUBLISH_DIR).exists(),
            "no dir created for an invalid name"
        );
    }
}
