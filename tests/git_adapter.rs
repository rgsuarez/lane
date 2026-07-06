//! Slice 3 WS1 — the git worktree/branch adapter, against REAL git (integration) plus the
//! injectable fake seam (deterministic fault paths). Scratch repos live under a `$HOME`
//! tempdir; worktree targets are siblings inside the same tempdir.

mod common;

use common::*;

use lane::git::{cross_device_warning, GitAdapter, GitError, StdGitRunner};

// ---------------------------------------------------------------------------
// Real-git integration.
// ---------------------------------------------------------------------------

#[test]
fn worktree_add_then_probe_round_trips() {
    let root = temp_root();
    let repo = root.path().join("repo");
    init_scratch_repo(&repo);
    let wt = root.path().join("repo-lane");

    let runner = StdGitRunner::new();
    let git = GitAdapter::new(&runner);

    git.worktree_add(&repo, &wt, "lane-branch", "HEAD")
        .expect("worktree add succeeds");
    assert!(wt.is_dir(), "the worktree directory now exists");

    let probed = git
        .probe_worktree(&wt)
        .expect("probe ok")
        .expect("live worktree");
    assert_eq!(probed.branch.as_deref(), Some("lane-branch"));
    assert!(probed.head.is_some(), "HEAD resolves to a commit");
    assert_eq!(probed.path, wt);
}

#[test]
fn probe_of_a_non_worktree_path_is_none() {
    let root = temp_root();
    let plain = root.path().join("just-a-dir");
    std::fs::create_dir_all(&plain).unwrap();
    let runner = StdGitRunner::new();
    let git = GitAdapter::new(&runner);
    assert!(git.probe_worktree(&plain).expect("probe ok").is_none());
}

#[test]
fn worktree_remove_happy_deletes_the_directory() {
    let root = temp_root();
    let repo = root.path().join("repo");
    init_scratch_repo(&repo);
    let wt = root.path().join("repo-lane");
    let runner = StdGitRunner::new();
    let git = GitAdapter::new(&runner);

    git.worktree_add(&repo, &wt, "b", "HEAD").unwrap();
    assert!(git.status_clean(&wt).expect("status"));
    git.worktree_remove(&repo, &wt)
        .expect("clean worktree removes");
    assert!(!wt.exists(), "worktree directory gone after remove");
}

#[test]
fn worktree_remove_refuses_a_dirty_worktree() {
    let root = temp_root();
    let repo = root.path().join("repo");
    init_scratch_repo(&repo);
    let wt = root.path().join("repo-lane");
    let runner = StdGitRunner::new();
    let git = GitAdapter::new(&runner);

    git.worktree_add(&repo, &wt, "b", "HEAD").unwrap();
    // An untracked file makes the worktree dirty; git refuses to remove without --force.
    std::fs::write(wt.join("scratch.txt"), "dirty\n").unwrap();
    assert!(!git.status_clean(&wt).expect("status"));

    let err = git
        .worktree_remove(&repo, &wt)
        .expect_err("dirty worktree refuses");
    assert!(
        matches!(err, GitError::DirtyWorktree { .. }),
        "real git dirty refusal maps to DirtyWorktree, got {err:?}"
    );
    assert!(wt.is_dir(), "the worktree is left intact on refusal");
}

#[test]
fn worktree_remove_of_a_non_worktree_is_plumbing_not_dirty() {
    let root = temp_root();
    let repo = root.path().join("repo");
    init_scratch_repo(&repo);
    let runner = StdGitRunner::new();
    let git = GitAdapter::new(&runner);
    let missing = root.path().join("never-a-worktree");
    let err = git
        .worktree_remove(&repo, &missing)
        .expect_err("removing a non-worktree errors");
    assert!(
        matches!(err, GitError::Plumbing { .. }),
        "a non-worktree removal is a plumbing error, not a dirty refusal, got {err:?}"
    );
}

#[test]
fn branch_exists_reflects_reality() {
    let root = temp_root();
    let repo = root.path().join("repo");
    init_scratch_repo(&repo);
    let wt = root.path().join("repo-lane");
    let runner = StdGitRunner::new();
    let git = GitAdapter::new(&runner);

    assert!(!git.branch_exists(&repo, "made-up").expect("absent"));
    git.worktree_add(&repo, &wt, "real-branch", "HEAD").unwrap();
    assert!(git.branch_exists(&repo, "real-branch").expect("present"));
}

#[test]
fn is_git_repo_true_for_repo_false_for_plain_dir() {
    let root = temp_root();
    let repo = root.path().join("repo");
    init_scratch_repo(&repo);
    let plain = root.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let runner = StdGitRunner::new();
    let git = GitAdapter::new(&runner);
    assert!(git.is_git_repo(&repo).expect("repo"));
    assert!(!git.is_git_repo(&plain).expect("plain dir"));
}

#[test]
fn delete_branch_removes_a_branch_after_its_worktree() {
    let root = temp_root();
    let repo = root.path().join("repo");
    init_scratch_repo(&repo);
    let wt = root.path().join("repo-lane");
    let runner = StdGitRunner::new();
    let git = GitAdapter::new(&runner);

    git.worktree_add(&repo, &wt, "to-delete", "HEAD").unwrap();
    // A checked-out branch cannot be deleted; remove the worktree first (compensation order).
    git.worktree_remove(&repo, &wt).unwrap();
    git.delete_branch(&repo, "to-delete")
        .expect("branch -D succeeds");
    assert!(!git.branch_exists(&repo, "to-delete").expect("gone"));
}

