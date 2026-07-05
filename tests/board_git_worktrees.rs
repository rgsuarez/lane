//! Slice 3 WS1 — the live board worktree provider (`--worktrees git`): opt-in, fail-soft,
//! zero-spawn for target-less claims, freshness computed after the probes so a lazily
//! discovered git failure flips the source to `ok:false` while the board still renders.

mod common;

use common::*;

use chrono::Utc;
use lane::board::worktrees::{resolve_real_case, CaseMatch, GitWorktreeProvider, WorktreeProvider};
use lane::git::GitError;
use lane::ClaimRecord;

fn claim_record(lane: &str, target: Option<&str>) -> ClaimRecord {
    let now = Utc::now();
    ClaimRecord {
        schema_version: Some(1),
        lane: lane.into(),
        repo: "ops".into(),
        instance: "a".into(),
        pid: None,
        target: target.map(str::to_string),
        target_normalized: target.map(str::to_string),
        note: None,
        claimed_at: now,
        updated_at: now,
        expires_at: now + chrono::Duration::hours(1),
        ttl_hours: 1.0,
        linear_key: None,
        branch: None,
        role: None,
        pr_url: None,
        gate: None,
        plan_path: None,
        claim_status: None,
        session_ref: None,
    }
}

// ---------------------------------------------------------------------------
// Pure case-matching logic (the ambiguous branch is only reachable on a
// case-sensitive volume, so it is pinned here, not via the filesystem).
// ---------------------------------------------------------------------------

#[test]
fn resolve_real_case_one_absent_ambiguous() {
    let entries = vec!["Repo-LANE".to_string(), "other".to_string()];
    assert_eq!(
        resolve_real_case(&entries, "repo-lane"),
        CaseMatch::One("Repo-LANE".into())
    );
    assert_eq!(resolve_real_case(&entries, "missing"), CaseMatch::Absent);
    let dupes = vec!["repo-lane".to_string(), "REPO-LANE".to_string()];
    assert_eq!(resolve_real_case(&dupes, "repo-lane"), CaseMatch::Ambiguous);
}

// ---------------------------------------------------------------------------
// Provider behavior over the fake seam.
// ---------------------------------------------------------------------------

#[test]
fn targetless_claims_never_spawn_git() {
    let runner = FakeGitRunner::new(|_| Ok(git_ok("true"))); // would answer if called
    let provider = GitWorktreeProvider::new(&runner);
    let rec = claim_record("coord", None);
    assert!(provider.for_claim(&rec).is_none());
    assert_eq!(
        runner.call_count(),
        0,
        "a target-less claim must cause ZERO git spawns"
    );
    let fresh = provider.freshness(Utc::now());
    assert!(fresh.ok, "no probes, no degradation");
}

#[test]
fn git_missing_degrades_source_but_never_errors() {
    let runner = FakeGitRunner::new(|_| {
        Err(GitError::Spawn(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "git not found",
        )))
    });
    let provider = GitWorktreeProvider::new(&runner);
    let rec = claim_record("demo", Some("/somewhere/repo-demo"));
    assert!(provider.for_claim(&rec).is_none(), "fail-soft: no row join");
    let fresh = provider.freshness(Utc::now());
    assert!(!fresh.ok, "git unavailable flips the source to ok:false");
    assert!(fresh.note.contains("git unavailable"));
}

#[test]
fn probe_error_degrades_source_after_availability_passed() {
    let runner = FakeGitRunner::new(|args| {
        if args == ["--version"] {
            Ok(git_ok("git version 2.x"))
        } else {
            Err(GitError::Timeout { secs: 10 })
        }
    });
    let provider = GitWorktreeProvider::new(&runner);
    let rec = claim_record("demo", Some("/somewhere/repo-demo"));
    assert!(provider.for_claim(&rec).is_none());
    let fresh = provider.freshness(Utc::now());
    assert!(!fresh.ok, "a probe timeout degrades the source");
    assert!(fresh.note.contains("probe failed"));
}

