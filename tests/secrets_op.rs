//! End-to-end secrets tests with a REAL spawned fake `op` (absolute-path program
//! override — no PATH mutation inside the parallel test process) and a real
//! root-level adapter audit file. The secret sentinel must never appear in the
//! audit log; the `secret_requested` event carries the role key only.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use chrono::Utc;
use lane::config::LaneConfig;
use lane::error::LaneError;
use lane::lock::audit::StdAuditSink;
use lane::secrets::{SecretResolver, StdOpRunner, UNSCOPED};

mod common;

/// Write an executable `#!/bin/sh` fixture and return its path.
fn write_fake_op(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write fake op");
    let mut perms = fs::metadata(&path).expect("meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).expect("chmod fake op");
    path
}

fn config_for(role: &str, reference: &str) -> LaneConfig {
    let toml = format!("[secrets.roles]\n{role} = \"{reference}\"\n");
    toml::from_str(&toml).expect("test config parses")
}

fn uid_of(p: &Path) -> u32 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(p).expect("meta").uid()
}

#[test]
fn fake_op_resolves_and_writes_secret_requested_event() {
    let root = common::temp_root();
    let fake = write_fake_op(root.path(), "op", r#"printf 's3kr1t-SENTINEL-77'"#);
    let cfg = config_for("linear_api", "op://Vault/Item/credential");
    let runner = StdOpRunner::with_program(fake.to_str().unwrap(), Duration::from_secs(10));
    let audit_path = root.path().join("audit.log");
    let sink = StdAuditSink::new(audit_path.clone(), uid_of(root.path()));
    let resolver = SecretResolver {
        config: &cfg,
        runner: &runner,
        sink: &sink,
        repo: UNSCOPED,
        lane: UNSCOPED,
        instance: "lg-test",
    };

    let (res, warn) = resolver.resolve("linear_api", Utc::now());
    assert_eq!(res.expect("resolves").expose(), "s3kr1t-SENTINEL-77");
    assert!(warn.is_none(), "audit append should succeed: {warn:?}");

    let log = fs::read_to_string(&audit_path).expect("root audit exists");
    assert!(log.contains("\"event\":\"secret_requested\""));
    assert!(log.contains("\"secret_role\":\"linear_api\""));
    assert!(log.contains("\"outcome\":\"ok\""));
    assert!(log.contains("\"repo\":\"-\""));
    assert!(
        !log.contains("SENTINEL"),
        "secret value leaked into the adapter audit log"
    );
    assert!(
        !log.contains("op://"),
        "op reference leaked into the adapter audit log"
    );
}

#[test]
fn fake_op_failure_maps_closed_and_audits_error() {
    let root = common::temp_root();
    let fake = write_fake_op(
        root.path(),
        "op",
        r#"echo 'VAULT-SENTINEL: item not found in vault Ops-Secrets' >&2; exit 6"#,
    );
    let cfg = config_for("linear_api", "op://Vault/Item/credential");
    let runner = StdOpRunner::with_program(fake.to_str().unwrap(), Duration::from_secs(10));
    let audit_path = root.path().join("audit.log");
    let sink = StdAuditSink::new(audit_path.clone(), uid_of(root.path()));
    let resolver = SecretResolver {
        config: &cfg,
        runner: &runner,
        sink: &sink,
        repo: UNSCOPED,
        lane: UNSCOPED,
        instance: "lg-test",
    };

    let (res, _warn) = resolver.resolve("linear_api", Utc::now());
    let err = res.expect_err("must fail");
    assert_eq!(err.exit_code(), 2);
    let msg = err.to_string();
    assert!(msg.contains("exit 6"), "actionable exit code: {msg}");
    assert!(msg.contains("linear_api"));
    assert!(
        !msg.contains("VAULT-SENTINEL"),
        "op stderr leaked into the error message: {msg}"
    );

    let log = fs::read_to_string(&audit_path).expect("root audit exists");
    assert!(log.contains("\"outcome\":\"error\""));
    assert!(!log.contains("VAULT-SENTINEL"));
}

#[test]
fn hung_op_is_killed_by_the_bounded_wait() {
    let root = common::temp_root();
    // Sleeps far past the bound; the runner must kill it. Generous margins on both
    // sides (no tight timing window — the ZER-83 anti-pattern is named and avoided).
    // `exec` is load-bearing: the shell must BECOME the sleeper. A plain `sleep 60`
    // leaves a grandchild holding the inherited pipes after the shell is killed,
    // which stalls the drainer joins — the documented out-of-threat-model caveat in
    // src/proc.rs (the real `op` is a single binary that spawns no children).
    let fake = write_fake_op(root.path(), "op", "exec sleep 60");
    let cfg = config_for("linear_api", "op://Vault/Item/credential");
    let runner = StdOpRunner::with_program(fake.to_str().unwrap(), Duration::from_secs(2));
    let audit_path = root.path().join("audit.log");
    let sink = StdAuditSink::new(audit_path, uid_of(root.path()));
    let resolver = SecretResolver {
        config: &cfg,
        runner: &runner,
        sink: &sink,
        repo: UNSCOPED,
        lane: UNSCOPED,
        instance: "lg-test",
    };

    let start = Instant::now();
    let (res, _warn) = resolver.resolve("linear_api", Utc::now());
    let elapsed = start.elapsed();
    let err = res.expect_err("must time out");
    assert!(matches!(err, LaneError::SecretUnavailable(_)));
    assert!(err.to_string().contains("timed out after 2s"));
    assert!(
        elapsed < Duration::from_secs(30),
        "kill path did not bound the wait: {elapsed:?}"
    );
}

#[test]
fn op_missing_program_is_secret_unavailable() {
    let root = common::temp_root();
    let cfg = config_for("linear_api", "op://Vault/Item/credential");
    let missing = root.path().join("no-such-op");
    let runner = StdOpRunner::with_program(missing.to_str().unwrap(), Duration::from_secs(5));
    let sink = StdAuditSink::new(root.path().join("audit.log"), uid_of(root.path()));
    let resolver = SecretResolver {
        config: &cfg,
        runner: &runner,
        sink: &sink,
        repo: UNSCOPED,
        lane: UNSCOPED,
        instance: "lg-test",
    };
    let (res, _warn) = resolver.resolve("linear_api", Utc::now());
    let err = res.expect_err("must fail");
    assert_eq!(err.exit_code(), 2);
    assert!(err.to_string().contains("not found"));
}