#[test]
fn precheck_start_happy_then_each_failure() {
    let root = temp_root();
    let repo = root.path().join("repo");
    init_scratch_repo(&repo);
    let wt = root.path().join("repo-lane");
    let runner = StdGitRunner::new();
    let git = GitAdapter::new(&runner);

    // Happy: real repo, absent branch, absent path → Ok, same device as HOME → no warning.
    let report = git
        .precheck_start(&repo, &wt, "lane-branch", Some(&home()))
        .expect("prechecks pass");
    assert!(
        report.device_warning.is_none(),
        "a target under HOME is on the home device"
    );

    // Not a repo.
    let plain = root.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    assert!(matches!(
        git.precheck_start(&plain, &wt, "b", Some(&home())),
        Err(GitError::NotARepo { .. })
    ));

    // Branch already exists.
    git.worktree_add(&repo, &wt, "taken", "HEAD").unwrap();
    let wt2 = root.path().join("repo-lane2");
    assert!(matches!(
        git.precheck_start(&repo, &wt2, "taken", Some(&home())),
        Err(GitError::BranchExists { .. })
    ));

    // Worktree path already exists (wt is now on disk).
    assert!(matches!(
        git.precheck_start(&repo, &wt, "fresh", Some(&home())),
        Err(GitError::WorktreePathExists { .. })
    ));
}

#[test]
fn cross_device_warning_none_on_nonexistent_leaf_under_home() {
    // The device check must walk up to an existing ancestor; a direct stat on the leaf would
    // NotFound-fail. A deep non-existent path under $HOME resolves to the home device.
    let root = temp_root();
    let leaf = root.path().join("a/b/c/does-not-exist-leaf");
    assert!(
        cross_device_warning(&leaf, Some(&home())).is_none(),
        "a non-existent leaf under HOME is on the home device (no NFS warning)"
    );
}

#[test]
fn two_step_branch_then_worktree_round_trips() {
    let root = temp_root();
    let repo = root.path().join("repo");
    init_scratch_repo(&repo);
    let wt = root.path().join("repo-lane");
    let runner = StdGitRunner::new();
    let git = GitAdapter::new(&runner);

    git.check_branch_name(&repo, "two-step")
        .expect("valid name");
    assert!(matches!(
        git.check_branch_name(&repo, "bad..name"),
        Err(GitError::Plumbing { .. })
    ));

    git.create_branch(&repo, "two-step", "HEAD")
        .expect("branch created");
    assert!(git.branch_exists(&repo, "two-step").unwrap());
    git.worktree_add_existing(&repo, &wt, "two-step")
        .expect("worktree attaches to the existing branch");
    let probed = git.probe_worktree(&wt).unwrap().expect("live");
    assert_eq!(probed.branch.as_deref(), Some("two-step"));

    // A second create of the same name fails (the exactly-once ownership signal).
    assert!(matches!(
        git.create_branch(&repo, "two-step", "HEAD"),
        Err(GitError::Plumbing { .. })
    ));
}

// ---------------------------------------------------------------------------
// Fake-seam fault paths (no real git).
// ---------------------------------------------------------------------------

#[test]
fn fake_worktree_add_plumbing_failure_surfaces_plumbing() {
    let runner = FakeGitRunner::new(|_args| Ok(git_fail(128, "fatal: could not create work tree")));
    let git = GitAdapter::new(&runner);
    let err = git
        .worktree_add(
            std::path::Path::new("/repo"),
            std::path::Path::new("/repo-lane"),
            "b",
            "HEAD",
        )
        .expect_err("add fails");
    assert!(matches!(
        err,
        GitError::Plumbing {
            code: Some(128),
            ..
        }
    ));
    assert_eq!(runner.call_count(), 1, "exactly one git spawn");
}

#[test]
fn fake_worktree_remove_maps_dirty_stderr_to_dirty_refusal() {
    let runner = FakeGitRunner::new(|_args| {
        Ok(git_fail(
            128,
            "fatal: '/repo-lane' contains modified or untracked files, use --force to delete it",
        ))
    });
    let git = GitAdapter::new(&runner);
    let err = git
        .worktree_remove(
            std::path::Path::new("/repo"),
            std::path::Path::new("/repo-lane"),
        )
        .expect_err("dirty refuses");
    assert!(matches!(err, GitError::DirtyWorktree { .. }));
}

#[test]
fn fake_spawn_error_propagates() {
    let runner = FakeGitRunner::new(|_args| {
        Err(GitError::Spawn(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "git not found",
        )))
    });
    let git = GitAdapter::new(&runner);
    let err = git
        .is_git_repo(std::path::Path::new("/anywhere"))
        .expect_err("spawn failure propagates");
    assert!(matches!(err, GitError::Spawn(_)));
}

#[test]
fn fake_timeout_via_seam_is_surfaced() {
    // The real kill path is unit-tested in src/git; here we confirm a runner timeout is
    // surfaced unchanged through an adapter op.
    let runner = FakeGitRunner::new(|_args| Err(GitError::Timeout { secs: 10 }));
    let git = GitAdapter::new(&runner);
    let err = git
        .status_clean(std::path::Path::new("/repo"))
        .expect_err("timeout surfaces");
    assert!(matches!(err, GitError::Timeout { secs: 10 }));
}
