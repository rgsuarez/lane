//! The offline local locking core (Slice 2).
//!
//! Authoritative writer of `LANE_ROOT/<repo>/locks/<lane>.lock`, crash-aware and
//! race-safe across processes via OS advisory locks. Works with **no** Linear /
//! GitHub / 1Password / Vantage / homebox / overseer / tmux / network / daemon / DB /
//! async. See the module docs in `paths`, `mutex`, `record`, `audit`, `claim`, and
//! `renew_release` for the per-area invariants, and `AGENTS.md` for the standing rules.

pub mod audit;
pub mod claim;
pub mod mutex;
pub mod paths;
pub mod record;
pub mod renew_release;
pub mod target;

use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::error::{LaneError, Reason, RefusedReason};
use crate::model::{ClaimRecord, Liveness, StaleState};

use self::audit::next_op_id;
use self::paths::LaneRoot;
use self::target::Target;

/// Default lease length when `--ttl-hours` is omitted.
pub const DEFAULT_TTL_HOURS: f64 = 12.0;
/// Maximum permitted lease length (30 days).
pub const MAX_TTL_HOURS: f64 = 720.0;
const FILE_MODE: u32 = 0o600;

// ---------------------------------------------------------------------------
// Injectable filesystem seam (test-only failpoints; production is plain std).
// ---------------------------------------------------------------------------

/// The injectable filesystem seam. Production is [`StdFs`]; tests inject failpoints at
/// `rename`/`hard_link` (the mutation), `device_of` (the local-FS check), and
/// `owner_uid` (the wrong-owner guard).
pub trait FsOps {
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// Device id of `path` (compared to `$HOME`'s for local-FS enforcement).
    fn device_of(&self, path: &Path) -> io::Result<u64>;
    /// Owner uid of `path` via lstat (compared to the expected uid by the object guard).
    fn owner_uid(&self, path: &Path) -> io::Result<u32>;
}

/// The production filesystem ops — plain std.
pub struct StdFs;

impl FsOps for StdFs {
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::hard_link(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
    fn device_of(&self, path: &Path) -> io::Result<u64> {
        Ok(std::fs::metadata(path)?.dev())
    }
    fn owner_uid(&self, path: &Path) -> io::Result<u32> {
        Ok(std::fs::symlink_metadata(path)?.uid())
    }
}

// ---------------------------------------------------------------------------
// Parameters and success values (the inputs/outputs of the core verbs).
// ---------------------------------------------------------------------------

/// Inputs to [`claim::claim_core`].
pub struct ClaimParams {
    pub repo: String,
    pub lane: String,
    pub instance: String,
    pub target: Option<String>,
    pub home: Option<String>,
    pub ttl_hours: Option<f64>,
    pub note: Option<String>,
    pub force: bool,
}

/// Result of a successful claim.
pub struct ClaimSuccess {
    pub lane: String,
    pub instance: String,
    pub expires_at: DateTime<Utc>,
    pub forced: bool,
    pub prior_instance: Option<String>,
    pub audit_warning: Option<String>,
}

/// Inputs to [`renew_release::renew_core`].
pub struct RenewParams {
    pub repo: String,
    pub lane: String,
    pub instance: String,
    pub ttl_hours: Option<f64>,
}

/// Result of a successful renew.
pub struct RenewSuccess {
    pub lane: String,
    pub expires_at: DateTime<Utc>,
    pub audit_warning: Option<String>,
}

/// Inputs to [`renew_release::handoff_core`].
pub struct HandoffParams {
    pub repo: String,
    pub lane: String,
    pub instance: String,
    /// Optional handoff digest replacing the claim note (non-secret; excluded from audit).
    pub note: Option<String>,
}

/// Result of a successful handoff (the claim stays held; only `claim_status` flips).
pub struct HandoffSuccess {
    pub lane: String,
    pub expires_at: DateTime<Utc>,
    pub audit_warning: Option<String>,
}

/// Inputs to [`renew_release::release_core`].
pub struct ReleaseParams {
    pub repo: String,
    pub lane: String,
    pub instance: String,
}

/// Result of a release (`present` distinguishes a real removal from a no-op).
pub struct ReleaseSuccess {
    pub present: bool,
    pub audit_warning: Option<String>,
}

/// Read-only status of a single lane (also the per-row shape for `list`).
#[derive(Debug, Clone, Serialize)]
pub struct StatusData {
    pub present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<ClaimRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_state: Option<StaleState>,
}

// ---------------------------------------------------------------------------
// Validation (§S2.9).
// ---------------------------------------------------------------------------

/// `^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`, and never `.`/`..` (so a lane is always a
/// single, traversal-free path component).
pub fn validate_name(kind: &str, s: &str) -> Result<(), LaneError> {
    let ok = s != "."
        && s != ".."
        && !s.is_empty()
        && s.len() <= 128
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(LaneError::Identity(format!(
            "invalid {kind} identifier: {s:?} (allowed: ^[A-Za-z0-9][A-Za-z0-9._-]{{0,127}}$, not . or ..)"
        )))
    }
}