#[test]
fn fallback_recovers_real_case_dir_when_direct_probe_misses() {
    // A real parent dir with a REAL-CASE entry; the stored target is the folded form.
    // The fake git says: folded path -> not a worktree; real-case path -> a live one.
    let area = tempfile::Builder::new()
        .prefix("lane-case-")
        .tempdir_in(home())
        .unwrap();
    let real = area.path().join("Repo-DEMO");
    std::fs::create_dir_all(&real).unwrap();
    let stored = area.path().join("repo-demo");
    let stored_s = stored.to_string_lossy().to_string();
    let real_s = real.to_string_lossy().to_string();

    let real_for_closure = real_s.clone();
    let runner = FakeGitRunner::new(move |args| {
        if args == ["--version"] {
            return Ok(git_ok("git version 2.x"));
        }
        let joined = args.join(" ");
        if joined.contains("rev-parse --is-inside-work-tree") {
            if joined.contains(&real_for_closure) {
                return Ok(git_ok("true"));
            }
            return Ok(git_fail(128, "fatal: not a git repository"));
        }
        if joined.contains("branch --show-current") {
            return Ok(git_ok("case-branch"));
        }
        if joined.contains("rev-parse HEAD") {
            return Ok(git_ok("abc123"));
        }
        Ok(git_ok(""))
    });
    let provider = GitWorktreeProvider::new(&runner);
    let rec = claim_record("demo", Some(&stored_s));
    let joined = provider
        .for_claim(&rec)
        .expect("fallback recovers the real-case dir");
    assert_eq!(joined.value.path, real_s);
    assert_eq!(joined.value.branch.as_deref(), Some("case-branch"));
    let fresh = provider.freshness(Utc::now());
    assert!(fresh.ok, "a successful fallback is not a degradation");
}

// ---------------------------------------------------------------------------
// End to end: real binary, real git, real worktree from `start`.
// ---------------------------------------------------------------------------

#[test]
fn board_git_mode_joins_a_real_started_worktree() {
    let root = temp_root();
    let r = root.path();
    let area = tempfile::Builder::new()
        .prefix("lane-git-")
        .tempdir_in(home())
        .unwrap();
    let repo = area.path().join("repo");
    init_scratch_repo(&repo);
    let repo_s = repo.to_str().unwrap();

    assert_eq!(
        code(&run(
            r,
            Some("a"),
            &[
                "start",
                "demo",
                "--repo",
                "ops",
                "--git-repo",
                repo_s,
                "--json"
            ]
        )),
        0
    );

    let out = run(r, None, &["board", "--worktrees", "git", "--json"]);
    assert_eq!(code(&out), 0);
    let v = stdout_json(&out);
    let row = &v["rows"][0];
    assert_eq!(row["worktree"]["value"]["branch"], "demo");
    assert_eq!(row["worktree"]["provenance"], "derived");
    let wt_src = v["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["source"] == "worktrees")
        .expect("worktrees source present");
    assert_eq!(wt_src["ok"], true);
    assert!(wt_src["note"].as_str().unwrap().contains("live git probe"));
}

#[test]
fn board_default_stays_offline_and_renders_even_with_no_git_on_path() {
    let root = temp_root();
    let r = root.path();
    let target = format!("{}/never-probed", home());
    assert_eq!(
        code(&run(
            r,
            Some("a"),
            &["claim", "demo", "--repo", "ops", "--target", &target, "--json"]
        )),
        0
    );

    // The DEFAULT board (worktrees off) runs with a PATH carrying NO git at all: it must
    // spawn nothing, stay ok, and render — board is in the MUST-work-offline verb set.
    let empty_path_dir = tempfile::Builder::new()
        .prefix("lane-nopath-")
        .tempdir_in(home())
        .unwrap();
    let out = std::process::Command::new(bin())
        .args(["board", "--json", "--lane-root"])
        .arg(r)
        .env_remove("LANE_ROOT")
        .env("PATH", empty_path_dir.path())
        .output()
        .expect("board runs");
    assert_eq!(out.status.code(), Some(0), "default board needs no git");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v["rows"].as_array().unwrap().len(),
        1,
        "board renders the claim"
    );
    let wt_src = v["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["source"] == "worktrees")
        .expect("worktrees source present");
    assert_eq!(wt_src["ok"], true, "the offline default is never degraded");

    // And GIT MODE with the same git-less PATH degrades the source but still renders.
    let out = std::process::Command::new(bin())
        .args(["board", "--worktrees", "git", "--json", "--lane-root"])
        .arg(r)
        .env_remove("LANE_ROOT")
        .env("PATH", empty_path_dir.path())
        .output()
        .expect("board runs");
    assert_eq!(
        out.status.code(),
        Some(0),
        "git mode is fail-soft, never fail-hard"
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v["rows"].as_array().unwrap().len(),
        1,
        "board still renders"
    );
    let wt_src = v["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["source"] == "worktrees")
        .expect("worktrees source present");
    assert_eq!(wt_src["ok"], false, "missing git degrades the live source");
}
