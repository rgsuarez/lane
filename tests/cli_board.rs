//! End-to-end CLI test: `lane board` against the offline fixture lane root.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lane_root")
}

#[test]
fn board_json_against_fixture_root_succeeds() {
    let mut cmd = Command::cargo_bin("lane").unwrap();
    cmd.arg("board")
        .arg("--json")
        .arg("--lane-root")
        .arg(fixture_root());
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("\"schema_version\""))
        .stdout(predicate::str::contains("lqos-148"))
        .stdout(predicate::str::contains("LQOS-148"));
}

#[test]
fn board_human_against_fixture_root_succeeds() {
    let mut cmd = Command::cargo_bin("lane").unwrap();
    cmd.arg("board").arg("--lane-root").arg(fixture_root());
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("LANE BOARD"))
        .stdout(predicate::str::contains("lqos-148[A]"))
        .stdout(predicate::str::contains("ops-tech[A]"))
        .stdout(predicate::str::contains("execute[A]"));
}
