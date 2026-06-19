//! Fix A regression — interior state-directory symlinks must fail closed. A symlinked
//! `<root>/ops` (repo) or `<root>/ops/locks` directory must NOT be followed by status,
//! list, board, or the audit/reconciliation read; no external claim content may surface.

mod common;

use common::*;

use lane::lock::audit;
use lane::lock::paths::LaneRoot;
use lane::lock::StdFs;
use lane::LaneError;
use std::path::Path;

fn valid_lock(lane: &str, instance: &str) -> String {
    format!(
        r#"{{"schema_version":1,"lane":"{lane}","repo":"ops","instance":"{instance}","claimed_at":"2026-06-18T00:00:00Z","updated_at":"2026-06-18T00:00:00Z","expires_at":"2090-01-01T00:00:00Z","ttl_hours":12}}"#
    )
}

/// `<root>/ops/locks` → external same-owner `locks/` holding a valid claim.
fn symlinked_locks(root: &Path, ext: &Path) {
    std::fs::create_dir_all(root.join("ops")).unwrap();
    std::fs::create_dir_all(ext.join("locks")).unwrap();
    std::fs::write(
        ext.join("locks").join("demo.lock"),
        valid_lock("demo", "EXTERNAL"),
    )
    .unwrap();
    std::os::unix::fs::symlink(ext.join("locks"), root.join("ops").join("locks")).unwrap();
}

/// `<root>/ops` (repo) → external same-owner `ops/` holding a valid claim.
fn symlinked_repo(root: &Path, ext: &Path) {
    std::fs::create_dir_all(ext.join("ops").join("locks")).unwrap();
    std::fs::write(
        ext.join("ops").join("locks").join("demo.lock"),
        valid_lock("demo", "EXTERNAL"),
    )
    .unwrap();
    std::os::unix::fs::symlink(ext.join("ops"), root.join("ops")).unwrap();
}

fn no_external(out: &std::process::Output) {
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("EXTERNAL"),
        "external claim content must never surface"
    );
}

// ---- status ----

#[test]
fn status_rejects_symlinked_locks_dir() {
    let root = temp_root();
    let ext = temp_root();
    symlinked_locks(root.path(), ext.path());
    let out = run(
        root.path(),
        None,
        &["status", "demo", "--repo", "ops", "--json"],
    );
    assert_eq!(code(&out), 2);
    assert_eq!(stdout_json(&out)["reason"], "identity");
    no_external(&out);
}

#[test]
fn status_rejects_symlinked_repo_dir() {
    let root = temp_root();
    let ext = temp_root();
    symlinked_repo(root.path(), ext.path());
    let out = run(
        root.path(),
        None,
        &["status", "demo", "--repo", "ops", "--json"],
    );
    assert_eq!(code(&out), 2);
    assert_eq!(stdout_json(&out)["reason"], "identity");
    no_external(&out);
}

// ---- list ----

#[test]
fn list_rejects_symlinked_locks_dir() {
    let root = temp_root();
    let ext = temp_root();
    symlinked_locks(root.path(), ext.path());
    let out = run(root.path(), None, &["list", "--json"]);
    assert_eq!(code(&out), 2);
    no_external(&out);
}

#[test]
fn list_rejects_symlinked_repo_dir() {
    let root = temp_root();
    let ext = temp_root();
    symlinked_repo(root.path(), ext.path());
    let out = run(root.path(), None, &["list", "--json"]);
    assert_eq!(code(&out), 2);
    no_external(&out);
}

// ---- board ----

#[test]
fn board_rejects_symlinked_locks_dir() {
    let root = temp_root();
    let ext = temp_root();
    symlinked_locks(root.path(), ext.path());
    let out = run(root.path(), None, &["board", "--json"]);
    assert_eq!(code(&out), 2);
    no_external(&out);
}

#[test]
fn board_rejects_symlinked_repo_dir() {
    let root = temp_root();
    let ext = temp_root();
    symlinked_repo(root.path(), ext.path());
    let out = run(root.path(), None, &["board", "--json"]);
    assert_eq!(code(&out), 2);
    no_external(&out);
}

// ---- audit / reconciliation read ----

#[test]
fn audit_read_rejects_symlinked_repo_dir() {
    let root = temp_root();
    let ext = temp_root();
    std::fs::create_dir_all(ext.path().join("ops")).unwrap();
    std::fs::write(
        ext.path().join("ops").join("audit.log"),
        "{\"ts\":\"2026-06-18T00:00:00Z\",\"event\":\"claim\",\"repo\":\"ops\",\"lane\":\"x\",\"instance\":\"i\",\"outcome\":\"ok\"}\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(ext.path().join("ops"), root.path().join("ops")).unwrap();

    let r = LaneRoot::resolve(root.path(), Some(&home()), &StdFs).unwrap();
    let res =
        audit::read_validated_events(&r.audit_path("ops"), r.path(), r.expected_uid(), &StdFs);
    assert!(
        matches!(res, Err(LaneError::Identity(_))),
        "the audit read refuses to follow a symlinked interior repo directory"
    );
}

// ---- missing paths stay absent / non-mutating ----

#[test]
fn missing_paths_remain_absent_and_nonmutating() {
    let root = temp_root();
    let r = root.path();
    // No ops/ at all.
    let out = run(r, None, &["status", "ghost", "--repo", "ops", "--json"]);
    assert_eq!(code(&out), 0);
    assert_eq!(stdout_json(&out)["outcome"], "not_held");
    let out = run(r, None, &["list", "--json"]);
    assert_eq!(code(&out), 0);
    assert_eq!(
        stdout_json(&out)["data"]["rows"].as_array().unwrap().len(),
        0
    );
    // Reads created nothing.
    assert!(
        !r.join("ops").exists(),
        "read verbs must not create state dirs"
    );
}
