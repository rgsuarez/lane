//! Shared helpers for the Slice 2 locking-core integration tests.
//!
//! Write-path tests put `$LANE_ROOT` UNDER `$HOME` so the local-filesystem device check
//! passes (a tempdir on a different volume would be rejected as non-local). Process tests
//! spawn the real `lane` binary via `CARGO_BIN_EXE_lane`. Fault tests inject the
//! `FsOps`/`AuditSink` seams.
#![allow(dead_code)]

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use lane::lock::audit::{AuditEvent, AuditEventKind, AuditSink, StdAuditSink};
use lane::lock::{FsOps, StdFs};
use lane::ClaimRecord;
use tempfile::TempDir;

/// `$HOME` (required for the local-FS device check + `~` expansion).
pub fn home() -> String {
    std::env::var("HOME").expect("HOME is set")
}

/// A temp lane root on the SAME filesystem device as `$HOME`.
pub fn temp_root() -> TempDir {
    tempfile::Builder::new()
        .prefix("lane-it-")
        .tempdir_in(home())
        .expect("tempdir under HOME")
}

/// Path to the built `lane` binary (set by Cargo for integration tests).
pub fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_lane")
}

/// Run `lane <args> --lane-root <root>` with an optional `$LANE_INSTANCE`. Clears the
/// ambient `$LANE_ROOT` and the test hold hook unless a caller sets them.
pub fn run(root: &Path, instance: Option<&str>, args: &[&str]) -> Output {
    let mut c = Command::new(bin());
    c.args(args).arg("--lane-root").arg(root);
    c.env_remove("LANE_ROOT");
    c.env_remove("LANE_TEST_HOLD_LANE_MUTEX_MS");
    match instance {
        Some(i) => {
            c.env("LANE_INSTANCE", i);
        }
        None => {
            c.env_remove("LANE_INSTANCE");
        }
    }
    c.output().expect("spawn lane")
}

/// Spawn (do not wait) `lane <args>` with a lane-mutex hold of `hold_ms` so the process
/// keeps the lane mutex while a sibling contends / it is SIGKILLed.
pub fn spawn_holding(
    root: &Path,
    instance: &str,
    hold_ms: u64,
    args: &[&str],
) -> std::process::Child {
    let mut c = Command::new(bin());
    c.args(args).arg("--lane-root").arg(root);
    c.env_remove("LANE_ROOT");
    c.env("LANE_INSTANCE", instance);
    c.env("LANE_TEST_HOLD_LANE_MUTEX_MS", hold_ms.to_string());
    c.spawn().expect("spawn holding lane")
}

pub fn code(out: &Output) -> i32 {
    out.status.code().expect("exit code")
}

pub fn stdout_json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

/// Read a claim record straight from disk (test inspection).
pub fn read_lock(root: &Path, repo: &str, lane: &str) -> Option<ClaimRecord> {
    let p = root.join(repo).join("locks").join(format!("{lane}.lock"));
    let text = std::fs::read_to_string(p).ok()?;
    Some(serde_json::from_str(&text).expect("lock parses"))
}

pub fn audit_path(root: &Path, repo: &str) -> PathBuf {
    root.join(repo).join("audit.log")
}

/// Parse every newline-terminated line of an audit log into events (asserts each parses).
pub fn read_audit(root: &Path, repo: &str) -> Vec<AuditEvent> {
    let text = std::fs::read_to_string(audit_path(root, repo)).unwrap_or_default();
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            serde_json::from_str::<AuditEvent>(l)
                .unwrap_or_else(|e| panic!("audit line not JSON ({e}): {l}"))
        })
        .collect()
}

// -------------------------------------------------------------------------
// Injectable fault impls (production traits live in the library; these are test-only).
// -------------------------------------------------------------------------

/// An `FsOps` that delegates to std but can fail a chosen mutation, or lie about a path's
/// device id / owner uid (to exercise non-local-root and wrong-owner fail-closed paths).
#[derive(Default)]
pub struct FaultFs {
    pub fail_rename: bool,
    pub fail_hard_link: bool,
    pub device: Option<u64>,
    pub owner: Option<u32>,
}

impl FsOps for FaultFs {
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        if self.fail_rename {
            Err(io::Error::other("injected rename failure"))
        } else {
            StdFs.rename(from, to)
        }
    }
    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        if self.fail_hard_link {
            Err(io::Error::other("injected hard_link failure"))
        } else {
            StdFs.hard_link(from, to)
        }
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        StdFs.remove_file(path)
    }
    fn device_of(&self, path: &Path) -> io::Result<u64> {
        match self.device {
            Some(d) => Ok(d),
            None => StdFs.device_of(path),
        }
    }
    fn owner_uid(&self, path: &Path) -> io::Result<u32> {
        match self.owner {
            Some(u) => Ok(u),
            None => StdFs.owner_uid(path),
        }
    }
}

/// An `AuditSink` that delegates to a real `StdAuditSink` but fails on a chosen event kind
/// (drives the intent-fail / completion-fail audit state-machine tests).
pub struct FaultAudit {
    inner: StdAuditSink,
    fail_on: AuditEventKind,
}

impl FaultAudit {
    pub fn new(path: PathBuf, expected_uid: u32, fail_on: AuditEventKind) -> Self {
        Self {
            inner: StdAuditSink::new(path, expected_uid),
            fail_on,
        }
    }
}

impl AuditSink for FaultAudit {
    fn append(&self, event: &AuditEvent, fsync: bool) -> io::Result<()> {
        if event.event == self.fail_on {
            return Err(io::Error::other("injected audit failure"));
        }
        self.inner.append(event, fsync)
    }
}