/// Non-empty, ≤128 chars, no control characters.
pub fn validate_instance(s: &str) -> Result<(), LaneError> {
    if s.is_empty() || s.chars().count() > 128 || s.chars().any(char::is_control) {
        return Err(LaneError::Identity(
            "instance must be non-empty, ≤128 chars, and free of control characters".into(),
        ));
    }
    Ok(())
}

/// Finite, `> 0`, `≤ 720` hours.
pub fn validate_ttl(h: f64) -> Result<(), LaneError> {
    if !h.is_finite() || h <= 0.0 || h > MAX_TTL_HOURS {
        return Err(LaneError::Identity(format!(
            "ttl-hours must be finite, > 0, and ≤ {MAX_TTL_HOURS}, got {h}"
        )));
    }
    Ok(())
}

/// ≤1024 chars (explicitly non-secret; excluded from the audit log).
pub fn validate_note(s: &str) -> Result<(), LaneError> {
    if s.chars().count() > 1024 {
        return Err(LaneError::Identity("note must be ≤1024 characters".into()));
    }
    Ok(())
}

/// Convert a finite, validated hour count to a `chrono::Duration`.
pub fn ttl_to_duration(hours: f64) -> Duration {
    Duration::milliseconds((hours * 3_600_000.0) as i64)
}

// ---------------------------------------------------------------------------
// Shared write helpers used by both claim and renew.
// ---------------------------------------------------------------------------

/// Write a complete `0600` temp claim record (fsync'd) and return its path. The unique
/// `op_id` token guarantees a fresh name, so `create_new` never follows a symlink.
pub(crate) fn write_temp(
    root: &LaneRoot,
    repo: &str,
    lane: &str,
    record: &ClaimRecord,
    fs: &dyn FsOps,
    expected_uid: u32,
) -> Result<PathBuf, LaneError> {
    let json = serde_json::to_string_pretty(record)
        .map_err(|e| LaneError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))?;
    let temp = root.temp_path(repo, lane, &next_op_id());
    let mut file = paths::open_or_create_writer(&temp, FILE_MODE, fs, expected_uid)?;
    file.write_all(json.as_bytes()).map_err(LaneError::Io)?;
    file.write_all(b"\n").map_err(LaneError::Io)?;
    file.sync_all().map_err(LaneError::Io)?;
    Ok(temp)
}

/// Scan sibling `*.lock` records under the target mutex for canonical-target overlap.
/// Skips self, expired, and target-less siblings; a malformed/identity-inconsistent
/// sibling **fails closed** (exit 2). Returns `Refused(TargetOverlap)` on conflict.
pub(crate) fn scan_overlap(
    root: &LaneRoot,
    repo: &str,
    self_lane: &str,
    target: &Target,
    now: DateTime<Utc>,
    fs: &dyn FsOps,
) -> Result<(), LaneError> {
    // Guarded chain: a symlinked repo/locks fails closed; absent locks ⇒ no siblings.
    let locks_dir = root.locks_dir(repo);
    match paths::guard_dir_chain(root.path(), &locks_dir, fs, root.expected_uid())? {
        paths::Presence::Absent => return Ok(()),
        paths::Presence::Present => {}
    }
    let rd = match std::fs::read_dir(&locks_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(LaneError::Io(e)),
    };
    for entry in rd {
        let entry = entry.map_err(LaneError::Io)?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("lock") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if stem == self_lane {
            continue;
        }
        // Guarded read: a symlinked / malformed / identity-inconsistent sibling fails closed.
        let Some(rec) = record::read_claim(&path, root.path(), root.expected_uid(), fs)? else {
            continue; // transient NotFound — sibling vanished mid-scan
        };
        if now >= rec.expires_at {
            continue; // expired siblings do not reserve their target
        }
        let other_norm = rec.target_normalized.or(rec.target);
        if let Some(on) = other_norm {
            let other = Target::from_normalized(&on);
            if target.overlaps(&other) {
                return Err(LaneError::Refused(RefusedReason::TargetOverlap));
            }
        }
    }
    Ok(())
}

