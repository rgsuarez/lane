//! Shared helpers for the Slice 2 locking-core integration tests.
//!
//! Write-path tests put `$LANE_ROOT` UNDER `$HOME` so the local-filesystem device check
//! passes (a tempdir on a different volume would be rejected as non-local). Process tests
//! spawn the real `lane` binary via `CARGO_BIN_EXE_lane`. Fault tests inject the
//! `FsOps`/`AuditSink` seams.
#![allow(dead_code)]

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use lane::git::{GitError, GitOutput, GitRunner};
use lane::lock::audit::{AuditEvent, AuditEventKind, AuditSink, StdAuditSink};
use lane::lock::{FsOps, StdFs};
use lane::ClaimRecord;
use std::cell::RefCell;
use tempfile::TempDir;

/// `$HOME` (required for the local-FS device check + `~` expansion).
pub fn home() -> String {
    std::env::var("HOME").expect("HOME is set")
}

/// A temp lane root on the SAME filesystem device as `$HOME`.
pub fn temp_root() -> TempDir {
    tempfile::Builder::new()
        .prefix("lane-it-")
        .tempdir_in(home())
        .expect("tempdir under HOME")
}

/// Path to the built `lane` binary (set by Cargo for integration tests).
pub fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_lane")
}

/// Run `lane <args> --lane-root <root>` with an optional `$LANE_INSTANCE`. Clears the
/// ambient `$LANE_ROOT` and the test hold hook unless a caller sets them.
pub fn run(root: &Path, instance: Option<&str>, args: &[&str]) -> Output {
    let mut c = Command::new(bin());
    c.args(args).arg("--lane-root").arg(root);
    c.env_remove("LANE_ROOT");
    c.env_remove("LANE_TEST_HOLD_FILE");
    match instance {
        Some(i) => {
            c.env("LANE_INSTANCE", i);
        }
        None => {
            c.env_remove("LANE_INSTANCE");
        }
    }
    c.output().expect("spawn lane")
}

/// Spawn (do not wait) `lane <args>` wired to the ZER-83 handshake hold hook: the child
/// signals `<base>.held` once it holds the lane mutex, then HOLDS the mutex until
/// `<base>.release` exists — deterministic at any optimization level (no wall-clock
/// windows). Markers live in a dedicated sync tempdir owned by the returned
/// [`HoldingChild`], outside `$LANE_ROOT` (lane state stays byte-clean of test objects).
pub fn spawn_holding(root: &Path, instance: &str, args: &[&str]) -> HoldingChild {
    let sync_dir = tempfile::Builder::new()
        .prefix("lane-holdsync-")
        .tempdir_in(home())
        .expect("sync tempdir under HOME");
    let base = sync_dir.path().join("hold");
    let mut c = Command::new(bin());
    c.args(args).arg("--lane-root").arg(root);
    c.env_remove("LANE_ROOT");
    c.env("LANE_INSTANCE", instance);
    c.env("LANE_TEST_HOLD_FILE", &base);
    let child = c.spawn().expect("spawn holding lane");
    HoldingChild {
        child,
        base,
        _sync_dir: sync_dir,
    }
}

/// A spawned mutex-holding `lane` process plus the sync tempdir its handshake markers
/// live in — ONE owner so no panic path can drop the tempdir before the holder observes
/// the release marker. Success paths call [`release`](Self::release) + [`wait`](Self::wait)
/// (or [`kill`](Self::kill) + `wait`) explicitly; [`Drop`] is the panic-path guarantee:
/// signal release → bounded reap (≤5s; the holder polls at 10ms, so it exits within ~2
/// ticks) → `kill` + reap as last resort → only then the owned tempdir drops. The
/// holder-side 30s fail-safe remains solely for a SIGKILLed test runner.
pub struct HoldingChild {
    child: std::process::Child,
    base: PathBuf,
    _sync_dir: TempDir,
}

