//! Slice 2 — happy-path lifecycle, JSON-envelope shape, and absent/expired exit-0 reads.

mod common;

use common::*;

#[test]
fn claim_status_list_renew_release_happy_path() {
    let root = temp_root();
    let r = root.path();

    // claim
    let out = run(
        r,
        Some("inst-A"),
        &[
            "claim",
            "demo",
            "--repo",
            "ops",
            "--target",
            "/tmp/lane-it-wt-A",
            "--json",
        ],
    );
    assert_eq!(code(&out), 0);
    let j = stdout_json(&out);
    assert_eq!(j["schema_version"], 1);
    assert_eq!(j["ok"], true);
    assert_eq!(j["verb"], "claim");
    assert_eq!(j["outcome"], "ok");
    assert_eq!(j["data"]["instance"], "inst-A");
    assert_eq!(j["data"]["forced"], false);
    assert!(j["reason"].is_null());

    // status (present)
    let out = run(r, None, &["status", "demo", "--repo", "ops", "--json"]);
    assert_eq!(code(&out), 0);
    let j = stdout_json(&out);
    assert_eq!(j["outcome"], "ok");
    assert_eq!(j["data"]["present"], true);
    assert_eq!(j["data"]["record"]["instance"], "inst-A");
    assert_eq!(j["data"]["stale_state"], "active");

    // list
    let out = run(r, None, &["list", "--json"]);
    assert_eq!(code(&out), 0);
    let j = stdout_json(&out);
    assert_eq!(j["outcome"], "ok");
    assert_eq!(j["data"]["rows"].as_array().unwrap().len(), 1);

    // renew
    let out = run(
        r,
        Some("inst-A"),
        &["renew", "demo", "--repo", "ops", "--json"],
    );
    assert_eq!(code(&out), 0);
    assert_eq!(stdout_json(&out)["outcome"], "ok");

    // release
    let out = run(
        r,
        Some("inst-A"),
        &["release", "demo", "--repo", "ops", "--json"],
    );
    assert_eq!(code(&out), 0);
    let j = stdout_json(&out);
    assert_eq!(j["outcome"], "released");
    assert_eq!(j["data"]["present"], true);
}

#[test]
fn status_of_absent_lane_is_exit0_not_held() {
    let root = temp_root();
    let out = run(
        root.path(),
        None,
        &["status", "ghost", "--repo", "ops", "--json"],
    );
    assert_eq!(code(&out), 0);
    let j = stdout_json(&out);
    assert_eq!(j["ok"], true);
    assert_eq!(j["outcome"], "not_held");
    assert_eq!(j["data"]["present"], false);
}

#[test]
fn release_of_absent_lane_is_exit0_not_held() {
    let root = temp_root();
    let out = run(
        root.path(),
        Some("whoever"),
        &["release", "ghost", "--repo", "ops", "--json"],
    );
    assert_eq!(code(&out), 0);
    let j = stdout_json(&out);
    assert_eq!(j["ok"], true);
    assert_eq!(j["outcome"], "not_held");
    assert_eq!(j["data"]["present"], false);
}

#[test]
fn list_of_empty_root_is_exit0_no_rows() {
    let root = temp_root();
    let out = run(root.path(), None, &["list", "--json"]);
    assert_eq!(code(&out), 0);
    assert_eq!(
        stdout_json(&out)["data"]["rows"].as_array().unwrap().len(),
        0
    );
}

#[test]
fn expired_claim_status_is_exit0_with_stale_expired() {
    let root = temp_root();
    let r = root.path();
    // Claim with the shortest possible lease, then wait it out (ttl is in hours; use a
    // tiny fraction so the lease lapses quickly without sleeping long).
    let out = run(
        r,
        Some("inst-A"),
        &[
            "claim",
            "demo",
            "--repo",
            "ops",
            "--ttl-hours",
            "0.0003",
            "--json",
        ],
    );
    assert_eq!(code(&out), 0);
    std::thread::sleep(std::time::Duration::from_millis(1300)); // > 0.0003h (~1.08s)

    let out = run(r, None, &["status", "demo", "--repo", "ops", "--json"]);
    assert_eq!(
        code(&out),
        0,
        "status of an expired-but-present claim is exit 0"
    );
    let j = stdout_json(&out);
    assert_eq!(j["ok"], true);
    assert_eq!(j["data"]["present"], true);
    assert_eq!(j["data"]["stale_state"], "expired");
}

#[test]
fn exactly_one_json_envelope_per_invocation() {
    // Both a success and a refusal print exactly one line of JSON to stdout.
    let root = temp_root();
    let r = root.path();
    let out = run(r, Some("a"), &["claim", "demo", "--repo", "ops", "--json"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim().lines().count(),
        1
    );

    let out = run(r, Some("b"), &["claim", "demo", "--repo", "ops", "--json"]);
    assert_eq!(code(&out), 1);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim().lines().count(),
        1
    );
    assert_eq!(stdout_json(&out)["outcome"], "refused");
}
