//! Slice 2 fail-closed remediation — focused adversarial regression tests for the five
//! Codex-review defects: guarded reads, guarded audit, full-stream validation, dangling-
//! intent reconciliation, and refusal/malformed audit coverage.

mod common;

use common::*;

use chrono::Utc;
use lane::lock::audit::{self, AuditEventKind};
use lane::lock::paths::LaneRoot;
use lane::lock::{claim, ClaimParams, StdFs};
use lane::{LaneError, RefusedReason};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

// ---- helpers ----

fn valid_lock(lane: &str, instance: &str) -> String {
    format!(
        r#"{{"schema_version":1,"lane":"{lane}","repo":"ops","instance":"{instance}","claimed_at":"2026-06-18T00:00:00Z","updated_at":"2026-06-18T00:00:00Z","expires_at":"2090-01-01T00:00:00Z","ttl_hours":12}}"#
    )
}

fn dangling_takeover(lane: &str, instance: &str, prior: &str, op_id: &str) -> String {
    format!(
        r#"{{"ts":"2026-06-18T00:00:00Z","op_id":"{op_id}","op":"takeover","event":"intent","repo":"ops","lane":"{lane}","instance":"{instance}","prior_instance":"{prior}","outcome":"ok"}}"#
    )
}

fn dangling_release(lane: &str, instance: &str, op_id: &str) -> String {
    format!(
        r#"{{"ts":"2026-06-18T00:00:00Z","op_id":"{op_id}","op":"release","event":"intent","repo":"ops","lane":"{lane}","instance":"{instance}","outcome":"ok"}}"#
    )
}

fn write_lock(root: &Path, lane: &str, body: &str) {
    let locks = root.join("ops").join("locks");
    std::fs::create_dir_all(&locks).unwrap();
    std::fs::write(locks.join(format!("{lane}.lock")), body).unwrap();
}

fn write_audit(root: &Path, lines: &[&str]) {
    std::fs::create_dir_all(root.join("ops")).unwrap();
    let mut body = String::new();
    for l in lines {
        body.push_str(l);
        body.push('\n');
    }
    std::fs::write(audit_path(root, "ops"), body).unwrap();
}

fn symlink_lock(root: &Path, lane: &str, target: &Path) {
    let locks = root.join("ops").join("locks");
    std::fs::create_dir_all(&locks).unwrap();
    std::os::unix::fs::symlink(target, locks.join(format!("{lane}.lock"))).unwrap();
}

// ---- defect 1: guarded reads (VULN 1) ----

#[test]
fn status_rejects_symlinked_claim() {
    let root = temp_root();
    let r = root.path();
    let ext = root.path().join("external.json");
    std::fs::write(&ext, valid_lock("demo", "EXTERNAL")).unwrap();
    symlink_lock(r, "demo", &ext);
    let out = run(r, None, &["status", "demo", "--repo", "ops", "--json"]);
    assert_eq!(code(&out), 2, "status must not follow a symlinked claim");
    assert_eq!(stdout_json(&out)["reason"], "identity");
    // The external content must NOT have been surfaced.
    assert!(!String::from_utf8_lossy(&out.stdout).contains("EXTERNAL"));
}

#[test]
fn list_rejects_symlinked_claim() {
    let root = temp_root();
    let r = root.path();
    let ext = root.path().join("external.json");
    std::fs::write(&ext, valid_lock("demo", "EXTERNAL")).unwrap();
    symlink_lock(r, "demo", &ext);
    let out = run(r, None, &["list", "--json"]);
    assert_eq!(code(&out), 2);
    assert!(!String::from_utf8_lossy(&out.stdout).contains("EXTERNAL"));
}

#[test]
fn board_rejects_symlinked_claim() {
    let root = temp_root();
    let r = root.path();
    let ext = root.path().join("external.json");
    std::fs::write(&ext, valid_lock("demo", "EXTERNAL")).unwrap();
    symlink_lock(r, "demo", &ext);
    let out = run(r, None, &["board", "--json"]);
    assert_eq!(code(&out), 2, "board must not follow a symlinked claim");
}

// ---- defect 1: force never bypasses the object guard ----

#[test]
fn force_takeover_accepts_malformed_regular_but_rejects_symlink() {
    // Malformed REGULAR same-lane record → force-takeable.
    let root = temp_root();
    let r = root.path();
    write_lock(r, "demo", "this-is-not-json");
    let out = run(
        r,
        Some("new"),
        &["claim", "demo", "--repo", "ops", "--force", "--json"],
    );
    assert_eq!(code(&out), 0, "force takes over a malformed regular record");
    assert_eq!(read_lock(r, "ops", "demo").unwrap().instance, "new");

    // Symlinked same-lane record → fails closed EVEN under --force.
    let root2 = temp_root();
    let r2 = root2.path();
    let ext = root2.path().join("ext.json");
    std::fs::write(&ext, valid_lock("demo", "EXTERNAL")).unwrap();
    symlink_lock(r2, "demo", &ext);
    let out = run(
        r2,
        Some("new"),
        &["claim", "demo", "--repo", "ops", "--force", "--json"],
    );
    assert_eq!(code(&out), 2, "force must not bypass the object guard");
    assert_eq!(stdout_json(&out)["reason"], "identity");
}