impl HoldingChild {
    /// Wait (10ms poll) until the child signals it holds the lane mutex. A LIVENESS
    /// bound on process spawn — not a race window: once `.held` exists, the mutex stays
    /// held until [`release`](Self::release).
    pub fn wait_held(&self, timeout: std::time::Duration) -> bool {
        let held = self.held_path();
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if held.exists() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    /// Tell the holder to proceed (create the release marker).
    pub fn release(&self) {
        std::fs::write(self.release_path(), b"go\n").expect("write release marker");
    }

    /// SIGKILL the holder (the sigkill test's deliberate mid-hold crash).
    pub fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }

    /// Reap the holder and return its exit status.
    pub fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        self.child.wait()
    }

    fn held_path(&self) -> PathBuf {
        marker(&self.base, "held")
    }

    fn release_path(&self) -> PathBuf {
        marker(&self.base, "release")
    }
}

impl Drop for HoldingChild {
    fn drop(&mut self) {
        // Best-effort: `kill`/`wait` on an already-reaped child is a no-op we ignore.
        let _ = std::fs::write(self.release_path(), b"go\n");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return, // reaped — the owned tempdir may drop now
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `<base>.<suffix>` handshake marker path (`hold` → `hold.held` / `hold.release`),
/// byte-appended to the raw OS string — the EXACT mirror of `hold_at`'s `suffixed` in
/// `src/lock/mod.rs` (no UTF-8 round-trip, no `with_extension` last-dot surgery). Keep
/// the two in lockstep or the handshake dead-locks at the fail-safe.
fn marker(base: &Path, suffix: &str) -> PathBuf {
    let mut s = base.as_os_str().to_owned();
    s.push(".");
    s.push(suffix);
    PathBuf::from(s)
}

pub fn code(out: &Output) -> i32 {
    out.status.code().expect("exit code")
}

pub fn stdout_json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

/// Read a claim record straight from disk (test inspection).
pub fn read_lock(root: &Path, repo: &str, lane: &str) -> Option<ClaimRecord> {
    let p = root.join(repo).join("locks").join(format!("{lane}.lock"));
    let text = std::fs::read_to_string(p).ok()?;
    Some(serde_json::from_str(&text).expect("lock parses"))
}

pub fn audit_path(root: &Path, repo: &str) -> PathBuf {
    root.join(repo).join("audit.log")
}

/// Parse every newline-terminated line of an audit log into events (asserts each parses).
pub fn read_audit(root: &Path, repo: &str) -> Vec<AuditEvent> {
    let text = std::fs::read_to_string(audit_path(root, repo)).unwrap_or_default();
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            serde_json::from_str::<AuditEvent>(l)
                .unwrap_or_else(|e| panic!("audit line not JSON ({e}): {l}"))
        })
        .collect()
}

// -------------------------------------------------------------------------
// Injectable fault impls (production traits live in the library; these are test-only).
// -------------------------------------------------------------------------

/// An `FsOps` that delegates to std but can fail a chosen mutation, or lie about a path's
/// device id / owner uid (to exercise non-local-root and wrong-owner fail-closed paths).
#[derive(Default)]
pub struct FaultFs {
    pub fail_rename: bool,
    pub fail_hard_link: bool,
    pub device: Option<u64>,
    pub owner: Option<u32>,
}

impl FsOps for FaultFs {
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        if self.fail_rename {
            Err(io::Error::other("injected rename failure"))
        } else {
            StdFs.rename(from, to)
        }
    }
    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        if self.fail_hard_link {
            Err(io::Error::other("injected hard_link failure"))
        } else {
            StdFs.hard_link(from, to)
        }
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        StdFs.remove_file(path)
    }
    fn device_of(&self, path: &Path) -> io::Result<u64> {
        match self.device {
            Some(d) => Ok(d),
            None => StdFs.device_of(path),
        }
    }
    fn owner_uid(&self, path: &Path) -> io::Result<u32> {
        match self.owner {
            Some(u) => Ok(u),
            None => StdFs.owner_uid(path),
        }
    }
}

