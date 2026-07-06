//! The git worktree/branch adapter (Slice 3) — **outside** the locking core.
//!
//! Wraps the local `git` binary through `std::process::Command` only (no `libc`/`rustix`/
//! `nix`; the manifest guard forbids them and this module keeps the crate std-only). Every
//! spawn runs under a MANDATORY bounded wait: poll [`std::process::Child::try_wait`] with
//! short sleeps and, on timeout, `kill` the child and return a [`GitError::Timeout`] so a
//! hung `git` can never stall a caller. `stdout`/`stderr` are captured on drainer threads so
//! a chatty command cannot deadlock the poll loop.
//!
//! The [`GitRunner`] trait is the injectable seam (mirroring the locking core's `FsOps`):
//! production is [`StdGitRunner`]; tests inject a fake to drive spawn/timeout/plumbing
//! faults deterministically. The locking core (`src/lock/`) never depends on this module —
//! composition happens one layer up in `src/lifecycle.rs`, and the read-only board provider
//! uses it opt-in.
//!
//! This adapter exposes worktree/branch LIFECYCLE only. There is deliberately NO
//! stage/commit/push surface. `worktree_remove` never passes `--force`: git's own refusal
//! to delete a dirty worktree is the guard, surfaced distinctly as [`GitError::DirtyWorktree`]
//! so a caller can map it to a refusal rather than a plumbing failure.

