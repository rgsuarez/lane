//! `lane check` integration: the read-only coverage verdict exercised through the real
//! binary. Expiry is fabricated by patching `expires_at` in the lock JSON after a real
//! claim (never by sleeping — timing-window tests are the ZER-83 anti-pattern).

mod common;

use std::path::Path;
use std::process::Command;

use common::{bin, code, run, stdout_json, temp_root};

/// A claim-target directory on the same device as `$HOME`, disjoint from the lane root.
fn temp_target() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("lane-ck-")
        .tempdir_in(common::home())
        .expect("tempdir under HOME")
}

fn claim(root: &Path, instance: &str, lane: &str, repo: &str, target: Option<&str>) {
    let mut args = vec!["claim", lane, "--repo", repo];
    if let Some(t) = target {
        args.extend_from_slice(&["--target", t]);
    }
    let out = run(root, Some(instance), &args);
    assert_eq!(code(&out), 0, "claim setup failed: {out:?}");
}

/// Rewrite a lock's `expires_at` into the past (the `board_stale_orphan` patching
/// precedent; identity fields stay intact so the guarded reader still accepts it).
fn expire_lock(root: &Path, repo: &str, lane: &str) {
    let p = root.join(repo).join("locks").join(format!("{lane}.lock"));
    let mut rec: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&p).expect("read lock")).expect("parse");
    rec["expires_at"] = serde_json::Value::String("2000-01-01T00:00:00Z".into());
    std::fs::write(&p, serde_json::to_string_pretty(&rec).unwrap() + "\n").expect("write lock");
}

fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn covered_exact_pass_with_full_envelope() {
    let root = temp_root();
    let w = temp_target();
    let ws = w.path().to_str().unwrap();
    claim(root.path(), "a", "demo", "ops", Some(ws));

    let out = run(root.path(), Some("a"), &["check", "--path", ws, "--json"]);
    assert_eq!(code(&out), 0, "{out:?}");
    let j = stdout_json(&out);
    assert_eq!(j["schema_version"], 1);
    assert_eq!(j["ok"], true);
    assert_eq!(j["verb"], "check");
    assert_eq!(j["outcome"], "ok");
    assert!(j.get("reason").is_none());
    assert_eq!(j["data"]["repo"], "ops");
    assert_eq!(j["data"]["lane"], "demo");
    assert_eq!(j["data"]["instance"], "a");
    assert!(j["data"]["path"].as_str().unwrap().starts_with('/'));
    assert!(j["data"]["target"].as_str().unwrap().starts_with('/'));
    assert!(j["data"]["expires_at"].as_str().is_some());
    assert!(j["data"].get("warning").is_none());
}

#[test]
fn covered_subdir_pass_including_nonexistent_tail() {
    let root = temp_root();
    let w = temp_target();
    let ws = w.path().to_str().unwrap();
    claim(root.path(), "a", "demo", "ops", Some(ws));

    let sub = format!("{ws}/sub/new-file.rs");
    let out = run(root.path(), Some("a"), &["check", "--path", &sub, "--json"]);
    assert_eq!(code(&out), 0, "{out:?}");
    assert_eq!(stdout_json(&out)["outcome"], "ok");
}

#[test]
fn parent_of_target_is_not_covered() {
    let root = temp_root();
    let w = temp_target();
    let deep = w.path().join("deeper");
    std::fs::create_dir(&deep).unwrap();
    claim(
        root.path(),
        "a",
        "demo",
        "ops",
        Some(deep.to_str().unwrap()),
    );

    // The PARENT of the claim's target is not covered (directional coverage).
    let out = run(
        root.path(),
        Some("a"),
        &["check", "--path", w.path().to_str().unwrap(), "--json"],
    );
    assert_eq!(code(&out), 1, "{out:?}");
    let j = stdout_json(&out);
    assert_eq!(j["outcome"], "refused");
    assert_eq!(j["reason"], "uncovered");
    assert!(j["data"].is_null());
}

#[test]
fn uncovered_empty_root_names_the_fix() {
    let root = temp_root();
    let w = temp_target();
    let ws = w.path().to_str().unwrap();

    let out = run(root.path(), Some("a"), &["check", "--path", ws]);
    assert_eq!(code(&out), 1, "{out:?}");
    assert!(
        out.stdout.is_empty(),
        "human refusal prints nothing on stdout"
    );
    let e = stderr_of(&out);
    assert!(e.contains("refused: no active claim covers"), "{e}");
    assert!(e.contains("lane claim"), "{e}");
    assert!(e.contains("lane start"), "{e}");
}