/// An `AuditSink` that delegates to a real `StdAuditSink` but fails on a chosen event kind
/// (drives the intent-fail / completion-fail audit state-machine tests).
pub struct FaultAudit {
    inner: StdAuditSink,
    fail_on: AuditEventKind,
}

impl FaultAudit {
    pub fn new(path: PathBuf, expected_uid: u32, fail_on: AuditEventKind) -> Self {
        Self {
            inner: StdAuditSink::new(path, expected_uid),
            fail_on,
        }
    }
}

impl AuditSink for FaultAudit {
    fn append(&self, event: &AuditEvent, fsync: bool) -> io::Result<()> {
        if event.event == self.fail_on {
            return Err(io::Error::other("injected audit failure"));
        }
        self.inner.append(event, fsync)
    }
}

// -------------------------------------------------------------------------
// Git seam test helpers (Slice 3): a programmable fake runner + canned outputs, and a
// hermetic real-git scratch-repo initializer.
// -------------------------------------------------------------------------

/// A canned successful git output with the given stdout.
pub fn git_ok(stdout: &str) -> GitOutput {
    GitOutput {
        code: Some(0),
        stdout: stdout.to_string(),
        stderr: String::new(),
    }
}

/// A canned non-zero git output with the given code and stderr.
pub fn git_fail(code: i32, stderr: &str) -> GitOutput {
    GitOutput {
        code: Some(code),
        stdout: String::new(),
        stderr: stderr.to_string(),
    }
}

/// The boxed programmable handler a [`FakeGitRunner`] dispatches each call to.
pub type FakeGitHandler = Box<dyn Fn(&[&str]) -> Result<GitOutput, GitError>>;

/// A programmable [`GitRunner`] for deterministic fault injection. The handler decides the
/// reply from the argument vector; every call is counted and logged (for zero-spawn and
/// call-shape assertions). Single-threaded test use only (interior `RefCell`).
pub struct FakeGitRunner {
    handler: FakeGitHandler,
    calls: RefCell<u32>,
    log: RefCell<Vec<String>>,
}

impl FakeGitRunner {
    pub fn new(handler: impl Fn(&[&str]) -> Result<GitOutput, GitError> + 'static) -> Self {
        Self {
            handler: Box::new(handler),
            calls: RefCell::new(0),
            log: RefCell::new(Vec::new()),
        }
    }
    /// Total number of `run` invocations so far.
    pub fn call_count(&self) -> u32 {
        *self.calls.borrow()
    }
    /// The space-joined argument vector of every call so far.
    pub fn calls(&self) -> Vec<String> {
        self.log.borrow().clone()
    }
}

impl GitRunner for FakeGitRunner {
    fn run(&self, args: &[&str]) -> Result<GitOutput, GitError> {
        *self.calls.borrow_mut() += 1;
        self.log.borrow_mut().push(args.join(" "));
        (self.handler)(args)
    }
}

/// Run `lane <args>` WITHOUT the `--lane-root` injection: the `hook` family takes no
/// `--lane-root` (it never touches lane state), so [`run`] would be a Clap usage error.
/// Baseline-scrubs the hook-relevant env; callers overlay what a test needs.
pub fn run_hook(args: &[&str]) -> Output {
    let mut c = Command::new(bin());
    c.args(args);
    c.env_remove("LANE_ROOT");
    c.env_remove("LANE_INSTANCE");
    c.env_remove("LANE_HOOK_BYPASS");
    c.env_remove("LANE_TEST_HOLD_FILE");
    c.output().expect("spawn lane hook")
}

/// A `$PATH` for hook-driven `git commit`s: the FRESHLY BUILT binary's dir first (never
/// a stale `~/.cargo/bin/lane`), then the system dirs `git`/`sh` live in.
pub fn hook_test_path() -> String {
    let bin_dir = Path::new(bin())
        .parent()
        .expect("bin dir")
        .to_string_lossy()
        .into_owned();
    format!("{bin_dir}:/usr/bin:/bin")
}

