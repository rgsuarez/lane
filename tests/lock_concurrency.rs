//! Slice 2 — multi-PROCESS concurrency (real `lane` processes, not in-process threads
//! sharing state). Proves exactly-one-winner, overlap/expired-takeover races, mutex
//! contention, OS crash-release on SIGKILL, and non-interleaved audit appends.

mod common;

use common::*;

use std::fs::{File, TryLockError};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

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
    // Holder keeps the lane mutex 4s (> the ~3s contender window).
    let mut holder = spawn_holding(
        r,
        "holder",
        4000,
        &["claim", "busy", "--repo", "ops", "--json"],
    );
    assert!(
        wait_until_mutex_held(r, "ops", "busy", Duration::from_secs(3)),
        "holder acquired the mutex"
    );

    let out = run(
        r,
        Some("contender"),
        &["claim", "busy", "--repo", "ops", "--json"],
    );
    assert_eq!(code(&out), 1, "contender times out as busy");
    assert_eq!(stdout_json(&out)["reason"], "mutex_busy");

    holder.wait().unwrap();
}

#[test]
fn sigkill_releases_the_lane_mutex() {
    let root = temp_root();
    let r = root.path();
    // Holder would keep the mutex 8s; we SIGKILL it mid-hold.
    let mut holder = spawn_holding(
        r,
        "holder",
        8000,
        &["claim", "crash", "--repo", "ops", "--json"],
    );
    assert!(
        wait_until_mutex_held(r, "ops", "crash", Duration::from_secs(4)),
        "holder acquired the mutex"
    );

    holder.kill().expect("SIGKILL holder");
    holder.wait().unwrap();

    // The kernel released the advisory lock on fd close at process death.
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

/// Poll the lane mutex file until a `try_lock` returns `WouldBlock` (the holder holds it).
fn wait_until_mutex_held(root: &Path, repo: &str, lane: &str, timeout: Duration) -> bool {
    let path = root
        .join(repo)
        .join("mutexes")
        .join(format!("{lane}.mutex"));
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(f) = File::open(&path) {
            match f.try_lock() {
                Err(TryLockError::WouldBlock) => return true, // held by the holder
                Ok(()) => {
                    let _ = f.unlock(); // not held yet — release and keep polling
                }
                Err(TryLockError::Error(_)) => {}
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}
