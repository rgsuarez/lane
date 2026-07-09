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
//! APPENDING a marked block (lane block LAST, preserving the repo owner's gate order —
//! but hooks only AND-compose when every earlier gate FALLS THROUGH: a success-path
//! early `exit 0` above the block makes it unreachable dead code (ZER-90), so install
//! and status WARN on that heuristically, and doctrine places PASTED blocks after the
//! secret scan and before any early-exit gate); re-install replaces exactly the marked
//! block (idempotent; also the version-upgrade path); a symlinked, non-executable,
//! non-UTF-8, oversize, or marker-damaged file is refused untouched. Every write is
//! temp-in-same-dir + chmod 0755 + atomic rename, so a racing `git commit` never execs
//! a half-written hook.

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
/// v1 → v2 (ZER-91): identity pre-check with distinct no-identity messaging, and
/// enforce-mode exit classes (1 = coverage violation, 2 = environment).
pub const HOOK_VERSION: u32 = 2;
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
/// `set -u`-safe (only `${LANE_HOOK_BYPASS:-}` / `${LANE_INSTANCE:-}` may be unset).
/// No `local` (not POSIX); `lane_`-prefixed names avoid host-variable collisions.
///
/// v2 (ZER-91): identity is pre-checked — the hook's only identity source is
/// `$LANE_INSTANCE` (it never passes `--instance`), so an unset/empty value is
/// diagnosed as IDENTITY PROPAGATION, never mislabeled "not covered". Enforce-mode
/// exit classes follow the host taxonomy (eleetai's documented contract):
/// 1 = genuine coverage violation (`lane check` exit 1: uncovered / foreign owner);
/// 2 = environment (lane not on PATH, no identity, `lane check` exit ≥ 2) — the
/// trailer `|| exit $?` propagates the class to the host hook. Advise mode never
/// blocks and never passes silently; the bypass is LOUD in both modes.
const BLOCK_TEMPLATE: &str = r#"# >>> lane hook v2 >>>
# lane pre-commit guard - managed by `lane hook`; do not edit inside the markers.
# Placement: keep this block AFTER any secret-scan gate but BEFORE any gate that
# can exit 0 early - an early exit above this block makes the guard unreachable.
# Mode: `git config lane.hook.mode` (advise | enforce; default advise).
# Enforce exits: 1 = commit not covered by a claim (violation); 2 = guard cannot
# run (lane not on PATH / no identity / integrity-io). Advise never blocks.
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
      return 2
    fi
    echo "lane-hook: WARNING: 'lane' not found on PATH; claim guard skipped (advise mode)" >&2
    return 0
  fi
  if [ -z "${LANE_INSTANCE:-}" ]; then
    if [ "$lane_mode" = "enforce" ]; then
      echo "lane-hook: BLOCKED: no caller identity (LANE_INSTANCE is not set); claim coverage cannot be verified." >&2
      echo "lane-hook: if you hold a covering claim, identity did not reach this shell; fix: export LANE_INSTANCE=<your-instance>" >&2
      echo "lane-hook: bypass once: LANE_HOOK_BYPASS=1 git commit ..." >&2
      return 2
    fi
    echo "lane-hook: WARNING: no caller identity (LANE_INSTANCE unset); claim coverage not verified (advise mode; commit allowed)." >&2
    echo "lane-hook: if you hold a covering claim, identity did not reach this shell; fix: export LANE_INSTANCE=<your-instance>" >&2
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
      echo "lane-hook: bypass once: LANE_HOOK_BYPASS=1 git commit ..." >&2
      return 1
    fi
    echo "lane-hook: BLOCKED: lane check failed (exit $lane_rc, integrity/io) under enforce mode." >&2
    echo "lane-hook: bypass once: LANE_HOOK_BYPASS=1 git commit ..." >&2
    return 2
  fi
  if [ "$lane_rc" -eq 1 ]; then
    echo "lane-hook: WARNING: commit path not covered by an active lane claim (advise mode; commit allowed)." >&2
  else
    echo "lane-hook: WARNING: lane check failed (exit $lane_rc, integrity/io); commit allowed (advise mode)." >&2
  fi
  return 0
}
lane_hook_guard || exit $?
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

/// Warning shared by install/status when a gate above the lane block can succeed-exit
/// before the block runs (ZER-90: an unreachable guard is worse than none — status
/// reports "installed" while nothing enforces).
const UNREACHABLE_WARNING: &str = "a gate above the lane block has a success-path early exit (`exit 0`), so the lane block may be UNREACHABLE; move the block above that gate (keep it after any secret scan) and verify with an uncovered test commit (expect a lane-hook line)";

