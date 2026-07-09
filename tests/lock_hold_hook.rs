//! ZER-83 ambient-scrub pin for the test hold hook — deliberately a SINGLE test in its
//! own integration binary (own process), so the one `std::env::set_var` below has zero
//! race surface against any other test. THAT single-test-per-binary shape is the
//! load-bearing invariant: do not add a second `#[test]` to this file (parallel test
//! threads + global env mutation would race), and no cleanup of the variable is needed —
//! the process exits with the test.
//!
//! The hook (`src/lock/mod.rs::test_hold_after_lane_mutex`) is compiled unconditionally
//! and gated only on `$LANE_TEST_HOLD_FILE` — a behavior-widening branch on the claim
//! path that must stay clamped for every spawn except `spawn_holding`. This pins that a
//! normal `run()` claim stays inert even when the PARENT process carries the variable:
//! `run()` scrubs it from the child env. Deterministic in both directions with no timing
//! assertion — `run()` blocks until the child exits, so the marker check always runs
//! post-child; had the scrub regressed, the child would have created `<base>.held` (and
//! held its claim open) — a loud failure.

mod common;

use common::*;

#[test]
fn ambient_hold_var_never_gates_ordinary_spawns() {
    let root = temp_root();
    let sync = temp_root();
    let base = sync.path().join("ambient");

    // Simulate an operator/test-runner environment that carries the variable.
    // SAFETY-adjacent note (edition 2021, safe API): this test binary runs exactly one
    // test, so no concurrent thread reads or spawns while the global env mutates.
    std::env::set_var("LANE_TEST_HOLD_FILE", &base);

    let out = run(
        root.path(),
        Some("plain"),
        &["claim", "demo", "--repo", "ops", "--json"],
    );

    assert_eq!(code(&out), 0, "ordinary claim completes: {out:?}");
    // Exact marker path the hook would create: `<base>.held`.
    let held = sync.path().join("ambient.held");
    assert!(
        !held.exists(),
        "scrubbed child never engaged the hold hook (no held marker)"
    );
    assert_eq!(
        read_lock(root.path(), "ops", "demo").unwrap().instance,
        "plain"
    );
}
