//! Fix 4: ordering (missing Linear key sorts last), expired boundary (now >= expires_at),
//! and age clamped to zero when `claimed_at` is in the future.

use chrono::{DateTime, Utc};
use lane::board::linear::NoLinearProvider;
use lane::board::liveness::StubLivenessProvider;
use lane::board::worktrees::EmptyWorktreeProvider;
use lane::board::{assemble, BoardInputs};
use lane::model::{Board, StaleState};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn write_lock(root: &Path, lane: &str, linear_key: Option<&str>, claimed: &str, expires: &str) {
    let locks = root.join("ops-tech").join("locks");
    fs::create_dir_all(&locks).unwrap();
    let key_field = match linear_key {
        Some(k) => format!(r#","linear_key":"{k}""#),
        None => String::new(),
    };
    let body = format!(
        r#"{{"lane":"{lane}","repo":"ops-tech","instance":"i","claimed_at":"{claimed}","updated_at":"{claimed}","expires_at":"{expires}","ttl_hours":12{key_field}}}"#
    );
    fs::write(locks.join(format!("{lane}.lock")), body).unwrap();
}

fn board_at(root: &Path, now: &str) -> Board {
    let now: DateTime<Utc> = now.parse().unwrap();
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
    assemble(&inputs).unwrap()
}

#[test]
fn missing_linear_key_sorts_last() {
    let dir = tempdir().unwrap();
    // Keyed lane sorts alphabetically AFTER the keyless lane, but must still come first.
    write_lock(
        dir.path(),
        "zzz-keyed",
        Some("LQOS-9"),
        "2026-06-17T11:00:00Z",
        "2026-06-17T23:00:00Z",
    );
    write_lock(
        dir.path(),
        "aaa-nokey",
        None,
        "2026-06-17T11:00:00Z",
        "2026-06-17T23:00:00Z",
    );
    let board = board_at(dir.path(), "2026-06-17T12:00:00Z");
    assert_eq!(board.rows.len(), 2);
    assert_eq!(board.rows[0].lane.value, "zzz-keyed");
    assert!(board.rows[0].linear_key.is_some());
    assert_eq!(board.rows[1].lane.value, "aaa-nokey");
    assert!(board.rows[1].linear_key.is_none());
}

#[test]
fn expired_at_exact_boundary() {
    let dir = tempdir().unwrap();
    // expires_at == now must classify as Expired (now >= expires_at).
    write_lock(
        dir.path(),
        "boundary",
        None,
        "2026-06-17T06:00:00Z",
        "2026-06-17T12:00:00Z",
    );
    let board = board_at(dir.path(), "2026-06-17T12:00:00Z");
    assert_eq!(board.rows[0].stale_state.value, StaleState::Expired);
}

#[test]
fn future_claimed_at_clamps_age_to_zero() {
    let dir = tempdir().unwrap();
    // claimed_at after now -> derived age clamped to 0 (never negative).
    write_lock(
        dir.path(),
        "future",
        None,
        "2026-06-17T18:00:00Z",
        "2026-06-18T06:00:00Z",
    );
    let board = board_at(dir.path(), "2026-06-17T12:00:00Z");
    assert_eq!(board.rows[0].age_secs.value, 0);
}
