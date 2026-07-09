//! `lane close --draft-closeout` / `--post-closeout` end-to-end: the spawned binary
//! against a loopback GraphQL fixture, secret via the `env:` pointer, config + claim
//! in a temp `$LANE_ROOT`. Proves: draft is a pure preview; the post flow is
//! preflight-gated, marker-deduped, audited, and secret-hygienic; failures leave the
//! claim held and rerunnable; the plain close is byte-identical to before.

use std::fs;
use std::io::Read;
use std::net::TcpListener;
use std::process::{Command, Output};
use std::thread;

use serde_json::{json, Value};

mod common;

const KEY_SENTINEL: &str = "lin_api_close-key-sentinel";
const KEY_ENV_VAR: &str = "LANE_TEST_CLOSE_KEY";

fn write_config(root: &std::path::Path, api_url: &str) {
    fs::write(
        root.join("config.toml"),
        format!(
            "[secrets.roles]\nlinear_api = \"env:{KEY_ENV_VAR}\"\n[linear]\napi_url = \"{api_url}\"\n"
        ),
    )
    .expect("write config");
}

/// Claim a lane and patch gated-closeout fields into the lock JSON (the established
/// idiom for record fields the plain claim verb doesn't set).
fn claim_patched(root: &std::path::Path, lane: &str, linear_key: Option<&str>) {
    let out = common::run(root, Some("close-test"), &["claim", lane, "--repo", "ops"]);
    assert_eq!(out.status.code(), Some(0), "claim failed");
    if let Some(key) = linear_key {
        let lock = root.join("ops/locks").join(format!("{lane}.lock"));
        let mut rec: Value = serde_json::from_str(&fs::read_to_string(&lock).unwrap()).unwrap();
        rec["linear_key"] = json!(key);
        rec["branch"] = json!("slice-branch");
        rec["pr_url"] = json!("https://github.com/x/y/pull/7");
        fs::write(&lock, rec.to_string()).unwrap();
    }
}

