//! 1Password-backed secret resolution (Slice 4, spec §7) — an ADAPTER, outside the
//! locking core.
//!
//! Law of this module:
//! - Secrets are addressed by logical ROLE KEYS mapped in `$LANE_ROOT/config.toml` to
//!   opaque references (`op://<vault>/<item>/<field>` or `env:VARNAME`). Values are
//!   resolved at call time via the `op` CLI (or the env pointer) and NEVER persisted,
//!   printed, logged, serialized, or embedded in errors.
//! - `op` stderr is NEVER stored or surfaced (it can name vaults/items): failures map
//!   to a closed [`SecretError`] vocabulary; messages carry the role key + a fix hint.
//! - Every resolution attempt appends one terminal `secret_requested` event (role key,
//!   outcome, ts — never the value or reference) to the ROOT-level adapter audit
//!   (`LaneRoot::root_audit_path`). An audit failure degrades to a warning; it never
//!   fails the resolution (adapter audit is observability, not availability). The root
//!   file is observability-only: no decision path reads it, and a torn trailing line
//!   from a crash mid-append is tolerated by line-oriented consumers, never repaired.
//! - The locking core never imports this module (enforced by the source scan in
//!   `tests/no_network_guard.rs`); local verbs never resolve secrets.

use std::fmt;
use std::process::Command;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::config::LaneConfig;
use crate::error::LaneError;
use crate::lock::audit::{AuditEvent, AuditEventKind, AuditOutcome, AuditSink};
use crate::proc::run_bounded;
// Re-exported so the public `OpRunner` seam's signature types are nameable by
// consumers and test fakes (`proc` itself stays a private plumbing module).
pub use crate::proc::{ProcError, ProcOutput};

/// Default bounded wait for one `op read`. Deliberately much longer than git's 10s:
/// an interactive `op` may block on Touch ID / desktop-app authorization.
pub const DEFAULT_OP_TIMEOUT: Duration = Duration::from_secs(60);

/// Sentinel used in audit events for fields with no applicable value (e.g. a repo-less
/// `lane pull` secret resolution).
pub const UNSCOPED: &str = "-";

/// A resolved secret value. Deliberately: no `Display`, no `Serialize`, no `Clone`;
/// `Debug` prints a redaction marker. The raw value is reachable ONLY through
/// [`SecretValue::expose`], which callers may use solely at the point of consumption
/// (e.g. constructing an Authorization header).
pub struct SecretValue(String);

impl SecretValue {
    /// Wrap an already-resolved secret (the resolution paths and test fixtures).
    /// The value discipline — never log, echo, serialize, or persist — applies from
    /// this point on.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The raw secret. Call ONLY at the point of consumption; never log/echo/store.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue(«redacted»)")
    }
}

/// The injectable `op` seam (mirrors `GitRunner`). Callers pass full argument vectors;
/// the runner spawns `op` under the mandatory bounded wait and returns raw bytes.
pub trait OpRunner {
    fn run(&self, args: &[&str]) -> Result<ProcOutput, ProcError>;
}

/// The production runner: spawn the real `op` binary with a bounded wait + kill.
/// The child inherits the environment untouched (op needs its desktop-app socket /
/// `OP_SERVICE_ACCOUNT_TOKEN` / `HOME`); lane sets and reads nothing of it.
pub struct StdOpRunner {
    program: String,
    timeout: Duration,
}

impl StdOpRunner {
    pub fn new() -> Self {
        Self {
            program: "op".to_string(),
            timeout: DEFAULT_OP_TIMEOUT,
        }
    }
    /// Custom timeout (tests pin the kill path).
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            program: "op".to_string(),
            timeout,
        }
    }
    /// Custom program path (tests point at a fake `op` fixture by absolute path,
    /// avoiding PATH races inside the test process).
    pub fn with_program(program: impl Into<String>, timeout: Duration) -> Self {
        Self {
            program: program.into(),
            timeout,
        }
    }
}

impl Default for StdOpRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl OpRunner for StdOpRunner {
    fn run(&self, args: &[&str]) -> Result<ProcOutput, ProcError> {
        let mut cmd = Command::new(&self.program);
        cmd.args(args);
        run_bounded(cmd, self.timeout)
    }
}

