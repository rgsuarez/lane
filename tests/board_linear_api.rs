//! `lane board --linear api` end-to-end: opt-in law (default board stays inert with
//! `op` absent and no config), live enrichment by `linear_key` against a loopback
//! fixture, TTL cache reuse across runs, fail-soft degradation, and key-less inertia.

use std::fs;
use std::process::{Command, Output};

use serde_json::{json, Value};

mod common;

const KEY_ENV_VAR: &str = "LANE_TEST_BOARD_KEY";

fn issue_by_key_response(identifier: &str, title: &str) -> String {
    json!({
        "data": { "issue": {
            "identifier": identifier, "title": title,
            "url": format!("https://linear.app/x/issue/{identifier}"),
            "updatedAt": "2026-07-08T11:00:00Z",
            "state": { "name": "In Progress", "type": "started" },
            "assignee": { "displayName": "Richie Suarez" }
        } }
    })
    .to_string()
}

fn write_config(root: &std::path::Path, api_url: &str) {
    fs::write(
        root.join("config.toml"),
        format!(
            "[secrets.roles]\nlinear_api = \"env:{KEY_ENV_VAR}\"\n[linear]\napi_url = \"{api_url}\"\ncache_ttl_seconds = 300\n"
        ),
    )
    .expect("write config");
}

/// Claim a lane through the real binary, then patch `linear_key` into the lock JSON
/// (the established test idiom for record fields the claim verb doesn't set).
fn claim_with_key(root: &std::path::Path, repo: &str, lane: &str, key: Option<&str>) {
    let out = common::run(root, Some("board-test"), &["claim", lane, "--repo", repo]);
    assert_eq!(out.status.code(), Some(0), "claim failed");
    if let Some(k) = key {
        let lock = root.join(repo).join("locks").join(format!("{lane}.lock"));
        let text = fs::read_to_string(&lock).expect("lock exists");
        let mut record: Value = serde_json::from_str(&text).expect("lock parses");
        record["linear_key"] = json!(k);
        fs::write(&lock, record.to_string()).expect("patch lock");
    }
}

fn run_board(root: &std::path::Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(common::bin());
    cmd.arg("board")
        .args(args)
        .arg("--lane-root")
        .arg(root)
        .env_remove("LANE_ROOT")
        .env_remove("LANE_INSTANCE")
        .env_remove(KEY_ENV_VAR);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn lane board")
}

fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn linear_source<'a>(board: &'a Value) -> &'a Value {
    board["sources"]
        .as_array()
        .expect("sources")
        .iter()
        .find(|s| s["source"] == json!("linear"))
        .expect("linear source entry")
}

#[test]
fn default_and_off_stay_inert_without_op_or_config() {
    let root = common::temp_root();
    claim_with_key(root.path(), "ops", "demo-1", Some("ZER-1"));
    let empty_path = tempfile::tempdir().expect("empty dir");

    for extra in [&[][..], &["--linear", "off"][..]] {
        let mut args = vec!["--json"];
        args.extend_from_slice(extra);
        let out = run_board(
            root.path(),
            &args,
            &[("PATH", empty_path.path().to_str().unwrap())],
        );
        assert_eq!(
            out.status.code(),
            Some(0),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let board = stdout_json(&out);
        assert_eq!(board["rows"].as_array().unwrap().len(), 1);
        assert_eq!(board["rows"][0]["linear"], Value::Null);
        let src = linear_source(&board);
        assert_eq!(src["ok"], json!(true));
    }
}

#[test]
fn linear_api_enriches_by_key_then_serves_cache() {
    let root = common::temp_root();
    let (url, server) = common::serve_http(vec![(
        "200 OK",
        issue_by_key_response("ZER-85", "lane Slice 4"),
    )]);
    write_config(root.path(), &url);
    claim_with_key(root.path(), "ops", "zer-85", Some("ZER-85"));

    // Run 1: live fetch.
    let out = run_board(
        root.path(),
        &["--json", "--linear", "api"],
        &[(KEY_ENV_VAR, "board-key")],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let board = stdout_json(&out);
    let linear = &board["rows"][0]["linear"];
    assert_eq!(linear["value"]["key"], json!("ZER-85"));
    assert_eq!(linear["value"]["title"], json!("lane Slice 4"));
    assert_eq!(linear["value"]["state"], json!("In Progress"));
    assert_eq!(linear["value"]["assignee"], json!("Richie Suarez"));
    assert_eq!(linear["provenance"], json!("live"));
    let src = linear_source(&board);
    assert_eq!(src["ok"], json!(true));
    assert!(src["note"].as_str().unwrap().contains("1 fetched"));
    let _ = server.join();

    // Run 2: no listener alive — the TTL cache must serve, and the human table
    // must show the live STATE/TITLE columns with the [L] tag.
    let out = run_board(
        root.path(),
        &["--linear", "api"],
        &[(KEY_ENV_VAR, "board-key")],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("STATE"), "header: {text}");
    assert!(text.contains("TITLE"), "header: {text}");
    assert!(text.contains("In Progress[L]"), "live tag: {text}");
    assert!(text.contains("lane Slice 4"), "title: {text}");
    assert!(text.contains("0 fetched, 1 cached"), "sources: {text}");
}

#[test]
fn linear_api_degrades_soft_when_unreachable() {
    let root = common::temp_root();
    // https to a closed loopback port: passes URL policy, fails to connect.
    write_config(root.path(), "https://127.0.0.1:1/graphql");
    claim_with_key(root.path(), "ops", "demo-2", Some("ZER-2"));

    let out = run_board(
        root.path(),
        &["--json", "--linear", "api"],
        &[(KEY_ENV_VAR, "board-key")],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "board must render despite a degraded source; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let board = stdout_json(&out);
    assert_eq!(board["rows"].as_array().unwrap().len(), 1);
    assert_eq!(board["rows"][0]["linear"], Value::Null);
    let src = linear_source(&board);
    assert_eq!(src["ok"], json!(false));
    assert!(src["note"].as_str().unwrap().contains("unreachable"));
}

#[test]
fn linear_api_without_keys_never_initializes() {
    let root = common::temp_root();
    // No config, no key env var, `op` absent from PATH: any init attempt would
    // degrade the source. A key-less board must never attempt it.
    claim_with_key(root.path(), "ops", "demo-3", None);
    let empty_path = tempfile::tempdir().expect("empty dir");

    let out = run_board(
        root.path(),
        &["--json", "--linear", "api"],
        &[("PATH", empty_path.path().to_str().unwrap())],
    );
    assert_eq!(out.status.code(), Some(0));
    let board = stdout_json(&out);
    assert_eq!(board["rows"].as_array().unwrap().len(), 1);
    let src = linear_source(&board);
    assert_eq!(src["ok"], json!(true), "no init ⇒ never degraded");
    assert!(src["note"]
        .as_str()
        .unwrap()
        .contains("0 fetched, 0 cached"));
}
