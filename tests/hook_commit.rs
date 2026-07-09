//! End-to-end pre-commit guard behavior: real `git commit` in scratch repos with the
//! installed hook, driven through claims made by the real binary. All timing-independent
//! (no TTL expiry in-flight; the default 12h lease is never approached).

mod common;

use std::path::{Path, PathBuf};

use common::{
    code, hook_test_path, init_scratch_repo, run, run_hook, scratch_commit, scratch_git, temp_root,
};

/// Scratch layout: one parent tempdir under `$HOME` holding the repo (and any worktree),
/// plus a lane root. Returns (parent, repo, lane_root_path).
fn scratch() -> (tempfile::TempDir, PathBuf, tempfile::TempDir) {
    let parent = temp_root();
    let repo = parent.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    init_scratch_repo(&repo);
    let root = temp_root();
    (parent, repo, root)
}

fn install(repo: &Path) {
    let out = run_hook(&["hook", "install", "--git-repo", repo.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "install failed: {out:?}");
}

fn set_enforce(repo: &Path) {
    assert!(scratch_git(repo, &["config", "lane.hook.mode", "enforce"])
        .status
        .success());
}

fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn advise_uncovered_commit_passes_with_warning() {
    let (_p, repo, root) = scratch();
    install(&repo);
    let path = hook_test_path();
    let out = scratch_commit(
        &repo,
        "uncovered advise",
        &[
            ("PATH", path.as_str()),
            ("LANE_ROOT", root.path().to_str().unwrap()),
            ("LANE_INSTANCE", "a"),
        ],
    );
    assert!(out.status.success(), "advise mode must not block: {out:?}");
    let e = stderr_of(&out);
    assert!(e.contains("lane-hook: WARNING"), "{e}");
    assert!(e.contains("commit allowed"), "{e}");
    // lane's own refusal text is relayed (the fix command reaches the operator).
    assert!(e.contains("no active claim covers"), "{e}");
}

#[test]
fn enforce_blocks_uncovered_commit_after_config_flip() {
    let (_p, repo, root) = scratch();
    install(&repo);
    set_enforce(&repo); // mode flip via git config — NO reinstall
    let path = hook_test_path();
    let out = scratch_commit(
        &repo,
        "uncovered enforce",
        &[
            ("PATH", path.as_str()),
            ("LANE_ROOT", root.path().to_str().unwrap()),
            ("LANE_INSTANCE", "a"),
        ],
    );
    assert!(!out.status.success(), "enforce must block: {out:?}");
    let e = stderr_of(&out);
    assert!(e.contains("lane-hook: BLOCKED"), "{e}");
    assert!(e.contains("lane claim"), "relays the fix command: {e}");
    assert!(e.contains("LANE_HOOK_BYPASS=1"), "names the bypass: {e}");
}

#[test]
fn bypass_env_passes_loudly_under_enforce() {
    let (_p, repo, root) = scratch();
    install(&repo);
    set_enforce(&repo);
    let path = hook_test_path();
    let out = scratch_commit(
        &repo,
        "bypassed",
        &[
            ("PATH", path.as_str()),
            ("LANE_ROOT", root.path().to_str().unwrap()),
            ("LANE_INSTANCE", "a"),
            ("LANE_HOOK_BYPASS", "1"),
        ],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(stderr_of(&out).contains("lane-hook: BYPASSED"), "{out:?}");
}

#[test]
fn covered_commit_passes_silently_under_enforce() {
    let (_p, repo, root) = scratch();
    install(&repo);
    set_enforce(&repo);
    // Claim the repo path as instance "a" through the real binary.
    let out = run(
        root.path(),
        Some("a"),
        &[
            "claim",
            "wt",
            "--repo",
            "scratch",
            "--target",
            repo.to_str().unwrap(),
        ],
    );
    assert_eq!(code(&out), 0, "claim setup failed: {out:?}");

    let path = hook_test_path();
    let out = scratch_commit(
        &repo,
        "covered",
        &[
            ("PATH", path.as_str()),
            ("LANE_ROOT", root.path().to_str().unwrap()),
            ("LANE_INSTANCE", "a"),
        ],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(
        !stderr_of(&out).contains("lane-hook:"),
        "a covered commit is silent: {out:?}"
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("covered:"),
        "lane check's success line must not leak into commit output: {out:?}"
    );
}

#[test]
fn foreign_owner_commit_blocked_and_named() {
    let (_p, repo, root) = scratch();
    install(&repo);
    set_enforce(&repo);
    let out = run(
        root.path(),
        Some("alice"),
        &[
            "claim",
            "wt",
            "--repo",
            "scratch",
            "--target",
            repo.to_str().unwrap(),
        ],
    );
    assert_eq!(code(&out), 0, "{out:?}");

    let path = hook_test_path();
    let out = scratch_commit(
        &repo,
        "colliding",
        &[
            ("PATH", path.as_str()),
            ("LANE_ROOT", root.path().to_str().unwrap()),
            ("LANE_INSTANCE", "bob"),
        ],
    );
    assert!(
        !out.status.success(),
        "the collision case must block: {out:?}"
    );
    let e = stderr_of(&out);
    assert!(e.contains("held by alice"), "{e}");
    assert!(e.contains("lane-hook: BLOCKED"), "{e}");
}

#[test]
fn missing_binary_posture() {
    let (_p, repo, root) = scratch();
    install(&repo);
    let sys_only = "/usr/bin:/bin"; // git + sh, no lane

    // Advise: warn + pass.
    let out = scratch_commit(
        &repo,
        "no binary advise",
        &[
            ("PATH", sys_only),
            ("LANE_ROOT", root.path().to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(
        stderr_of(&out).contains("'lane' not found on PATH"),
        "{out:?}"
    );

    // Enforce: fail closed.
    set_enforce(&repo);
    let out = scratch_commit(
        &repo,
        "no binary enforce",
        &[
            ("PATH", sys_only),
            ("LANE_ROOT", root.path().to_str().unwrap()),
        ],
    );
    assert!(!out.status.success(), "{out:?}");
    let e = stderr_of(&out);
    assert!(e.contains("lane-hook: BLOCKED"), "{e}");
    assert!(e.contains("'lane' not found on PATH"), "{e}");
}

#[test]
fn check_integrity_failure_posture() {
    let (_p, repo, root) = scratch();
    let _ = root; // integrity failure is forced via a RELATIVE $LANE_ROOT (exit 2)
    install(&repo);
    let path = hook_test_path();

    // Advise: loud warn + pass.
    let out = scratch_commit(
        &repo,
        "integrity advise",
        &[
            ("PATH", path.as_str()),
            ("LANE_ROOT", "not/absolute"),
            ("LANE_INSTANCE", "a"),
        ],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(stderr_of(&out).contains("exit 2, integrity/io"), "{out:?}");

    // Enforce: never silent-open.
    set_enforce(&repo);
    let out = scratch_commit(
        &repo,
        "integrity enforce",
        &[
            ("PATH", path.as_str()),
            ("LANE_ROOT", "not/absolute"),
            ("LANE_INSTANCE", "a"),
        ],
    );
    assert!(!out.status.success(), "{out:?}");
    assert!(stderr_of(&out).contains("lane-hook: BLOCKED"), "{out:?}");
}

#[test]
fn worktree_commit_is_covered_by_the_common_dir_install() {
    let (parent, repo, root) = scratch();
    install(&repo); // ONE install, in the main checkout
    set_enforce(&repo);
    let wt = parent.path().join("wt");
    let add = scratch_git(
        &repo,
        &["worktree", "add", "-b", "wtb", wt.to_str().unwrap()],
    );
    assert!(add.status.success(), "worktree add failed: {add:?}");

    let path = hook_test_path();
    // Uncovered commit FROM THE WORKTREE is blocked — the shared hook fires there.
    let out = scratch_commit(
        &wt,
        "wt uncovered",
        &[
            ("PATH", path.as_str()),
            ("LANE_ROOT", root.path().to_str().unwrap()),
            ("LANE_INSTANCE", "a"),
        ],
    );
    assert!(!out.status.success(), "{out:?}");
    assert!(stderr_of(&out).contains("lane-hook: BLOCKED"), "{out:?}");

    // Claim the worktree path; the same commit now passes.
    let claim = run(
        root.path(),
        Some("a"),
        &[
            "claim",
            "wtb",
            "--repo",
            "scratch",
            "--target",
            wt.to_str().unwrap(),
        ],
    );
    assert_eq!(code(&claim), 0, "{claim:?}");
    let out = scratch_commit(
        &wt,
        "wt covered",
        &[
            ("PATH", path.as_str()),
            ("LANE_ROOT", root.path().to_str().unwrap()),
            ("LANE_INSTANCE", "a"),
        ],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(!stderr_of(&out).contains("lane-hook:"), "{out:?}");
}

#[test]
fn composed_foreign_gate_and_lane_gate_both_run() {
    let (_p, repo, root) = scratch();
    let hp = repo.join(".git/hooks/pre-commit");
    std::fs::write(&hp, "#!/bin/sh\necho \"foreign-gate-ran\" >&2\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&hp).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hp, perms).unwrap();
    }
    install(&repo); // composes: foreign first, lane block LAST
    set_enforce(&repo);

    let path = hook_test_path();
    let out = scratch_commit(
        &repo,
        "both gates",
        &[
            ("PATH", path.as_str()),
            ("LANE_ROOT", root.path().to_str().unwrap()),
            ("LANE_INSTANCE", "a"),
        ],
    );
    assert!(!out.status.success(), "{out:?}");
    let e = stderr_of(&out);
    assert!(
        e.contains("foreign-gate-ran"),
        "foreign gate preserved and ran: {e}"
    );
    assert!(
        e.contains("lane-hook: BLOCKED"),
        "lane gate ran after it: {e}"
    );
}

// ---------------------------------------------------------------------------
// ZER-91 regressions: distinct no-identity diagnosis + enforce exit classes.
// ---------------------------------------------------------------------------

#[test]
fn no_identity_advise_names_identity_not_coverage() {
    let (_p, repo, root) = scratch();
    install(&repo);
    let path = hook_test_path();
    // LANE_INSTANCE deliberately absent from the overlay — scratch_commit's baseline
    // scrub removes any ambient value, so the hook sees no identity at all.
    let out = scratch_commit(
        &repo,
        "no identity advise",
        &[
            ("PATH", path.as_str()),
            ("LANE_ROOT", root.path().to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "advise must not block: {out:?}");
    let e = stderr_of(&out);
    assert!(e.contains("lane-hook: WARNING"), "{e}");
    assert!(e.contains("no caller identity"), "{e}");
    assert!(
        e.contains("export LANE_INSTANCE="),
        "carries the propagation fix: {e}"
    );
    assert!(
        !e.contains("not covered"),
        "identity failure must not be labeled a coverage failure: {e}"
    );
}

#[test]
fn no_identity_enforce_blocks_with_identity_cause() {
    let (_p, repo, root) = scratch();
    install(&repo);
    set_enforce(&repo);
    let path = hook_test_path();
    let out = scratch_commit(
        &repo,
        "no identity enforce",
        &[
            ("PATH", path.as_str()),
            ("LANE_ROOT", root.path().to_str().unwrap()),
        ],
    );
    assert!(!out.status.success(), "enforce fails closed: {out:?}");
    let e = stderr_of(&out);
    assert!(e.contains("lane-hook: BLOCKED"), "{e}");
    assert!(e.contains("no caller identity"), "{e}");
    assert!(e.contains("export LANE_INSTANCE="), "{e}");
    assert!(e.contains("LANE_HOOK_BYPASS=1"), "names the bypass: {e}");
    assert!(
        !e.contains("not covered"),
        "identity failure must not be labeled a coverage failure: {e}"
    );
}

/// Run the installed pre-commit hook DIRECTLY via `sh` from the repo toplevel (what git
/// does), so the hook's own exit code is observable — `git commit` flattens any hook
/// failure to its own exit, which cannot pin the 1-vs-2 taxonomy.
fn run_hook_directly(repo: &Path, envs: &[(&str, &str)]) -> std::process::Output {
    let mut c = std::process::Command::new("sh");
    c.arg(".git/hooks/pre-commit");
    c.current_dir(repo);
    c.env_remove("LANE_ROOT");
    c.env_remove("LANE_INSTANCE");
    c.env_remove("LANE_HOOK_BYPASS");
    for (k, v) in envs {
        c.env(k, v);
    }
    c.output().expect("spawn sh pre-commit")
}

/// ZER-91 remediation 3: pin the full advise/enforce × outcome exit-code matrix.
/// Enforce: 0 covered, 1 = genuine coverage violation, 2 = environment (no identity /
/// lane missing / integrity-io). Advise: always 0. Bypass is LOUD in BOTH modes and
/// only the covered success path is silent.
#[test]
fn hook_exit_class_matrix_is_pinned() {
    let (_p, repo, root) = scratch();
    install(&repo);
    let path = hook_test_path();
    let rootv = root.path().to_str().unwrap().to_string();
    let sys_only = "/usr/bin:/bin"; // git + sh, no lane

    let expect = |label: &str, out: &std::process::Output, code: i32, needle: &str| {
        assert_eq!(
            out.status.code(),
            Some(code),
            "{label}: wrong exit class: {out:?}"
        );
        let e = String::from_utf8_lossy(&out.stderr);
        if needle.is_empty() {
            assert!(
                !e.contains("lane-hook:"),
                "{label}: expected silence, got: {e}"
            );
        } else {
            assert!(e.contains(needle), "{label}: missing {needle:?} in: {e}");
        }
    };

    // -------- enforce --------
    set_enforce(&repo);
    let out = run_hook_directly(
        &repo,
        &[
            ("PATH", path.as_str()),
            ("LANE_ROOT", rootv.as_str()),
            ("LANE_INSTANCE", "a"),
        ],
    );
    expect("enforce/uncovered", &out, 1, "lane-hook: BLOCKED");

    let out = run_hook_directly(
        &repo,
        &[("PATH", path.as_str()), ("LANE_ROOT", rootv.as_str())],
    );
    expect("enforce/no-identity", &out, 2, "no caller identity");

    let out = run_hook_directly(&repo, &[("PATH", sys_only), ("LANE_ROOT", rootv.as_str())]);
    expect("enforce/lane-missing", &out, 2, "'lane' not found on PATH");

    let out = run_hook_directly(
        &repo,
        &[
            ("PATH", path.as_str()),
            ("LANE_ROOT", "not/absolute"),
            ("LANE_INSTANCE", "a"),
        ],
    );
    expect("enforce/integrity", &out, 2, "exit 2, integrity/io");

    let out = run_hook_directly(
        &repo,
        &[
            ("PATH", path.as_str()),
            ("LANE_ROOT", rootv.as_str()),
            ("LANE_INSTANCE", "a"),
            ("LANE_HOOK_BYPASS", "1"),
        ],
    );
    expect("enforce/bypass", &out, 0, "lane-hook: BYPASSED");

    // Claim the repo as "a": covered is exit 0 and fully silent.
    let claim = run(
        root.path(),
        Some("a"),
        &[
            "claim",
            "wt",
            "--repo",
            "scratch",
            "--target",
            repo.to_str().unwrap(),
        ],
    );
    assert_eq!(code(&claim), 0, "claim setup failed: {claim:?}");
    let out = run_hook_directly(
        &repo,
        &[
            ("PATH", path.as_str()),
            ("LANE_ROOT", rootv.as_str()),
            ("LANE_INSTANCE", "a"),
        ],
    );
    expect("enforce/covered", &out, 0, "");

    // -------- advise (never blocks, never silent except covered) --------
    assert!(scratch_git(&repo, &["config", "lane.hook.mode", "advise"])
        .status
        .success());
    let out = run_hook_directly(
        &repo,
        &[
            ("PATH", path.as_str()),
            ("LANE_ROOT", rootv.as_str()),
            ("LANE_INSTANCE", "b"), // foreign to a's claim: rc=1 class
        ],
    );
    expect("advise/uncovered", &out, 0, "lane-hook: WARNING");

    let out = run_hook_directly(
        &repo,
        &[("PATH", path.as_str()), ("LANE_ROOT", rootv.as_str())],
    );
    expect("advise/no-identity", &out, 0, "no caller identity");

    let out = run_hook_directly(&repo, &[("PATH", sys_only), ("LANE_ROOT", rootv.as_str())]);
    expect("advise/lane-missing", &out, 0, "'lane' not found on PATH");

    let out = run_hook_directly(
        &repo,
        &[
            ("PATH", path.as_str()),
            ("LANE_ROOT", "not/absolute"),
            ("LANE_INSTANCE", "a"),
        ],
    );
    expect("advise/integrity", &out, 0, "exit 2, integrity/io");

    let out = run_hook_directly(
        &repo,
        &[
            ("PATH", path.as_str()),
            ("LANE_ROOT", rootv.as_str()),
            ("LANE_HOOK_BYPASS", "1"),
        ],
    );
    expect("advise/bypass", &out, 0, "lane-hook: BYPASSED");

    let out = run_hook_directly(
        &repo,
        &[
            ("PATH", path.as_str()),
            ("LANE_ROOT", rootv.as_str()),
            ("LANE_INSTANCE", "a"),
        ],
    );
    expect("advise/covered", &out, 0, "");
}
