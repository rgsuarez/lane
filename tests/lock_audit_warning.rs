//! Fix B regression — a refusal/malformed audit-append failure is surfaced as a non-secret
//! `audit_warning` while the primary exit code / `Reason` is preserved unchanged. (The human
//! stderr rendering is unit-tested in `src/lock/mod.rs::tests::error_stderr_*`.)

mod common;

use common::*;

use chrono::Utc;
use lane::lock::audit::AuditEventKind;
use lane::lock::paths::LaneRoot;
use lane::lock::{claim, ClaimParams, StdFs};
use lane::{LaneError, RefusedReason};
use std::path::Path;

fn params(lane: &str, instance: &str, target: Option<&str>, force: bool) -> ClaimParams {
    ClaimParams {
        repo: "ops".into(),
        lane: lane.into(),
        instance: instance.into(),
        target: target.map(str::to_string),
        home: Some(home()),
        force,
        ..Default::default()
    }
}

fn write_malformed_lock(root: &Path, lane: &str) {
    let locks = root.join("ops").join("locks");
    std::fs::create_dir_all(&locks).unwrap();
    std::fs::write(locks.join(format!("{lane}.lock")), "this-is-not-json").unwrap();
}

#[test]
fn active_held_audit_failure_keeps_code_and_surfaces_warning() {
    let root = temp_root();
    let r = root.path();
    assert_eq!(
        code(&run(
            r,
            Some("a"),
            &["claim", "demo", "--repo", "ops", "--json"]
        )),
        0
    );
    let root_obj = LaneRoot::resolve(r, Some(&home()), &StdFs).unwrap();
    let audit = FaultAudit::new(
        root_obj.audit_path("ops"),
        root_obj.expected_uid(),
        AuditEventKind::ClaimRefused,
    );
    let err = claim::claim_core(
        &root_obj,
        &params("demo", "b", None, false),
        Utc::now(),
        &StdFs,
        &audit,
    )
    .err()
    .expect("refused");
    assert!(matches!(
        err.error,
        LaneError::Refused(RefusedReason::ActiveHeld)
    ));
    assert_eq!(err.error.exit_code(), 1);
    assert!(
        err.audit_warning.is_some(),
        "active-held + audit failure surfaces a warning"
    );
}

#[test]
fn target_overlap_audit_failure_keeps_code_and_surfaces_warning() {
    let root = temp_root();
    let r = root.path();
    assert_eq!(
        code(&run(
            r,
            Some("a"),
            &[
                "claim",
                "alpha",
                "--repo",
                "ops",
                "--target",
                "/tmp/lane-aw-shared",
                "--json"
            ]
        )),
        0
    );
    let root_obj = LaneRoot::resolve(r, Some(&home()), &StdFs).unwrap();
    let audit = FaultAudit::new(
        root_obj.audit_path("ops"),
        root_obj.expected_uid(),
        AuditEventKind::ClaimRefused,
    );
    let err = claim::claim_core(
        &root_obj,
        &params("beta", "b", Some("/tmp/lane-aw-shared"), false),
        Utc::now(),
        &StdFs,
        &audit,
    )
    .err()
    .expect("refused");
    assert!(matches!(
        err.error,
        LaneError::Refused(RefusedReason::TargetOverlap)
    ));
    assert_eq!(err.error.exit_code(), 1);
    assert!(
        err.audit_warning.is_some(),
        "target-overlap + audit failure surfaces a warning"
    );
}

#[test]
fn malformed_audit_failure_keeps_code_and_surfaces_warning() {
    let root = temp_root();
    let r = root.path();
    write_malformed_lock(r, "demo");
    let root_obj = LaneRoot::resolve(r, Some(&home()), &StdFs).unwrap();
    let audit = FaultAudit::new(
        root_obj.audit_path("ops"),
        root_obj.expected_uid(),
        AuditEventKind::Malformed,
    );
    let err = claim::claim_core(
        &root_obj,
        &params("demo", "a", None, false),
        Utc::now(),
        &StdFs,
        &audit,
    )
    .err()
    .expect("malformed");
    assert!(matches!(err.error, LaneError::Malformed { .. }));
    assert_eq!(err.error.exit_code(), 2);
    assert!(
        err.audit_warning.is_some(),
        "malformed + audit failure surfaces a warning"
    );
}

#[test]
fn successful_refusal_audit_has_no_warning() {
    // When the refusal audit append SUCCEEDS, the error envelope carries no audit_warning.
    let root = temp_root();
    let r = root.path();
    assert_eq!(
        code(&run(
            r,
            Some("a"),
            &["claim", "demo", "--repo", "ops", "--json"]
        )),
        0
    );
    let out = run(r, Some("b"), &["claim", "demo", "--repo", "ops", "--json"]);
    assert_eq!(code(&out), 1);
    let j = stdout_json(&out);
    assert_eq!(j["reason"], "active_held");
    assert!(
        j["audit_warning"].is_null(),
        "no warning when the refusal audit append succeeds"
    );
}

#[test]
fn refusal_emits_exactly_one_json_envelope() {
    let root = temp_root();
    let r = root.path();
    assert_eq!(
        code(&run(
            r,
            Some("a"),
            &["claim", "demo", "--repo", "ops", "--json"]
        )),
        0
    );
    let out = run(r, Some("b"), &["claim", "demo", "--repo", "ops", "--json"]);
    assert_eq!(code(&out), 1);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim().lines().count(),
        1,
        "exactly one JSON envelope on a refusal"
    );
}
