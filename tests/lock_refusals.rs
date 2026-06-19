//! Slice 2 — refusals and the force semantics (binary).

mod common;

use common::*;

#[test]
fn active_lane_without_force_is_refused() {
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
    assert_eq!(j["outcome"], "refused");
    assert_eq!(j["reason"], "active_held");
}

#[test]
fn force_takes_over_active_lane() {
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
        &["claim", "demo", "--repo", "ops", "--force", "--json"],
    );
    assert_eq!(code(&out), 0);
    let j = stdout_json(&out);
    assert_eq!(j["outcome"], "ok");
    assert_eq!(j["data"]["forced"], true);
    assert_eq!(j["data"]["prior_instance"], "a");
    assert_eq!(read_lock(r, "ops", "demo").unwrap().instance, "b");
    // The takeover is write-ahead audited: intent + completion share an op_id.
    let events = read_audit(r, "ops");
    assert!(events.iter().any(|e| format!("{:?}", e.event) == "Intent"));
    assert!(events
        .iter()
        .any(|e| format!("{:?}", e.event) == "Completion"));
}

#[test]
fn force_never_bypasses_target_overlap() {
    let root = temp_root();
    let r = root.path();
    // Lane A reserves a target.
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
                "/tmp/lane-it-shared",
                "--json"
            ]
        )),
        0
    );
    // Lane B (a different lane) with --force still must pass the overlap scan → refused.
    let out = run(
        r,
        Some("b"),
        &[
            "claim",
            "beta",
            "--repo",
            "ops",
            "--target",
            "/tmp/lane-it-shared",
            "--force",
            "--json",
        ],
    );
    assert_eq!(code(&out), 1);
    assert_eq!(stdout_json(&out)["reason"], "target_overlap");
}

#[test]
fn ancestor_target_overlap_is_refused() {
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
                "/tmp/lane-it-tree/child",
                "--json"
            ]
        )),
        0
    );
    // An ancestor of the reserved target overlaps.
    let out = run(
        r,
        Some("b"),
        &[
            "claim",
            "beta",
            "--repo",
            "ops",
            "--target",
            "/tmp/lane-it-tree",
            "--json",
        ],
    );
    assert_eq!(code(&out), 1);
    assert_eq!(stdout_json(&out)["reason"], "target_overlap");
}

#[test]
fn distinct_targets_do_not_overlap() {
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
                "/tmp/lane-it-x",
                "--json"
            ]
        )),
        0
    );
    assert_eq!(
        code(&run(
            r,
            Some("b"),
            &[
                "claim",
                "beta",
                "--repo",
                "ops",
                "--target",
                "/tmp/lane-it-y",
                "--json"
            ]
        )),
        0
    );
}

#[test]
fn renew_and_release_reject_not_owner() {
    let root = temp_root();
    let r = root.path();
    assert_eq!(
        code(&run(
            r,
            Some("owner"),
            &["claim", "demo", "--repo", "ops", "--json"]
        )),
        0
    );

    let out = run(
        r,
        Some("intruder"),
        &["renew", "demo", "--repo", "ops", "--json"],
    );
    assert_eq!(code(&out), 1);
    assert_eq!(stdout_json(&out)["reason"], "not_owner");

    let out = run(
        r,
        Some("intruder"),
        &["release", "demo", "--repo", "ops", "--json"],
    );
    assert_eq!(code(&out), 1);
    assert_eq!(stdout_json(&out)["reason"], "not_owner");
}

#[test]
fn renew_of_unheld_lane_is_refused_not_held() {
    let root = temp_root();
    let out = run(
        root.path(),
        Some("x"),
        &["renew", "ghost", "--repo", "ops", "--json"],
    );
    assert_eq!(code(&out), 1);
    assert_eq!(stdout_json(&out)["reason"], "not_held");
}

#[test]
fn renew_of_expired_lease_is_refused_expired() {
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
    let out = run(r, Some("a"), &["renew", "demo", "--repo", "ops", "--json"]);
    assert_eq!(code(&out), 1);
    assert_eq!(stdout_json(&out)["reason"], "expired");
}

#[test]
fn force_flag_does_not_exist_on_renew_or_release() {
    let root = temp_root();
    let r = root.path();
    // Clap usage error: exit 2, human-only (no JSON envelope on stdout).
    let out = run(r, Some("a"), &["renew", "demo", "--repo", "ops", "--force"]);
    assert_eq!(code(&out), 2);
    assert!(
        out.stdout.is_empty(),
        "clap usage error is not a JSON envelope"
    );
    assert!(!out.stderr.is_empty());

    let out = run(
        r,
        Some("a"),
        &["release", "demo", "--repo", "ops", "--force"],
    );
    assert_eq!(code(&out), 2);
    assert!(out.stdout.is_empty());
}
