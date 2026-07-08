//! `lane hook` — the git pre-commit guard family (Slice 3.5, ZER-84) — OUTSIDE the
//! locking core.
//!
//! Four verbs: `print` (emit the guard script/snippet), `install` (write it into a
//! consumer repo's RESOLVED hooks dir), `status` (read-only report), `uninstall`
//! (surgical removal). The installed hook runs `lane check --path "$PWD"` at commit time
//! and maps its exit code by mode (`git config lane.hook.mode`: `advise` warns and
//! passes, `enforce` fails closed; `LANE_HOOK_BYPASS=1` is the loud human bypass). The
//! hook itself NEVER auto-claims and never mutates lane state; this module takes NO core
//! locks and never touches `$LANE_ROOT`.
//!
//! Composition law: NEVER clobber a foreign hook. A managed hooks dir
//! (`core.hooksPath` set — husky et al.) is refused with the exact snippet to paste,
//! because tracked hook files are repo content (PR territory) and generated shim dirs
//! get regenerated. A foreign `pre-commit` in a native hooks dir is composed by
//! APPENDING a marked block (lane block LAST — hooks AND-compose, so order only decides
//! which refusal prints first, and the repo owner's gate order is preserved); re-install
//! replaces exactly the marked block (idempotent; also the version-upgrade path); a
//! symlinked, non-executable, non-UTF-8, oversize, or marker-damaged file is refused
//! untouched. Every write is temp-in-same-dir + chmod 0755 + atomic rename, so a racing
//! `git commit` never execs a half-written hook.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::cli::{
    HookArgs, HookCmd, HookInstallArgs, HookMode, HookPrintArgs, HookStatusArgs, HookUninstallArgs,
};
use crate::error::{LaneError, RefusedReason};
use crate::git::{GitAdapter, GitRunner, StdGitRunner};
use crate::lifecycle::git_to_lane;
use crate::lock::{emit, validate_name, CommandError, Outcome, VerbData};

/// Version stamped into the marker line; parsed back by `status` and the upgrade path.
pub const HOOK_VERSION: u32 = 1;
/// Version-agnostic detection prefix (any `# >>> lane hook vN >>>`).
const MARKER_OPEN_PREFIX: &str = "# >>> lane hook v";
const MARKER_OPEN_SUFFIX: &str = " >>>";
const MARKER_CLOSE: &str = "# <<< lane hook <<<";
/// A hook file larger than this is not something we edit programmatically.
const MAX_HOOK_BYTES: u64 = 1024 * 1024;
/// The git config key holding the per-repo guard mode.
const MODE_KEY: &str = "lane.hook.mode";

/// The canonical guard block. Function-wrapped so every outcome `return`s and FALLS
/// THROUGH to the host script's remaining gates — never `exit 0` from inside a composed
/// hook (that would skip a later gitleaks/ci gate). `sh -e`-safe (every command is an
/// if-condition, `|| true`, assignment, or echo — husky's loader runs `sh -e`) and
/// `set -u`-safe (only `${LANE_HOOK_BYPASS:-}` may be unset). No `local` (not POSIX);
/// `lane_`-prefixed names avoid host-variable collisions.
const BLOCK_TEMPLATE: &str = r#"# >>> lane hook v1 >>>
# lane pre-commit guard - managed by `lane hook`; do not edit inside the markers.
# Mode: `git config lane.hook.mode` (advise | enforce; default advise).
# Bypass once (loud): LANE_HOOK_BYPASS=1 git commit ...
lane_hook_guard() {
  if [ "${LANE_HOOK_BYPASS:-}" = "1" ]; then
    echo "lane-hook: BYPASSED via LANE_HOOK_BYPASS=1 (claim guard skipped for this commit)" >&2
    return 0
  fi
  lane_mode="$(git config --get lane.hook.mode 2>/dev/null || true)"
  case "$lane_mode" in
    enforce) ;;
    ""|advise) lane_mode=advise ;;
    *)
      echo "lane-hook: WARNING: unknown lane.hook.mode '$lane_mode'; treating as advise" >&2
      lane_mode=advise
      ;;
  esac
  if ! command -v lane >/dev/null 2>&1; then
    if [ "$lane_mode" = "enforce" ]; then
      echo "lane-hook: BLOCKED: 'lane' not found on PATH and lane.hook.mode=enforce." >&2
      echo "lane-hook: install lane, or bypass once: LANE_HOOK_BYPASS=1 git commit ..." >&2
      return 1
    fi
    echo "lane-hook: WARNING: 'lane' not found on PATH; claim guard skipped (advise mode)" >&2
    return 0
  fi
  if lane check --path "$PWD"__LANE_REPO__ >/dev/null; then
    return 0
  else
    lane_rc=$?
  fi
  if [ "$lane_mode" = "enforce" ]; then
    if [ "$lane_rc" -eq 1 ]; then
      echo "lane-hook: BLOCKED: commit path not covered by your active lane claim (fix command above)." >&2
    else
      echo "lane-hook: BLOCKED: lane check failed (exit $lane_rc, integrity/io) under enforce mode." >&2
    fi
    echo "lane-hook: bypass once: LANE_HOOK_BYPASS=1 git commit ..." >&2
    return 1
  fi
  if [ "$lane_rc" -eq 1 ]; then
    echo "lane-hook: WARNING: commit path not covered by an active lane claim (advise mode; commit allowed)." >&2
  else
    echo "lane-hook: WARNING: lane check failed (exit $lane_rc, integrity/io); commit allowed (advise mode)." >&2
  fi
  return 0
}
lane_hook_guard || exit 1
# <<< lane hook <<<
"#;

