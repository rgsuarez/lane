//! Command-line surface: the read-only `board` (Slice 1) plus the offline locking-core
//! verbs `claim | renew | release | status | list` (Slice 2).
//!
//! `--instance` (or `$LANE_INSTANCE`) is required for claim/renew/release and is never
//! guessed. `--force` exists ONLY on `claim` (the audited takeover); passing it to renew
//! or release is a Clap usage error (exit 2). `--lane-root` overrides `$LANE_ROOT`, else
//! `~/.lane`; absolute-only.

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "lane",
    version,
    about = "Linear-first local agent-work orchestration (offline lane locking core)"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Read-only, issue-keyed board of local lanes (offline; fixtures/stubs only).
    Board(BoardArgs),
    /// Claim a lane (exactly-one-winner; `--force` audited takeover).
    Claim(ClaimArgs),
    /// Renew an owned lease (owner-only; no `--force`).
    Renew(RenewArgs),
    /// Flip an owned claim to handoff status without releasing it (owner-only).
    Handoff(HandoffArgs),
    /// Release an owned lane (owner-only; no `--force`).
    Release(ReleaseArgs),
    /// Read-only status of one lane.
    Status(StatusArgs),
    /// Read-only listing of claims (all namespaces, or one `--repo`).
    List(ListArgs),
    /// Create a branch + git worktree and claim the lane with the worktree as its target
    /// (claim-first; no session is spawned).
    Start(StartArgs),
    /// Release an owned lane, optionally removing its git worktree first (never forced;
    /// a dirty worktree refuses and the claim stays held).
    Close(CloseArgs),
}

/// `lane claim <lane> --repo <repo> [--target <abs>] [--ttl-hours <h>] [--note <s>] [--force] [--json]`
#[derive(Args, Debug)]
pub struct ClaimArgs {
    /// Lane identifier (Linear key or slug); a single path component.
    pub lane: String,
    /// Repo namespace.
    #[arg(long)]
    pub repo: String,
    /// Absolute worktree/target path to reserve (overlap-checked).
    #[arg(long)]
    pub target: Option<String>,
    /// Lease length in hours (default 12, max 720).
    #[arg(long)]
    pub ttl_hours: Option<f64>,
    /// Free-text note (non-secret; excluded from the audit log).
    #[arg(long)]
    pub note: Option<String>,
    /// Take over an actively-held or malformed same-lane claim (audited; never bypasses
    /// the target-overlap scan).
    #[arg(long)]
    pub force: bool,
    /// Emit the JSON envelope instead of a human line.
    #[arg(long)]
    pub json: bool,
    /// Override `$LANE_ROOT` (else `~/.lane`). Absolute path.
    #[arg(long)]
    pub lane_root: Option<PathBuf>,
    /// Caller identity (required; or `$LANE_INSTANCE`). Never guessed.
    #[arg(long)]
    pub instance: Option<String>,
}

/// `lane renew <lane> --repo <repo> [--ttl-hours <h>] [--json]` (owner-only; no `--force`).
#[derive(Args, Debug)]
pub struct RenewArgs {
    pub lane: String,
    #[arg(long)]
    pub repo: String,
    #[arg(long)]
    pub ttl_hours: Option<f64>,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub lane_root: Option<PathBuf>,
    #[arg(long)]
    pub instance: Option<String>,
}

/// `lane handoff <lane> --repo <repo> [--note <digest>] [--json]` (owner-only). Flips
/// `claim_status -> handoff` and optionally replaces the note; the claim STAYS HELD
/// (TTL keeps ticking) so the target stays protected until a successor takes over.
#[derive(Args, Debug)]
pub struct HandoffArgs {
    pub lane: String,
    #[arg(long)]
    pub repo: String,
    /// Optional handoff digest replacing the claim note (non-secret).
    #[arg(long)]
    pub note: Option<String>,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub lane_root: Option<PathBuf>,
    #[arg(long)]
    pub instance: Option<String>,
}

/// `lane release <lane> --repo <repo> [--json]` (owner-only; no `--force`).
#[derive(Args, Debug)]
pub struct ReleaseArgs {
    pub lane: String,
    #[arg(long)]
    pub repo: String,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub lane_root: Option<PathBuf>,
    #[arg(long)]
    pub instance: Option<String>,
}

/// `lane status <lane> --repo <repo> [--json]` (read-only).
#[derive(Args, Debug)]
pub struct StatusArgs {
    pub lane: String,
    #[arg(long)]
    pub repo: String,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub lane_root: Option<PathBuf>,
}

/// `lane list [--repo <repo>] [--json]` (read-only; all namespaces if `--repo` omitted).
#[derive(Args, Debug)]
pub struct ListArgs {
    #[arg(long)]
    pub repo: Option<String>,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub lane_root: Option<PathBuf>,
}