/// Combine an optional reconciliation warning with an optional post-mutation audit warning.
pub(crate) fn combine_warnings(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(format!("{a}; {b}")),
        (Some(w), None) | (None, Some(w)) => Some(w),
        (None, None) => None,
    }
}

fn describe_dangling(intent: &audit::AuditEvent, disp: audit::IntentDisposition) -> String {
    let disposition = match disp {
        audit::IntentDisposition::Applied => "applied",
        audit::IntentDisposition::NotApplied => "not-applied",
        audit::IntentDisposition::Indeterminate => "indeterminate",
    };
    format!(
        "dangling {} intent (op_id={}) on lane {}: {disposition}",
        intent.op.as_deref().unwrap_or("?"),
        intent.op_id.as_deref().unwrap_or("?"),
        intent.lane,
    )
}

/// Reconcile dangling intents for `lane` before a mutation (§ defect 4). Blocks (fail
/// closed, exit 2) on a genuinely indeterminate intent; returns a structured warning for
/// an applied/not-applied disposition. The lock files are the source of truth; a
/// completion is never fabricated and no repair occurs.
pub(crate) fn reconcile_for_mutation(
    root: &LaneRoot,
    repo: &str,
    lane: &str,
    fs: &dyn FsOps,
) -> Result<Option<String>, LaneError> {
    let events = match audit::read_validated_events(
        &root.audit_path(repo),
        root.path(),
        root.expected_uid(),
        fs,
    )? {
        Some(e) => e,
        None => return Ok(None),
    };
    let dangling = audit::dangling_intents(&events, lane);
    if dangling.is_empty() {
        return Ok(None);
    }
    // A dangling intent + an unreadable lock is itself indeterminate → fail closed.
    let current = match record::read_claim(&root.lock_path(repo, lane), root.path(), root.expected_uid(), fs) {
        Ok(opt) => opt,
        Err(_) => {
            return Err(LaneError::Identity(format!(
                "indeterminate: lane {lane} has a dangling intent and an unreadable lock; resolve manually"
            )))
        }
    };
    let mut notes = Vec::new();
    for it in dangling {
        match audit::classify_intent(it, current.as_ref()) {
            audit::IntentDisposition::Indeterminate => {
                return Err(LaneError::Identity(format!(
                    "indeterminate {}; lock matches neither outcome — resolve manually",
                    describe_dangling(it, audit::IntentDisposition::Indeterminate)
                )))
            }
            d => notes.push(format!("reconciled {}", describe_dangling(it, d))),
        }
    }
    Ok(if notes.is_empty() {
        None
    } else {
        Some(notes.join("; "))
    })
}

/// Reconcile dangling intents for a read-only `status` (§ defect 4). Never blocks:
/// describes every disposition (including indeterminate) as a warning. A malformed audit
/// record is noted but does not fail the read; a tampered audit log fails closed.
pub(crate) fn reconcile_for_status(
    root: &LaneRoot,
    repo: &str,
    lane: &str,
    current: Option<&ClaimRecord>,
    fs: &dyn FsOps,
) -> Result<Option<String>, LaneError> {
    let events = match audit::read_validated_events(
        &root.audit_path(repo),
        root.path(),
        root.expected_uid(),
        fs,
    ) {
        Ok(Some(e)) => e,
        Ok(None) => return Ok(None),
        Err(LaneError::Malformed { .. }) => {
            return Ok(Some(
                "audit stream has a malformed record; reconciliation skipped".into(),
            ))
        }
        Err(e) => return Err(e),
    };
    let dangling = audit::dangling_intents(&events, lane);
    if dangling.is_empty() {
        return Ok(None);
    }
    let notes: Vec<String> = dangling
        .into_iter()
        .map(|it| describe_dangling(it, audit::classify_intent(it, current)))
        .collect();
    Ok(Some(notes.join("; ")))
}

