//! `lane hook uninstall` integration: surgical block removal, byte-identical foreign
//! restoration, config-key cleanup, damage refusal, absent no-op.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use common::{code, init_scratch_repo, run_hook, scratch_git, stdout_json, temp_root};

fn scratch_repo() -> (tempfile::TempDir, std::path::PathBuf) {
    let parent = temp_root();
    let repo = parent.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    init_scratch_repo(&repo);
    (parent, repo)
}

fn hook_path(repo: &Path) -> std::path::PathBuf {
    repo.join(".git/hooks/pre-commit")
}

fn mode_key_present(repo: &Path) -> bool {
    scratch_git(repo, &["config", "--get", "lane.hook.mode"])
        .status
        .success()
}

#[test]
fn uninstall_restores_foreign_hook_byte_identical() {
    let (_p, repo) = scratch_repo();
    let hp = hook_path(&repo);
    let foreign = "#!/bin/sh\necho \"gitleaks gate\" >&2\nexit 0\n";
    std::fs::write(&hp, foreign).unwrap();
    let mut perms = std::fs::metadata(&hp).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&hp, perms).unwrap();

    let rs = repo.to_str().unwrap();
    assert_eq!(code(&run_hook(&["hook", "install", "--git-repo", rs])), 0);
    assert!(mode_key_present(&repo));

    let out = run_hook(&["hook", "uninstall", "--git-repo", rs, "--json"]);
    assert_eq!(code(&out), 0, "{out:?}");
    let j = stdout_json(&out);
    assert_eq!(j["verb"], "hook.uninstall");
    assert_eq!(j["data"]["removed_block"], true);
    assert_eq!(j["data"]["removed_file"], false);
    assert_eq!(
        std::fs::read_to_string(&hp).unwrap(),
        foreign,
        "foreign hook restored byte-identical"
    );
    assert!(!mode_key_present(&repo), "lane.hook.mode removed");
}

#[test]
fn uninstall_deletes_lane_only_file_then_noops() {
    let (_p, repo) = scratch_repo();
    let rs = repo.to_str().unwrap();
    assert_eq!(code(&run_hook(&["hook", "install", "--git-repo", rs])), 0);

    let out = run_hook(&["hook", "uninstall", "--git-repo", rs, "--json"]);
    assert_eq!(code(&out), 0, "{out:?}");
    let j = stdout_json(&out);
    assert_eq!(j["data"]["removed_block"], true);
    assert_eq!(j["data"]["removed_file"], true);
    assert!(!hook_path(&repo).exists());
    assert!(!mode_key_present(&repo));

    // Uninstall of a never/no-longer-installed repo is a clean no-op (exit 0).
    let again = run_hook(&["hook", "uninstall", "--git-repo", rs, "--json"]);
    assert_eq!(code(&again), 0, "{again:?}");
    let j = stdout_json(&again);
    assert_eq!(j["data"]["removed_block"], false);
    assert_eq!(j["data"]["removed_file"], false);
}

#[test]
fn uninstall_leaves_pure_foreign_hook_alone() {
    let (_p, repo) = scratch_repo();
    let hp = hook_path(&repo);
    let foreign = "#!/bin/sh\nexit 0\n";
    std::fs::write(&hp, foreign).unwrap();
    let mut perms = std::fs::metadata(&hp).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&hp, perms).unwrap();

    let out = run_hook(&[
        "hook",
        "uninstall",
        "--git-repo",
        repo.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(code(&out), 0, "{out:?}");
    assert_eq!(stdout_json(&out)["data"]["removed_block"], false);
    assert_eq!(std::fs::read_to_string(&hp).unwrap(), foreign, "untouched");
}

#[test]
fn damaged_markers_refuse_uninstall() {
    let (_p, repo) = scratch_repo();
    let rs = repo.to_str().unwrap();
    assert_eq!(code(&run_hook(&["hook", "install", "--git-repo", rs])), 0);
    let hp = hook_path(&repo);
    let body = std::fs::read_to_string(&hp).unwrap();
    let broken = body.replace("# <<< lane hook <<<", "");
    std::fs::write(&hp, &broken).unwrap();

    let out = run_hook(&["hook", "uninstall", "--git-repo", rs, "--json"]);
    assert_eq!(code(&out), 1, "never guess on damaged markers: {out:?}");
    assert_eq!(stdout_json(&out)["reason"], "hook_compose_refused");
    assert_eq!(
        std::fs::read_to_string(&hp).unwrap(),
        broken,
        "file untouched"
    );
}