/// The marker the binary will compute for this claim generation.
fn marker_for(root: &std::path::Path, lane: &str) -> String {
    let lock = root.join("ops/locks").join(format!("{lane}.lock"));
    let rec: Value = serde_json::from_str(&fs::read_to_string(&lock).unwrap()).unwrap();
    let claimed_at = chrono::DateTime::parse_from_rfc3339(rec["claimed_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    format!("lane-closeout: {lane}@{}", claimed_at.to_rfc3339())
}

fn run_close(root: &std::path::Path, args: &[&str], key: Option<&str>) -> Output {
    let mut cmd = Command::new(common::bin());
    cmd.arg("close")
        .args(args)
        .arg("--lane-root")
        .arg(root)
        .arg("--instance")
        .arg("close-test")
        .env_remove("LANE_ROOT")
        .env_remove("LANE_INSTANCE")
        .env_remove(KEY_ENV_VAR);
    if let Some(k) = key {
        cmd.env(KEY_ENV_VAR, k);
    }
    cmd.output().expect("spawn lane close")
}

fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn preflight_response(marker_bodies: &[&str]) -> String {
    let nodes: Vec<Value> = marker_bodies.iter().map(|b| json!({ "body": b })).collect();
    json!({ "data": { "issue": { "id": "uuid-77", "comments": { "nodes": nodes } } } }).to_string()
}

fn create_response() -> String {
    json!({ "data": { "commentCreate": { "success": true,
        "comment": { "url": "https://linear.app/x/comment/1" } } } })
    .to_string()
}

#[test]
fn draft_is_pure_preview() {
    let root = common::temp_root();
    claim_patched(root.path(), "zer-1", Some("ZER-1"));
    let lock = root.path().join("ops/locks/zer-1.lock");
    let before = fs::read(&lock).unwrap();
    let empty_path = tempfile::tempdir().unwrap();

    // Human mode, no config, no op on PATH, no network anywhere: pure local.
    let out = {
        let mut cmd = Command::new(common::bin());
        cmd.args(["close", "zer-1", "--repo", "ops", "--draft-closeout"])
            .arg("--lane-root")
            .arg(root.path())
            .arg("--instance")
            .arg("close-test")
            .env_remove("LANE_ROOT")
            .env_remove("LANE_INSTANCE")
            .env("PATH", empty_path.path());
        cmd.output().expect("spawn")
    };
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("**lane closeout**"), "draft body: {text}");
    assert!(text.contains("`slice-branch`"));
    assert!(text.contains("lane-closeout: zer-1@"));
    assert!(
        text.contains("draft only — not posted"),
        "gate text: {text}"
    );
    assert!(!text.contains("close-test"), "instance never in the draft");

    // Zero mutation: the lock is byte-identical and the lane still closes normally.
    assert_eq!(fs::read(&lock).unwrap(), before, "lock bytes unchanged");

    // JSON mode carries the draft in the envelope.
    let out = run_close(
        root.path(),
        &["zer-1", "--repo", "ops", "--draft-closeout", "--json"],
        None,
    );
    let env = stdout_json(&out);
    assert_eq!(env["outcome"], json!("ok"));
    assert!(env["data"]["closeout_draft"]
        .as_str()
        .unwrap()
        .contains("lane-closeout"));
    assert_eq!(env["data"]["released"], json!(false));
}

#[test]
fn draft_without_linear_key_refuses() {
    let root = common::temp_root();
    claim_patched(root.path(), "nokey", None);
    let out = run_close(
        root.path(),
        &["nokey", "--repo", "ops", "--draft-closeout", "--json"],
        None,
    );
    assert_eq!(out.status.code(), Some(1));
    let env = stdout_json(&out);
    assert_eq!(env["reason"], json!("no_linear_key"));
}

#[test]
fn post_closeout_full_flow() {
    let root = common::temp_root();
    let (url, server) = common::serve_http(vec![
        ("200 OK", preflight_response(&[])),
        ("200 OK", create_response()),
    ]);
    write_config(root.path(), &url);
    claim_patched(root.path(), "zer-2", Some("ZER-2"));

    let out = run_close(
        root.path(),
        &["zer-2", "--repo", "ops", "--post-closeout", "--json"],
        Some(KEY_SENTINEL),
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = stdout_json(&out);
    assert_eq!(env["outcome"], json!("released"));
    assert_eq!(env["data"]["closeout_posted"], json!(true));
    assert_eq!(
        env["data"]["closeout_comment_url"],
        json!("https://linear.app/x/comment/1")
    );

    // The posted body IS the shown draft (incl. the marker footer), and the key rode
    // only the Authorization header.
    let requests = server.join().expect("server");
    let create_req = &requests[1];
    assert!(create_req.contains("commentCreate"));
    assert!(create_req.contains("lane-closeout: zer-2@"));
    assert!(create_req.contains("slice-branch"));
    assert!(create_req
        .to_lowercase()
        .contains(&format!("authorization: {}", KEY_SENTINEL.to_lowercase())));

    // Lane released; audit trail on both files; secret nowhere.
    assert!(!root.path().join("ops/locks/zer-2.lock").exists());
    let root_audit = fs::read_to_string(root.path().join(".adapter-audit.log")).unwrap();
    assert!(root_audit.contains("\"event\":\"secret_requested\""));
    assert!(root_audit.contains("\"event\":\"linear_write\""));
    assert!(root_audit.contains("\"linear_key\":\"ZER-2\""));
    assert!(root_audit.contains("\"outcome\":\"ok\""));
    assert!(!root_audit.contains(KEY_SENTINEL));
    let repo_audit = fs::read_to_string(root.path().join("ops/audit.log")).unwrap();
    assert!(
        repo_audit.contains("\"event\":\"completion\""),
        "release audited"
    );
    for stream in [&out.stdout, &out.stderr] {
        assert!(
            !String::from_utf8_lossy(stream).contains(KEY_SENTINEL),
            "key leaked to an output stream"
        );
    }
}

#[test]
fn post_failure_keeps_claim_and_rerun_dedupes_a_committed_comment() {
    let root = common::temp_root();
    claim_patched(root.path(), "zer-3", Some("ZER-3"));
    let marker = marker_for(root.path(), "zer-3");

    // Run 1: preflight OK (no marker), then the create connection is read and DROPPED
    // without a response — Linear may have committed the comment; the client can't know.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/graphql");
    let pre = preflight_response(&[]);
    let run1 = thread::spawn(move || {
        // Connection 1: preflight → respond.
        let (mut s, _) = listener.accept().unwrap();
        let _ = common::read_full_request(&mut s);
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{pre}",
            pre.len()
        );
        use std::io::Write;
        s.write_all(resp.as_bytes()).unwrap();
        // Connection 2: create → read fully, then close with NO response.
        let (mut s, _) = listener.accept().unwrap();
        let mut buf = [0u8; 65536];
        let _ = s.read(&mut buf);
        drop(s);
    });
    write_config(root.path(), &url);
    let out = run_close(
        root.path(),
        &["zer-3", "--repo", "ops", "--post-closeout", "--json"],
        Some(KEY_SENTINEL),
    );
    run1.join().unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "ambiguous post failure is exit 2"
    );
    let env = stdout_json(&out);
    assert_eq!(env["reason"], json!("network"));
    assert!(
        root.path().join("ops/locks/zer-3.lock").exists(),
        "claim held after the failed post — rerunnable"
    );
    let root_audit = fs::read_to_string(root.path().join(".adapter-audit.log")).unwrap();
    assert!(
        root_audit.contains("\"event\":\"linear_write\"")
            && root_audit.contains("\"outcome\":\"error\""),
        "failed write audited: {root_audit}"
    );

    // Run 2: the preflight now REPORTS the marker (Linear had committed it) — the
    // rerun dedupes (zero creates), then releases.
    let (url2, server2) = common::serve_http(vec![("200 OK", preflight_response(&[&marker]))]);
    write_config(root.path(), &url2);
    let out = run_close(
        root.path(),
        &["zer-3", "--repo", "ops", "--post-closeout", "--json"],
        Some(KEY_SENTINEL),
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = stdout_json(&out);
    assert_eq!(env["outcome"], json!("released"));
    assert_eq!(env["data"]["closeout_already_posted"], json!(true));
    assert_eq!(env["data"]["closeout_posted"], Value::Null);
    let requests = server2.join().unwrap();
    assert_eq!(requests.len(), 1, "preflight only — no create");
    assert!(!root.path().join("ops/locks/zer-3.lock").exists());
}

#[test]
fn dirty_worktree_refuses_before_any_publish() {
    let root = common::temp_root();
    // The git repo (and its derived sibling worktree) must live OUTSIDE the lane
    // root — the core refuses targets under $LANE_ROOT by design.
    let repos = common::temp_root();
    let repo_dir = repos.path().join("scratch-repo");
    fs::create_dir_all(&repo_dir).unwrap();
    common::init_scratch_repo(&repo_dir);

    // Real lifecycle start: claim + branch + worktree.
    let out = common::run(
        root.path(),
        Some("close-test"),
        &[
            "start",
            "zer-4",
            "--repo",
            "ops",
            "--git-repo",
            repo_dir.to_str().unwrap(),
            "--linear-key",
            "ZER-4",
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let worktree = repos.path().join("scratch-repo-zer-4");
    assert!(worktree.exists(), "worktree created");
    // Dirty it.
    fs::write(worktree.join("untracked.txt"), "dirt\n").unwrap();

    // Preflight will be served; the dirty refusal must land BEFORE any create.
    let (url, server) = common::serve_http(vec![("200 OK", preflight_response(&[]))]);
    write_config(root.path(), &url);
    let out = run_close(
        root.path(),
        &[
            "zer-4",
            "--repo",
            "ops",
            "--post-closeout",
            "--remove-worktree",
            "--json",
        ],
        Some(KEY_SENTINEL),
    );
    assert_eq!(out.status.code(), Some(1));
    let env = stdout_json(&out);
    assert_eq!(env["reason"], json!("dirty_worktree"));
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 1, "exactly the preflight read; zero writes");
    assert!(
        root.path().join("ops/locks/zer-4.lock").exists(),
        "claim held (and renewed)"
    );
    assert!(worktree.exists(), "worktree untouched");
}

#[test]
fn secret_unavailable_before_any_mutation() {
    let root = common::temp_root();
    // Role maps to an UNSET env var; api_url irrelevant (never reached).
    write_config(root.path(), "http://127.0.0.1:9/graphql");
    claim_patched(root.path(), "zer-5", Some("ZER-5"));
    let lock = root.path().join("ops/locks/zer-5.lock");
    let before = fs::read(&lock).unwrap();

    let out = run_close(
        root.path(),
        &["zer-5", "--repo", "ops", "--post-closeout", "--json"],
        None,
    );
    assert_eq!(out.status.code(), Some(2));
    let env = stdout_json(&out);
    assert_eq!(env["reason"], json!("secret_unavailable"));
    assert_eq!(
        fs::read(&lock).unwrap(),
        before,
        "nothing mutated — not even the renew (expires_at unchanged)"
    );
}

#[test]
fn post_on_absent_lane_is_not_held_with_zero_requests() {
    let root = common::temp_root();
    let out = run_close(
        root.path(),
        &["ghost", "--repo", "ops", "--post-closeout", "--json"],
        None,
    );
    assert_eq!(out.status.code(), Some(0));
    let env = stdout_json(&out);
    assert_eq!(env["outcome"], json!("not_held"));
    assert_eq!(env["data"]["closeout_posted"], Value::Null);
}

#[test]
fn plain_close_needs_no_config_op_or_network() {
    let root = common::temp_root();
    claim_patched(root.path(), "zer-6", Some("ZER-6"));
    let empty_path = tempfile::tempdir().unwrap();
    let mut cmd = Command::new(common::bin());
    cmd.args(["close", "zer-6", "--repo", "ops", "--json"])
        .arg("--lane-root")
        .arg(root.path())
        .arg("--instance")
        .arg("close-test")
        .env_remove("LANE_ROOT")
        .env_remove("LANE_INSTANCE")
        .env("PATH", empty_path.path());
    let out = cmd.output().expect("spawn");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = stdout_json(&out);
    assert_eq!(env["outcome"], json!("released"));
    assert_eq!(env["data"]["closeout_draft"], Value::Null);
}

#[test]
fn draft_and_post_flags_conflict() {
    let root = common::temp_root();
    let out = run_close(
        root.path(),
        &["x", "--repo", "ops", "--draft-closeout", "--post-closeout"],
        None,
    );
    assert_eq!(out.status.code(), Some(2), "clap usage error");
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot be used with"));
}