/// Best-effort audit of a claim refusal / malformed rejection (§ defect 5). NEVER changes
/// the primary error. Returns `Some(non_secret_warning)` iff the audit append FAILED, so
/// the caller can surface it as an `audit_warning` without altering the exit code / reason.
pub(crate) fn audit_refusal(
    sink: &dyn audit::AuditSink,
    repo: &str,
    lane: &str,
    instance: &str,
    now: DateTime<Utc>,
    err: &LaneError,
) -> Option<String> {
    use audit::{AuditEvent, AuditEventKind, AuditOutcome};
    let (kind, outcome, reason) = match err {
        LaneError::Refused(RefusedReason::ActiveHeld) => (
            AuditEventKind::ClaimRefused,
            AuditOutcome::Refused,
            "active_held",
        ),
        LaneError::Refused(RefusedReason::TargetOverlap) => (
            AuditEventKind::ClaimRefused,
            AuditOutcome::Refused,
            "target_overlap",
        ),
        LaneError::Malformed { .. } => {
            (AuditEventKind::Malformed, AuditOutcome::Error, "malformed")
        }
        LaneError::Identity(_) => (AuditEventKind::Malformed, AuditOutcome::Error, "identity"),
        _ => return None,
    };
    let mut e = AuditEvent::new(kind, repo, lane, instance, outcome, now);
    e.op_id = Some(audit::next_op_id());
    e.reason = Some(reason.to_string());
    // The kind() is a non-secret enum (e.g. "permission denied") — no path/contents leak.
    match sink.append(&e, false) {
        Ok(()) => None,
        Err(io) => Some(format!("refusal audit append failed: {}", io.kind())),
    }
}

/// Test-only hook: while holding the lane mutex, sleep `$LANE_TEST_HOLD_LANE_MUTEX_MS`
/// so a sibling process can observe contention / be SIGKILLed mid-hold. Compiled out of
/// release builds (no production surface).
#[cfg(debug_assertions)]
pub(crate) fn test_hold_after_lane_mutex() {
    if let Ok(ms) = std::env::var("LANE_TEST_HOLD_LANE_MUTEX_MS") {
        if let Ok(ms) = ms.parse::<u64>() {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
    }
}
#[cfg(not(debug_assertions))]
pub(crate) fn test_hold_after_lane_mutex() {}

// ---------------------------------------------------------------------------
// Versioned JSON envelope (§S2.11) — exactly one per post-parse exit path.
// ---------------------------------------------------------------------------

/// The closed set of envelope outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    Refused,
    Released,
    NotHeld,
    Error,
}

/// Per-verb `data` payload (serialized inline; untagged is serialize-only here).
#[derive(Serialize)]
#[serde(untagged)]
enum VerbData {
    Claim {
        lane: String,
        instance: String,
        expires_at: DateTime<Utc>,
        forced: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        prior_instance: Option<String>,
    },
    Renew {
        lane: String,
        expires_at: DateTime<Utc>,
    },
    Handoff {
        lane: String,
        claim_status: crate::model::ClaimStatus,
        expires_at: DateTime<Utc>,
    },
    Release {
        lane: String,
        present: bool,
    },
    // Boxed: a `ClaimRecord` is far larger than the other variants (clippy large_enum_variant).
    Status(Box<StatusData>),
    List {
        rows: Vec<StatusData>,
    },
}

#[derive(Serialize)]
struct Envelope {
    schema_version: u32,
    ok: bool,
    verb: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lane: Option<String>,
    outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<Reason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    audit_warning: Option<String>,
    // `data` is intentionally NOT skipped: it is `null` on refused/error and a struct on success.
    data: Option<VerbData>,
}

/// Internal CLI-runner error carrier: the authoritative [`LaneError`] plus an optional
/// non-secret audit-degradation warning. Exit code / `Reason` always come from `error`;
/// the warning is surfaced (JSON `audit_warning` / human stderr) without altering them.
#[derive(Debug)]
pub struct CommandError {
    pub error: LaneError,
    pub audit_warning: Option<String>,
}

impl From<LaneError> for CommandError {
    fn from(error: LaneError) -> Self {
        Self {
            error,
            audit_warning: None,
        }
    }
}