/// The closed failure vocabulary. `op` stderr is classified into these and then
/// DROPPED — it is never stored, so it can never leak into a message.
#[derive(Debug, PartialEq, Eq)]
pub enum SecretError {
    /// No reference is mapped for the role in `[secrets.roles]`.
    RoleUnmapped,
    /// The mapped reference failed pre-spawn validation (the detail names the rule,
    /// never the reference).
    RefInvalid(&'static str),
    /// The `op` binary could not be found.
    OpMissing,
    /// `op` exited non-zero (or abnormally, `code: None`).
    OpFailed { code: Option<i32> },
    /// `op` exceeded the bounded wait and was killed.
    Timeout { secs: u64 },
    /// The resolved value was not valid UTF-8 (never lossily decoded).
    NotUtf8,
    /// The resolved value was empty.
    Empty,
    /// An `env:` reference named an unset environment variable.
    EnvMissing(String),
    /// The reference scheme is not `op://` or `env:`.
    ProviderUnsupported,
}

impl SecretError {
    /// Compose the single operator-facing message. Carries the ROLE KEY and a fix
    /// hint — never a reference, never a value, never `op` stderr.
    pub fn to_lane(self, role: &str) -> LaneError {
        let msg = match self {
            SecretError::RoleUnmapped => format!(
                "no reference is mapped for role `{role}`; add it under [secrets.roles] in $LANE_ROOT/config.toml"
            ),
            SecretError::RefInvalid(rule) => {
                format!("the reference mapped for role `{role}` is invalid ({rule})")
            }
            SecretError::OpMissing => format!(
                "the `op` CLI was not found for role `{role}`; install the 1Password CLI or map an env: reference"
            ),
            SecretError::OpFailed { code: Some(c) } => format!(
                "op read failed (exit {c}) for role `{role}`; is 1Password unlocked/signed in? try `op signin`"
            ),
            SecretError::OpFailed { code: None } => {
                format!("op terminated abnormally for role `{role}`")
            }
            SecretError::Timeout { secs } => {
                format!("op read timed out after {secs}s for role `{role}`")
            }
            SecretError::NotUtf8 => {
                format!("the secret for role `{role}` is not valid UTF-8")
            }
            SecretError::Empty => format!("the secret for role `{role}` resolved empty"),
            SecretError::EnvMissing(var) => format!(
                "environment variable `{var}` (mapped for role `{role}`) is unset"
            ),
            SecretError::ProviderUnsupported => format!(
                "the reference for role `{role}` uses an unsupported scheme (use op://… or env:VARNAME)"
            ),
        };
        LaneError::SecretUnavailable(msg)
    }
}

/// Context for one resolution: config, the seam, the ROOT-level audit sink, and the
/// claim coordinates for the audit event (use [`UNSCOPED`] where not applicable).
pub struct SecretResolver<'a> {
    pub config: &'a LaneConfig,
    pub runner: &'a dyn OpRunner,
    pub sink: &'a dyn AuditSink,
    pub repo: &'a str,
    pub lane: &'a str,
    pub instance: &'a str,
}