/// Render the bare guard block (the `--snippet` form; also what installs are built
/// from). `repo_ns` is already `validate_name`-checked — the grammar is shell-inert
/// inside double quotes.
pub fn render_block(repo_ns: Option<&str>) -> String {
    let repo_arg = repo_ns
        .map(|ns| format!(" --repo \"{ns}\""))
        .unwrap_or_default();
    BLOCK_TEMPLATE.replace("__LANE_REPO__", &repo_arg)
}

/// Render the full standalone hook script (fresh native install).
pub fn render_script(repo_ns: Option<&str>) -> String {
    format!("#!/bin/sh\n\n{}", render_block(repo_ns))
}

/// Classification of an existing `pre-commit` file's contents.
#[derive(Debug, PartialEq, Eq)]
enum HookFileState {
    /// No file.
    Absent,
    /// Exactly our markers and nothing else of substance: we own the file.
    LaneOnly { version: u32 },
    /// A hook with no lane markers: someone else's gate.
    ForeignOnly,
    /// Foreign content plus exactly one well-formed lane block.
    Composed { version: u32 },
    /// Marker text present but not exactly one open-before-close pair — never guess.
    Damaged(String),
    /// Not something we edit programmatically (non-UTF-8 / oversize).
    NonComposable(String),
}

/// Parse the version from a marker-open line (`# >>> lane hook v1 >>>`).
fn parse_version(line: &str) -> Option<u32> {
    let rest = line.trim().strip_prefix(MARKER_OPEN_PREFIX)?;
    let digits = rest.strip_suffix(MARKER_OPEN_SUFFIX)?;
    digits.parse().ok()
}

/// Locate the (sole) marker pair as line indices, or explain why the markers are bad.
fn find_block(lines: &[&str]) -> Result<Option<(usize, usize, u32)>, String> {
    let opens: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim().starts_with(MARKER_OPEN_PREFIX))
        .map(|(i, _)| i)
        .collect();
    let closes: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim() == MARKER_CLOSE)
        .map(|(i, _)| i)
        .collect();
    match (opens.as_slice(), closes.as_slice()) {
        ([], []) => Ok(None),
        ([o], [c]) if o < c => {
            let version = parse_version(lines[*o])
                .ok_or_else(|| format!("unparseable marker line: {:?}", lines[*o].trim()))?;
            Ok(Some((*o, *c, version)))
        }
        ([o], [c]) => Err(format!(
            "close marker (line {}) precedes open marker (line {})",
            c + 1,
            o + 1
        )),
        _ => Err(format!(
            "expected exactly one lane marker pair, found {} open / {} close",
            opens.len(),
            closes.len()
        )),
    }
}

/// True when `lines` contain nothing but blanks and an optional shebang — i.e. removing
/// the lane block would leave an empty shell of a file we created ourselves.
fn only_scaffolding(lines: &[&str]) -> bool {
    lines
        .iter()
        .all(|l| l.trim().is_empty() || l.trim_start().starts_with("#!"))
}