/// `lane start <lane> --repo <ns> --git-repo <abs> [--branch] [--base] [--worktree]
/// [--linear-key] [--ttl-hours] [--note] [--json]`. Claim-first composition: read-only
/// git prechecks, then the claim (target = the worktree path), then branch + worktree
/// creation; a git failure cleans up (branch deleted, claim released) and reports.
#[derive(Args, Debug)]
pub struct StartArgs {
    /// Lane identifier (Linear key or slug); a single path component.
    pub lane: String,
    /// Repo namespace (the lane namespace, not the git repo).
    #[arg(long)]
    pub repo: String,
    /// The git repository to create the branch + worktree in. Absolute path.
    #[arg(long)]
    pub git_repo: PathBuf,
    /// Branch to create (default: the lowercased lane name).
    #[arg(long)]
    pub branch: Option<String>,
    /// Base ref for the new branch (default: HEAD of --git-repo).
    #[arg(long)]
    pub base: Option<String>,
    /// Worktree path to create (default: sibling `<git-repo>-<lowercased-lane>`).
    /// Absolute path.
    #[arg(long)]
    pub worktree: Option<PathBuf>,
    /// Linear issue key recorded on the claim (informational).
    #[arg(long)]
    pub linear_key: Option<String>,
    /// Lease length in hours (default 12, max 720).
    #[arg(long)]
    pub ttl_hours: Option<f64>,
    /// Free-text note (non-secret; excluded from the audit log).
    #[arg(long)]
    pub note: Option<String>,
    /// Emit the JSON envelope instead of a human line.
    #[arg(long)]
    pub json: bool,
    /// Override `$LANE_ROOT` (else `~/.lane`). Absolute path.
    #[arg(long)]
    pub lane_root: Option<PathBuf>,
    /// Caller identity (required; or `$LANE_INSTANCE`). Never guessed.
    #[arg(long)]
    pub instance: Option<String>,
}

/// `lane close <lane> --repo <ns> [--remove-worktree] [--json]` (owner-only). Without
/// `--remove-worktree`, close == release. With it: renew first (owner + expiry guard +
/// lease extension), probe, remove (dirty -> refuse, claim intact), then release.
#[derive(Args, Debug)]
pub struct CloseArgs {
    pub lane: String,
    #[arg(long)]
    pub repo: String,
    /// Also remove the claim's git worktree (never forced; dirty refuses).
    #[arg(long)]
    pub remove_worktree: bool,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub lane_root: Option<PathBuf>,
    #[arg(long)]
    pub instance: Option<String>,
}

#[derive(Args, Debug, Default)]
pub struct BoardArgs {
    /// Emit JSON instead of the human table.
    #[arg(long)]
    pub json: bool,
    /// Filter to a single repo namespace.
    #[arg(long)]
    pub repo: Option<String>,
    /// Override the lane root (else $LANE_ROOT, else ~/.lane). Absolute path.
    #[arg(long)]
    pub lane_root: Option<PathBuf>,
    /// Slice-1 fixture: Linear issues JSON file (offline; no network).
    #[arg(long)]
    pub linear_fixture: Option<PathBuf>,
    /// Slice-1 fixture: worktree list JSON file (offline; no `git` shell-out).
    #[arg(long)]
    pub worktree_fixture: Option<PathBuf>,
}

/// Resolve the lane root from the process environment: explicit flag, then
/// `$LANE_ROOT`, then `~/.lane`. Delegates to [`resolve_lane_root_from`], which
/// enforces that the resolved path is absolute.
pub fn resolve_lane_root(arg: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let env_val = std::env::var("LANE_ROOT").ok();
    let home = std::env::var("HOME").ok();
    resolve_lane_root_from(arg, env_val, home)
}

/// Pure resolver (testable without touching process env). The chosen path MUST be
/// absolute regardless of source — CLI flag, `$LANE_ROOT`, or the `HOME`-derived
/// default; relative paths are rejected with a clear error.
pub fn resolve_lane_root_from(
    arg: Option<PathBuf>,
    env_val: Option<String>,
    home: Option<String>,
) -> anyhow::Result<PathBuf> {
    if let Some(p) = arg {
        return require_absolute(p, "--lane-root");
    }
    if let Some(v) = env_val {
        if !v.trim().is_empty() {
            return require_absolute(PathBuf::from(v), "$LANE_ROOT");
        }
    }
    let home = home.context("HOME not set; pass --lane-root")?;
    require_absolute(PathBuf::from(home).join(".lane"), "HOME-derived lane root")
}

fn require_absolute(path: PathBuf, source: &str) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        path.is_absolute(),
        "{source} must be an absolute path, got {}",
        path.display()
    );
    Ok(path)
}
