//! Worktree providers. Slice 1 shipped fixtures/empty; Slice 3 adds the OPT-IN live git
//! probe (`lane board --worktrees git`). The default stays offline: `board` is in the
//! MUST-work-offline verb set, so worktree enrichment never becomes a dependency.

use anyhow::Context;
use chrono::{DateTime, Utc};
use std::cell::{Cell, RefCell};
use std::path::Path;

use crate::git::{GitAdapter, GitRunner};
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
            note: "worktrees off (the offline default; opt in with --worktrees git)".to_string(),
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

/// The Slice-3 LIVE provider (`--worktrees git`): probes each TARGETED claim's stored
/// path with stable git plumbing. Guarantees: `target=None` claims spawn NOTHING; every
/// spawn runs under the adapter's bounded wait; any git absence, probe failure, timeout,
/// or ambiguous case match degrades the source to `ok:false` while the board still
/// renders (fail-soft, never fail-hard). Default derivations store a lowercased leaf so
/// the stored target probes directly; the parent-readdir fallback recovers the real-case
/// directory for operator-overridden uppercase paths, and NEVER guesses on ambiguity.
pub struct GitWorktreeProvider<'a> {
    git: GitAdapter<'a>,
    /// Lazily-checked `git --version` result (an upfront availability gate on first use).
    available: Cell<Option<bool>>,
    /// Set on any probe failure/timeout/ambiguity: freshness reports `ok:false`.
    degraded: Cell<bool>,
    notes: RefCell<Vec<String>>,
    probes: Cell<u32>,
}

impl<'a> GitWorktreeProvider<'a> {
    pub fn new(runner: &'a dyn GitRunner) -> Self {
        Self {
            git: GitAdapter::new(runner),
            available: Cell::new(None),
            degraded: Cell::new(false),
            notes: RefCell::new(Vec::new()),
            probes: Cell::new(0),
        }
    }

    fn note(&self, s: String) {
        let mut notes = self.notes.borrow_mut();
        if notes.len() < 8 {
            notes.push(s);
        }
    }

    fn git_available(&self) -> bool {
        if let Some(v) = self.available.get() {
            return v;
        }
        let ok = self.git.version_ok();
        if !ok {
            self.degraded.set(true);
            self.note("git unavailable".to_string());
        }
        self.available.set(Some(ok));
        ok
    }

    fn probe(&self, path: &Path) -> Option<WorktreeInfo> {
        self.probes.set(self.probes.get() + 1);
        match self.git.probe_worktree(path) {
            Ok(Some(wt)) => Some(WorktreeInfo {
                path: wt.path.to_string_lossy().to_string(),
                branch: wt.branch,
                head: wt.head,
            }),
            Ok(None) => None,
            Err(e) => {
                self.degraded.set(true);
                self.note(format!("probe failed for {}: {e}", path.display()));
                None
            }
        }
    }
}

impl WorktreeProvider for GitWorktreeProvider<'_> {
    fn for_claim(&self, claim: &ClaimRecord) -> Option<Provenanced<WorktreeInfo>> {
        // ZERO spawns for target-less (coordination) claims — most claims.
        let stored = claim
            .target_normalized
            .as_deref()
            .or(claim.target.as_deref())?;
        if !self.git_available() {
            return None;
        }
        let stored_path = Path::new(stored);

        // Direct probe: default derivations store a lowercased leaf, so stored == on-disk.
        if let Some(info) = self.probe(stored_path) {
            return Some(Provenanced::derived(info));
        }
        if self.degraded.get() {
            // The direct probe errored (not merely missed); do not double-probe.
            return None;
        }

        // Fallback for operator-overridden real-case paths: readdir the existing parent
        // and case-insensitively match the folded tail. Never guess on ambiguity.
        let parent = stored_path.parent()?;
        let tail = stored_path.file_name()?.to_string_lossy().to_string();
        let entries: Vec<String> = match std::fs::read_dir(parent) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect(),
            Err(_) => return None, // parent gone: the worktree genuinely is not there
        };
        match resolve_real_case(&entries, &tail) {
            CaseMatch::One(real) => {
                // Skip a re-probe of the identical path (the direct probe already missed).
                if real == tail {
                    return None;
                }
                let real_path = parent.join(real);
                self.probe(&real_path).map(Provenanced::derived)
            }
            CaseMatch::Absent => None,
            CaseMatch::Ambiguous => {
                self.degraded.set(true);
                self.note(format!(
                    "ambiguous case-insensitive matches for {} (skipped, never guessed)",
                    stored_path.display()
                ));
                None
            }
        }
    }

    fn freshness(&self, now: DateTime<Utc>) -> SourceFreshness {
        // Called AFTER the per-claim probes (the assemble order guarantees it), so a
        // lazily-discovered failure is reflected here.
        let ok = !self.degraded.get();
        let notes = self.notes.borrow();
        let note = if notes.is_empty() {
            format!("live git probe; {} probe(s)", self.probes.get())
        } else {
            format!(
                "live git probe; {} probe(s); {}",
                self.probes.get(),
                notes.join("; ")
            )
        };
        SourceFreshness {
            source: SourceKind::Worktrees,
            provenance: Provenance::Derived,
            ok,
            fetched_at: now,
            note,
        }
    }
}