/// Classify raw `pre-commit` contents (pure; `None` = no file).
fn classify(bytes: Option<&[u8]>) -> HookFileState {
    let Some(bytes) = bytes else {
        return HookFileState::Absent;
    };
    if bytes.len() as u64 > MAX_HOOK_BYTES {
        return HookFileState::NonComposable("existing pre-commit is larger than 1 MiB".into());
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return HookFileState::NonComposable("existing pre-commit is not valid UTF-8".into());
    };
    let lines: Vec<&str> = text.lines().collect();
    match find_block(&lines) {
        Err(detail) => HookFileState::Damaged(detail),
        Ok(None) => HookFileState::ForeignOnly,
        Ok(Some((o, c, version))) => {
            let mut rest: Vec<&str> = Vec::new();
            rest.extend_from_slice(&lines[..o]);
            rest.extend_from_slice(&lines[c + 1..]);
            if only_scaffolding(&rest) {
                HookFileState::LaneOnly { version }
            } else {
                HookFileState::Composed { version }
            }
        }
    }
}

/// Replace the existing lane block with `block`, preserving every foreign byte-line
/// outside the markers. Errors carry the damage detail (never guess).
fn splice(existing: &str, block: &str) -> Result<String, String> {
    let lines: Vec<&str> = existing.lines().collect();
    let Some((o, c, _)) = find_block(&lines)? else {
        return Err("no lane marker pair to replace".into());
    };
    let mut out: Vec<&str> = Vec::new();
    out.extend_from_slice(&lines[..o]);
    out.extend(block.trim_end().lines());
    out.extend_from_slice(&lines[c + 1..]);
    Ok(format!("{}\n", out.join("\n")))
}

/// Remove the lane block. `Ok(None)` = only scaffolding remains (delete the file);
/// `Ok(Some(rest))` = the foreign remainder to write back.
fn remove_block(existing: &str) -> Result<Option<String>, String> {
    let lines: Vec<&str> = existing.lines().collect();
    let Some((o, c, _)) = find_block(&lines)? else {
        return Err("no lane marker pair present".into());
    };
    let mut rest: Vec<&str> = Vec::new();
    rest.extend_from_slice(&lines[..o]);
    rest.extend_from_slice(&lines[c + 1..]);
    if only_scaffolding(&rest) {
        return Ok(None);
    }
    // Trim trailing blank lines the append seam introduced.
    while rest.last().is_some_and(|l| l.trim().is_empty()) {
        rest.pop();
    }
    Ok(Some(format!("{}\n", rest.join("\n"))))
}

/// Name the manager implied by a `core.hooksPath` value (for the refusal message).
fn detect_manager(hooks_path: &str) -> &'static str {
    if hooks_path.contains(".husky") {
        "husky"
    } else if hooks_path.contains("lefthook") {
        "lefthook"
    } else {
        "a hooks manager"
    }
}

/// Temp-in-same-dir + chmod 0755 + atomic rename: a racing `git commit` either execs the
/// old hook or the new one, never a torn file. The dot-named temp is inert to git.
fn atomic_write_hook(path: &Path, contents: &str) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "hook path has no parent dir")
    })?;
    let tmp = dir.join(".pre-commit.lane-tmp");
    std::fs::write(&tmp, contents)?;
    let mut perms = std::fs::metadata(&tmp)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&tmp, perms)?;
    std::fs::rename(&tmp, path)
}

/// In-process `$PATH` probe for the `lane` binary (no spawn): first hit that is a
/// regular file with any exec bit.
fn lane_on_path() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join("lane");
        std::fs::metadata(&candidate)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

fn mode_str(mode: HookMode) -> &'static str {
    match mode {
        HookMode::Advise => "advise",
        HookMode::Enforce => "enforce",
    }
}

fn compose_refused(msg: String) -> CommandError {
    LaneError::RefusedMsg {
        reason: RefusedReason::HookComposeRefused,
        msg,
    }
    .into()
}

/// Resolve `--git-repo` (absolute-only, mirroring `start`) or default to the cwd, then
/// normalize to the working tree's toplevel.
fn resolve_toplevel(git: &GitAdapter, arg: Option<&PathBuf>) -> Result<PathBuf, CommandError> {
    let base = match arg {
        Some(p) => {
            if !p.is_absolute() {
                return Err(LaneError::Identity(format!(
                    "--git-repo must be an absolute path, got {}",
                    p.display()
                ))
                .into());
            }
            p.clone()
        }
        None => std::env::current_dir().map_err(LaneError::Io)?,
    };
    git.toplevel(&base).map_err(git_to_lane).map_err(Into::into)
}

