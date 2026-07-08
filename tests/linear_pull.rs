//! `lane pull` end-to-end: the spawned binary against a loopback GraphQL fixture,
//! secret via the spec-§7 `env:` pointer (no `op` needed), config + cache in a temp
//! `$LANE_ROOT`. Proves the envelope shape, cache semantics (fresh serve with zero
//! network + zero secret, TTL expiry, corrupt ⇒ refetch, `--refresh`), fail-closed
//! secret errors, and that the key never lands on stdout.

use std::fs;
use std::process::{Command, Output};

use serde_json::{json, Value};

mod common;

const KEY_SENTINEL: &str = "lin_api_pull-key-sentinel";
const KEY_ENV_VAR: &str = "LANE_TEST_PULL_KEY";

fn viewer_response(identifier: &str, title: &str) -> String {
    json!({
        "data": { "viewer": { "assignedIssues": { "nodes": [
            { "identifier": identifier, "title": title,
              "url": format!("https://linear.app/x/issue/{identifier}"),
              "updatedAt": "2026-07-08T11:00:00Z",
              "state": { "name": "In Progress", "type": "started" } }
        ] } } }
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

/// Spawn `lane pull` with a controlled environment (key env var set; hermetic root).
fn run_pull(root: &std::path::Path, args: &[&str], key: Option<&str>) -> Output {
    let mut cmd = Command::new(common::bin());
    cmd.arg("pull")
        .args(args)
        .arg("--lane-root")
        .arg(root)
        .env_remove("LANE_ROOT")
        .env_remove("LANE_INSTANCE")
        .env_remove(KEY_ENV_VAR);
    if let Some(k) = key {
        cmd.env(KEY_ENV_VAR, k);
    }
    cmd.output().expect("spawn lane pull")
}

fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

#[test]
fn pull_live_fetch_envelope_and_wire_facts() {
    let root = common::temp_root();
    let (url, server) =
        common::serve_http(vec![("200 OK", viewer_response("ZER-85", "lane Slice 4"))]);
    write_config(root.path(), &url);

    let out = run_pull(root.path(), &["--json"], Some(KEY_SENTINEL));
    assert_eq!(out.status.code(), Some(0));
    let env = stdout_json(&out);
    assert_eq!(env["schema_version"], json!(1));
    assert_eq!(env["ok"], json!(true));
    assert_eq!(env["verb"], json!("pull"));
    assert_eq!(env["outcome"], json!("ok"));
    assert_eq!(env["data"]["source"], json!("api"));
    assert_eq!(env["data"]["issues"][0]["identifier"], json!("ZER-85"));
    assert_eq!(env["data"]["issues"][0]["state_type"], json!("started"));

    // The key rode the Authorization header — and nowhere else.
    let raw = server.join().expect("server").remove(0);
    assert!(raw
        .to_lowercase()
        .contains(&format!("authorization: {}", KEY_SENTINEL.to_lowercase())));
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains(KEY_SENTINEL),
        "key leaked to stdout"
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains(KEY_SENTINEL),
        "key leaked to stderr"
    );

    // The root adapter audit recorded the request — role key only.
    let audit = fs::read_to_string(root.path().join(".adapter-audit.log")).expect("root audit");
    assert!(audit.contains("\"event\":\"secret_requested\""));
    assert!(audit.contains("\"secret_role\":\"linear_api\""));
    assert!(!audit.contains(KEY_SENTINEL));

    // The cache was written.
    let cache = fs::read_to_string(root.path().join(".cache/linear/viewer-issues.json"))
        .expect("cache written");
    assert!(cache.contains("ZER-85"));
    assert!(!cache.contains(KEY_SENTINEL), "key leaked into the cache");
}

#[test]
fn pull_serves_fresh_cache_with_no_secret_and_no_network() {
    let root = common::temp_root();
    // Unroutable-by-policy api_url + NO key env var: any live path would fail loudly.
    write_config(root.path(), "http://127.0.0.1:9/graphql");
    let cache_dir = root.path().join(".cache/linear");
    fs::create_dir_all(&cache_dir).unwrap();
    let envelope = json!({
        "fetched_at": chrono::Utc::now().to_rfc3339(),
        "payload": { "limit": 50, "issues": [
            { "identifier": "ZER-1", "title": "cached", "state": "Backlog",
              "state_type": "backlog", "url": "https://x", "updated_at": "2026-07-08T00:00:00Z" }
        ] }
    });
    fs::write(cache_dir.join("viewer-issues.json"), envelope.to_string()).unwrap();

    let out = run_pull(root.path(), &["--json"], None);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = stdout_json(&out);
    assert_eq!(env["data"]["source"], json!("cache"));
    assert_eq!(env["data"]["issues"][0]["identifier"], json!("ZER-1"));
}

#[test]
fn pull_refresh_bypasses_cache_and_rewrites_it() {
    let root = common::temp_root();
    let (url, server) = common::serve_http(vec![("200 OK", viewer_response("ZER-9", "fresh"))]);
    write_config(root.path(), &url);
    let cache_dir = root.path().join(".cache/linear");
    fs::create_dir_all(&cache_dir).unwrap();
    let stale = json!({
        "fetched_at": chrono::Utc::now().to_rfc3339(),
        "payload": { "limit": 50, "issues": [
            { "identifier": "ZER-OLD", "title": "stale", "state": "Backlog",
              "state_type": "backlog", "url": "https://x", "updated_at": "2026-07-08T00:00:00Z" }
        ] }
    });
    fs::write(cache_dir.join("viewer-issues.json"), stale.to_string()).unwrap();

    let out = run_pull(root.path(), &["--json", "--refresh"], Some(KEY_SENTINEL));
    assert_eq!(out.status.code(), Some(0));
    let env = stdout_json(&out);
    assert_eq!(env["data"]["source"], json!("api"));
    assert_eq!(env["data"]["issues"][0]["identifier"], json!("ZER-9"));
    let _ = server.join();

    let rewritten = fs::read_to_string(cache_dir.join("viewer-issues.json")).unwrap();
    assert!(rewritten.contains("ZER-9"));
    assert!(!rewritten.contains("ZER-OLD"));
}

#[test]
fn pull_expired_and_corrupt_cache_refetch() {
    for seed in ["expired", "corrupt"] {
        let root = common::temp_root();
        let (url, server) = common::serve_http(vec![("200 OK", viewer_response("ZER-2", "live"))]);
        write_config(root.path(), &url);
        let cache_dir = root.path().join(".cache/linear");
        fs::create_dir_all(&cache_dir).unwrap();
        let content = match seed {
            "expired" => json!({
                "fetched_at": (chrono::Utc::now() - chrono::Duration::seconds(3600)).to_rfc3339(),
                "payload": { "limit": 50, "issues": [] }
            })
            .to_string(),
            _ => "{ definitely not json".to_string(),
        };
        fs::write(cache_dir.join("viewer-issues.json"), content).unwrap();

        let out = run_pull(root.path(), &["--json"], Some(KEY_SENTINEL));
        assert_eq!(out.status.code(), Some(0), "seed {seed}");
        let env = stdout_json(&out);
        assert_eq!(env["data"]["source"], json!("api"), "seed {seed}");
        let _ = server.join();
    }
}

#[test]
fn pull_smaller_limit_serves_cache_larger_limit_refetches() {
    let root = common::temp_root();
    let (url, server) = common::serve_http(vec![("200 OK", viewer_response("ZER-3", "live"))]);
    write_config(root.path(), &url);
    let cache_dir = root.path().join(".cache/linear");
    fs::create_dir_all(&cache_dir).unwrap();
    let cached = json!({
        "fetched_at": chrono::Utc::now().to_rfc3339(),
        "payload": { "limit": 10, "issues": [
            { "identifier": "ZER-A", "title": "a", "state": "Backlog",
              "state_type": "backlog", "url": "https://x", "updated_at": "t" },
            { "identifier": "ZER-B", "title": "b", "state": "Backlog",
              "state_type": "backlog", "url": "https://x", "updated_at": "t" }
        ] }
    });
    fs::write(cache_dir.join("viewer-issues.json"), cached.to_string()).unwrap();

    // limit 1 ≤ cached limit 10 → cache serve, truncated to 1.
    let out = run_pull(root.path(), &["--json", "--limit", "1"], None);
    let env = stdout_json(&out);
    assert_eq!(env["data"]["source"], json!("cache"));
    assert_eq!(env["data"]["issues"].as_array().unwrap().len(), 1);

    // limit 50 > cached limit 10 → live refetch.
    let out = run_pull(
        root.path(),
        &["--json", "--limit", "50"],
        Some(KEY_SENTINEL),
    );
    let env = stdout_json(&out);
    assert_eq!(env["data"]["source"], json!("api"));
    let _ = server.join();
}

#[test]
fn pull_without_role_fails_closed_secret_unavailable() {
    let root = common::temp_root();
    // No config at all → defaults → no role mapped → fail closed, no network attempted.
    let out = run_pull(root.path(), &["--json"], None);
    assert_eq!(out.status.code(), Some(2));
    let env = stdout_json(&out);
    assert_eq!(env["ok"], json!(false));
    assert_eq!(env["outcome"], json!("error"));
    assert_eq!(env["reason"], json!("secret_unavailable"));
    assert_eq!(env["data"], Value::Null);
}

#[test]
fn pull_env_pointer_unset_names_var_and_role() {
    let root = common::temp_root();
    write_config(root.path(), "http://127.0.0.1:9/graphql");
    let out = run_pull(root.path(), &[], None); // human mode, no key in env
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("secret unavailable"), "stderr: {err}");
    assert!(err.contains(KEY_ENV_VAR));
    assert!(err.contains("linear_api"));
}

#[test]
fn pull_human_lines() {
    let root = common::temp_root();
    let (url, server) =
        common::serve_http(vec![("200 OK", viewer_response("ZER-85", "lane Slice 4"))]);
    write_config(root.path(), &url);

    let out = run_pull(root.path(), &[], Some(KEY_SENTINEL));
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("ZER-85") && text.contains("In Progress") && text.contains("lane Slice 4"),
        "human line: {text}"
    );
    assert!(text.contains("(1 issue, api, fetched "), "tail: {text}");
    assert!(!text.contains(KEY_SENTINEL));
    let _ = server.join();
}

#[test]
fn pull_human_output_strips_control_chars_from_network_titles() {
    let root = common::temp_root();
    // A hostile issue title carrying an ANSI escape + an embedded newline.
    let evil_title = "pwn\u{1b}[2K\nZER-999  Done  spoofed";
    let (url, server) = common::serve_http(vec![("200 OK", viewer_response("ZER-7", evil_title))]);
    write_config(root.path(), &url);

    let out = run_pull(root.path(), &[], Some(KEY_SENTINEL));
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.contains('\u{1b}'),
        "ANSI escape reached the terminal: {text:?}"
    );
    // One issue line + the summary tail = exactly 2 lines; the embedded newline must
    // NOT have split the title into a spoofed extra row.
    assert_eq!(
        text.trim_end().lines().count(),
        2,
        "embedded newline injected an extra row: {text:?}"
    );
    let _ = server.join();
}
