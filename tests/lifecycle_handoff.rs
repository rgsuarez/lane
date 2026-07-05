//! Slice 3 WS2 — `lane handoff`: owner-only, non-releasing, expiry-refusing claim_status
//! flip (the exact renew posture), write-audited with a single terminal `handoff` event.

mod common;

use common::*;
use lane::lock::audit::AuditEventKind;

#[test]
fn handoff_flips_status_keeps_claim_held_and_preserves_expiry() {
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
    let before = read_lock(r, "ops", "demo").expect("claim on disk");
    assert_eq!(before.claim_status, None, "plain claim writes no status");

    let out = run(
        r,
        Some("a"),
        &["handoff", "demo", "--repo", "ops", "--json"],
    );
    assert_eq!(code(&out), 0);
    let v = stdout_json(&out);
    assert_eq!(v["ok"], true);
    assert_eq!(v["verb"], "handoff");
    assert_eq!(v["outcome"], "ok");
    assert_eq!(v["data"]["claim_status"], "handoff");

    let after = read_lock(r, "ops", "demo").expect("claim STILL on disk (non-releasing)");
    assert_eq!(
        format!("{:?}", after.claim_status),
        "Some(Handoff)",
        "status flipped"
    );
    assert_eq!(
        after.expires_at, before.expires_at,
        "handoff does not extend the lease"
    );
    assert!(after.updated_at > before.updated_at, "updated_at refreshed");
    assert_eq!(after.instance, before.instance, "owner unchanged");
}

#[test]
fn handoff_claim_stays_protected_against_other_claimers() {
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
            Some("a"),
            &["handoff", "demo", "--repo", "ops", "--json"]
        )),
        0
    );
    // A handoff claim is still an ACTIVE claim: another instance's plain claim refuses.
    let steal = run(r, Some("b"), &["claim", "demo", "--repo", "ops", "--json"]);
    assert_eq!(code(&steal), 1);
    assert_eq!(stdout_json(&steal)["reason"], "active_held");
    // And the owner can still release it normally afterward.
    let rel = run(
        r,
        Some("a"),
        &["release", "demo", "--repo", "ops", "--json"],
    );
    assert_eq!(code(&rel), 0);
}

#[test]
fn re_handoff_is_idempotent_success_and_replaces_note() {
    let root = temp_root();
    let r = root.path();
    assert_eq!(
        code(&run(
            r,
            Some("a"),
            &["claim", "demo", "--repo", "ops", "--note", "original", "--json"]
        )),
        0
    );
    assert_eq!(
        code(&run(
            r,
            Some("a"),
            &["handoff", "demo", "--repo", "ops", "--json"]
        )),
        0
    );
    let first = read_lock(r, "ops", "demo").unwrap();
    assert_eq!(
        first.note.as_deref(),
        Some("original"),
        "note preserved when --note omitted"
    );

    // Re-handoff of an already-handoff claim: exit 0, note replaced when given.
    let out = run(
        r,
        Some("a"),
        &[
            "handoff",
            "demo",
            "--repo",
            "ops",
            "--note",
            "digest v2",
            "--json",
        ],
    );
    assert_eq!(code(&out), 0, "idempotent re-handoff succeeds");
    let second = read_lock(r, "ops", "demo").unwrap();
    assert_eq!(format!("{:?}", second.claim_status), "Some(Handoff)");
    assert_eq!(second.note.as_deref(), Some("digest v2"), "note replaced");
}

#[test]
fn handoff_is_owner_only() {
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
    let out = run(
        r,
        Some("b"),
        &["handoff", "demo", "--repo", "ops", "--json"],
    );
    assert_eq!(code(&out), 1);
    assert_eq!(stdout_json(&out)["reason"], "not_owner");
    let rec = read_lock(r, "ops", "demo").unwrap();
    assert_eq!(
        rec.claim_status, None,
        "refusal leaves the record untouched"
    );
}

#[test]
fn handoff_of_unheld_lane_is_refused_not_held() {
    let root = temp_root();
    let out = run(
        root.path(),
        Some("x"),
        &["handoff", "ghost", "--repo", "ops", "--json"],
    );
    assert_eq!(code(&out), 1);
    assert_eq!(stdout_json(&out)["reason"], "not_held");
}

#[test]
fn handoff_of_expired_lease_is_refused_expired() {
    let root = temp_root();
    let r = root.path();
    assert_eq!(
        code(&run(
            r,
            Some("a"),
            &[
                "claim",
                "demo",
                "--repo",
                "ops",
                "--ttl-hours",
                "0.0003",
                "--json"
            ]
        )),
        0
    );
    std::thread::sleep(std::time::Duration::from_millis(1300));
    let out = run(
        r,
        Some("a"),
        &["handoff", "demo", "--repo", "ops", "--json"],
    );
    assert_eq!(code(&out), 1);
    assert_eq!(stdout_json(&out)["reason"], "expired");
}

#[test]
fn handoff_appends_a_terminal_handoff_audit_event() {
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
            Some("a"),
            &["handoff", "demo", "--repo", "ops", "--json"]
        )),
        0
    );
    let events = read_audit(r, "ops");
    let handoffs: Vec<_> = events
        .iter()
        .filter(|e| e.event == AuditEventKind::Handoff)
        .collect();
    assert_eq!(handoffs.len(), 1, "exactly one terminal handoff event");
    assert_eq!(handoffs[0].lane, "demo");
    assert_eq!(handoffs[0].instance, "a");
    assert!(handoffs[0].op_id.is_some());
    // Non-destructive: no intent/completion pair for a handoff.
    let intents: Vec<_> = events
        .iter()
        .filter(|e| e.event == AuditEventKind::Intent)
        .collect();
    assert!(
        intents.is_empty(),
        "handoff writes no write-ahead intent (non-destructive)"
    );
}

#[test]
fn handoff_note_is_length_validated() {
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
    let long = "x".repeat(2000);
    let out = run(
        r,
        Some("a"),
        &[
            "handoff", "demo", "--repo", "ops", "--note", &long, "--json",
        ],
    );
    assert_eq!(code(&out), 2, "an over-long note is a validation error");
}