/// Render exactly one envelope (or human line) and return the process exit code.
fn emit(
    json: bool,
    verb: &'static str,
    repo: Option<String>,
    lane: Option<String>,
    result: Result<(Outcome, Option<VerbData>, Option<String>), CommandError>,
) -> i32 {
    match result {
        Ok((outcome, data, warn)) => {
            if json {
                let env = Envelope {
                    schema_version: 1,
                    ok: true,
                    verb,
                    repo,
                    lane,
                    outcome,
                    reason: None,
                    audit_warning: warn,
                    data,
                };
                println!(
                    "{}",
                    serde_json::to_string(&env).expect("envelope serializes")
                );
            } else {
                println!(
                    "{}",
                    human_success(verb, outcome, lane.as_deref(), data.as_ref())
                );
                if let Some(w) = warn {
                    eprintln!("lane: audit warning: {w}");
                }
            }
            0
        }
        Err(ce) => {
            let outcome = match &ce.error {
                LaneError::Refused(_) => Outcome::Refused,
                _ => Outcome::Error,
            };
            if json {
                let env = Envelope {
                    schema_version: 1,
                    ok: false,
                    verb,
                    repo,
                    lane,
                    outcome,
                    reason: Some(ce.error.reason()),
                    audit_warning: ce.audit_warning.clone(),
                    data: None,
                };
                println!(
                    "{}",
                    serde_json::to_string(&env).expect("envelope serializes")
                );
            } else {
                eprint!("{}", error_stderr(&ce.error, ce.audit_warning.as_deref()));
            }
            ce.error.exit_code()
        }
    }
}

/// The human-readable stderr block for an error: the primary error, then (if present) the
/// non-secret audit-degradation warning on its own line. The original error is never elided.
fn error_stderr(err: &LaneError, audit_warning: Option<&str>) -> String {
    let mut s = format!("lane: {err}\n");
    if let Some(w) = audit_warning {
        s.push_str(&format!("lane: audit warning: {w}\n"));
    }
    s
}

fn human_success(
    verb: &str,
    outcome: Outcome,
    lane: Option<&str>,
    data: Option<&VerbData>,
) -> String {
    let lane = lane.unwrap_or("-");
    match (verb, data) {
        (
            "claim",
            Some(VerbData::Claim {
                expires_at,
                forced,
                prior_instance,
                ..
            }),
        ) => {
            let f = if *forced { " (forced)" } else { "" };
            let p = prior_instance
                .as_deref()
                .map(|p| format!(" [took over {p}]"))
                .unwrap_or_default();
            format!("claimed {lane}{f}{p}; expires {}", expires_at.to_rfc3339())
        }
        ("renew", Some(VerbData::Renew { expires_at, .. })) => {
            format!("renewed {lane}; expires {}", expires_at.to_rfc3339())
        }
        ("handoff", Some(VerbData::Handoff { expires_at, .. })) => {
            format!(
                "handoff {lane}; claim stays held, expires {}",
                expires_at.to_rfc3339()
            )
        }
        ("release", _) => match outcome {
            Outcome::Released => format!("released {lane}"),
            _ => format!("{lane} was not held"),
        },
        ("status", Some(VerbData::Status(sd))) => {
            if sd.present {
                let ss = sd
                    .stale_state
                    .map(|s| format!("{s:?}").to_lowercase())
                    .unwrap_or_else(|| "unknown".into());
                format!("{lane}: held ({ss})")
            } else {
                format!("{lane}: not held")
            }
        }
        ("list", Some(VerbData::List { rows })) => {
            if rows.is_empty() {
                "(no claims)".to_string()
            } else {
                let mut out = String::new();
                for r in rows {
                    if let Some(rec) = &r.record {
                        let ss = r
                            .stale_state
                            .map(|s| format!("{s:?}").to_lowercase())
                            .unwrap_or_else(|| "unknown".into());
                        out.push_str(&format!(
                            "{:<16} {:<20} {:<14} {}\n",
                            rec.repo, rec.lane, ss, rec.instance
                        ));
                    }
                }
                out.trim_end().to_string()
            }
        }
        _ => format!("{verb} ok"),
    }
}

// ---------------------------------------------------------------------------
// CLI runners — resolve environment, call the core, render the envelope, exit code.
// ---------------------------------------------------------------------------

use crate::cli::{ClaimArgs, HandoffArgs, ListArgs, ReleaseArgs, RenewArgs, StatusArgs};

fn home_env() -> Option<String> {
    std::env::var("HOME").ok()
}

fn resolve_root(
    arg: Option<PathBuf>,
    home: Option<&str>,
    fs: &dyn FsOps,
) -> Result<LaneRoot, LaneError> {
    let raw = paths::resolve_raw_root(arg, std::env::var("LANE_ROOT").ok(), home)?;
    LaneRoot::resolve(&raw, home, fs)
}