/// Read the existing pre-commit: `Ok(None)` = absent; refuses a symlink or a dormant
/// (non-executable) regular file — composing onto a dormant hook would silently
/// ACTIVATE a gate git currently ignores.
fn read_existing_hook(pre_commit: &Path) -> Result<Option<Vec<u8>>, CommandError> {
    let meta = match std::fs::symlink_metadata(pre_commit) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(LaneError::Io(e).into()),
    };
    if meta.file_type().is_symlink() {
        return Err(compose_refused(format!(
            "{} is a symlink (a hooks manager may own it); install manually: append `lane hook print --snippet` to the real hook via your normal flow",
            pre_commit.display()
        )));
    }
    if !meta.is_file() {
        return Err(compose_refused(format!(
            "{} exists but is not a regular file",
            pre_commit.display()
        )));
    }
    if meta.permissions().mode() & 0o111 == 0 {
        return Err(compose_refused(format!(
            "existing {} is not executable, so git currently IGNORES it; composing would silently activate it. chmod +x (or remove) the file, then re-run install",
            pre_commit.display()
        )));
    }
    let bytes = std::fs::read(pre_commit).map_err(LaneError::Io)?;
    Ok(Some(bytes))
}

// ---------------------------------------------------------------------------
// Runners.
// ---------------------------------------------------------------------------

/// `lane hook` dispatch (production runner + clock).
pub fn run_hook(args: &HookArgs) -> i32 {
    let runner = StdGitRunner::new();
    match &args.cmd {
        HookCmd::Print(a) => run_print(a),
        HookCmd::Install(a) => run_install_at(a, &runner),
        HookCmd::Status(a) => run_status_at(a, &runner),
        HookCmd::Uninstall(a) => run_uninstall_at(a, &runner),
    }
}

/// `lane hook print` — pure render, no git, no filesystem.
fn run_print(args: &HookPrintArgs) -> i32 {
    let repo = args.repo.clone();
    let result = (|| -> Result<(Outcome, Option<VerbData>, Option<String>), CommandError> {
        if let Some(r) = &args.repo {
            validate_name("repo", r)?;
        }
        let script = if args.snippet {
            render_block(args.repo.as_deref())
        } else {
            render_script(args.repo.as_deref())
        };
        Ok((
            Outcome::Ok,
            Some(VerbData::HookPrint {
                script,
                snippet: args.snippet,
            }),
            None,
        ))
    })();
    emit(args.json, "hook.print", repo, None, result)
}