impl SecretResolver<'_> {
    /// Resolve `role`. Appends one `secret_requested` event (outcome ok|error) to the
    /// root adapter audit; an audit-append failure NEVER fails the resolution and is
    /// returned as the warning half instead.
    pub fn resolve(
        &self,
        role: &str,
        now: DateTime<Utc>,
    ) -> (Result<SecretValue, LaneError>, Option<String>) {
        let result = self.resolve_inner(role);
        let outcome = if result.is_ok() {
            AuditOutcome::Ok
        } else {
            AuditOutcome::Error
        };
        let mut event = AuditEvent::new(
            AuditEventKind::SecretRequested,
            self.repo,
            self.lane,
            self.instance,
            outcome,
            now,
        );
        event.secret_role = Some(role.to_string());
        let warning = match self.sink.append(&event, true) {
            Ok(()) => None,
            Err(e) => Some(format!("adapter audit degraded (secret_requested): {e}")),
        };
        (result.map_err(|e| e.to_lane(role)), warning)
    }

    fn resolve_inner(&self, role: &str) -> Result<SecretValue, SecretError> {
        let reference = self
            .config
            .role_reference(role)
            .ok_or(SecretError::RoleUnmapped)?;
        if let Some(var) = reference.strip_prefix("env:") {
            return resolve_env(var);
        }
        if reference.starts_with("op://") {
            return self.resolve_op(reference);
        }
        Err(SecretError::ProviderUnsupported)
    }

    fn resolve_op(&self, reference: &str) -> Result<SecretValue, SecretError> {
        // Pre-spawn validation (the git adapter's require_flag_safe discipline): the
        // scheme prefix rules out a leading `-`; ban control bytes outright. Spaces
        // are legal inside op vault/item names and stay allowed.
        let tail = &reference["op://".len()..];
        if tail.is_empty() {
            return Err(SecretError::RefInvalid("empty op:// reference"));
        }
        if reference
            .chars()
            .any(|c| c == '\0' || c == '\n' || c == '\r')
        {
            return Err(SecretError::RefInvalid("control byte in reference"));
        }
        let mut args: Vec<&str> = vec!["read", "--no-newline"];
        if let Some(account) = self.config.secrets.op_account.as_deref() {
            if account.is_empty()
                || account.starts_with('-')
                || account.chars().any(|c| c == '\0' || c == '\n' || c == '\r')
            {
                return Err(SecretError::RefInvalid("invalid op_account value"));
            }
            args.push("--account");
            args.push(account);
        }
        args.push(reference);

        let out = self.runner.run(&args).map_err(|e| match e {
            ProcError::Spawn(e) if e.kind() == std::io::ErrorKind::NotFound => {
                SecretError::OpMissing
            }
            ProcError::Spawn(_) => SecretError::OpFailed { code: None },
            ProcError::Timeout { secs } => SecretError::Timeout { secs },
        })?;
        if out.code != Some(0) {
            // out.stderr is deliberately dropped unread: op's message can name the
            // vault/item (the human label the config law keeps out of lane's world).
            return Err(SecretError::OpFailed { code: out.code });
        }
        // STRICT decode — a lossily-decoded secret would be silent corruption.
        let mut value = String::from_utf8(out.stdout).map_err(|_| SecretError::NotUtf8)?;
        // `--no-newline` should make this a no-op; trim a single trailing line
        // terminator defensively (an older `op` would otherwise 401 confusingly).
        while value.ends_with('\n') || value.ends_with('\r') {
            value.pop();
        }
        if value.is_empty() {
            return Err(SecretError::Empty);
        }
        Ok(SecretValue(value))
    }
}