fn require_instance(arg: Option<String>) -> Result<String, LaneError> {
    arg.or_else(|| std::env::var("LANE_INSTANCE").ok())
        .ok_or_else(|| LaneError::Identity("--instance is required (or set $LANE_INSTANCE)".into()))
}

/// `lane claim` runner.
pub fn run_claim(args: &ClaimArgs) -> i32 {
    run_claim_at(args, Utc::now())
}

fn run_claim_at(args: &ClaimArgs, now: DateTime<Utc>) -> i32 {
    let repo = Some(args.repo.clone());
    let lane = Some(args.lane.clone());
    let fs = StdFs;
    let home = home_env();
    let result = (|| -> Result<(Outcome, Option<VerbData>, Option<String>), CommandError> {
        let instance = require_instance(args.instance.clone())?;
        let root = resolve_root(args.lane_root.clone(), home.as_deref(), &fs)?;
        let sink = audit::StdAuditSink::new(root.audit_path(&args.repo), root.expected_uid());
        let params = ClaimParams {
            repo: args.repo.clone(),
            lane: args.lane.clone(),
            instance,
            target: args.target.clone(),
            home: home.clone(),
            ttl_hours: args.ttl_hours,
            note: args.note.clone(),
            force: args.force,
        };
        let s = claim::claim_core(&root, &params, now, &fs, &sink)?;
        let data = VerbData::Claim {
            lane: s.lane,
            instance: s.instance,
            expires_at: s.expires_at,
            forced: s.forced,
            prior_instance: s.prior_instance,
        };
        Ok((Outcome::Ok, Some(data), s.audit_warning))
    })();
    emit(args.json, "claim", repo, lane, result)
}

/// `lane renew` runner.
pub fn run_renew(args: &RenewArgs) -> i32 {
    run_renew_at(args, Utc::now())
}

fn run_renew_at(args: &RenewArgs, now: DateTime<Utc>) -> i32 {
    let repo = Some(args.repo.clone());
    let lane = Some(args.lane.clone());
    let fs = StdFs;
    let home = home_env();
    let result = (|| -> Result<(Outcome, Option<VerbData>, Option<String>), CommandError> {
        let instance = require_instance(args.instance.clone())?;
        let root = resolve_root(args.lane_root.clone(), home.as_deref(), &fs)?;
        let sink = audit::StdAuditSink::new(root.audit_path(&args.repo), root.expected_uid());
        let params = RenewParams {
            repo: args.repo.clone(),
            lane: args.lane.clone(),
            instance,
            ttl_hours: args.ttl_hours,
        };
        let s = renew_release::renew_core(&root, &params, now, &fs, &sink)?;
        let data = VerbData::Renew {
            lane: s.lane,
            expires_at: s.expires_at,
        };
        Ok((Outcome::Ok, Some(data), s.audit_warning))
    })();
    emit(args.json, "renew", repo, lane, result)
}

/// `lane handoff` runner.
pub fn run_handoff(args: &HandoffArgs) -> i32 {
    run_handoff_at(args, Utc::now())
}

fn run_handoff_at(args: &HandoffArgs, now: DateTime<Utc>) -> i32 {
    let repo = Some(args.repo.clone());
    let lane = Some(args.lane.clone());
    let fs = StdFs;
    let home = home_env();
    let result = (|| -> Result<(Outcome, Option<VerbData>, Option<String>), CommandError> {
        let instance = require_instance(args.instance.clone())?;
        let root = resolve_root(args.lane_root.clone(), home.as_deref(), &fs)?;
        let sink = audit::StdAuditSink::new(root.audit_path(&args.repo), root.expected_uid());
        let params = HandoffParams {
            repo: args.repo.clone(),
            lane: args.lane.clone(),
            instance,
            note: args.note.clone(),
        };
        let s = renew_release::handoff_core(&root, &params, now, &fs, &sink)?;
        let data = VerbData::Handoff {
            lane: s.lane,
            claim_status: crate::model::ClaimStatus::Handoff,
            expires_at: s.expires_at,
        };
        Ok((Outcome::Ok, Some(data), s.audit_warning))
    })();
    emit(args.json, "handoff", repo, lane, result)
}

/// `lane release` runner.
pub fn run_release(args: &ReleaseArgs) -> i32 {
    run_release_at(args, Utc::now())
}