// ---- defect 2: full-stream audit validation ----

#[test]
fn earlier_malformed_audit_with_valid_final_fails_closed() {
    let root = temp_root();
    let r = root.path();
    // Earlier malformed record + a VALID final record (both newline-terminated).
    let valid = r#"{"ts":"2026-06-18T00:00:00Z","event":"claim","repo":"ops","lane":"x","instance":"i","outcome":"ok"}"#;
    write_audit(r, &["EARLIER-MALFORMED-RECORD", valid]);
    let out = run(
        r,
        Some("a"),
        &["claim", "newlane", "--repo", "ops", "--json"],
    );
    assert_eq!(
        code(&out),
        2,
        "an earlier malformed complete record fails closed"
    );
    assert_eq!(stdout_json(&out)["reason"], "malformed");
    // State unchanged: the claim did not proceed.
    assert!(read_lock(r, "ops", "newlane").is_none());
}

// ---- defect 2: guarded audit object ----

#[test]
fn non_regular_audit_log_fails_closed() {
    let root = temp_root();
    let r = root.path();
    // A directory where audit.log is expected.
    std::fs::create_dir_all(r.join("ops").join("audit.log")).unwrap();
    let out = run(r, Some("a"), &["claim", "demo", "--repo", "ops", "--json"]);
    assert_eq!(code(&out), 2);
    assert_eq!(stdout_json(&out)["reason"], "identity");
}

#[test]
fn wrong_owner_audit_log_fails_closed_via_injected_seam() {
    let root = temp_root();
    let r = root.path();
    write_audit(
        r,
        &[
            r#"{"ts":"2026-06-18T00:00:00Z","event":"claim","repo":"ops","lane":"x","instance":"i","outcome":"ok"}"#,
        ],
    );
    let expected = std::fs::metadata(r).unwrap().uid();
    let fault = FaultFs {
        owner: Some(expected.wrapping_add(1)),
        ..Default::default()
    };
    let res = audit::recover_if_needed(
        &audit_path(r, "ops"),
        "ops",
        "demo",
        "i",
        Utc::now(),
        &fault,
        expected,
    );
    assert!(
        matches!(res, Err(LaneError::Identity(_))),
        "wrong-owner audit log fails closed"
    );
}

#[test]
fn audit_recovered_dir_must_be_a_directory() {
    let root = temp_root();
    let r = root.path();
    // A trailing fragment forces recovery; a regular file blocks the audit.recovered dir.
    let valid = r#"{"ts":"2026-06-18T00:00:00Z","event":"claim","repo":"ops","lane":"x","instance":"i","outcome":"ok"}"#;
    std::fs::create_dir_all(r.join("ops")).unwrap();
    std::fs::write(
        audit_path(r, "ops"),
        format!("{valid}\nFRAGMENT-NO-NEWLINE"),
    )
    .unwrap();
    std::fs::write(
        r.join("ops").join("audit.recovered"),
        b"i am a file not a dir",
    )
    .unwrap();
    let out = run(r, Some("a"), &["claim", "demo", "--repo", "ops", "--json"]);
    assert_eq!(
        code(&out),
        2,
        "a non-directory audit.recovered fails closed"
    );
    assert_eq!(stdout_json(&out)["reason"], "identity");
}

// ---- defect 4: dangling-intent reconciliation ----

#[test]
fn indeterminate_dangling_intent_blocks_mutation() {
    let root = temp_root();
    let r = root.path();
    // Dangling takeover intent (new over old) but the lock holds a THIRD instance.
    write_audit(r, &[&dangling_takeover("demo", "new", "old", "op-X")]);
    write_lock(r, "demo", &valid_lock("demo", "stranger"));
    let out = run(
        r,
        Some("whoever"),
        &["claim", "demo", "--repo", "ops", "--force", "--json"],
    );
    assert_eq!(
        code(&out),
        2,
        "indeterminate intent blocks the mutation (fail closed)"
    );
    assert_eq!(stdout_json(&out)["reason"], "identity");
    // State unchanged: the lock still holds the stranger.
    assert_eq!(read_lock(r, "ops", "demo").unwrap().instance, "stranger");
}

