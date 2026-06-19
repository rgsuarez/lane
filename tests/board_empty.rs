//! Fixture test: an empty lane root yields a board with no rows.

use chrono::Utc;
use lane::board::linear::NoLinearProvider;
use lane::board::liveness::StubLivenessProvider;
use lane::board::worktrees::EmptyWorktreeProvider;
use lane::board::{assemble, BoardInputs};
use lane::model::SourceKind;
use std::os::unix::fs::MetadataExt;
use tempfile::tempdir;

fn uid_of(p: &std::path::Path) -> u32 {
    std::fs::metadata(p).unwrap().uid()
}

#[test]
fn empty_lane_root_yields_no_rows() {
    let dir = tempdir().unwrap();
    let wt = EmptyWorktreeProvider;
    let lin = NoLinearProvider;
    let live = StubLivenessProvider;
    let inputs = BoardInputs {
        lane_root: dir.path(),
        repo_filter: None,
        expected_uid: uid_of(dir.path()),
        now: Utc::now(),
        worktrees: &wt,
        linear: &lin,
        liveness: &live,
    };

    let board = assemble(&inputs).unwrap();

    assert!(board.rows.is_empty());
    assert_eq!(board.schema_version, 0);
    // claims + worktrees + linear + liveness
    assert_eq!(board.sources.len(), 4);
    assert!(board.sources.iter().any(|s| s.source == SourceKind::Claims));

    let json = serde_json::to_string(&board).unwrap();
    assert!(json.contains("\"rows\""));
}