fn resolve_env(var: &str) -> Result<SecretValue, SecretError> {
    let valid = !var.is_empty()
        && var
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        return Err(SecretError::RefInvalid("invalid env: variable name"));
    }
    match std::env::var(var) {
        Ok(v) if v.is_empty() => Err(SecretError::Empty),
        Ok(v) => Ok(SecretValue(v)),
        Err(_) => Err(SecretError::EnvMissing(var.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SecretsConfig;
    use std::cell::RefCell;
    use std::io;

    struct Scripted {
        out: ProcOutput,
    }
    impl OpRunner for Scripted {
        fn run(&self, _args: &[&str]) -> Result<ProcOutput, ProcError> {
            Ok(self.out.clone())
        }
    }

    struct Failing(fn() -> ProcError);
    impl OpRunner for Failing {
        fn run(&self, _args: &[&str]) -> Result<ProcOutput, ProcError> {
            Err(self.0())
        }
    }

    struct Counting {
        calls: RefCell<u32>,
    }
    impl OpRunner for Counting {
        fn run(&self, _args: &[&str]) -> Result<ProcOutput, ProcError> {
            *self.calls.borrow_mut() += 1;
            Ok(ok_out("never"))
        }
    }

    struct Logging {
        argv: RefCell<Vec<String>>,
    }
    impl OpRunner for Logging {
        fn run(&self, args: &[&str]) -> Result<ProcOutput, ProcError> {
            *self.argv.borrow_mut() = args.iter().map(|s| s.to_string()).collect();
            Ok(ok_out("v"))
        }
    }

    #[derive(Default)]
    struct MemSink {
        events: RefCell<Vec<AuditEvent>>,
    }
    impl AuditSink for MemSink {
        fn append(&self, event: &AuditEvent, _fsync: bool) -> io::Result<()> {
            self.events.borrow_mut().push(event.clone());
            Ok(())
        }
    }

    struct FailSink;
    impl AuditSink for FailSink {
        fn append(&self, _event: &AuditEvent, _fsync: bool) -> io::Result<()> {
            Err(io::Error::other("sink down"))
        }
    }

    fn ok_out(stdout: &str) -> ProcOutput {
        ProcOutput {
            code: Some(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    fn config_with(role: &str, reference: &str, account: Option<&str>) -> LaneConfig {
        let mut roles = std::collections::BTreeMap::new();
        roles.insert(role.to_string(), reference.to_string());
        LaneConfig {
            secrets: SecretsConfig {
                op_account: account.map(str::to_string),
                roles,
            },
            linear: Default::default(),
        }
    }

    fn resolver<'a>(
        config: &'a LaneConfig,
        runner: &'a dyn OpRunner,
        sink: &'a dyn AuditSink,
    ) -> SecretResolver<'a> {
        SecretResolver {
            config,
            runner,
            sink,
            repo: UNSCOPED,
            lane: UNSCOPED,
            instance: "t-1",
        }
    }

    #[test]
    fn op_scheme_resolves_and_audits() {
        let cfg = config_with("linear_api", "op://V/I/credential", None);
        let runner = Scripted {
            out: ok_out("s3kr1t"),
        };
        let sink = MemSink::default();
        let (res, warn) = resolver(&cfg, &runner, &sink).resolve("linear_api", Utc::now());
        assert_eq!(res.unwrap().expose(), "s3kr1t");
        assert!(warn.is_none());
        let events = sink.events.borrow();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, AuditEventKind::SecretRequested);
        assert_eq!(events[0].outcome, AuditOutcome::Ok);
        assert_eq!(events[0].secret_role.as_deref(), Some("linear_api"));
        assert_eq!(events[0].repo, UNSCOPED);
        let json = serde_json::to_string(&events[0]).unwrap();
        assert!(json.contains("\"secret_requested\""));
        assert!(!json.contains("s3kr1t"));
        assert!(!json.contains("op://"));
    }

    #[test]
    fn argv_discipline_no_newline_and_account_iff_configured() {
        let cfg = config_with("r", "op://V/I/f", None);
        let runner = Logging {
            argv: RefCell::new(Vec::new()),
        };
        let sink = MemSink::default();
        let _ = resolver(&cfg, &runner, &sink).resolve("r", Utc::now());
        assert_eq!(
            *runner.argv.borrow(),
            vec!["read", "--no-newline", "op://V/I/f"]
        );

        let cfg = config_with("r", "op://V/I/f", Some("my.1password.com"));
        let _ = resolver(&cfg, &runner, &sink).resolve("r", Utc::now());
        assert_eq!(
            *runner.argv.borrow(),
            vec![
                "read",
                "--no-newline",
                "--account",
                "my.1password.com",
                "op://V/I/f"
            ]
        );
    }

    #[test]
    fn invalid_references_never_spawn() {
        let sink = MemSink::default();
        for (reference, account) in [
            ("op://", None),
            ("op://V/I\nf", None),
            ("op://V/I/f", Some("-evil")),
            ("op://V/I/f", Some("")),
        ] {
            let cfg = config_with("r", reference, account);
            let runner = Counting {
                calls: RefCell::new(0),
            };
            let (res, _) = resolver(&cfg, &runner, &sink).resolve("r", Utc::now());
            let err = res.unwrap_err();
            assert!(matches!(err, LaneError::SecretUnavailable(_)));
            assert_eq!(*runner.calls.borrow(), 0, "spawned for {reference:?}");
        }
    }

    #[test]
    fn env_scheme_reads_env_and_missing_fails() {
        let var = "LANE_TEST_SECRET_ENV_VAR_XYZZY";
        std::env::set_var(var, "env-secret");
        let cfg = config_with("r", &format!("env:{var}"), None);
        let runner = Counting {
            calls: RefCell::new(0),
        };
        let sink = MemSink::default();
        let (res, _) = resolver(&cfg, &runner, &sink).resolve("r", Utc::now());
        assert_eq!(res.unwrap().expose(), "env-secret");
        assert_eq!(*runner.calls.borrow(), 0, "env scheme must not spawn op");
        std::env::remove_var(var);

        let cfg = config_with("r", "env:LANE_TEST_SECRET_UNSET_VAR_XYZZY", None);
        let (res, _) = resolver(&cfg, &runner, &sink).resolve("r", Utc::now());
        let msg = match res.unwrap_err() {
            LaneError::SecretUnavailable(m) => m,
            other => panic!("wrong error: {other:?}"),
        };
        assert!(msg.contains("LANE_TEST_SECRET_UNSET_VAR_XYZZY"));
        assert!(msg.contains("`r`"));
    }

    #[test]
    fn role_unmapped_and_unsupported_scheme() {
        let cfg = LaneConfig::default();
        let runner = Counting {
            calls: RefCell::new(0),
        };
        let sink = MemSink::default();
        let (res, _) = resolver(&cfg, &runner, &sink).resolve("nope", Utc::now());
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("`nope`"));
        assert!(msg.contains("[secrets.roles]"));

        let cfg = config_with("r", "file:/etc/passwd", None);
        let (res, _) = resolver(&cfg, &runner, &sink).resolve("r", Utc::now());
        assert!(res.unwrap_err().to_string().contains("unsupported scheme"));
        assert_eq!(*runner.calls.borrow(), 0);
    }

    #[test]
    fn op_failure_maps_without_stderr_leak_and_audits_error() {
        let cfg = config_with("r", "op://V/I/f", None);
        let runner = Scripted {
            out: ProcOutput {
                code: Some(1),
                stdout: Vec::new(),
                stderr: b"VAULT-SENTINEL item not found".to_vec(),
            },
        };
        let sink = MemSink::default();
        let (res, _) = resolver(&cfg, &runner, &sink).resolve("r", Utc::now());
        let msg = res.unwrap_err().to_string();
        assert!(!msg.contains("VAULT-SENTINEL"));
        assert!(msg.contains("exit 1"));
        assert!(msg.contains("op signin"));
        assert_eq!(sink.events.borrow()[0].outcome, AuditOutcome::Error);
    }

    #[test]
    fn op_missing_timeout_empty_and_non_utf8_map() {
        let cfg = config_with("r", "op://V/I/f", None);
        let sink = MemSink::default();

        let runner = Failing(|| ProcError::Spawn(io::Error::from(io::ErrorKind::NotFound)));
        let (res, _) = resolver(&cfg, &runner, &sink).resolve("r", Utc::now());
        assert!(res.unwrap_err().to_string().contains("not found"));

        let runner = Failing(|| ProcError::Timeout { secs: 60 });
        let (res, _) = resolver(&cfg, &runner, &sink).resolve("r", Utc::now());
        assert!(res.unwrap_err().to_string().contains("timed out after 60s"));

        let runner = Scripted { out: ok_out("") };
        let (res, _) = resolver(&cfg, &runner, &sink).resolve("r", Utc::now());
        assert!(res.unwrap_err().to_string().contains("resolved empty"));

        let runner = Scripted {
            out: ProcOutput {
                code: Some(0),
                stdout: vec![0xff, 0xfe],
                stderr: Vec::new(),
            },
        };
        let (res, _) = resolver(&cfg, &runner, &sink).resolve("r", Utc::now());
        assert!(res.unwrap_err().to_string().contains("not valid UTF-8"));
    }

    #[test]
    fn trailing_newline_trimmed_but_inner_preserved() {
        let cfg = config_with("r", "op://V/I/f", None);
        let sink = MemSink::default();
        let runner = Scripted {
            out: ok_out("value\n"),
        };
        let (res, _) = resolver(&cfg, &runner, &sink).resolve("r", Utc::now());
        assert_eq!(res.unwrap().expose(), "value");
    }

    #[test]
    fn secret_value_debug_is_redacted() {
        let v = SecretValue("hunter2".to_string());
        let dbg = format!("{v:?}");
        assert_eq!(dbg, "SecretValue(«redacted»)");
        assert!(!dbg.contains("hunter2"));
    }

    #[test]
    fn audit_failure_degrades_to_warning_never_fails_resolution() {
        let cfg = config_with("r", "op://V/I/f", None);
        let runner = Scripted { out: ok_out("v") };
        let (res, warn) = resolver(&cfg, &runner, &FailSink).resolve("r", Utc::now());
        assert!(res.is_ok());
        let warn = warn.unwrap();
        assert!(warn.contains("adapter audit degraded"));
    }
}