#[test]
fn status_surfaces_applied_takeover_warning_without_mutating() {
    let root = temp_root();
    let r = root.path();
    write_audit(r, &[&dangling_takeover("demo", "new", "old", "op-Y")]);
    write_lock(r, "demo", &valid_lock("demo", "new")); // lock matches the intent → applied
    let lock_file = r.join("ops").join("locks").join("demo.lock");
    std::fs::set_permissions(&lock_file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let audit_before = std::fs::read(audit_path(r, "ops")).unwrap();

    let out = run(r, None, &["status", "demo", "--repo", "ops", "--json"]);
    assert_eq!(code(&out), 0);
    let j = stdout_json(&out);
    assert_eq!(j["data"]["present"], true);
    let warn = j["audit_warning"].as_str().unwrap_or("");
    assert!(
        warn.contains("applied"),
        "status surfaces the applied disposition: {warn}"
    );
    // Read verb mutated nothing: audit unchanged, lock mode unchanged.
    assert_eq!(std::fs::read(audit_path(r, "ops")).unwrap(), audit_before);
    let mode = std::fs::metadata(&lock_file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o644);
}

#[test]
fn status_surfaces_dangling_release_warning() {
    let root = temp_root();
    let r = root.path();
    // Dangling release intent + absent lock → applied.
    write_audit(r, &[&dangling_release("demo", "owner", "op-Z")]);
    std::fs::create_dir_all(r.join("ops").join("locks")).unwrap();
    let out = run(r, None, &["status", "demo", "--repo", "ops", "--json"]);
    assert_eq!(code(&out), 0);
    let warn = stdout_json(&out)["audit_warning"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(
        warn.contains("release") && warn.contains("applied"),
        "warning: {warn}"
    );
}

// ---- defect 5: refusal/malformed audit coverage + code preservation ----

#[test]
fn refusals_emit_claim_refused_events() {
    // active_held
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
    assert_eq!(
        code(&run(
            r,
            Some("b"),
            &["claim", "demo", "--repo", "ops", "--json"]
        )),
        1
    );
    let events = read_audit(r, "ops");
    assert!(
        events
            .iter()
            .any(|e| format!("{:?}", e.event) == "ClaimRefused"
                && e.reason.as_deref() == Some("active_held")),
        "active-held refusal emits claim_refused"
    );

    // target_overlap
    let root2 = temp_root();
    let r2 = root2.path();
    assert_eq!(
        code(&run(
            r2,
            Some("a"),
            &[
                "claim",
                "alpha",
                "--repo",
                "ops",
                "--target",
                "/tmp/lane-fc-shared",
                "--json"
            ]
        )),
        0
    );
    assert_eq!(
        code(&run(
            r2,
            Some("b"),
            &[
                "claim",
                "beta",
                "--repo",
                "ops",
                "--target",
                "/tmp/lane-fc-shared",
                "--json"
            ]
        )),
        1
    );
    let events = read_audit(r2, "ops");
    assert!(
        events
            .iter()
            .any(|e| format!("{:?}", e.event) == "ClaimRefused"
                && e.reason.as_deref() == Some("target_overlap")),
        "overlap refusal emits claim_refused"
    );
}

#[test]
fn malformed_same_lane_emits_malformed_event() {
    let root = temp_root();
    let r = root.path();
    write_lock(r, "demo", "not-json"); // malformed REGULAR record, no --force
    let out = run(r, Some("a"), &["claim", "demo", "--repo", "ops", "--json"]);
    assert_eq!(code(&out), 2);
    assert_eq!(stdout_json(&out)["reason"], "malformed");
    let events = read_audit(r, "ops");
    assert!(
        events
            .iter()
            .any(|e| format!("{:?}", e.event) == "Malformed"),
        "a malformed same-lane rejection emits a malformed event"
    );
}

#[test]
fn malformed_sibling_during_overlap_scan_emits_malformed_event() {
    let root = temp_root();
    let r = root.path();
    write_lock(r, "bad", "}{ not json"); // malformed sibling
    let out = run(
        r,
        Some("a"),
        &[
            "claim",
            "newlane",
            "--repo",
            "ops",
            "--target",
            "/tmp/lane-fc-x",
            "--json",
        ],
    );
    assert_eq!(
        code(&out),
        2,
        "a malformed sibling during the scan fails closed"
    );
    assert_eq!(stdout_json(&out)["reason"], "malformed");
    let events = read_audit(r, "ops");
    assert!(events
        .iter()
        .any(|e| format!("{:?}", e.event) == "Malformed"));
}

#[test]
fn refusal_audit_failure_preserves_the_refusal_code() {
    // A failed claim_refused append must NOT change the primary Refused(active_held) error.
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
    let params = ClaimParams {
        repo: "ops".into(),
        lane: "demo".into(),
        instance: "b".into(),
        home: Some(home()),
        ..Default::default()
    };
    let res = claim::claim_core(&root_obj, &params, Utc::now(), &StdFs, &audit);
    let err = res.err().expect("refused");
    assert!(
        matches!(err.error, LaneError::Refused(RefusedReason::ActiveHeld)),
        "the primary refusal (exit 1) is preserved even when the refusal audit fails"
    );
    assert_eq!(err.error.exit_code(), 1);
    // Fix B: the audit-append failure is now surfaced as a warning (not silently dropped).
    assert!(
        err.audit_warning.is_some(),
        "a failed refusal-audit append is surfaced as an audit_warning"
    );
}