/// Naive success-path early-exit detector (ZER-90): flags a bare `exit` line, and any
/// `exit 0` command fragment — including the idiomatic one-liners `[ … ] && exit 0`,
/// `cmd || exit 0`, and `cmd; exit 0` (fragments split on `&`/`|`/`;`), plus trailing
/// comments (eleetai's `exit 0  # Nothing relevant staged.`). Failure exits (`exit 1`,
/// `cmd || exit`) are NOT flagged — a failing gate refuses the commit anyway, so a
/// block below it is morally moot. Line-lexical by design (heredoc/string content and
/// conditional exits are indistinguishable without parsing sh): this feeds a WARNING,
/// never a refusal — the acceptance smoke test is authoritative.
fn has_early_success_exit(lines: &[&str]) -> bool {
    fn exit0_fragment(frag: &str) -> bool {
        let t = frag.trim();
        t == "exit 0"
            || t.strip_prefix("exit 0")
                .is_some_and(|rest| rest.starts_with([' ', '\t', ';']))
    }
    lines.iter().any(|l| {
        let t = l.trim();
        if t.starts_with('#') {
            return false; // whole-line comment (fragments of it would false-positive)
        }
        t == "exit" || t.split(['&', '|', ';']).any(exit0_fragment)
    })
}

/// The single ZER-90 unreachability scan shared by install and status (one detector, so
/// the two surfaces can never drift): success-path early exits count only ABOVE the
/// lane block — or anywhere when no block exists yet, since an append lands below every
/// line. Damaged markers return `None` (the caller surfaces damage on its own path).
fn unreachable_warning(text: &str) -> Option<&'static str> {
    let lines: Vec<&str> = text.lines().collect();
    let scan = match find_block(&lines) {
        Ok(Some((open, _, _))) => &lines[..open],
        Ok(None) => &lines[..],
        Err(_) => return None,
    };
    has_early_success_exit(scan).then_some(UNREACHABLE_WARNING)
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
                "core.hooksPath = '{hooks_path}' is set — {manager} manages hooks in {top}; lane will not write into a managed hooks dir.\nPaste this block into the managed pre-commit hook via your normal PR flow, placing it immediately AFTER your secret-scan gate (first, if none) and BEFORE any gate that can exit early — an early `exit 0` above the block makes it unreachable dead code.\nThen verify: `git commit --allow-empty -m smoke` with LANE_INSTANCE unset must print a lane-hook line (drop the smoke commit afterwards):\n\n{}",
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
                // ZER-90: a re-install (upgrade) is the moment an operator would
                // re-place a dead block — warn if a gate above it can exit 0 early.
                warning = unreachable_warning(&spliced).map(String::from);
                (spliced, composed, true)
            }
            HookFileState::ForeignOnly => {
                let text = String::from_utf8(existing.unwrap()).expect("classified as UTF-8");
                let mut notes: Vec<&str> = Vec::new();
                if text.lines().any(|l| l.trim_start().starts_with("exec ")) {
                    notes.push("existing hook appears to exec another program; the appended lane block may be unreachable — verify with a test commit");
                }
                // ZER-90: the appended block lands BELOW any existing early exit
                // (no markers yet, so the shared scan covers the whole foreign body).
                if let Some(w) = unreachable_warning(&text) {
                    notes.push(w);
                }
                if !notes.is_empty() {
                    warning = Some(notes.join("; also: "));
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
                    HookFileState::Composed { version } => {
                        // ZER-90: report a possibly-unreachable block (an existing
                        // symlink/dormant warning outranks this heuristic one).
                        if warning.is_none() {
                            // Safe: classify only returns Composed for valid UTF-8.
                            let text = std::str::from_utf8(&bytes).expect("classified as UTF-8");
                            warning = unreachable_warning(text).map(String::from);
                        }
                        (true, Some(version), true)
                    }
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
        assert!(script.contains("# >>> lane hook v2 >>>"));
        assert!(script.contains(MARKER_CLOSE));
        assert!(script.contains(r#"lane check --path "$PWD""#));
        assert!(!script.contains("__LANE_REPO__"));
        assert!(!script.contains("--repo"));
        // The block carries its own placement law (ZER-90) and never the dead guidance.
        assert!(script.contains("BEFORE any gate"));
        assert!(!script.contains("at the END"));

        let snippet = render_block(Some("eleetai"));
        assert!(!snippet.starts_with("#!"));
        assert!(snippet.contains(r#"lane check --path "$PWD" --repo "eleetai""#));
        // One well-formed pair, and it classifies as ours alone.
        assert_eq!(
            classify(Some(render_script(None).as_bytes())),
            HookFileState::LaneOnly { version: 2 }
        );
    }

    #[test]
    fn template_marker_matches_hook_version() {
        // The marker literal and the const must never drift (status/upgrade parse it).
        assert!(BLOCK_TEMPLATE.starts_with(&format!(
            "{MARKER_OPEN_PREFIX}{HOOK_VERSION}{MARKER_OPEN_SUFFIX}\n"
        )));
    }

    #[test]
    fn early_success_exit_detector() {
        assert!(has_early_success_exit(&["exit 0"]));
        assert!(has_early_success_exit(&[
            "  exit 0  # Nothing relevant staged."
        ]));
        assert!(has_early_success_exit(&["exit 0;"]));
        assert!(has_early_success_exit(&["exit"]));
        // Idiomatic one-liner short-circuits are the common real-world form.
        assert!(has_early_success_exit(&[
            r#"[ -z "$(git diff --cached --name-only)" ] && exit 0"#
        ]));
        assert!(has_early_success_exit(&[
            "optional_gate || exit 0  # tolerated"
        ]));
        assert!(has_early_success_exit(&["cleanup; exit 0"]));
        assert!(!has_early_success_exit(&["exit 1"]));
        assert!(!has_early_success_exit(&["run_check || exit"]));
        assert!(!has_early_success_exit(&["# exit 0"]));
        assert!(!has_early_success_exit(&[
            "# a comment about cmd && exit 0"
        ]));
        assert!(!has_early_success_exit(&["exit 0all"]));
        assert!(!has_early_success_exit(&["cmd >/dev/null 2>&1"]));
        assert!(!has_early_success_exit(&["lane_hook_guard || exit $?"]));
        assert!(!has_early_success_exit(&[]));
        // The v2 block itself must never trip the detector (status scans above it only,
        // but install scans a whole foreign body that could one day contain our text).
        let block = render_block(None);
        assert!(!has_early_success_exit(&block.lines().collect::<Vec<_>>()));
    }

    #[test]
    fn unreachable_warning_scans_above_block_or_whole_foreign_body() {
        // Early exit ABOVE the block → warns.
        let dead = format!("#!/bin/sh\nfoo && exit 0\n{}", render_block(None));
        assert_eq!(unreachable_warning(&dead), Some(UNREACHABLE_WARNING));
        // Early exit only INSIDE/BELOW the block region → clean (the block's own
        // trailer and anything after it do not gate the block).
        let live = format!("#!/bin/sh\necho scan\n{}", render_block(None));
        assert_eq!(unreachable_warning(&live), None);
        // No block yet (ForeignOnly): the whole body is scanned — an append would
        // land below the exit.
        assert_eq!(
            unreachable_warning("#!/bin/sh\nexit 0\n"),
            Some(UNREACHABLE_WARNING)
        );
        assert_eq!(unreachable_warning("#!/bin/sh\necho ok\n"), None);
        // Damaged markers: the caller's damage path owns the message.
        assert_eq!(
            unreachable_warning("# >>> lane hook v1 >>>\nexit 0\n"),
            None
        );
    }

    #[test]
    fn v1_block_upgrades_to_v2_via_splice() {
        let foreign = "#!/bin/sh\necho secret-scan >&2\n";
        let v1 = "# >>> lane hook v1 >>>\nold_guard() { return 0; }\nold_guard || exit 1\n# <<< lane hook <<<\n";
        let file = format!("{foreign}{v1}");
        assert_eq!(
            classify(Some(file.as_bytes())),
            HookFileState::Composed { version: 1 },
            "v1 detection keeps working"
        );
        let out = splice(&file, &render_block(None)).unwrap();
        assert!(out.starts_with(foreign), "foreign bytes preserved");
        assert!(out.contains("# >>> lane hook v2 >>>"));
        assert!(!out.contains("# >>> lane hook v1 >>>"));
        assert!(!out.contains("old_guard"), "old body fully replaced");
        assert_eq!(
            classify(Some(out.as_bytes())),
            HookFileState::Composed { version: 2 }
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
            HookFileState::Composed { version: 2 }
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
            HookFileState::Composed { version: 2 }
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
        assert_eq!(parse_version("# >>> lane hook v2 >>>"), Some(2));
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
