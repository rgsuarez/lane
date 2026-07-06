//! Slice 2 — deterministic fault/recovery tests via the injectable `FsOps`/`AuditSink`
//! seams (library-level), plus the read-verbs-never-mutate guarantee.

mod common;

use common::*;

use chrono::Utc;
use lane::lock::audit::{AuditEventKind, StdAuditSink};
use lane::lock::paths::LaneRoot;
use lane::lock::{claim, renew_release, ClaimParams, RenewParams, StdFs};
use lane::{LaneError, RefusedReason};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn force_params(instance: &str) -> ClaimParams {
    ClaimParams {
        repo: "ops".into(),
        lane: "demo".into(),
        instance: instance.into(),
        home: Some(home()),
        force: true,
        ..Default::default()
    }
}

/// Pre-place an active claim (instance "old") via the real binary, then return a resolved root.
fn setup_active(root: &Path) -> LaneRoot {
    assert_eq!(
        code(&run(
            root,
            Some("old"),
            &["claim", "demo", "--repo", "ops", "--json"]
        )),
        0
    );
    LaneRoot::resolve(root, Some(&home()), &StdFs).unwrap()
}

#[test]
fn intent_failure_aborts_before_mutating() {
    let root = temp_root();
    let r = root.path();
    let root_obj = setup_active(r);
    // Audit fails on the write-ahead `intent` → the destructive op must abort untouched.
    let audit = FaultAudit::new(
        root_obj.audit_path("ops"),
        root_obj.expected_uid(),
        AuditEventKind::Intent,
    );
    let res = claim::claim_core(&root_obj, &force_params("new"), Utc::now(), &StdFs, &audit);
    assert!(res.is_err(), "intent failure aborts the claim");
    assert_eq!(
        read_lock(r, "ops", "demo").unwrap().instance,
        "old",
        "prior claim intact"
    );
}

#[test]
fn mutation_failure_records_completion_error() {
    let root = temp_root();
    let r = root.path();
    let root_obj = setup_active(r);
    // Real audit; the rename (mutation) fails AFTER the intent is written.
    let audit = StdAuditSink::new(root_obj.audit_path("ops"), root_obj.expected_uid());
    let fs = FaultFs {
        fail_rename: true,
        ..Default::default()
    };
    let res = claim::claim_core(&root_obj, &force_params("new"), Utc::now(), &fs, &audit);
    assert!(res.is_err(), "mutation failure surfaces an error");
    assert_eq!(
        read_lock(r, "ops", "demo").unwrap().instance,
        "old",
        "prior claim intact"
    );

    let events = read_audit(r, "ops");
    assert!(
        events
            .iter()
            .any(|e| format!("{:?}", e.event) == "Completion"
                && format!("{:?}", e.outcome) == "Error"),
        "a completion{{error}} is recorded for the failed mutation"
    );
}

#[test]
fn post_mutation_audit_failure_is_success_with_warning() {
    let root = temp_root();
    let r = root.path();
    let root_obj = setup_active(r);
    // The mutation succeeds; only the `completion` audit write fails.
    let audit = FaultAudit::new(
        root_obj.audit_path("ops"),
        root_obj.expected_uid(),
        AuditEventKind::Completion,
    );
    let s = claim::claim_core(&root_obj, &force_params("new"), Utc::now(), &StdFs, &audit)
        .expect("mutation succeeded despite completion-audit failure");
    assert!(
        s.audit_warning.is_some(),
        "exit 0 with an audit_warning, never 'mutation failed'"
    );
    assert_eq!(
        read_lock(r, "ops", "demo").unwrap().instance,
        "new",
        "takeover committed"
    );
}

