//! Slice 3 WS2 — `lane start` / `lane close` end to end: real binary, real git, real
//! locking root, all under a `$HOME` tempdir. Pins the claim-first invariant (a refusal
//! never mutates git), the compensation path (real git failure via a bad base ref), the
//! close outcome table (dirty refusal, expired refusal, absent exit 0, skip-missing),
//! and the renew-first teardown (a dirty refusal EXTENDS the lease, proving renew ran
//! before the failed remove).

mod common;

use common::*;

/// The git repo (and thus the derived worktree) must live OUTSIDE the lane root: a
/// target contained by `LANE_ROOT` is rejected by design (root/target ancestry, both
/// directions). A separate `$HOME` tempdir keeps the device check green while staying
/// disjoint from the claim store.
fn scratch_area() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("lane-git-")
        .tempdir_in(home())
        .expect("scratch area under HOME")
}

fn scratch_repo(area: &std::path::Path) -> std::path::PathBuf {
    let repo = area.join("repo");
    init_scratch_repo(&repo);
    repo
}

fn start_args<'a>(repo_path: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut v = vec![
        "start",
        "LQOS-9",
        "--repo",
        "ops",
        "--git-repo",
        repo_path,
        "--json",
    ];
    v.extend_from_slice(extra);
    v
}