#[test]
fn foreign_owner_refused_and_named() {
    let root = temp_root();
    let w = temp_target();
    let ws = w.path().to_str().unwrap();
    claim(root.path(), "alice", "demo", "ops", Some(ws));

    let out = run(root.path(), Some("bob"), &["check", "--path", ws, "--json"]);
    assert_eq!(code(&out), 1, "{out:?}");
    let j = stdout_json(&out);
    assert_eq!(j["outcome"], "refused");
    assert_eq!(j["reason"], "foreign_owner");
    assert!(j["data"].is_null());

    let human = run(root.path(), Some("bob"), &["check", "--path", ws]);
    let e = stderr_of(&human);
    assert!(e.contains("alice"), "{e}");
    assert!(e.contains("ops/demo"), "{e}");
    assert!(e.contains("coordinate with alice"), "{e}");
}

#[test]
fn expired_own_claim_is_uncovered_with_reclaim_fix() {
    let root = temp_root();
    let w = temp_target();
    let ws = w.path().to_str().unwrap();
    claim(root.path(), "a", "demo", "ops", Some(ws));
    expire_lock(root.path(), "ops", "demo");

    let out = run(root.path(), Some("a"), &["check", "--path", ws, "--json"]);
    assert_eq!(code(&out), 1, "{out:?}");
    assert_eq!(stdout_json(&out)["reason"], "uncovered");

    let human = run(root.path(), Some("a"), &["check", "--path", ws]);
    let e = stderr_of(&human);
    assert!(e.contains("expired at 2000-01-01"), "{e}");
    assert!(e.contains("lane claim demo --repo ops --target"), "{e}");
    // Expired is takeable without --force: the fix command must NOT suggest it.
    assert!(!e.contains("--force"), "{e}");
}

#[test]
fn expired_own_plus_foreign_active_is_foreign_owner() {
    let root = temp_root();
    let w = temp_target();
    let ws = w.path().to_str().unwrap();
    claim(root.path(), "a", "mine", "ops", Some(ws));
    expire_lock(root.path(), "ops", "mine");
    // An expired sibling does not reserve its target, so alice can claim it.
    claim(root.path(), "alice", "theirs", "ops", Some(ws));

    let out = run(root.path(), Some("a"), &["check", "--path", ws, "--json"]);
    assert_eq!(code(&out), 1, "{out:?}");
    assert_eq!(stdout_json(&out)["reason"], "foreign_owner");
}

#[test]
fn no_identity_is_a_refusal_not_an_error() {
    let root = temp_root();
    let w = temp_target();
    let ws = w.path().to_str().unwrap();

    let out = run(root.path(), None, &["check", "--path", ws, "--json"]);
    assert_eq!(code(&out), 1, "{out:?}");
    let j = stdout_json(&out);
    assert_eq!(j["outcome"], "refused");
    assert_eq!(j["reason"], "no_identity");
    assert!(j["data"].is_null());

    let human = run(root.path(), None, &["check", "--path", ws]);
    assert!(stderr_of(&human).contains("LANE_INSTANCE"));
}

#[test]
fn invalid_instance_stays_exit_2() {
    let root = temp_root();
    let w = temp_target();
    let ws = w.path().to_str().unwrap();

    let out = run(
        root.path(),
        None,
        &[
            "check",
            "--path",
            ws,
            "--instance",
            "bad\u{7}name",
            "--json",
        ],
    );
    assert_eq!(code(&out), 2, "{out:?}");
    let j = stdout_json(&out);
    assert_eq!(j["outcome"], "error");
    assert_eq!(j["reason"], "identity");
}

#[test]
fn targetless_own_claim_hint_carries_force() {
    let root = temp_root();
    let w = temp_target();
    let ws = w.path().to_str().unwrap();
    claim(root.path(), "a", "demo", "ops", None);

    let out = run(root.path(), Some("a"), &["check", "--path", ws]);
    assert_eq!(code(&out), 1, "{out:?}");
    let e = stderr_of(&out);
    assert!(e.contains("your claim ops/demo has no target"), "{e}");
    assert!(e.contains("--target"), "{e}");
    // Same-instance re-claim of an ACTIVE lane refuses active_held; the fix needs --force.
    assert!(e.contains("--force"), "{e}");
}