fn run_release_at(args: &ReleaseArgs, now: DateTime<Utc>) -> i32 {
    let repo = Some(args.repo.clone());
    let lane = Some(args.lane.clone());
    let fs = StdFs;
    let home = home_env();
    let result = (|| -> Result<(Outcome, Option<VerbData>, Option<String>), CommandError> {
        let instance = require_instance(args.instance.clone())?;
        let root = resolve_root(args.lane_root.clone(), home.as_deref(), &fs)?;
        let sink = audit::StdAuditSink::new(root.audit_path(&args.repo), root.expected_uid());
        let params = ReleaseParams {
            repo: args.repo.clone(),
            lane: args.lane.clone(),
            instance,
        };
        let s = renew_release::release_core(&root, &params, now, &fs, &sink)?;
        let outcome = if s.present {
            Outcome::Released
        } else {
            Outcome::NotHeld
        };
        let data = VerbData::Release {
            lane: args.lane.clone(),
            present: s.present,
        };
        Ok((outcome, Some(data), s.audit_warning))
    })();
    emit(args.json, "release", repo, lane, result)
}

/// `lane status` runner (read-only).
pub fn run_status(args: &StatusArgs) -> i32 {
    run_status_at(args, Utc::now())
}

fn run_status_at(args: &StatusArgs, now: DateTime<Utc>) -> i32 {
    let repo = Some(args.repo.clone());
    let lane = Some(args.lane.clone());
    let fs = StdFs;
    let home = home_env();
    let result = (|| -> Result<(Outcome, Option<VerbData>, Option<String>), CommandError> {
        validate_name("repo", &args.repo)?;
        validate_name("lane", &args.lane)?;
        let root = resolve_root(args.lane_root.clone(), home.as_deref(), &fs)?;
        let (sd, warn) = renew_release::status_core(&root, &args.repo, &args.lane, now, &fs)?;
        let outcome = if sd.present {
            Outcome::Ok
        } else {
            Outcome::NotHeld
        };
        Ok((outcome, Some(VerbData::Status(Box::new(sd))), warn))
    })();
    emit(args.json, "status", repo, lane, result)
}

/// `lane list` runner (read-only).
pub fn run_list(args: &ListArgs) -> i32 {
    run_list_at(args, Utc::now())
}

fn run_list_at(args: &ListArgs, now: DateTime<Utc>) -> i32 {
    let repo = args.repo.clone();
    let fs = StdFs;
    let home = home_env();
    let result = (|| -> Result<(Outcome, Option<VerbData>, Option<String>), CommandError> {
        if let Some(r) = &args.repo {
            validate_name("repo", r)?;
        }
        let root = resolve_root(args.lane_root.clone(), home.as_deref(), &fs)?;
        let rows = renew_release::list_core(&root, args.repo.as_deref(), now, &fs)?;
        Ok((Outcome::Ok, Some(VerbData::List { rows }), None))
    })();
    emit(args.json, "list", repo, None, result)
}

/// Liveness for read verbs is always `Unknown` in Slice 2 (no overseer/tmux), so
/// `status`/`list` never classify a claim `Orphaned`.
pub(crate) fn read_liveness() -> Liveness {
    Liveness::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_stderr_surfaces_audit_warning() {
        let s = error_stderr(
            &LaneError::Refused(RefusedReason::ActiveHeld),
            Some("refusal audit append failed: permission denied"),
        );
        assert!(s.starts_with("lane: "));
        assert!(s.contains("audit warning: refusal audit append failed: permission denied"));
    }

    #[test]
    fn error_stderr_without_warning_has_no_warning_line() {
        let s = error_stderr(&LaneError::Refused(RefusedReason::ActiveHeld), None);
        assert!(!s.contains("audit warning"));
    }

    #[test]
    fn command_error_from_lane_error_carries_no_warning() {
        let ce: CommandError = LaneError::Refused(RefusedReason::NotOwner).into();
        assert!(ce.audit_warning.is_none());
        assert_eq!(ce.error.exit_code(), 1);
    }

    #[test]
    fn combine_warnings_joins() {
        assert_eq!(combine_warnings(None, None), None);
        assert_eq!(
            combine_warnings(Some("a".into()), None).as_deref(),
            Some("a")
        );
        assert_eq!(
            combine_warnings(Some("a".into()), Some("b".into())).as_deref(),
            Some("a; b")
        );
    }
}
