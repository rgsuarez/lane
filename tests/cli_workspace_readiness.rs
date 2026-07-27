//! End-to-end CLI test for the advisory unbootstrapped-workspace warning on `lane claim`.
//!
//! The load-bearing invariant here is NOT that the warning appears - it is that the warning can
//! never cost anything. A fresh `git worktree add` leaves a workspace with its lockfile and no
//! install directory; `lane claim` reports that so an executor learns it before its first gate
//! rather than mid-run through an error string that also means "dependency-graph regression".
//! But `lane claim` is the coordination chokepoint every executor runs before any mutation, so
//! a claim MUST still succeed in that state. These tests pin exit 0 in both renderings.

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// A workspace as `git worktree add` leaves it: lockfile present (tracked), install dir absent
/// (gitignored, so it never came along).
fn unbootstrapped_workspace() -> TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("bun.lock"), "{}").unwrap();
    d
}

fn claim_in(dir: &TempDir, root: &TempDir, lane: &str, json: bool) -> Command {
    let mut cmd = Command::cargo_bin("lane").unwrap();
    cmd.current_dir(dir.path())
        .arg("claim")
        .arg(lane)
        .arg("--repo")
        .arg("wsready")
        .arg("--instance")
        .arg("WSREADY-TEST")
        .arg("--lane-root")
        .arg(root.path());
    if json {
        cmd.arg("--json");
    }
    cmd
}

#[test]
fn claim_in_unbootstrapped_workspace_succeeds_and_warns() {
    let ws = unbootstrapped_workspace();
    let root = tempfile::tempdir().unwrap();
    claim_in(&ws, &root, "ws-human", false)
        .assert()
        // THE invariant: an advisory warning never wedges the claim.
        .success()
        .stderr(predicate::str::contains("workspace not bootstrapped"))
        // Remedy and disambiguation ride in the same breath as the detection.
        .stderr(predicate::str::contains("install dependencies"))
        .stderr(predicate::str::contains("Cannot find package"));
}

#[test]
fn claim_json_envelope_carries_the_warning_and_stays_ok() {
    let ws = unbootstrapped_workspace();
    let root = tempfile::tempdir().unwrap();
    claim_in(&ws, &root, "ws-json", true)
        .assert()
        .success()
        // The envelope is emitted COMPACT (no space after the colon).
        .stdout(predicate::str::contains("\"ok\":true"))
        .stdout(predicate::str::contains("\"outcome\":\"ok\""))
        .stdout(predicate::str::contains("workspace not bootstrapped"));
}

#[test]
fn claim_in_bootstrapped_workspace_is_silent() {
    let ws = unbootstrapped_workspace();
    std::fs::create_dir(ws.path().join("node_modules")).unwrap();
    let root = tempfile::tempdir().unwrap();
    claim_in(&ws, &root, "ws-quiet", false)
        .assert()
        .success()
        .stderr(predicate::str::contains("workspace not bootstrapped").not());
}

#[test]
fn claim_outside_any_workspace_is_silent() {
    let ws = tempfile::tempdir().unwrap(); // no lockfile at all
    let root = tempfile::tempdir().unwrap();
    claim_in(&ws, &root, "ws-none", false)
        .assert()
        .success()
        .stderr(predicate::str::contains("workspace not bootstrapped").not());
}
