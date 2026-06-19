//! Fixture test: one active claim yields one provenance-tagged row.

use chrono::{DateTime, Utc};
use lane::board::linear::NoLinearProvider;
use lane::board::liveness::StubLivenessProvider;
use lane::board::worktrees::EmptyWorktreeProvider;
use lane::board::{assemble, BoardInputs};
use lane::model::{ClaimStatus, Gate, Liveness, Provenance, Role, StaleState};
use std::fs;
use std::os::unix::fs::MetadataExt;
use tempfile::tempdir;

fn uid_of(p: &std::path::Path) -> u32 {
    std::fs::metadata(p).unwrap().uid()
}

fn lock_json(lane: &str, claimed: &str, updated: &str, expires: &str) -> String {
    format!(
        r#"{{
  "lane": "{lane}", "repo": "ops-tech", "instance": "2026-06-17-001-claude",
  "pid": 111, "target": "/tmp/ops-tech-{lane}", "target_normalized": "/tmp/ops-tech-{lane}",
  "note": "test", "claimed_at": "{claimed}", "updated_at": "{updated}", "expires_at": "{expires}",
  "ttl_hours": 12, "linear_key": "LQOS-148", "branch": "richie/{lane}-x", "role": "executor",
  "pr_url": null, "gate": "execute", "plan_path": null, "claim_status": "active",
  "session_ref": "claude-3:0.0"
}}"#
    )
}

#[test]
fn one_active_claim_yields_one_row() {
    let dir = tempdir().unwrap();
    let locks = dir.path().join("ops-tech").join("locks");
    fs::create_dir_all(&locks).unwrap();
    fs::write(
        locks.join("lqos-148.lock"),
        lock_json(
            "lqos-148",
            "2026-06-17T11:30:00Z",
            "2026-06-17T11:30:00Z",
            "2026-06-17T23:30:00Z",
        ),
    )
    .unwrap();

    let now: DateTime<Utc> = "2026-06-17T12:00:00Z".parse().unwrap();
    let wt = EmptyWorktreeProvider;
    let lin = NoLinearProvider;
    let live = StubLivenessProvider;
    let inputs = BoardInputs {
        lane_root: dir.path(),
        repo_filter: None,
        expected_uid: uid_of(dir.path()),
        now,
        worktrees: &wt,
        linear: &lin,
        liveness: &live,
    };

    let board = assemble(&inputs).unwrap();

    assert_eq!(board.rows.len(), 1);
    let row = &board.rows[0];
    assert_eq!(row.lane.value, "lqos-148");
    assert_eq!(row.lane.provenance, Provenance::Authoritative);
    assert_eq!(row.repo.value, "ops-tech");
    assert_eq!(row.instance.value, "2026-06-17-001-claude");

    let key = row.linear_key.as_ref().expect("linear_key present");
    assert_eq!(key.value, "LQOS-148");
    assert_eq!(key.provenance, Provenance::Authoritative);

    let role = row.role.as_ref().expect("role present");
    assert_eq!(role.value, Role::Executor);
    assert_eq!(role.provenance, Provenance::Authoritative);
    assert_eq!(row.gate.as_ref().unwrap().value, Gate::Execute);
    assert_eq!(
        row.claim_status.as_ref().unwrap().value,
        ClaimStatus::Active
    );
    assert_eq!(row.expires_at.provenance, Provenance::Authoritative);

    assert_eq!(row.stale_state.value, StaleState::Active);
    assert_eq!(row.stale_state.provenance, Provenance::Derived);
    assert_eq!(row.liveness.value, Liveness::Unknown);
    assert!(row.worktree.is_none());
    assert!(row.linear.is_none());
    assert_eq!(row.age_secs.value, 30 * 60);
    assert_eq!(row.age_secs.provenance, Provenance::Derived);
}
