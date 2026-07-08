//! `lane hook print|install|status` integration: rendering, native installs, composition
//! with foreign hooks, managed-hooksPath refusal, idempotency, damage refusal.

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

fn config_get(repo: &Path, key: &str) -> Option<String> {
    let out = scratch_git(repo, &["config", "--get", key]);
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

#[test]
fn print_full_script_shape() {
    let out = run_hook(&["hook", "print"]);
    assert_eq!(code(&out), 0, "{out:?}");
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(s.starts_with("#!/bin/sh"), "{s}");
    assert!(s.contains("# >>> lane hook v1 >>>"), "{s}");
    assert!(s.contains("# <<< lane hook <<<"), "{s}");
    assert!(s.contains(r#"lane check --path "$PWD""#), "{s}");
    assert!(!s.contains("--repo"), "{s}");
}

#[test]
fn print_snippet_bakes_repo_and_drops_shebang() {
    let out = run_hook(&["hook", "print", "--snippet", "--repo", "eleetai", "--json"]);
    assert_eq!(code(&out), 0, "{out:?}");
    let j = stdout_json(&out);
    assert_eq!(j["verb"], "hook.print");
    assert_eq!(j["repo"], "eleetai");
    assert_eq!(j["data"]["snippet"], true);
    let script = j["data"]["script"].as_str().unwrap();
    assert!(!script.starts_with("#!"), "{script}");
    assert!(
        script.contains(r#"lane check --path "$PWD" --repo "eleetai""#),
        "{script}"
    );
}

#[test]
fn install_fresh_native_writes_executable_and_mode() {
    let (_p, repo) = scratch_repo();
    let out = run_hook(&[
        "hook",
        "install",
        "--git-repo",
        repo.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(code(&out), 0, "{out:?}");
    let j = stdout_json(&out);
    assert_eq!(j["verb"], "hook.install");
    assert_eq!(j["data"]["mode"], "advise");
    assert_eq!(j["data"]["composed"], false);

    let hp = hook_path(&repo);
    let meta = std::fs::metadata(&hp).expect("hook written");
    assert_ne!(meta.permissions().mode() & 0o111, 0, "hook is executable");
    let body = std::fs::read_to_string(&hp).unwrap();
    assert!(body.starts_with("#!/bin/sh"), "{body}");
    assert_eq!(
        config_get(&repo, "lane.hook.mode").as_deref(),
        Some("advise")
    );
}

#[test]
fn install_idempotent_and_never_downgrades_mode() {
    let (_p, repo) = scratch_repo();
    let rs = repo.to_str().unwrap();
    assert_eq!(code(&run_hook(&["hook", "install", "--git-repo", rs])), 0);
    let first = std::fs::read(hook_path(&repo)).unwrap();

    // Operator flips enforce; a plain re-install must keep it AND not duplicate the block.
    assert!(scratch_git(&repo, &["config", "lane.hook.mode", "enforce"])
        .status
        .success());
    let again = run_hook(&["hook", "install", "--git-repo", rs, "--json"]);
    assert_eq!(code(&again), 0, "{again:?}");
    let j = stdout_json(&again);
    assert_eq!(
        j["data"]["mode"], "enforce",
        "re-install keeps operator's enforce"
    );
    assert_eq!(j["data"]["replaced_own_block"], true);
    let second = std::fs::read(hook_path(&repo)).unwrap();
    assert_eq!(first, second, "double install is byte-identical");

    // Explicit --mode writes.
    let explicit = run_hook(&[
        "hook",
        "install",
        "--git-repo",
        rs,
        "--mode",
        "advise",
        "--json",
    ]);
    assert_eq!(code(&explicit), 0);
    assert_eq!(
        config_get(&repo, "lane.hook.mode").as_deref(),
        Some("advise")
    );
}

#[test]
fn install_composes_after_foreign_hook() {
    let (_p, repo) = scratch_repo();
    let hp = hook_path(&repo);
    let foreign = "#!/bin/sh\necho \"foreign-gate-ran\" >&2\n";
    std::fs::write(&hp, foreign).unwrap();
    let mut perms = std::fs::metadata(&hp).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&hp, perms).unwrap();

    let out = run_hook(&[
        "hook",
        "install",
        "--git-repo",
        repo.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(code(&out), 0, "{out:?}");
    assert_eq!(stdout_json(&out)["data"]["composed"], true);
    let body = std::fs::read_to_string(&hp).unwrap();
    assert!(
        body.starts_with(foreign),
        "foreign prefix preserved byte-for-byte"
    );
    assert!(
        body.trim_end().ends_with("# <<< lane hook <<<"),
        "lane block LAST"
    );
}

#[test]
fn install_refuses_dormant_foreign_hook() {
    let (_p, repo) = scratch_repo();
    let hp = hook_path(&repo);
    std::fs::write(&hp, "#!/bin/sh\nexit 1\n").unwrap();
    let mut perms = std::fs::metadata(&hp).unwrap().permissions();
    perms.set_mode(0o644); // NOT executable: git ignores it today
    std::fs::set_permissions(&hp, perms).unwrap();

    let out = run_hook(&[
        "hook",
        "install",
        "--git-repo",
        repo.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(code(&out), 1, "{out:?}");
    let j = stdout_json(&out);
    assert_eq!(j["outcome"], "refused");
    assert_eq!(j["reason"], "hook_compose_refused");
    assert_eq!(
        std::fs::read_to_string(&hp).unwrap(),
        "#!/bin/sh\nexit 1\n",
        "refused file is untouched"
    );
}

#[test]
fn managed_hookspath_refused_with_snippet() {
    let (_p, repo) = scratch_repo();
    assert!(
        scratch_git(&repo, &["config", "core.hooksPath", ".husky/_"])
            .status
            .success()
    );
    let rs = repo.to_str().unwrap();

    let json = run_hook(&["hook", "install", "--git-repo", rs, "--json"]);
    assert_eq!(code(&json), 1, "{json:?}");
    let j = stdout_json(&json);
    assert_eq!(j["outcome"], "refused");
    assert_eq!(j["reason"], "hook_compose_refused");
    assert!(j["data"].is_null());

    let human = run_hook(&["hook", "install", "--git-repo", rs]);
    let e = String::from_utf8_lossy(&human.stderr).into_owned();
    assert!(e.contains("husky"), "{e}");
    assert!(e.contains(".husky/_"), "{e}");
    assert!(
        e.contains("# >>> lane hook v1 >>>"),
        "refusal carries the snippet: {e}"
    );

    assert!(!hook_path(&repo).exists(), "no write into .git/hooks");
    assert!(
        !repo.join(".husky/_").exists(),
        "no write into the managed dir"
    );
    assert_eq!(
        config_get(&repo, "lane.hook.mode"),
        None,
        "no mode key on refusal"
    );
}

#[test]
fn damaged_markers_refuse_install() {
    let (_p, repo) = scratch_repo();
    let rs = repo.to_str().unwrap();
    assert_eq!(code(&run_hook(&["hook", "install", "--git-repo", rs])), 0);
    let hp = hook_path(&repo);
    let body = std::fs::read_to_string(&hp).unwrap();
    let broken = body.replace("# <<< lane hook <<<", "# <<< broken <<<");
    std::fs::write(&hp, &broken).unwrap();

    let out = run_hook(&["hook", "install", "--git-repo", rs, "--json"]);
    assert_eq!(code(&out), 1, "{out:?}");
    assert_eq!(stdout_json(&out)["reason"], "hook_compose_refused");
    assert_eq!(
        std::fs::read_to_string(&hp).unwrap(),
        broken,
        "file untouched"
    );
}

#[test]
fn status_reports_before_and_after() {
    let (_p, repo) = scratch_repo();
    let rs = repo.to_str().unwrap();

    let before = run_hook(&["hook", "status", "--git-repo", rs, "--json"]);
    assert_eq!(code(&before), 0, "{before:?}");
    let j = stdout_json(&before);
    assert_eq!(j["verb"], "hook.status");
    assert_eq!(j["data"]["installed"], false);
    assert_eq!(j["data"]["managed"], false);
    assert_eq!(j["data"]["foreign_hook"], false);
    assert!(j["data"]["lane_on_path"].is_boolean());

    assert_eq!(code(&run_hook(&["hook", "install", "--git-repo", rs])), 0);
    let after = run_hook(&["hook", "status", "--git-repo", rs, "--json"]);
    let j = stdout_json(&after);
    assert_eq!(j["data"]["installed"], true);
    assert_eq!(j["data"]["script_version"], 1);
    assert_eq!(j["data"]["mode"], "advise");
    assert_eq!(j["data"]["foreign_hook"], false);

    // Managed repo: status still answers, exit 0.
    assert!(
        scratch_git(&repo, &["config", "core.hooksPath", ".husky/_"])
            .status
            .success()
    );
    let managed = run_hook(&["hook", "status", "--git-repo", rs, "--json"]);
    assert_eq!(code(&managed), 0);
    let j = stdout_json(&managed);
    assert_eq!(j["data"]["managed"], true);
    assert_eq!(j["data"]["manager"], "husky");
}

#[test]
fn install_outside_a_repo_is_exit_2() {
    let parent = temp_root();
    let not_repo = parent.path().join("plain");
    std::fs::create_dir(&not_repo).unwrap();
    let out = run_hook(&[
        "hook",
        "install",
        "--git-repo",
        not_repo.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(code(&out), 2, "{out:?}");
    assert_eq!(stdout_json(&out)["outcome"], "error");
}
