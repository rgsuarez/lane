//! Slice 2 — input validation, root canonicalization/local-FS enforcement, and the
//! object guard (symlink/wrong-owner fail-closed).

mod common;

use common::*;

use lane::lock::paths::{open_or_create_writer, LaneRoot};
use lane::lock::target::Target;
use lane::lock::{validate_instance, validate_ttl, StdFs};
use lane::LaneError;
use std::path::Path;

// ---- input validation (binary) ----

#[test]
fn path_traversal_lane_id_is_rejected() {
    let root = temp_root();
    for bad in ["..", "."] {
        let out = run(
            root.path(),
            Some("a"),
            &["claim", bad, "--repo", "ops", "--json"],
        );
        assert_eq!(code(&out), 2, "lane {bad:?} must fail closed");
        assert_eq!(stdout_json(&out)["reason"], "identity");
    }
}

#[test]
fn zero_and_oversized_ttl_are_rejected() {
    let root = temp_root();
    let r = root.path();
    let out = run(
        r,
        Some("a"),
        &["claim", "demo", "--repo", "ops", "--ttl-hours=0", "--json"],
    );
    assert_eq!(code(&out), 2);
    assert_eq!(stdout_json(&out)["reason"], "identity");

    let out = run(
        r,
        Some("a"),
        &[
            "claim",
            "demo",
            "--repo",
            "ops",
            "--ttl-hours=99999",
            "--json",
        ],
    );
    assert_eq!(code(&out), 2);
    assert_eq!(stdout_json(&out)["reason"], "identity");
}

#[test]
fn relative_and_root_targets_are_rejected() {
    let root = temp_root();
    let r = root.path();
    let out = run(
        r,
        Some("a"),
        &[
            "claim",
            "demo",
            "--repo",
            "ops",
            "--target",
            "relative/path",
            "--json",
        ],
    );
    assert_eq!(code(&out), 2);
    assert_eq!(stdout_json(&out)["reason"], "identity");

    let out = run(
        r,
        Some("a"),
        &["claim", "demo", "--repo", "ops", "--target", "/", "--json"],
    );
    assert_eq!(code(&out), 2);
    assert_eq!(stdout_json(&out)["reason"], "identity");
}

// ---- input validation (library: cases the CLI can't easily express) ----

#[test]
fn instance_validation_rejects_control_chars_and_empty() {
    assert!(validate_instance("ok-instance").is_ok());
    assert!(validate_instance("").is_err());
    assert!(validate_instance("has\u{0007}bell").is_err());
    assert!(validate_instance(&"x".repeat(129)).is_err());
}

#[test]
fn ttl_validation_rejects_nan_inf_negative() {
    assert!(validate_ttl(12.0).is_ok());
    assert!(validate_ttl(0.0).is_err());
    assert!(validate_ttl(-1.0).is_err());
    assert!(validate_ttl(f64::NAN).is_err());
    assert!(validate_ttl(f64::INFINITY).is_err());
    assert!(validate_ttl(721.0).is_err());
}

#[test]
fn target_inside_lane_root_is_rejected() {
    let root = temp_root();
    let r = root.path();
    let inside = r.join("ops").join("locks");
    let res = Target::resolve(&inside.to_string_lossy(), Some(&home()), r);
    assert!(matches!(res, Err(LaneError::Identity(_))));
}

// ---- root canonicalization + local-FS enforcement (library) ----

#[test]
fn non_local_root_fails_closed() {
    let root = temp_root();
    let fake_device = FaultFs {
        device: Some(u64::MAX),
        ..Default::default()
    };
    let res = LaneRoot::resolve(root.path(), Some(&home()), &fake_device);
    assert!(matches!(res, Err(LaneError::NonLocalRoot(_))));
}

#[test]
fn aliased_roots_collapse_to_one_domain() {
    let root = temp_root();
    let real = root.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = root.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let via_real = LaneRoot::resolve(&real, Some(&home()), &StdFs).unwrap();
    let via_link = LaneRoot::resolve(&link, Some(&home()), &StdFs).unwrap();
    assert_eq!(via_real.path(), via_link.path());
}

// ---- object guard (library + binary) ----

#[test]
fn writer_rejects_wrong_owner() {
    let root = temp_root();
    let file = root.path().join("state.bin");
    std::fs::write(&file, b"x").unwrap();
    // The opened object's owner (injected 9999) disagrees with the expected uid (4242).
    let fault = FaultFs {
        owner: Some(9999),
        ..Default::default()
    };
    let res = open_or_create_writer(&file, 0o600, &fault, 4242);
    assert!(matches!(res, Err(LaneError::Identity(_))));
}

#[test]
fn writer_rejects_symlink_where_regular_file_expected() {
    let root = temp_root();
    let target = root.path().join("real-file");
    std::fs::write(&target, b"x").unwrap();
    let link = root.path().join("link-file");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let res = open_or_create_writer(&link, 0o600, &StdFs, current_uid(&target));
    assert!(matches!(res, Err(LaneError::Identity(_))));
}

#[test]
fn symlinked_mutex_file_fails_closed() {
    let root = temp_root();
    let r = root.path();
    // Plant a symlink where the lane mutex will be opened.
    let mutexes = r.join("ops").join("mutexes");
    std::fs::create_dir_all(&mutexes).unwrap();
    std::os::unix::fs::symlink("/tmp/lane-it-evil-target", mutexes.join("demo.mutex")).unwrap();

    let out = run(r, Some("a"), &["claim", "demo", "--repo", "ops", "--json"]);
    assert_eq!(code(&out), 2);
    assert_eq!(stdout_json(&out)["reason"], "identity");
}

#[test]
fn symlinked_lock_file_fails_closed() {
    let root = temp_root();
    let r = root.path();
    let locks = r.join("ops").join("locks");
    std::fs::create_dir_all(&locks).unwrap();
    std::os::unix::fs::symlink("/tmp/lane-it-evil-lock", locks.join("demo.lock")).unwrap();

    let out = run(r, Some("a"), &["claim", "demo", "--repo", "ops", "--json"]);
    assert_eq!(code(&out), 2);
    assert_eq!(stdout_json(&out)["reason"], "identity");
}

#[test]
fn symlinked_audit_log_fails_closed() {
    let root = temp_root();
    let r = root.path();
    let repo = r.join("ops");
    std::fs::create_dir_all(&repo).unwrap();
    std::os::unix::fs::symlink("/tmp/lane-it-evil-audit", repo.join("audit.log")).unwrap();

    let out = run(r, Some("a"), &["claim", "demo", "--repo", "ops", "--json"]);
    assert_eq!(code(&out), 2);
    assert_eq!(stdout_json(&out)["reason"], "identity");
}

fn current_uid(path: &Path) -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).unwrap().uid()
}