/// `git commit --allow-empty` in a scratch repo with hermetic identity/signing `-c`
/// overrides (the operator's global config has `commit.gpgsign=true`). The hook's
/// environment is fully controlled: a baseline scrub of `LANE_ROOT`/`LANE_INSTANCE`/
/// `LANE_HOOK_BYPASS`, then the caller's `envs` overlay (PATH, LANE_ROOT, …).
pub fn scratch_commit(dir: &Path, msg: &str, envs: &[(&str, &str)]) -> Output {
    let mut c = Command::new("git");
    c.args([
        "-c",
        "user.name=Lane Test",
        "-c",
        "user.email=lane-test@example.com",
        "-c",
        "commit.gpgsign=false",
        "-C",
        dir.to_str().expect("utf-8 scratch path"),
        "commit",
        "--allow-empty",
        "-m",
        msg,
    ]);
    c.env_remove("LANE_ROOT");
    c.env_remove("LANE_INSTANCE");
    c.env_remove("LANE_HOOK_BYPASS");
    c.env_remove("LANE_TEST_HOLD_FILE");
    for (k, v) in envs {
        c.env(k, v);
    }
    c.output().expect("spawn git commit")
}

/// Plain `git -C <dir> <args>` for test-side repo state (config flips, worktree add).
pub fn scratch_git(dir: &Path, args: &[&str]) -> Output {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir).args(args);
    c.output().expect("spawn git")
}

/// Initialize a hermetic scratch git repo at `dir` with one initial commit on `main`, using
/// explicit `-c` overrides so it never depends on (or mutates) the operator's global config,
/// signing keys, or hooks. Panics on any git failure.
pub fn init_scratch_repo(dir: &Path) {
    let dir_s = dir.to_str().expect("utf-8 scratch path");
    let base: &[&str] = &[
        "-c",
        "user.name=Lane Test",
        "-c",
        "user.email=lane-test@example.com",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "init.defaultBranch=main",
    ];
    let git = |args: &[&str]| {
        let mut full: Vec<&str> = base.to_vec();
        full.extend_from_slice(args);
        let out = Command::new("git")
            .args(&full)
            .output()
            .expect("spawn git for scratch repo");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", dir_s]);
    std::fs::write(dir.join("seed.txt"), "seed\n").expect("write seed file");
    git(&["-C", dir_s, "add", "-A"]);
    git(&["-C", dir_s, "commit", "-m", "seed"]);
}

// ---------------------------------------------------------------------------
// Loopback HTTP fixture (Slice 4) — a std TcpListener speaking minimal HTTP/1.1,
// shared by the linear transport / pull / board / gated-close tests. Zero dev-deps.
// ---------------------------------------------------------------------------

/// Serve `script.len()` sequential connections on 127.0.0.1. Each connection gets
/// one full request captured (headers + Content-Length body) and one scripted
/// `(status_line, body)` JSON response with `Connection: close`. Returns the
/// GraphQL-ish URL and a handle yielding the raw captured request texts.
pub fn serve_http(
    script: Vec<(&'static str, String)>,
) -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    let url = format!("http://{addr}/graphql");
    let handle = std::thread::spawn(move || {
        let mut captured = Vec::new();
        for (status_line, body) in script {
            let (mut stream, _) = listener.accept().expect("accept");
            captured.push(read_full_request(&mut stream));
            let response = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            use std::io::Write;
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        }
        captured
    });
    (url, handle)
}

/// Read one HTTP/1.1 request: headers to CRLFCRLF, then Content-Length body bytes.
pub fn read_full_request(stream: &mut std::net::TcpStream) -> String {
    use std::io::Read;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk).expect("read request");
        assert!(n > 0, "client closed before sending a full request");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_lowercase();
    let content_length: usize = headers
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .map(|v| v.trim().parse().expect("content-length"))
        .unwrap_or(0);
    while buf.len() < header_end + content_length {
        let n = stream.read(&mut chunk).expect("read body");
        assert!(n > 0, "client closed mid-body");
        buf.extend_from_slice(&chunk[..n]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