use std::fmt;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Default per-call bounded wait before a hung `git` is killed.
pub const DEFAULT_GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Captured result of one bounded `git` invocation.
#[derive(Debug, Clone)]
pub struct GitOutput {
    /// Exit code, or `None` when the process was terminated by a signal (including our kill).
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl GitOutput {
    /// True iff the process exited with status 0.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// The distinct failure classes the adapter surfaces. `DirtyWorktree` is kept separate from
/// `Plumbing` so `close` can map a dirty-worktree refusal to an exit-1 refusal while every
/// other git failure stays an exit-2 io-class error.
#[derive(Debug)]
pub enum GitError {
    /// A start precheck: the given path is not inside a git work tree.
    NotARepo { path: PathBuf },
    /// A start precheck: the branch already exists (start would not be creating it).
    BranchExists { branch: String },
    /// A start precheck: the worktree target path already exists on disk.
    WorktreePathExists { path: PathBuf },
    /// `git worktree remove` refused because the worktree has modified or untracked files
    /// (detected from git's stderr). Distinct so a caller maps it to a dirty refusal.
    DirtyWorktree { stderr: String },
    /// A git command exited non-zero for a plumbing/usage reason (not a dirty refusal).
    Plumbing { code: Option<i32>, stderr: String },
    /// The process did not finish within the bounded wait and was killed.
    Timeout { secs: u64 },
    /// The process could not be spawned, or an I/O error occurred while waiting.
    Spawn(io::Error),
    /// An argument (branch name, base ref, or path) begins with `-` and could be parsed
    /// by git as a FLAG rather than a value (argv flag smuggling). Refused before spawn.
    FlagLikeArgument { what: &'static str, value: String },
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitError::NotARepo { path } => {
                write!(f, "not a git repository: {}", path.display())
            }
            GitError::BranchExists { branch } => {
                write!(f, "branch already exists: {branch}")
            }
            GitError::WorktreePathExists { path } => {
                write!(f, "worktree path already exists: {}", path.display())
            }
            GitError::DirtyWorktree { stderr } => {
                write!(
                    f,
                    "worktree has modified or untracked files: {}",
                    stderr.trim()
                )
            }
            GitError::Plumbing { code, stderr } => match code {
                Some(c) => write!(f, "git exited {c}: {}", stderr.trim()),
                None => write!(f, "git terminated by signal: {}", stderr.trim()),
            },
            GitError::Timeout { secs } => {
                write!(f, "git timed out after {secs}s and was killed")
            }
            GitError::Spawn(e) => write!(f, "could not run git: {e}"),
            GitError::FlagLikeArgument { what, value } => {
                write!(
                    f,
                    "refusing {what} that could be parsed as a git flag: {value}"
                )
            }
        }
    }
}

/// Refuse a value git could parse as a flag (leading `-`), and the empty string. Branch
/// names, base refs, and paths are handed to git as argv tokens; a value like `--force`
/// smuggled into any of them would change the command's meaning.
fn require_flag_safe(what: &'static str, value: &str) -> Result<(), GitError> {
    if value.is_empty() || value.starts_with('-') {
        return Err(GitError::FlagLikeArgument {
            what,
            value: value.to_string(),
        });
    }
    Ok(())
}

/// The injectable git seam. Callers pass full argument vectors (including `-C <dir>`); the
/// runner spawns `git` under a mandatory bounded wait and returns captured output.
pub trait GitRunner {
    /// Run `git <args>` under a bounded wait, capturing stdout/stderr.
    fn run(&self, args: &[&str]) -> Result<GitOutput, GitError>;
}

/// The production runner: spawn the real `git` binary with a bounded wait + kill.
pub struct StdGitRunner {
    timeout: Duration,
}

impl StdGitRunner {
    pub fn new() -> Self {
        Self {
            timeout: DEFAULT_GIT_TIMEOUT,
        }
    }
    /// Construct with a custom per-call timeout (used by tests to pin the kill path).
    pub fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl Default for StdGitRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl GitRunner for StdGitRunner {
    fn run(&self, args: &[&str]) -> Result<GitOutput, GitError> {
        let mut cmd = Command::new("git");
        // Pin a stable locale: the dirty-refusal detector (and any stderr matching)
        // parses git's message text, which is localized under a non-English LANG.
        cmd.env("LC_ALL", "C");
        cmd.args(args);
        run_bounded(cmd, self.timeout)
    }
}

/// Spawn a command, drain its pipes on threads, and wait no longer than `timeout` before
/// killing it. The drainer threads prevent a full pipe buffer from deadlocking the poll
/// loop, so even a chatty command is captured or cleanly killed.
fn run_bounded(mut cmd: Command, timeout: Duration) -> Result<GitOutput, GitError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(GitError::Spawn)?;

    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = out_pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = err_pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });

    let start = Instant::now();
    let mut backoff = Duration::from_millis(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    // Joining after the kill relies on the pipes closing with the child;
                    // a grandchild holding an inherited pipe could delay this, which is
                    // pathological for git's own subprocesses and accepted as out of
                    // threat-model (git spawns no long-lived detached children).
                    let _ = out_handle.join();
                    let _ = err_handle.join();
                    return Err(GitError::Timeout {
                        secs: timeout.as_secs(),
                    });
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_handle.join();
                let _ = err_handle.join();
                return Err(GitError::Spawn(e));
            }
        }
    };
    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();
    Ok(GitOutput {
        code: status.code(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// Minimal facts about a live worktree (from stable plumbing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitWorktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
}

/// A start precheck result: the worktree target is safe to create. Carries a non-fatal
/// cross-device warning (the NFS landmine) when the target resolves to a different
/// filesystem device than `$HOME`.
#[derive(Debug, Clone, Default)]
pub struct PrecheckReport {
    pub device_warning: Option<String>,
}

/// The worktree/branch adapter over an injectable [`GitRunner`].
pub struct GitAdapter<'a> {
    runner: &'a dyn GitRunner,
}

impl<'a> GitAdapter<'a> {
    pub fn new(runner: &'a dyn GitRunner) -> Self {
        Self { runner }
    }

    /// `git -C <repo> check-ref-format --branch <name>` — validates a branch name the
    /// way git itself does (rejects `..`, `~`, spaces, control chars, and every other
    /// invalid-refname shape the leading-dash guard cannot catch).
    pub fn check_branch_name(&self, repo: &Path, name: &str) -> Result<(), GitError> {
        let repo_s = path_str(repo)?;
        require_flag_safe("branch name", name)?;
        let out = self
            .runner
            .run(&["-C", repo_s, "check-ref-format", "--branch", name])?;
        if out.success() {
            Ok(())
        } else {
            Err(GitError::Plumbing {
                code: out.code,
                stderr: format!("invalid branch name {name:?}: {}", out.stderr.trim()),
            })
        }
    }

    /// `git -C <repo> branch <name> <base>` — create the branch as its OWN tracked step.
    /// `start` uses this (instead of `worktree add -b`) so branch OWNERSHIP is a known
    /// fact: the compensation path deletes the branch if and only if THIS call
    /// succeeded, never by inferring creation from a later `branch_exists` (which races
    /// an external git process creating the same name).
    pub fn create_branch(&self, repo: &Path, name: &str, base: &str) -> Result<(), GitError> {
        let repo_s = path_str(repo)?;
        require_flag_safe("branch name", name)?;
        require_flag_safe("base ref", base)?;
        let out = self.runner.run(&["-C", repo_s, "branch", name, base])?;
        if out.success() {
            Ok(())
        } else {
            Err(GitError::Plumbing {
                code: out.code,
                stderr: out.stderr,
            })
        }
    }

    /// `git -C <repo> worktree add <path> <branch>` — attach a worktree to an EXISTING
    /// branch (created by [`Self::create_branch`]).
    pub fn worktree_add_existing(
        &self,
        repo: &Path,
        path: &Path,
        branch: &str,
    ) -> Result<(), GitError> {
        let repo_s = path_str(repo)?;
        let path_s = path_str(path)?;
        require_flag_safe("branch name", branch)?;
        require_flag_safe("worktree path", path_s)?;
        let out = self
            .runner
            .run(&["-C", repo_s, "worktree", "add", path_s, branch])?;
        if out.success() {
            Ok(())
        } else {
            Err(GitError::Plumbing {
                code: out.code,
                stderr: out.stderr,
            })
        }
    }

    /// `git -C <repo> worktree add -b <branch> <path> <base>` (single-step form; the
    /// lifecycle layer prefers the two-step create_branch + worktree_add_existing for
    /// compensation-ownership reasons — see [`Self::create_branch`]).
    pub fn worktree_add(
        &self,
        repo: &Path,
        path: &Path,
        new_branch: &str,
        base: &str,
    ) -> Result<(), GitError> {
        let repo_s = path_str(repo)?;
        let path_s = path_str(path)?;
        require_flag_safe("branch name", new_branch)?;
        require_flag_safe("base ref", base)?;
        require_flag_safe("worktree path", path_s)?;
        let out = self.runner.run(&[
            "-C", repo_s, "worktree", "add", "-b", new_branch, path_s, base,
        ])?;
        if out.success() {
            Ok(())
        } else {
            Err(GitError::Plumbing {
                code: out.code,
                stderr: out.stderr,
            })
        }
    }

    /// `git -C <repo> worktree remove <path>` — NEVER `--force`. A git dirty refusal maps to
    /// [`GitError::DirtyWorktree`]; any other non-zero exit is [`GitError::Plumbing`].
    pub fn worktree_remove(&self, repo: &Path, path: &Path) -> Result<(), GitError> {
        let repo_s = path_str(repo)?;
        let path_s = path_str(path)?;
        require_flag_safe("worktree path", path_s)?;
        let out = self
            .runner
            .run(&["-C", repo_s, "worktree", "remove", path_s])?;
        if out.success() {
            return Ok(());
        }
        if is_dirty_refusal(&out.stderr) {
            Err(GitError::DirtyWorktree { stderr: out.stderr })
        } else {
            Err(GitError::Plumbing {
                code: out.code,
                stderr: out.stderr,
            })
        }
    }

    /// `git -C <repo> branch -D <branch>` — compensation-only (start failure cleanup).
    pub fn delete_branch(&self, repo: &Path, branch: &str) -> Result<(), GitError> {
        let repo_s = path_str(repo)?;
        require_flag_safe("branch name", branch)?;
        let out = self.runner.run(&["-C", repo_s, "branch", "-D", branch])?;
        if out.success() {
            Ok(())
        } else {
            Err(GitError::Plumbing {
                code: out.code,
                stderr: out.stderr,
            })
        }
    }

    /// `git -C <repo> show-ref --verify --quiet refs/heads/<name>` (exit 0 = exists, 1 = not).
    pub fn branch_exists(&self, repo: &Path, name: &str) -> Result<bool, GitError> {
        let repo_s = path_str(repo)?;
        require_flag_safe("branch name", name)?;
        let refname = format!("refs/heads/{name}");
        let out = self
            .runner
            .run(&["-C", repo_s, "show-ref", "--verify", "--quiet", &refname])?;
        match out.code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(GitError::Plumbing {
                code: out.code,
                stderr: out.stderr,
            }),
        }
    }

    /// `git -C <path> rev-parse --is-inside-work-tree` → true only when git reports `true`.
    pub fn is_git_repo(&self, path: &Path) -> Result<bool, GitError> {
        let path_s = path_str(path)?;
        let out = self
            .runner
            .run(&["-C", path_s, "rev-parse", "--is-inside-work-tree"])?;
        Ok(out.success() && out.stdout.trim() == "true")
    }

    /// `git -C <path> status --porcelain` → true when the working tree reports no changes.
    pub fn status_clean(&self, path: &Path) -> Result<bool, GitError> {
        let path_s = path_str(path)?;
        let out = self.runner.run(&["-C", path_s, "status", "--porcelain"])?;
        if !out.success() {
            return Err(GitError::Plumbing {
                code: out.code,
                stderr: out.stderr,
            });
        }
        Ok(out.stdout.trim().is_empty())
    }

    /// Probe a path for a live worktree via stable plumbing. Returns `None` when the path is
    /// not inside a work tree (deleted, or a non-worktree directory), so a caller can safely
    /// skip a worktree removal over a target that no longer exists.
    pub fn probe_worktree(&self, path: &Path) -> Result<Option<GitWorktree>, GitError> {
        let path_s = path_str(path)?;
        let inside = self
            .runner
            .run(&["-C", path_s, "rev-parse", "--is-inside-work-tree"])?;
        if !(inside.success() && inside.stdout.trim() == "true") {
            // A MISS (Ok(None)) is only the two benign shapes: the path is GONE
            // ("cannot change to ... no such file or directory" - the ENOENT form
            // ONLY; the same prefix with "permission denied" means an inaccessible
            // path that may still hold a real worktree), or a plain directory under
            // no repository ("not a git repository"). An INSPECTION failure -
            // dubious ownership, permission denied, a corrupt object store - must
            // PROPAGATE so a destructive caller (close) fails closed instead of
            // releasing a claim over a surviving worktree. LC_ALL=C is pinned by
            // the runner, so the message text is stable.
            if inside.success() {
                return Ok(None); // printed something other than "true": not a work tree
            }
            let s = inside.stderr.to_ascii_lowercase();
            let benign_miss = (s.contains("cannot change to")
                && s.contains("no such file or directory"))
                || s.contains("not a git repository");
            if benign_miss {
                return Ok(None);
            }
            return Err(GitError::Plumbing {
                code: inside.code,
                stderr: inside.stderr,
            });
        }
        let branch_out = self
            .runner
            .run(&["-C", path_s, "branch", "--show-current"])?;
        let branch_s = branch_out.stdout.trim();
        let branch = if branch_s.is_empty() {
            None
        } else {
            Some(branch_s.to_string())
        };
        let head_out = self.runner.run(&["-C", path_s, "rev-parse", "HEAD"])?;
        let head = if head_out.success() {
            let h = head_out.stdout.trim();
            if h.is_empty() {
                None
            } else {
                Some(h.to_string())
            }
        } else {
            None
        };
        Ok(Some(GitWorktree {
            path: path.to_path_buf(),
            branch,
            head,
        }))
    }

    /// `git --version` succeeds: the cheap availability gate the live board provider
    /// runs once before its first probe.
    pub fn version_ok(&self) -> bool {
        matches!(self.runner.run(&["--version"]), Ok(out) if out.success())
    }

    /// The MAIN repository working directory a worktree belongs to, via
    /// `git -C <worktree> rev-parse --path-format=absolute --git-common-dir` (the shared
    /// `.git` dir; its parent is the main checkout). Used by `close` to run
    /// `worktree remove` from the owning repo, since the claim record carries only the
    /// worktree path.
    pub fn main_repo_of(&self, worktree: &Path) -> Result<PathBuf, GitError> {
        let path_s = path_str(worktree)?;
        let out = self.runner.run(&[
            "-C",
            path_s,
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ])?;
        if !out.success() {
            return Err(GitError::Plumbing {
                code: out.code,
                stderr: out.stderr,
            });
        }
        let common = PathBuf::from(out.stdout.trim());
        common
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| GitError::Plumbing {
                code: Some(0),
                stderr: format!("git-common-dir has no parent: {}", common.display()),
            })
    }

    /// Read-only prechecks for `start`, run BEFORE any claim: the repo must be a real git
    /// repo, the branch must not already exist, and the worktree path must not exist. On
    /// success the target is safe to create (and the branch is known-absent, so a later
    /// failure may clean it up); the report carries any cross-device warning.
    pub fn precheck_start(
        &self,
        repo: &Path,
        worktree_path: &Path,
        branch: &str,
        home: Option<&str>,
    ) -> Result<PrecheckReport, GitError> {
        if !self.is_git_repo(repo)? {
            return Err(GitError::NotARepo {
                path: repo.to_path_buf(),
            });
        }
        if self.branch_exists(repo, branch)? {
            return Err(GitError::BranchExists {
                branch: branch.to_string(),
            });
        }
        if worktree_path.symlink_metadata().is_ok() {
            return Err(GitError::WorktreePathExists {
                path: worktree_path.to_path_buf(),
            });
        }
        Ok(PrecheckReport {
            device_warning: cross_device_warning(worktree_path, home),
        })
    }
}