pub(crate) fn run_install_at(args: &HookInstallArgs, runner: &dyn GitRunner) -> i32 {
    let repo = args.repo.clone();
    let result = (|| -> Result<(Outcome, Option<VerbData>, Option<String>), CommandError> {
        if let Some(r) = &args.repo {
            validate_name("repo", r)?;
        }
        let git = GitAdapter::new(runner);
        let top = resolve_toplevel(&git, args.git_repo.as_ref())?;

        // Managed hooks dir → refuse with the exact snippet (tracked hook files are repo
        // content; generated shim dirs get clobbered on regeneration).
        if let Some(hooks_path) = git
            .config_get(&top, "core.hooksPath")
            .map_err(git_to_lane)?
        {
            let manager = detect_manager(&hooks_path);
            return Err(compose_refused(format!(
                "core.hooksPath = '{hooks_path}' is set — {manager} manages hooks in {top}; lane will not write into a managed hooks dir.\nAppend this block at the END of the managed pre-commit hook (after existing gates) via your normal PR flow:\n\n{}",
                render_block(args.repo.as_deref()),
                top = top.display(),
            )));
        }

        let hooks_dir = git.hooks_dir(&top).map_err(git_to_lane)?;
        std::fs::create_dir_all(&hooks_dir).map_err(LaneError::Io)?;
        let pre_commit = hooks_dir.join("pre-commit");
        let block = render_block(args.repo.as_deref());

        let existing = read_existing_hook(&pre_commit)?;
        let mut warning: Option<String> = None;
        let (contents, composed, replaced_own_block) = match classify(existing.as_deref()) {
            HookFileState::Absent => (render_script(args.repo.as_deref()), false, false),
            HookFileState::LaneOnly { .. } | HookFileState::Composed { .. } => {
                // Safe: classify only returns these for valid UTF-8.
                let text = String::from_utf8(existing.unwrap()).expect("classified as UTF-8");
                let spliced = splice(&text, &block).map_err(compose_refused_detail)?;
                let composed = matches!(
                    classify(Some(spliced.as_bytes())),
                    HookFileState::Composed { .. }
                );
                (spliced, composed, true)
            }
            HookFileState::ForeignOnly => {
                let text = String::from_utf8(existing.unwrap()).expect("classified as UTF-8");
                if text.lines().any(|l| l.trim_start().starts_with("exec ")) {
                    warning = Some(
                        "existing hook appears to exec another program; the appended lane block may be unreachable — verify with a test commit".into(),
                    );
                }
                let sep = if text.ends_with('\n') { "" } else { "\n" };
                (format!("{text}{sep}{block}"), true, false)
            }
            HookFileState::Damaged(detail) => {
                return Err(compose_refused(format!(
                    "lane markers in {} are damaged ({detail}); repair or remove the block manually between `{MARKER_OPEN_PREFIX}…` and `{MARKER_CLOSE}`",
                    pre_commit.display()
                )));
            }
            HookFileState::NonComposable(detail) => {
                return Err(compose_refused(format!(
                    "{detail} at {}; compose manually with `lane hook print --snippet`",
                    pre_commit.display()
                )));
            }
        };
        atomic_write_hook(&pre_commit, &contents).map_err(LaneError::Io)?;

        // Mode: explicit flag always writes; otherwise default `advise` ONLY when the
        // key is unset (a re-install must never silently downgrade an operator's enforce).
        let mode = match args.mode {
            Some(m) => {
                git.config_set(&top, MODE_KEY, mode_str(m))
                    .map_err(git_to_lane)?;
                mode_str(m).to_string()
            }
            None => match git.config_get(&top, MODE_KEY).map_err(git_to_lane)? {
                Some(existing_mode) => existing_mode,
                None => {
                    git.config_set(&top, MODE_KEY, "advise")
                        .map_err(git_to_lane)?;
                    "advise".to_string()
                }
            },
        };

        Ok((
            Outcome::Ok,
            Some(VerbData::HookInstall {
                hooks_dir: hooks_dir.display().to_string(),
                hook_path: pre_commit.display().to_string(),
                mode,
                composed,
                replaced_own_block,
                warning: warning.clone(),
            }),
            None,
        ))
    })();
    emit(args.json, "hook.install", repo, None, result)
}

fn compose_refused_detail(detail: String) -> CommandError {
    compose_refused(format!(
        "lane markers are damaged ({detail}); repair or remove the block manually"
    ))
}

pub(crate) fn run_status_at(args: &HookStatusArgs, runner: &dyn GitRunner) -> i32 {
    let result = (|| -> Result<(Outcome, Option<VerbData>, Option<String>), CommandError> {
        let git = GitAdapter::new(runner);
        let top = resolve_toplevel(&git, args.git_repo.as_ref())?;
        let hooks_path_cfg = git
            .config_get(&top, "core.hooksPath")
            .map_err(git_to_lane)?;
        let managed = hooks_path_cfg.is_some();
        let manager = hooks_path_cfg
            .as_deref()
            .map(detect_manager)
            .map(String::from);
        let hooks_dir = git.hooks_dir(&top).map_err(git_to_lane)?;
        let pre_commit = hooks_dir.join("pre-commit");

        // Status REPORTS, never refuses: a symlink/dormant/damaged hook is a fact.
        let mut warning: Option<String> = None;
        let meta = std::fs::symlink_metadata(&pre_commit);
        let (installed, script_version, foreign_hook) = match meta {
            Err(e) if e.kind() == io::ErrorKind::NotFound => (false, None, false),
            Err(e) => return Err(LaneError::Io(e).into()),
            Ok(m) if m.file_type().is_symlink() => {
                warning = Some("pre-commit is a symlink; lane does not manage it".into());
                (false, None, true)
            }
            Ok(m) => {
                if m.is_file() && m.permissions().mode() & 0o111 == 0 {
                    warning =
                        Some("pre-commit exists but is not executable; git ignores it".into());
                }
                let bytes = std::fs::read(&pre_commit).map_err(LaneError::Io)?;
                match classify(Some(&bytes)) {
                    HookFileState::Absent => (false, None, false),
                    HookFileState::LaneOnly { version } => (true, Some(version), false),
                    HookFileState::Composed { version } => (true, Some(version), true),
                    HookFileState::ForeignOnly => (false, None, true),
                    HookFileState::Damaged(d) => {
                        warning = Some(format!("lane markers are damaged: {d}"));
                        (false, None, true)
                    }
                    HookFileState::NonComposable(d) => {
                        warning = Some(d);
                        (false, None, true)
                    }
                }
            }
        };
        let mode = git.config_get(&top, MODE_KEY).map_err(git_to_lane)?;
        Ok((
            Outcome::Ok,
            Some(VerbData::HookStatus {
                git_repo: top.display().to_string(),
                managed,
                manager,
                hooks_dir: hooks_dir.display().to_string(),
                installed,
                script_version,
                foreign_hook,
                mode,
                lane_on_path: lane_on_path(),
                warning: warning.clone(),
            }),
            None,
        ))
    })();
    emit(args.json, "hook.status", None, None, result)
}

