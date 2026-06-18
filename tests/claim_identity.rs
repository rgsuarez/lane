//! Fix 3: a claim lock's `repo`/`lane` fields must match its directory and filename stem.

use chrono::{DateTime, Utc};
use lane::board::linear::NoLinearProvider;
use lane::board::liveness::StubLivenessProvider;
use lane::board::worktrees::EmptyWorktreeProvider;
use lane::board::{assemble, BoardInputs};
use lane::model::Board;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn write_lock(root: &Path, dir_repo: &str, file_stem: &str, rec_repo: &str, rec_lane: &str) {
    let locks = root.join(dir_repo).join("locks");
    fs::create_dir_all(&locks).unwrap();
    let body = format!(
        r#"{{"lane":"{rec_lane}","repo":"{rec_repo}","instance":"i","claimed_at":"2026-06-17T11:00:00Z","updated_at":"2026-06-17T11:00:00Z","expires_at":"2026-06-17T23:00:00Z","ttl_hours":12}}"#
    );
    fs::write(locks.join(format!("{file_stem}.lock")), body).unwrap();
}

fn assemble_root(root: &Path) -> anyhow::Result<Board> {
    let now: DateTime<Utc> = "2026-06-17T12:00:00Z".parse().unwrap();
    let wt = EmptyWorktreeProvider;
    let lin = NoLinearProvider;
    let live = StubLivenessProvider;
    let inputs = BoardInputs {
        lane_root: root,
        repo_filter: None,
        now,
        worktrees: &wt,
        linear: &lin,
        liveness: &live,
    };
    assemble(&inputs)
}

#[test]
fn repo_field_mismatch_is_rejected() {
    let dir = tempdir().unwrap();
    // Directory says ops-tech; record claims a different repo.
    write_lock(dir.path(), "ops-tech", "lqos-1", "wrong-repo", "lqos-1");
    assert!(assemble_root(dir.path()).is_err());
}

#[test]
fn lane_filename_mismatch_is_rejected() {
    let dir = tempdir().unwrap();
    // Filename stem is lqos-1; record claims a different lane.
    write_lock(dir.path(), "ops-tech", "lqos-1", "ops-tech", "lqos-2");
    assert!(assemble_root(dir.path()).is_err());
}

#[test]
fn matching_identity_is_accepted() {
    let dir = tempdir().unwrap();
    write_lock(dir.path(), "ops-tech", "lqos-1", "ops-tech", "lqos-1");
    let board = assemble_root(dir.path()).unwrap();
    assert_eq!(board.rows.len(), 1);
}