/// Detect git's refusal to remove a dirty worktree (modified or untracked files) from its
/// stderr. Validated against real git by the dirty-refusal integration test.
fn is_dirty_refusal(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("contains modified or untracked files")
        || s.contains("use --force to delete")
        || s.contains("is dirty")
}

/// A non-fatal warning when `path` resolves to a different filesystem device than `$HOME`
/// (advisory locks and worktrees on NFS are unreliable). Computed on the LONGEST EXISTING
/// ANCESTOR of `path`, because a fresh `start` target's leaf does not exist yet and a direct
/// stat on it would fail with `NotFound`.
pub fn cross_device_warning(path: &Path, home: Option<&str>) -> Option<String> {
    let home = home?;
    let home_dev = std::fs::metadata(home).ok()?.dev();
    let target_dev = device_of_longest_existing_ancestor(path).ok()?;
    if target_dev != home_dev {
        Some(format!(
            "target {} resolves to a different filesystem device than $HOME; advisory locks and worktrees on NFS are unreliable",
            path.display()
        ))
    } else {
        None
    }
}

/// The device id of `path`'s longest existing ancestor (walking up until a component
/// exists). Uses FOLLOWING metadata deliberately: a symlinked ancestor (for example a
/// local `~/projects` symlink onto an NFS volume) must report the device the path
/// actually RESOLVES to — the exact case the cross-device warning exists for — not the
/// symlink's own local device.
fn device_of_longest_existing_ancestor(path: &Path) -> io::Result<u64> {
    let mut acc = path.to_path_buf();
    loop {
        match std::fs::metadata(&acc) {
            Ok(m) => return Ok(m.dev()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if !acc.pop() {
                    // An absolute path always reaches "/"; fall back to the root device.
                    return std::fs::metadata(Path::new("/")).map(|m| m.dev());
                }
            }
            Err(e) => return Err(e),
        }
    }
}