#[test]
fn start_happy_creates_branch_worktree_and_lifecycle_claim() {
    let root = temp_root();
    let r = root.path();
    let area = scratch_area();
    let repo = scratch_repo(area.path());
    let repo_s = repo.to_str().unwrap();

    let out = run(
        r,
        Some("a"),
        &start_args(repo_s, &["--linear-key", "LQOS-9"]),
    );
    assert_eq!(
        code(&out),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = stdout_json(&out);
    assert_eq!(v["ok"], true);
    assert_eq!(v["verb"], "start");

    // Uppercase lane -> lowercased branch + worktree leaf; stored target == on-disk path.
    let expected_wt = format!("{repo_s}-lqos-9");
    assert_eq!(v["data"]["worktree"], expected_wt.as_str());
    assert_eq!(v["data"]["branch"], "lqos-9");
    assert!(
        std::path::Path::new(&expected_wt).is_dir(),
        "worktree exists on disk"
    );

    let rec = read_lock(r, "ops", "LQOS-9").expect("claim on disk");
    assert_eq!(
        rec.target.as_deref(),
        Some(expected_wt.as_str()),
        "stored target IS the on-disk path"
    );
    assert_eq!(rec.branch.as_deref(), Some("lqos-9"));
    assert_eq!(rec.linear_key.as_deref(), Some("LQOS-9"));
    assert_eq!(format!("{:?}", rec.role), "Some(Executor)");
    assert_eq!(format!("{:?}", rec.claim_status), "Some(Active)");
}

#[test]
fn start_claim_refusal_never_mutates_git() {
    let root = temp_root();
    let r = root.path();
    let area = scratch_area();
    let repo = scratch_repo(area.path());
    let repo_s = repo.to_str().unwrap();

    // Another instance holds the lane: start must refuse WITHOUT touching git (N1).
    assert_eq!(
        code(&run(
            r,
            Some("other"),
            &["claim", "LQOS-9", "--repo", "ops", "--json"]
        )),
        0
    );
    let out = run(r, Some("a"), &start_args(repo_s, &[]));
    assert_eq!(code(&out), 1);
    assert_eq!(stdout_json(&out)["reason"], "active_held");
    assert!(
        !std::path::Path::new(&format!("{repo_s}-lqos-9")).exists(),
        "no worktree was created on a claim refusal"
    );
    let branches = std::process::Command::new("git")
        .args(["-C", repo_s, "branch", "--list", "lqos-9"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
        "no branch was created on a claim refusal"
    );
}

#[test]
fn start_target_overlap_refusal_never_mutates_git() {
    let root = temp_root();
    let r = root.path();
    let area = scratch_area();
    let repo = scratch_repo(area.path());
    let repo_s = repo.to_str().unwrap();
    let wt = format!("{repo_s}-lqos-9");

    // A sibling lane already reserves the derived worktree path as its target.
    assert_eq!(
        code(&run(
            r,
            Some("other"),
            &["claim", "sibling", "--repo", "ops", "--target", &wt, "--json"]
        )),
        0
    );
    let out = run(r, Some("a"), &start_args(repo_s, &[]));
    assert_eq!(code(&out), 1);
    assert_eq!(stdout_json(&out)["reason"], "target_overlap");
    assert!(
        !std::path::Path::new(&wt).exists(),
        "no git mutation on overlap refusal"
    );
}

#[test]
fn start_git_failure_compensates_release_so_retry_succeeds() {
    let root = temp_root();
    let r = root.path();
    let area = scratch_area();
    let repo = scratch_repo(area.path());
    let repo_s = repo.to_str().unwrap();

    // A nonexistent base ref passes the prechecks (repo valid, branch absent, path
    // absent) and fails the real `git worktree add` -> the compensation path releases
    // the just-made claim, so an immediate corrected retry succeeds.
    let out = run(
        r,
        Some("a"),
        &start_args(repo_s, &["--base", "no-such-ref-anywhere"]),
    );
    assert_eq!(code(&out), 2, "a git failure is an io-class error");
    let v = stdout_json(&out);
    assert_eq!(v["ok"], false);
    assert_eq!(v["reason"], "io");
    assert!(
        read_lock(r, "ops", "LQOS-9").is_none(),
        "the claim was compensating-released"
    );

    let retry = run(r, Some("a"), &start_args(repo_s, &[]));
    assert_eq!(code(&retry), 0, "retry succeeds after compensation");
}

#[test]
fn start_precheck_failures_exit_2_before_any_claim() {
    let root = temp_root();
    let r = root.path();
    let plain = r.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let plain_s = plain.to_str().unwrap();

    // Not a git repo: refused in step 0, no claim ever made, no audit-log churn.
    let out = run(r, Some("a"), &start_args(plain_s, &[]));
    assert_eq!(code(&out), 2);
    assert!(
        read_lock(r, "ops", "LQOS-9").is_none(),
        "no claim on a precheck failure"
    );
    assert!(
        !r.join("ops").join("audit.log").exists(),
        "typo-class failures never reach the audit log"
    );
}

#[test]
fn start_rejects_an_invalid_branch_name_before_any_claim() {
    let root = temp_root();
    let r = root.path();
    let area = scratch_area();
    let repo = scratch_repo(area.path());
    let repo_s = repo.to_str().unwrap();

    // check-ref-format catches shapes the leading-dash guard cannot (.., ~, spaces).
    let out = run(
        r,
        Some("a"),
        &start_args(repo_s, &["--branch", "bad..name"]),
    );
    assert_eq!(code(&out), 2, "an invalid refname is refused");
    assert!(
        read_lock(r, "ops", "LQOS-9").is_none(),
        "no claim on an invalid branch name"
    );
    assert!(
        !r.join("ops").join("audit.log").exists(),
        "refused before the claim, so no audit churn"
    );
}

#[test]
fn close_plain_releases_without_touching_the_worktree() {
    let root = temp_root();
    let r = root.path();
    let area = scratch_area();
    let repo = scratch_repo(area.path());
    let repo_s = repo.to_str().unwrap();
    assert_eq!(code(&run(r, Some("a"), &start_args(repo_s, &[]))), 0);
    let wt = format!("{repo_s}-lqos-9");

    let out = run(
        r,
        Some("a"),
        &["close", "LQOS-9", "--repo", "ops", "--json"],
    );
    assert_eq!(code(&out), 0);
    let v = stdout_json(&out);
    assert_eq!(v["outcome"], "released");
    assert_eq!(v["data"]["released"], true);
    assert_eq!(v["data"]["worktree_removed"], false);
    assert!(std::path::Path::new(&wt).is_dir(), "worktree left in place");
    assert!(read_lock(r, "ops", "LQOS-9").is_none(), "claim released");
}

#[test]
fn close_remove_worktree_happy_removes_and_releases() {
    let root = temp_root();
    let r = root.path();
    let area = scratch_area();
    let repo = scratch_repo(area.path());
    let repo_s = repo.to_str().unwrap();
    assert_eq!(code(&run(r, Some("a"), &start_args(repo_s, &[]))), 0);
    let wt = format!("{repo_s}-lqos-9");
    assert!(std::path::Path::new(&wt).is_dir());

    let out = run(
        r,
        Some("a"),
        &[
            "close",
            "LQOS-9",
            "--repo",
            "ops",
            "--remove-worktree",
            "--json",
        ],
    );
    assert_eq!(
        code(&out),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = stdout_json(&out);
    assert_eq!(v["outcome"], "released");
    assert_eq!(v["data"]["worktree_removed"], true);
    assert!(!std::path::Path::new(&wt).exists(), "worktree removed");
    assert!(read_lock(r, "ops", "LQOS-9").is_none(), "claim released");
}

#[test]
fn close_dirty_worktree_refuses_with_claim_intact_and_lease_extended() {
    let root = temp_root();
    let r = root.path();
    let area = scratch_area();
    let repo = scratch_repo(area.path());
    let repo_s = repo.to_str().unwrap();
    assert_eq!(code(&run(r, Some("a"), &start_args(repo_s, &[]))), 0);
    let wt = format!("{repo_s}-lqos-9");
    let before = read_lock(r, "ops", "LQOS-9").unwrap();

    // Dirty the worktree; the no-force remove must refuse.
    std::fs::write(std::path::Path::new(&wt).join("scratch.txt"), "wip\n").unwrap();
    let out = run(
        r,
        Some("a"),
        &[
            "close",
            "LQOS-9",
            "--repo",
            "ops",
            "--remove-worktree",
            "--json",
        ],
    );
    assert_eq!(code(&out), 1, "a dirty worktree is a safe refusal");
    assert_eq!(stdout_json(&out)["reason"], "dirty_worktree");
    assert!(
        std::path::Path::new(&wt).is_dir(),
        "worktree intact (work never lost)"
    );

    let after = read_lock(r, "ops", "LQOS-9").expect("claim intact");
    // Renew-first ordering pin: the failed close still EXTENDED the lease, proving the
    // renew ran before the refused remove.
    assert!(
        after.expires_at > before.expires_at,
        "renew-first: the lease was extended before the remove was attempted"
    );

    // A plain close (worktree left for the operator) still works afterward.
    assert_eq!(
        code(&run(
            r,
            Some("a"),
            &["close", "LQOS-9", "--repo", "ops", "--json"]
        )),
        0
    );
}

#[test]
fn close_of_absent_lane_is_not_held_exit_0_even_with_remove_flag() {
    let root = temp_root();
    let out = run(
        root.path(),
        Some("x"),
        &[
            "close",
            "ghost",
            "--repo",
            "ops",
            "--remove-worktree",
            "--json",
        ],
    );
    assert_eq!(code(&out), 0, "close-of-absent mirrors release (exit 0)");
    let v = stdout_json(&out);
    assert_eq!(v["outcome"], "not_held");
    assert_eq!(v["data"]["released"], false);
}

#[test]
fn close_remove_worktree_is_owner_only_and_expiry_refusing() {
    let root = temp_root();
    let r = root.path();
    let area = scratch_area();
    let repo = scratch_repo(area.path());
    let repo_s = repo.to_str().unwrap();
    assert_eq!(code(&run(r, Some("a"), &start_args(repo_s, &[]))), 0);

    // Not the owner: the renew-first gate refuses.
    let out = run(
        r,
        Some("b"),
        &[
            "close",
            "LQOS-9",
            "--repo",
            "ops",
            "--remove-worktree",
            "--json",
        ],
    );
    assert_eq!(code(&out), 1);
    assert_eq!(stdout_json(&out)["reason"], "not_owner");

    // Expired: a lapsed lease is not yours to tear down.
    assert_eq!(
        code(&run(
            r,
            Some("c"),
            &[
                "claim",
                "shortie",
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
        Some("c"),
        &[
            "close",
            "shortie",
            "--repo",
            "ops",
            "--remove-worktree",
            "--json",
        ],
    );
    assert_eq!(code(&out), 1);
    assert_eq!(stdout_json(&out)["reason"], "expired");
}

#[test]
fn forced_takeover_blocks_the_prior_owners_destructive_close() {
    let root = temp_root();
    let r = root.path();
    let area = scratch_area();
    let repo = scratch_repo(area.path());
    let repo_s = repo.to_str().unwrap();
    assert_eq!(code(&run(r, Some("a"), &start_args(repo_s, &[]))), 0);
    let wt = format!("{repo_s}-lqos-9");

    // A deliberate forced takeover moves ownership to another instance.
    assert_eq!(
        code(&run(
            r,
            Some("b"),
            &["claim", "LQOS-9", "--repo", "ops", "--force", "--json"]
        )),
        0
    );

    // The prior owner's destructive close is refused at the owner gate and the
    // (now the new owner's) worktree survives untouched.
    let out = run(
        r,
        Some("a"),
        &[
            "close",
            "LQOS-9",
            "--repo",
            "ops",
            "--remove-worktree",
            "--json",
        ],
    );
    assert_eq!(code(&out), 1);
    assert_eq!(stdout_json(&out)["reason"], "not_owner");
    assert!(
        std::path::Path::new(&wt).is_dir(),
        "the new owner's worktree is never destroyed by the prior owner"
    );
}

#[test]
fn close_skips_an_already_deleted_worktree_and_releases() {
    let root = temp_root();
    let r = root.path();
    let area = scratch_area();
    let repo = scratch_repo(area.path());
    let repo_s = repo.to_str().unwrap();
    assert_eq!(code(&run(r, Some("a"), &start_args(repo_s, &[]))), 0);
    let wt = format!("{repo_s}-lqos-9");

    // The worktree directory vanishes out-of-band (manual rm).
    std::fs::remove_dir_all(&wt).unwrap();

    let out = run(
        r,
        Some("a"),
        &[
            "close",
            "LQOS-9",
            "--repo",
            "ops",
            "--remove-worktree",
            "--json",
        ],
    );
    assert_eq!(code(&out), 0, "a missing worktree never blocks a close");
    let v = stdout_json(&out);
    assert_eq!(v["outcome"], "released");
    assert_eq!(v["data"]["worktree_removed"], false);
    assert_eq!(v["data"]["skipped_missing_worktree"], true);
    assert!(read_lock(r, "ops", "LQOS-9").is_none(), "claim released");
}

#[test]
fn plain_claim_record_and_envelope_are_unchanged_by_slice3() {
    let root = temp_root();
    let r = root.path();
    let out = run(r, Some("a"), &["claim", "demo", "--repo", "ops", "--json"]);
    assert_eq!(code(&out), 0);

    // The on-disk record still carries None for every lifecycle field.
    let rec = read_lock(r, "ops", "demo").unwrap();
    assert_eq!(rec.branch, None);
    assert_eq!(rec.linear_key, None);
    assert!(rec.role.is_none());
    assert!(rec.claim_status.is_none());

    // VerbData::Claim JSON keys are byte-identical to Slice 2 (no lifecycle leakage).
    let v = stdout_json(&out);
    let keys: Vec<&str> = v["data"]
        .as_object()
        .unwrap()
        .keys()
        .map(|s| s.as_str())
        .collect();
    assert_eq!(keys, vec!["expires_at", "forced", "instance", "lane"]);
}