#[test]
fn renew_at_expiry_boundary_is_refused() {
    let root = temp_root();
    let r = root.path();
    let root_obj = LaneRoot::resolve(r, Some(&home()), &StdFs).unwrap();
    let audit = StdAuditSink::new(root_obj.audit_path("ops"), root_obj.expected_uid());
    let now0 = Utc::now();
    let params = ClaimParams {
        repo: "ops".into(),
        lane: "demo".into(),
        instance: "a".into(),
        home: Some(home()),
        ttl_hours: Some(1.0),
        ..Default::default()
    };
    claim::claim_core(&root_obj, &params, now0, &StdFs, &audit).unwrap();
    let expires = read_lock(r, "ops", "demo").unwrap().expires_at;

    // At now == expires_at, renew must refuse (re-claim instead). Upholds "no resurrection".
    let renew = RenewParams {
        repo: "ops".into(),
        lane: "demo".into(),
        instance: "a".into(),
        ttl_hours: None,
    };
    let res = renew_release::renew_core(&root_obj, &renew, expires, &StdFs, &audit);
    assert!(matches!(
        res,
        Err(LaneError::Refused(RefusedReason::Expired))
    ));
}

#[test]
fn trailing_fragment_is_recovered_then_claim_proceeds() {
    let root = temp_root();
    let r = root.path();
    std::fs::create_dir_all(r.join("ops")).unwrap();
    // A complete (valid) record followed by a crash-truncated fragment (no newline).
    let valid = r#"{"ts":"2026-06-18T00:00:00Z","event":"claim","repo":"ops","lane":"x","instance":"i","outcome":"ok"}"#;
    std::fs::write(
        audit_path(r, "ops"),
        format!("{valid}\n{{\"ts\":\"2026-06-18T00:00:01Z\",\"eve"),
    )
    .unwrap();

    let out = run(
        r,
        Some("a"),
        &["claim", "newlane", "--repo", "ops", "--json"],
    );
    assert_eq!(
        code(&out),
        0,
        "claim proceeds after recovering the fragment"
    );

    assert!(
        r.join("ops").join("audit.recovered").is_dir(),
        "fragment quarantined"
    );
    let frags = std::fs::read_dir(r.join("ops").join("audit.recovered"))
        .unwrap()
        .count();
    assert!(frags >= 1, "a .frag file was written");

    let events = read_audit(r, "ops"); // asserts every remaining line parses
    assert!(
        events
            .iter()
            .any(|e| format!("{:?}", e.event) == "AuditRecovery"),
        "an audit_recovery event was appended"
    );
}

#[test]
fn newline_terminated_malformed_audit_fails_closed() {
    let root = temp_root();
    let r = root.path();
    std::fs::create_dir_all(r.join("ops")).unwrap();
    // Newline-terminated but malformed trailing record → evidence, never truncated.
    std::fs::write(audit_path(r, "ops"), "this-is-not-an-audit-record\n").unwrap();

    let out = run(r, Some("a"), &["claim", "x", "--repo", "ops", "--json"]);
    assert_eq!(code(&out), 2, "fails closed");
    assert_eq!(stdout_json(&out)["reason"], "malformed");
}

#[test]
fn read_verbs_do_not_create_an_absent_root() {
    let root = temp_root();
    let absent = root.path().join("never-created-subroot");
    let out = run(&absent, None, &["list", "--json"]);
    assert_eq!(code(&out), 0);
    assert_eq!(
        stdout_json(&out)["data"]["rows"].as_array().unwrap().len(),
        0
    );
    assert!(!absent.exists(), "list must not create the root");
}

#[test]
fn read_verbs_do_not_chmod_or_audit() {
    let root = temp_root();
    let r = root.path();
    let locks = r.join("ops").join("locks");
    std::fs::create_dir_all(&locks).unwrap();
    let lock = locks.join("demo.lock");
    std::fs::write(
        &lock,
        r#"{"schema_version":1,"lane":"demo","repo":"ops","instance":"i","claimed_at":"2026-06-18T00:00:00Z","updated_at":"2026-06-18T00:00:00Z","expires_at":"2090-01-01T00:00:00Z","ttl_hours":12}"#,
    )
    .unwrap();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();

    let out = run(r, None, &["status", "demo", "--repo", "ops", "--json"]);
    assert_eq!(code(&out), 0);
    assert_eq!(stdout_json(&out)["data"]["present"], true);

    let mode = std::fs::metadata(&lock).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o644, "a read verb must not chmod the object");
    assert!(
        !audit_path(r, "ops").exists(),
        "a read verb must not write the audit log"
    );
}