pub(crate) fn run_uninstall_at(args: &HookUninstallArgs, runner: &dyn GitRunner) -> i32 {
    let result = (|| -> Result<(Outcome, Option<VerbData>, Option<String>), CommandError> {
        let git = GitAdapter::new(runner);
        let top = resolve_toplevel(&git, args.git_repo.as_ref())?;
        let hooks_dir = git.hooks_dir(&top).map_err(git_to_lane)?;
        let pre_commit = hooks_dir.join("pre-commit");

        let mut warning: Option<String> = None;
        if git
            .config_get(&top, "core.hooksPath")
            .map_err(git_to_lane)?
            .is_some()
        {
            warning = Some(
                "core.hooksPath is set: if a lane snippet was pasted into the managed hook, remove it manually".into(),
            );
        }

        let (removed_block, removed_file) = match std::fs::symlink_metadata(&pre_commit) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => (false, false),
            Err(e) => return Err(LaneError::Io(e).into()),
            Ok(m) if m.file_type().is_symlink() => (false, false),
            Ok(_) => {
                let bytes = std::fs::read(&pre_commit).map_err(LaneError::Io)?;
                match classify(Some(&bytes)) {
                    HookFileState::Absent | HookFileState::ForeignOnly => (false, false),
                    HookFileState::NonComposable(_) => (false, false),
                    HookFileState::LaneOnly { .. } | HookFileState::Composed { .. } => {
                        let text = String::from_utf8(bytes).expect("classified as UTF-8");
                        match remove_block(&text).map_err(compose_refused_detail)? {
                            None => {
                                std::fs::remove_file(&pre_commit).map_err(LaneError::Io)?;
                                (true, true)
                            }
                            Some(rest) => {
                                atomic_write_hook(&pre_commit, &rest).map_err(LaneError::Io)?;
                                (true, false)
                            }
                        }
                    }
                    HookFileState::Damaged(d) => {
                        return Err(compose_refused(format!(
                            "lane markers in {} are damaged ({d}); remove the block manually — refusing to guess",
                            pre_commit.display()
                        )));
                    }
                }
            }
        };
        git.config_unset(&top, MODE_KEY).map_err(git_to_lane)?;
        Ok((
            Outcome::Ok,
            Some(VerbData::HookUninstall {
                removed_block,
                removed_file,
                warning: warning.clone(),
            }),
            None,
        ))
    })();
    emit(args.json, "hook.uninstall", None, None, result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_shapes() {
        let script = render_script(None);
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(script.contains("# >>> lane hook v1 >>>"));
        assert!(script.contains(MARKER_CLOSE));
        assert!(script.contains(r#"lane check --path "$PWD""#));
        assert!(!script.contains("__LANE_REPO__"));
        assert!(!script.contains("--repo"));

        let snippet = render_block(Some("eleetai"));
        assert!(!snippet.starts_with("#!"));
        assert!(snippet.contains(r#"lane check --path "$PWD" --repo "eleetai""#));
        // One well-formed pair, and it classifies as ours alone.
        assert_eq!(
            classify(Some(render_script(None).as_bytes())),
            HookFileState::LaneOnly { version: 1 }
        );
    }

    #[test]
    fn classify_states() {
        assert_eq!(classify(None), HookFileState::Absent);
        assert_eq!(
            classify(Some(b"#!/bin/sh\nexec gitleaks\n")),
            HookFileState::ForeignOnly
        );
        let composed = format!("#!/bin/sh\necho foreign\n{}", render_block(None));
        assert_eq!(
            classify(Some(composed.as_bytes())),
            HookFileState::Composed { version: 1 }
        );
        let damaged = "#!/bin/sh\n# >>> lane hook v1 >>>\n";
        assert!(matches!(
            classify(Some(damaged.as_bytes())),
            HookFileState::Damaged(_)
        ));
        let dup = format!("{}{}", render_block(None), render_block(None));
        assert!(matches!(
            classify(Some(dup.as_bytes())),
            HookFileState::Damaged(_)
        ));
        assert!(matches!(
            classify(Some(&[0xff, 0xfe])),
            HookFileState::NonComposable(_)
        ));
        let big = vec![b'#'; (MAX_HOOK_BYTES + 1) as usize];
        assert!(matches!(
            classify(Some(&big)),
            HookFileState::NonComposable(_)
        ));
    }

    #[test]
    fn splice_replaces_only_our_block_and_is_idempotent() {
        let foreign_top = "#!/bin/sh\necho gitleaks-gate\n";
        let original = format!("{foreign_top}{}", render_block(None));
        let upgraded_block = render_block(Some("ops"));
        let once = splice(&original, &upgraded_block).unwrap();
        assert!(once.starts_with(foreign_top), "foreign prefix preserved");
        assert!(once.contains(r#"--repo "ops""#));
        // The OLD (bare, no --repo) check line must be fully replaced, not duplicated.
        assert!(
            !once.contains(r#"lane check --path "$PWD" >/dev/null; then"#),
            "stale bare check line survived the splice: {once}"
        );
        let twice = splice(&once, &upgraded_block).unwrap();
        assert_eq!(once, twice, "splice is idempotent");
        assert_eq!(
            classify(Some(once.as_bytes())),
            HookFileState::Composed { version: 1 }
        );
    }

    #[test]
    fn remove_block_returns_foreign_rest_or_none() {
        // Whole file ours → None (delete).
        assert_eq!(remove_block(&render_script(None)).unwrap(), None);
        // Composed → foreign remainder, byte-line preserved.
        let foreign = "#!/bin/sh\necho keep-me\n";
        let composed = format!("{foreign}{}", render_block(None));
        let rest = remove_block(&composed).unwrap().unwrap();
        assert_eq!(rest, foreign);
        // Damaged → error.
        assert!(remove_block("# >>> lane hook v1 >>>\n").is_err());
        // No markers → error.
        assert!(remove_block("#!/bin/sh\n").is_err());
    }

    #[test]
    fn parse_version_and_manager_detection() {
        assert_eq!(parse_version("# >>> lane hook v1 >>>"), Some(1));
        assert_eq!(parse_version("# >>> lane hook v12 >>>"), Some(12));
        assert_eq!(parse_version("# >>> lane hook vx >>>"), None);
        assert_eq!(detect_manager(".husky/_"), "husky");
        assert_eq!(detect_manager("lefthook/.checked"), "lefthook");
        assert_eq!(detect_manager("custom/hooks"), "a hooks manager");
    }

    #[test]
    fn managed_hookspath_refusal_performs_zero_writes() {
        use crate::git::GitOutput;
        struct Scripted;
        impl GitRunner for Scripted {
            fn run(&self, args: &[&str]) -> Result<GitOutput, crate::git::GitError> {
                let joined = args.join(" ");
                if joined.contains("--show-toplevel") {
                    return Ok(GitOutput {
                        code: Some(0),
                        stdout: "/no-such-repo-root\n".into(),
                        stderr: String::new(),
                    });
                }
                if joined.contains("config --get core.hooksPath") {
                    return Ok(GitOutput {
                        code: Some(0),
                        stdout: ".husky/_\n".into(),
                        stderr: String::new(),
                    });
                }
                panic!("unexpected git call after managed refusal: {joined}");
            }
        }
        let args = HookInstallArgs {
            git_repo: Some(PathBuf::from("/no-such-repo-root")),
            mode: None,
            repo: Some("eleetai".into()),
            json: true,
        };
        // Exit 1 refusal, and the scripted runner proves no config-set / no hooks-dir
        // resolution happened after the managed detection (it would panic).
        assert_eq!(run_install_at(&args, &Scripted), 1);
        assert!(!Path::new("/no-such-repo-root").exists());
    }
}
