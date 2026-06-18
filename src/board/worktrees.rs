//! Worktree provider. Slice 1 uses fixtures only — NO real `git worktree list` shell-out.

use anyhow::Context;
use chrono::{DateTime, Utc};
use std::path::Path;

use crate::model::{
    ClaimRecord, Provenance, Provenanced, SourceFreshness, SourceKind, WorktreeInfo,
};

/// Supplies worktree facts for a claim. Real (git) implementations land in a later slice.
pub trait WorktreeProvider {
    fn for_claim(&self, claim: &ClaimRecord) -> Option<Provenanced<WorktreeInfo>>;
    fn freshness(&self, now: DateTime<Utc>) -> SourceFreshness;
}

/// No worktree data (the default in Slice 1 when no fixture is supplied).
pub struct EmptyWorktreeProvider;

impl WorktreeProvider for EmptyWorktreeProvider {
    fn for_claim(&self, _claim: &ClaimRecord) -> Option<Provenanced<WorktreeInfo>> {
        None
    }
    fn freshness(&self, now: DateTime<Utc>) -> SourceFreshness {
        SourceFreshness {
            source: SourceKind::Worktrees,
            provenance: Provenance::Derived,
            ok: true,
            fetched_at: now,
            note: "no worktree provider (Slice 1; real `git worktree list` deferred)".to_string(),
        }
    }
}

/// Worktrees loaded from a fixture JSON array (offline).
pub struct FixtureWorktreeProvider {
    worktrees: Vec<WorktreeInfo>,
}

impl FixtureWorktreeProvider {
    pub fn new(worktrees: Vec<WorktreeInfo>) -> Self {
        Self { worktrees }
    }
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read worktree fixture {}", path.display()))?;
        let worktrees: Vec<WorktreeInfo> = serde_json::from_str(&text)
            .with_context(|| format!("parse worktree fixture {}", path.display()))?;
        Ok(Self { worktrees })
    }
}

impl WorktreeProvider for FixtureWorktreeProvider {
    fn for_claim(&self, claim: &ClaimRecord) -> Option<Provenanced<WorktreeInfo>> {
        self.worktrees
            .iter()
            .find(|w| {
                (claim.branch.is_some() && w.branch == claim.branch)
                    || (claim.target.is_some() && Some(&w.path) == claim.target.as_ref())
            })
            .cloned()
            .map(Provenanced::fixture)
    }
    fn freshness(&self, now: DateTime<Utc>) -> SourceFreshness {
        SourceFreshness {
            source: SourceKind::Worktrees,
            provenance: Provenance::Fixture,
            ok: true,
            fetched_at: now,
            note: format!("{} fixture worktree(s)", self.worktrees.len()),
        }
    }
}
