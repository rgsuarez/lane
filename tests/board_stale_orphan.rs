//! Fixture test: stale/orphan classification, using an injected fake liveness provider
//! (no real tmux/overseer) and a fixed `now`.

use chrono::{DateTime, Utc};
use lane::board::linear::NoLinearProvider;
use lane::board::liveness::LivenessProvider;
use lane::board::worktrees::EmptyWorktreeProvider;
use lane::board::{assemble, BoardInputs};
use lane::model::{
    ClaimRecord, Liveness, Provenance, Provenanced, SourceFreshness, SourceKind, StaleState,
};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

/// Returns `NotLive` for the lane named `orphan`, `Unknown` otherwise.
struct FakeLiveness;

impl LivenessProvider for FakeLiveness {
    fn liveness_for(&self, claim: &ClaimRecord) -> Provenanced<Liveness> {
        if claim.lane == "orphan" {
            Provenanced::fixture(Liveness::NotLive)
        } else {
            Provenanced::unknown(Liveness::Unknown)
        }
    }
    fn freshness(&self, now: DateTime<Utc>) -> SourceFreshness {
        SourceFreshness {
            source: SourceKind::Liveness,
            provenance: Provenance::Fixture,
            ok: true,
            fetched_at: now,
            note: "fake liveness".to_string(),
        }
    }
}

fn write_lock(root: &Path, lane: &str, updated: &str, expires: &str) {
    let locks = root.join("ops-tech").join("locks");
    fs::create_dir_all(&locks).unwrap();
    let body = format!(
        r#"{{"lane":"{lane}","repo":"ops-tech","instance":"i","claimed_at":"2026-06-17T06:00:00Z","updated_at":"{updated}","expires_at":"{expires}","ttl_hours":12}}"#
    );
    fs::write(locks.join(format!("{lane}.lock")), body).unwrap();
}

#[test]
fn classifies_expired_orphan_and_stale() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    // now = 2026-06-17T12:00:00Z
    write_lock(
        root,
        "expired",
        "2026-06-17T11:00:00Z",
        "2026-06-17T11:00:00Z",
    ); // expires past -> Expired
    write_lock(
        root,
        "orphan",
        "2026-06-17T11:55:00Z",
        "2026-06-17T23:00:00Z",
    ); // active + NotLive -> Orphaned
    write_lock(
        root,
        "stale",
        "2026-06-17T07:00:00Z",
        "2026-06-17T23:00:00Z",
    ); // idle 5h, Unknown -> PossiblyStale

    let now: DateTime<Utc> = "2026-06-17T12:00:00Z".parse().unwrap();
    let wt = EmptyWorktreeProvider;
    let lin = NoLinearProvider;
    let live = FakeLiveness;
    let inputs = BoardInputs {
        lane_root: root,
        repo_filter: None,
        now,
        worktrees: &wt,
        linear: &lin,
        liveness: &live,
    };

    let board = assemble(&inputs).unwrap();
    let stale_of = |lane: &str| {
        board
            .rows
            .iter()
            .find(|r| r.lane.value == lane)
            .unwrap()
            .stale_state
            .value
    };

    assert_eq!(stale_of("expired"), StaleState::Expired);
    assert_eq!(stale_of("orphan"), StaleState::Orphaned);
    assert_eq!(stale_of("stale"), StaleState::PossiblyStale);
}