#[test]
fn repo_filter_narrows_scan_and_echoes_in_envelope() {
    let root = temp_root();
    let w = temp_target();
    let ws = w.path().to_str().unwrap();
    claim(root.path(), "a", "demo", "ops", Some(ws));

    let hit = run(
        root.path(),
        Some("a"),
        &["check", "--path", ws, "--repo", "ops", "--json"],
    );
    assert_eq!(code(&hit), 0, "{hit:?}");
    assert_eq!(stdout_json(&hit)["repo"], "ops");

    let miss = run(
        root.path(),
        Some("a"),
        &["check", "--path", ws, "--repo", "other", "--json"],
    );
    assert_eq!(code(&miss), 1, "{miss:?}");
    let j = stdout_json(&miss);
    assert_eq!(j["reason"], "uncovered");
    assert_eq!(j["repo"], "other");
}

#[test]
fn cross_namespace_own_claim_wins_with_foreign_warning() {
    let root = temp_root();
    let w = temp_target();
    let ws = w.path().to_str().unwrap();
    // Same target claimed in two namespaces (the per-repo overlap scan permits this).
    claim(root.path(), "a", "mine", "ops", Some(ws));
    claim(root.path(), "alice", "theirs", "ns2", Some(ws));

    let out = run(root.path(), Some("a"), &["check", "--path", ws, "--json"]);
    assert_eq!(code(&out), 0, "{out:?}");
    let j = stdout_json(&out);
    assert_eq!(j["data"]["repo"], "ops");
    let warn = j["data"]["warning"].as_str().expect("warning present");
    assert!(warn.contains("ns2/theirs"), "{warn}");
    assert!(warn.contains("alice"), "{warn}");

    let human = run(root.path(), Some("a"), &["check", "--path", ws]);
    let s = String::from_utf8_lossy(&human.stdout).into_owned();
    assert!(s.starts_with("covered:"), "{s}");
    assert!(s.contains("lane: warning:"), "{s}");
}

#[test]
fn malformed_sibling_lock_fails_closed() {
    let root = temp_root();
    let w = temp_target();
    let ws = w.path().to_str().unwrap();
    claim(root.path(), "a", "demo", "ops", Some(ws));
    std::fs::write(root.path().join("ops/locks/garbage.lock"), "not json").unwrap();

    let out = run(root.path(), Some("a"), &["check", "--path", ws, "--json"]);
    assert_eq!(code(&out), 2, "{out:?}");
    let j = stdout_json(&out);
    assert_eq!(j["outcome"], "error");
    assert_eq!(j["reason"], "malformed");
    assert!(j["data"].is_null());
}

#[test]
fn human_success_line_shape() {
    let root = temp_root();
    let w = temp_target();
    let ws = w.path().to_str().unwrap();
    claim(root.path(), "a", "demo", "ops", Some(ws));

    let out = run(root.path(), Some("a"), &["check", "--path", ws]);
    assert_eq!(code(&out), 0, "{out:?}");
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(s.starts_with("covered: "), "{s}");
    assert!(s.contains("by ops/demo"), "{s}");
    assert!(out.stderr.is_empty(), "{out:?}");
}

#[test]
fn relative_path_is_absolutized_against_cwd() {
    let root = temp_root();
    let w = temp_target();
    let ws = w.path().to_str().unwrap();
    claim(root.path(), "a", "demo", "ops", Some(ws));

    let out = Command::new(bin())
        .args(["check", "--path", "sub", "--json", "--lane-root"])
        .arg(root.path())
        .current_dir(w.path())
        .env_remove("LANE_ROOT")
        .env("LANE_INSTANCE", "a")
        .output()
        .expect("spawn lane");
    assert_eq!(code(&out), 0, "{out:?}");
    assert_eq!(stdout_json(&out)["outcome"], "ok");
}

#[test]
fn default_path_is_cwd() {
    let root = temp_root();
    let w = temp_target();
    let ws = w.path().to_str().unwrap();
    claim(root.path(), "a", "demo", "ops", Some(ws));

    let out = Command::new(bin())
        .args(["check", "--json", "--lane-root"])
        .arg(root.path())
        .current_dir(w.path())
        .env_remove("LANE_ROOT")
        .env("LANE_INSTANCE", "a")
        .output()
        .expect("spawn lane");
    assert_eq!(code(&out), 0, "{out:?}");
}
