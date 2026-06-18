//! Command-line surface for Slice 1 (the `board` subcommand only).

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "lane",
    version,
    about = "Linear-first local agent-work orchestration (Slice 1: read-only board)"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Read-only, issue-keyed board of local lanes (offline; fixtures/stubs only).
    Board(BoardArgs),
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
