//! Slice 2 — multi-PROCESS concurrency (real `lane` processes, not in-process threads
//! sharing state). Proves exactly-one-winner, overlap/expired-takeover races, mutex
//! contention, OS crash-release on SIGKILL, and non-interleaved audit appends.
//!
//! The hold-dependent tests synchronize on OBSERVABLE STATE via the ZER-83 handshake
//! (`spawn_holding` + `LANE_TEST_HOLD_FILE` markers), never on how fast a binary runs —
//! deterministic under any optimization level (`cargo test --release` is a stated gate).

mod common;

use common::*;

use std::fs::{File, TryLockError};
use std::path::Path;
use std::thread;
use std::time::Duration;

const N: usize = 8;

fn claim_codes<F>(root: &Path, make_args: F) -> Vec<i32>
where
    F: Fn(usize) -> Vec<String> + Sync,
{
    thread::scope(|s| {
        let handles: Vec<_> = (0..N)
            .map(|i| {
                let make_args = &make_args;
                s.spawn(move || {
                    let args = make_args(i);
                    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
                    code(&run(root, Some(&format!("inst-{i}")), &argv))
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

#[test]
fn exactly_one_winner_for_a_free_lane() {
    let root = temp_root();
    let codes = claim_codes(root.path(), |_| {
        vec![
            "claim".into(),
            "demo".into(),
            "--repo".into(),
            "ops".into(),
            "--json".into(),
        ]
    });
    let wins = codes.iter().filter(|&&c| c == 0).count();
    assert_eq!(wins, 1, "exactly one winner; codes={codes:?}");
    assert!(
        codes.iter().all(|&c| c == 0 || c == 1),
        "losers refuse (1); codes={codes:?}"
    );
}

#[test]
fn overlapping_target_race_has_one_winner() {
    let root = temp_root();
    let codes = claim_codes(root.path(), |i| {
        vec![
            "claim".into(),
            format!("lane-{i}"),
            "--repo".into(),
            "ops".into(),
            "--target".into(),
            "/tmp/lane-it-race-shared".into(),
            "--json".into(),
        ]
    });
    let wins = codes.iter().filter(|&&c| c == 0).count();
    assert_eq!(wins, 1, "one target winner; codes={codes:?}");
}

#[test]
fn distinct_targets_all_win() {
    let root = temp_root();
    let codes = claim_codes(root.path(), |i| {
        vec![
            "claim".into(),
            format!("lane-{i}"),
            "--repo".into(),
            "ops".into(),
            "--target".into(),
            format!("/tmp/lane-it-distinct-{i}"),
            "--json".into(),
        ]
    });
    assert_eq!(
        codes.iter().filter(|&&c| c == 0).count(),
        N,
        "all distinct; codes={codes:?}"
    );
}

#[test]
fn expired_takeover_race_has_one_winner() {
    let root = temp_root();
    let r = root.path();
    // Hand-place an EXPIRED lock so claimers take it over without --force.
    let locks = r.join("ops").join("locks");
    std::fs::create_dir_all(&locks).unwrap();
    let body = r#"{"schema_version":1,"lane":"exp","repo":"ops","instance":"old","claimed_at":"2000-01-01T00:00:00Z","updated_at":"2000-01-01T00:00:00Z","expires_at":"2000-01-01T12:00:00Z","ttl_hours":12}"#;
    std::fs::write(locks.join("exp.lock"), body).unwrap();

    let codes = claim_codes(r, |_| {
        vec![
            "claim".into(),
            "exp".into(),
            "--repo".into(),
            "ops".into(),
            "--json".into(),
        ]
    });
    let wins = codes.iter().filter(|&&c| c == 0).count();
    assert_eq!(wins, 1, "one expired-takeover winner; codes={codes:?}");
}

#[test]
fn audit_appends_never_interleave() {
    let root = temp_root();
    let r = root.path();
    let codes = claim_codes(r, |i| {
        vec![
            "claim".into(),
            format!("lane-{i}"),
            "--repo".into(),
            "ops".into(),
            "--json".into(),
        ]
    });
    assert_eq!(codes.iter().filter(|&&c| c == 0).count(), N);
    // read_audit asserts every line parses (no interleaving / partial lines).
    let events = read_audit(r, "ops");
    let claims = events
        .iter()
        .filter(|e| format!("{:?}", e.event) == "Claim")
        .count();
    assert_eq!(
        claims, N,
        "one claim event per winner; all lines well-formed"
    );
}

#[test]
fn mutex_contention_reports_busy() {
    let root = temp_root();
    let r = root.path();
    // The holder signals once it HOLDS the lane mutex, then holds until released — the
    // contender's refusal below cannot race the holder's completion.
    let mut holder = spawn_holding(r, "holder", &["claim", "busy", "--repo", "ops", "--json"]);
    assert!(
        holder.wait_held(Duration::from_secs(10)),
        "holder signaled it acquired the mutex (liveness bound, not a race window)"
    );
    assert_lane_mutex_held_now(r, "ops", "busy");

    // The contender burns its full bounded mutex wait (the holder cannot proceed until
    // we say so), then refuses deterministically.
    let out = run(
        r,
        Some("contender"),
        &["claim", "busy", "--repo", "ops", "--json"],
    );
    assert_eq!(code(&out), 1, "contender times out as busy");
    assert_eq!(stdout_json(&out)["reason"], "mutex_busy");

    holder.release();
    let status = holder.wait().unwrap();
    assert!(
        status.success(),
        "holder completes its claim after release: {status:?}"
    );
    assert_eq!(
        read_lock(r, "ops", "busy").unwrap().instance,
        "holder",
        "the hold hook never corrupts the claim it rides"
    );
}

#[test]
fn sigkill_releases_the_lane_mutex() {
    let root = temp_root();
    let r = root.path();
    // The holder holds the mutex until released — which never happens: we SIGKILL it
    // provably mid-hold (it cannot complete naturally before the kill).
    let mut holder = spawn_holding(r, "holder", &["claim", "crash", "--repo", "ops", "--json"]);
    assert!(
        holder.wait_held(Duration::from_secs(10)),
        "holder signaled it acquired the mutex (liveness bound, not a race window)"
    );
    assert_lane_mutex_held_now(r, "ops", "crash");

    holder.kill().expect("SIGKILL holder");
    holder.wait().unwrap();

    // The kernel released the advisory lock on fd close at process death; the holder
    // died before writing any lock record.
    let out = run(
        r,
        Some("fresh"),
        &["claim", "crash", "--repo", "ops", "--json"],
    );
    assert_eq!(
        code(&out),
        0,
        "fresh claim acquires the released mutex; no stale wedging"
    );
    assert_eq!(read_lock(r, "ops", "crash").unwrap().instance, "fresh");
}

/// One-shot observable-state assertion: the lane mutex is held RIGHT NOW (`try_lock`
/// returns `WouldBlock`). Deterministic after `wait_held`: the claim's RAII mutex guard
/// spans the hold hook, so between `.held` appearing and `.release` being created the
/// holder holds the flock continuously.
fn assert_lane_mutex_held_now(root: &Path, repo: &str, lane: &str) {
    let path = root
        .join(repo)
        .join("mutexes")
        .join(format!("{lane}.mutex"));
    let f = File::open(&path).expect("mutex file exists once held");
    match f.try_lock() {
        Err(TryLockError::WouldBlock) => {} // held by the holder — the invariant
        Ok(()) => panic!("lane mutex unexpectedly free while holder signals held"),
        Err(TryLockError::Error(e)) => panic!("mutex probe failed: {e}"),
    }
}