/// The outcome of matching a case-folded stored tail against real directory entries.
#[derive(Debug, PartialEq, Eq)]
pub enum CaseMatch {
    /// Exactly one entry matches case-insensitively: the real on-disk name.
    One(String),
    /// No entry matches: the worktree genuinely is not there.
    Absent,
    /// Two or more entries differ only by case (possible on a case-sensitive volume):
    /// never guess — skip the claim and degrade the source.
    Ambiguous,
}

/// Case-insensitively match a stored (ASCII-folded) leaf name against real entries.
/// Pure logic, unit-tested directly: the ambiguous branch is only REACHABLE on a
/// case-sensitive filesystem, which the dev machine's default volume is not.
pub fn resolve_real_case(entries: &[String], folded_tail: &str) -> CaseMatch {
    let mut hits = entries
        .iter()
        .filter(|e| e.eq_ignore_ascii_case(folded_tail));
    match (hits.next(), hits.next()) {
        (None, _) => CaseMatch::Absent,
        (Some(one), None) => CaseMatch::One(one.clone()),
        (Some(_), Some(_)) => CaseMatch::Ambiguous,
    }
}

/// Convert a path to `&str`, failing loudly on the (pathological) non-UTF-8 case rather than
/// lossily corrupting a path handed to real git.
fn path_str(p: &Path) -> Result<&str, GitError> {
    p.to_str().ok_or_else(|| {
        GitError::Spawn(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path is not valid UTF-8: {}", p.display()),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_wait_kills_a_hung_process() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 60"]);
        let start = Instant::now();
        let res = run_bounded(cmd, Duration::from_millis(300));
        let elapsed = start.elapsed();
        assert!(
            matches!(res, Err(GitError::Timeout { .. })),
            "a hung process must time out, got {res:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the hung process must be killed promptly, took {elapsed:?}"
        );
    }

    #[test]
    fn bounded_wait_captures_output_and_code() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf hello; printf oops 1>&2; exit 3"]);
        let out = run_bounded(cmd, Duration::from_secs(5)).expect("completes");
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stdout, "hello");
        assert_eq!(out.stderr, "oops");
        assert!(!out.success());
    }

    #[test]
    fn flag_like_arguments_are_refused_before_spawn() {
        struct Counting(std::cell::Cell<u32>);
        impl GitRunner for Counting {
            fn run(&self, _args: &[&str]) -> Result<GitOutput, GitError> {
                self.0.set(self.0.get() + 1);
                Ok(GitOutput {
                    code: Some(0),
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
        }
        let runner = Counting(std::cell::Cell::new(0));
        let git = GitAdapter::new(&runner);
        // A branch or base that looks like a flag must be refused with ZERO spawns.
        assert!(matches!(
            git.worktree_add(Path::new("/r"), Path::new("/r-x"), "--force", "HEAD"),
            Err(GitError::FlagLikeArgument {
                what: "branch name",
                ..
            })
        ));
        assert!(matches!(
            git.worktree_add(Path::new("/r"), Path::new("/r-x"), "b", "--help"),
            Err(GitError::FlagLikeArgument {
                what: "base ref",
                ..
            })
        ));
        assert!(matches!(
            git.delete_branch(Path::new("/r"), "-D"),
            Err(GitError::FlagLikeArgument {
                what: "branch name",
                ..
            })
        ));
        assert!(matches!(
            git.branch_exists(Path::new("/r"), "--verify"),
            Err(GitError::FlagLikeArgument {
                what: "branch name",
                ..
            })
        ));
        assert_eq!(runner.0.get(), 0, "no git spawn for a refused argument");
    }

    #[test]
    fn probe_classifies_misses_vs_inspection_failures() {
        struct Scripted(&'static str, Option<i32>);
        impl GitRunner for Scripted {
            fn run(&self, _args: &[&str]) -> Result<GitOutput, GitError> {
                Ok(GitOutput {
                    code: self.1,
                    stdout: String::new(),
                    stderr: self.0.to_string(),
                })
            }
        }
        // Benign misses -> Ok(None): path gone, or a plain dir under no repository.
        for stderr in [
            "fatal: cannot change to '/x': No such file or directory",
            "fatal: not a git repository (or any of the parent directories): .git",
        ] {
            let r = Scripted(stderr, Some(128));
            let git = GitAdapter::new(&r);
            assert!(
                git.probe_worktree(Path::new("/x")).expect("miss").is_none(),
                "benign miss for {stderr:?}"
            );
        }
        // Inspection failures on a possibly-real worktree -> Err (fail closed).
        for stderr in [
            "fatal: detected dubious ownership in repository at '/x'",
            "fatal: cannot open '/x/.git': Permission denied",
            "fatal: cannot change to '/x': Permission denied",
        ] {
            let r = Scripted(stderr, Some(128));
            let git = GitAdapter::new(&r);
            assert!(
                git.probe_worktree(Path::new("/x")).is_err(),
                "inspection failure propagates for {stderr:?}"
            );
        }
    }

    #[test]
    fn dirty_refusal_matcher_recognizes_git_message() {
        assert!(is_dirty_refusal(
            "fatal: '/x' contains modified or untracked files, use --force to delete it"
        ));
        assert!(!is_dirty_refusal("fatal: not a valid ref"));
    }

    #[test]
    fn device_check_walks_up_to_an_existing_ancestor() {
        // A deep non-existent leaf under the current dir must resolve via the ancestor, not
        // NotFound-fail. Same device as itself → no warning.
        let base = std::env::current_dir().unwrap();
        let leaf = base.join("no-such-a/no-such-b/no-such-c");
        let dev = device_of_longest_existing_ancestor(&leaf).expect("resolves via ancestor");
        assert_eq!(dev, std::fs::metadata(&base).unwrap().dev());
    }
}
