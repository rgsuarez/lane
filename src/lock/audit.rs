//! The append-only, locked, write-ahead, **object-guarded** audit log (§S2.10 +
//! fail-closed remediation).
//!
//! Every append routes through the shared object guard (reject symlink / non-regular /
//! wrong-owner / `(dev, ino)` swap), acquires an exclusive advisory lock on `audit.log`,
//! writes one newline-terminated JSON line at end-of-file, and releases — the lock (not
//! `O_APPEND`) is the no-interleave guarantee. Destructive operations (claim-takeover /
//! release) write an fsync'd `intent` BEFORE the mutation and a `completion` after; a
//! non-destructive operation (free-lane claim, renew) writes one terminal event. Intent
//! and completion records carry an `op` discriminator (`takeover` / `release`) so a crash
//! that leaves a dangling intent can be reconciled deterministically against the lock
//! files (the source of truth).
//!
//! Recovery is conservative and validates the **entire** complete stream: any malformed
//! newline-terminated record anywhere fails closed (exit 2) before a mutation; only a
//! final non-newline fragment is quarantined (to a guarded `audit.recovered/` file) and
//! truncated, then an `audit_recovery` event is appended. Earlier valid records are never
//! altered, and a completion is never fabricated.

use std::collections::HashSet;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::LaneError;
use crate::lock::{paths, record, FsOps, StdFs};
use crate::model::ClaimRecord;

static OP_COUNTER: AtomicU64 = AtomicU64::new(0);
const AUDIT_MODE: u32 = 0o600;
const LOCK_MAX_WAIT: Duration = Duration::from_millis(3000);

/// A collision-resistant per-host operation id: `"<unix_nanos>-<pid>-<counter>"`.
pub fn next_op_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let n = OP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos}-{pid}-{n}")
}

/// The closed set of audit event kinds (§S2.10; Slice 3 adds the spec-anticipated
/// `handoff` — a non-destructive owner-only status flip, audited like `renew` with a
/// single terminal event, no intent/completion pair).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEventKind {
    Claim,
    ClaimRefused,
    Renew,
    Release,
    Handoff,
    Intent,
    Completion,
    Takeover,
    Malformed,
    AuditRecovery,
}

/// The closed set of audit outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    Ok,
    Refused,
    Error,
}

/// One audit record. The claim `note`, secrets, references, and PII are NEVER logged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub ts: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub op_id: Option<String>,
    /// Operation discriminator for intent/completion pairing across crashes
    /// (`takeover` | `release`). Backwards-compatible: absent on older records.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub op: Option<String>,
    pub event: AuditEventKind,
    pub repo: String,
    pub lane: String,
    pub instance: String,
    pub outcome: AuditOutcome,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub forced: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prior_instance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub was_malformed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ttl_hours: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub recovered_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub recovered_bytes: Option<u64>,
}

impl AuditEvent {
    /// A bare event with all optional fields unset.
    pub fn new(
        event: AuditEventKind,
        repo: &str,
        lane: &str,
        instance: &str,
        outcome: AuditOutcome,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            ts: now,
            op_id: None,
            op: None,
            event,
            repo: repo.to_string(),
            lane: lane.to_string(),
            instance: instance.to_string(),
            outcome,
            forced: None,
            prior_instance: None,
            was_malformed: None,
            reason: None,
            target: None,
            ttl_hours: None,
            recovered_path: None,
            recovered_bytes: None,
        }
    }
}

/// The injectable append seam. Production is [`StdAuditSink`]; tests inject failpoints.
pub trait AuditSink {
    /// Append one event. `fsync` forces durability (always set for destructive `intent`s).
    fn append(&self, event: &AuditEvent, fsync: bool) -> io::Result<()>;
}

/// The production sink: a guarded, locked, newline-terminated append to `audit.log`.
pub struct StdAuditSink {
    path: PathBuf,
    expected_uid: u32,
}

impl StdAuditSink {
    pub fn new(path: PathBuf, expected_uid: u32) -> Self {
        Self { path, expected_uid }
    }
}

impl AuditSink for StdAuditSink {
    fn append(&self, event: &AuditEvent, fsync: bool) -> io::Result<()> {
        let line = serde_json::to_string(event)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        // Route the open through the shared object guard (symlink/non-regular/wrong-owner/
        // TOCTOU). Map the guard's LaneError to io::Error for the trait surface.
        let file = paths::open_or_create_writer(&self.path, AUDIT_MODE, &StdFs, self.expected_uid)
            .map_err(lane_err_to_io)?;
        lock_with_retry(&file)?;
        let result = (|| {
            let mut f = &file;
            f.seek(SeekFrom::End(0))?;
            f.write_all(line.as_bytes())?;
            f.write_all(b"\n")?;
            if fsync {
                file.sync_all()?;
            }
            Ok(())
        })();
        let _ = file.unlock();
        result
    }
}

fn lane_err_to_io(e: LaneError) -> io::Error {
    match e {
        LaneError::Io(io) => io,
        other => io::Error::other(other.to_string()),
    }
}

/// Acquire the audit-file lock with bounded retry; returns `WouldBlock` on timeout.
fn lock_with_retry(file: &std::fs::File) -> io::Result<()> {
    use std::fs::TryLockError;
    let start = SystemTime::now();
    let mut backoff = Duration::from_millis(2);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(TryLockError::WouldBlock) => {
                if start.elapsed().unwrap_or_default() >= LOCK_MAX_WAIT {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "audit.log lock busy",
                    ));
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(50));
            }
            Err(TryLockError::Error(e)) => return Err(e),
        }
    }
}

/// Recover `audit.log` before any mutation appends, under the audit lock.
///
/// Object-guards the log (symlink/non-regular/wrong-owner/TOCTOU). Validates the ENTIRE
/// complete stream: any malformed newline-terminated record anywhere fails closed (exit
/// 2). Only a trailing fragment (no terminating newline) is quarantined to a guarded
/// `audit.recovered/` file and truncated, with an `audit_recovery` event appended.
pub fn recover_if_needed(
    path: &Path,
    repo: &str,
    lane: &str,
    instance: &str,
    now: DateTime<Utc>,
    fs: &dyn FsOps,
    expected_uid: u32,
) -> Result<(), LaneError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(LaneError::Io(e)),
    }
    // Guarded open (existing file → object guard; never creates here because it exists).
    let file = paths::open_or_create_writer(path, AUDIT_MODE, fs, expected_uid)?;
    lock_with_retry(&file).map_err(LaneError::Io)?;
    let result = recover_locked(&file, path, repo, lane, instance, now, fs, expected_uid);
    let _ = file.unlock();
    result
}

#[allow(clippy::too_many_arguments)]
fn recover_locked(
    file: &std::fs::File,
    path: &Path,
    repo: &str,
    lane: &str,
    instance: &str,
    now: DateTime<Utc>,
    fs: &dyn FsOps,
    expected_uid: u32,
) -> Result<(), LaneError> {
    let mut content = Vec::new();
    {
        let mut f = file;
        f.read_to_end(&mut content).map_err(LaneError::Io)?;
    }
    if content.is_empty() {
        return Ok(());
    }
    let last_nl = content.iter().rposition(|&b| b == b'\n');
    let (complete_end, has_fragment) = match last_nl {
        Some(i) => (i + 1, i + 1 < content.len()),
        None => (0, true), // no newline at all → the whole file is a fragment
    };

    // Validate EVERY complete record (not only the last): a newline-terminated malformed
    // record anywhere is evidence and fails closed (never truncated).
    for line in content[..complete_end].split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        if serde_json::from_slice::<AuditEvent>(line).is_err() {
            return Err(LaneError::Malformed {
                path: path.to_path_buf(),
                detail: "a newline-terminated audit record is malformed".into(),
            });
        }
    }

    if has_fragment {
        let fragment = &content[complete_end..];
        let rec_dir = path
            .parent()
            .ok_or_else(|| LaneError::Identity("audit.log has no parent".into()))?
            .join("audit.recovered");
        // Guarded directory + fragment-file creation.
        paths::ensure_dir_guarded(&rec_dir, fs, expected_uid)?;
        let frag_path = rec_dir.join(format!("{}.frag", next_op_id()));
        {
            let mut ff = paths::open_or_create_writer(&frag_path, AUDIT_MODE, fs, expected_uid)?;
            ff.write_all(fragment).map_err(LaneError::Io)?;
            ff.sync_all().map_err(LaneError::Io)?;
        }
        // `ensure_dir_guarded` already set 0700 on rec_dir; no extra unguarded chmod here.
        let frag_len = fragment.len() as u64;

        // Truncate the fragment (keep through the last complete newline) and fsync.
        file.set_len(complete_end as u64).map_err(LaneError::Io)?;
        file.sync_all().map_err(LaneError::Io)?;

        // Append an audit_recovery event under the same held lock.
        let mut ev = AuditEvent::new(
            AuditEventKind::AuditRecovery,
            repo,
            lane,
            instance,
            AuditOutcome::Ok,
            now,
        );
        ev.op_id = Some(next_op_id());
        ev.recovered_path = Some(frag_path.to_string_lossy().to_string());
        ev.recovered_bytes = Some(frag_len);
        let line = serde_json::to_string(&ev)
            .map_err(|e| LaneError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))?;
        let mut f = file;
        f.seek(SeekFrom::End(0)).map_err(LaneError::Io)?;
        f.write_all(line.as_bytes()).map_err(LaneError::Io)?;
        f.write_all(b"\n").map_err(LaneError::Io)?;
        file.sync_all().map_err(LaneError::Io)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Dangling-intent reconciliation (defects 3/4). The lock files are the source of truth;
// a completion is never fabricated, and no automatic repair occurs beyond fragment recovery.
// ---------------------------------------------------------------------------

/// Disposition of a dangling intent (an `intent` with no matching `completion`), judged
/// against the current lock record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentDisposition {
    /// The intended mutation took effect (lock matches the intent's outcome).
    Applied,
    /// The intended mutation did not take effect (lock matches the prior state).
    NotApplied,
    /// The lock matches neither expected state — genuinely ambiguous; mutations fail closed.
    Indeterminate,
}

/// Read and fully validate the complete audit stream (object-guarded, read-only). A
/// trailing non-newline fragment is ignored (not validated) here; complete records that
/// fail to parse fail closed (`Malformed`). `Ok(None)` if the log is absent.
pub fn read_validated_events(
    path: &Path,
    root: &Path,
    expected_uid: u32,
    fs: &dyn FsOps,
) -> Result<Option<Vec<AuditEvent>>, LaneError> {
    let Some(text) = record::read_guarded(path, root, expected_uid, fs)? else {
        return Ok(None);
    };
    let ends_nl = text.ends_with('\n');
    let parts: Vec<&str> = text.split('\n').collect();
    let n = parts.len();
    let mut events = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        // The last part is a trailing fragment when the text does not end with a newline.
        if i + 1 == n && !ends_nl {
            continue;
        }
        let ev: AuditEvent = serde_json::from_str(part).map_err(|e| LaneError::Malformed {
            path: path.to_path_buf(),
            detail: format!("unparseable audit record: {e}"),
        })?;
        events.push(ev);
    }
    Ok(Some(events))
}

/// Intents (event==Intent) for `lane` whose `op_id` has no matching `completion`.
pub fn dangling_intents<'a>(events: &'a [AuditEvent], lane: &str) -> Vec<&'a AuditEvent> {
    let completed: HashSet<&str> = events
        .iter()
        .filter(|e| e.event == AuditEventKind::Completion)
        .filter_map(|e| e.op_id.as_deref())
        .collect();
    events
        .iter()
        .filter(|e| e.event == AuditEventKind::Intent && e.lane == lane)
        .filter(|e| e.op_id.as_deref().is_none_or(|id| !completed.contains(id)))
        .collect()
}

/// Classify a dangling intent against the current lock record (the source of truth).
pub fn classify_intent(intent: &AuditEvent, current: Option<&ClaimRecord>) -> IntentDisposition {
    match intent.op.as_deref() {
        Some("takeover") => match current {
            Some(rec) if rec.instance == intent.instance => IntentDisposition::Applied,
            Some(rec) if intent.prior_instance.as_deref() == Some(rec.instance.as_str()) => {
                IntentDisposition::NotApplied
            }
            _ => IntentDisposition::Indeterminate,
        },
        Some("release") => match current {
            None => IntentDisposition::Applied,
            Some(rec) if rec.instance == intent.instance => IntentDisposition::NotApplied,
            _ => IntentDisposition::Indeterminate,
        },
        _ => IntentDisposition::Indeterminate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(
        op: &str,
        lane: &str,
        instance: &str,
        prior: Option<&str>,
        op_id: &str,
    ) -> AuditEvent {
        let mut e = AuditEvent::new(
            AuditEventKind::Intent,
            "ops",
            lane,
            instance,
            AuditOutcome::Ok,
            Utc::now(),
        );
        e.op = Some(op.into());
        e.op_id = Some(op_id.into());
        e.prior_instance = prior.map(str::to_string);
        e
    }
    fn completion(op_id: &str) -> AuditEvent {
        let mut e = AuditEvent::new(
            AuditEventKind::Completion,
            "ops",
            "x",
            "i",
            AuditOutcome::Ok,
            Utc::now(),
        );
        e.op_id = Some(op_id.into());
        e
    }
    fn rec(instance: &str) -> ClaimRecord {
        ClaimRecord {
            schema_version: Some(1),
            lane: "demo".into(),
            repo: "ops".into(),
            instance: instance.into(),
            pid: None,
            target: None,
            target_normalized: None,
            note: None,
            claimed_at: Utc::now(),
            updated_at: Utc::now(),
            expires_at: Utc::now(),
            ttl_hours: 12.0,
            linear_key: None,
            branch: None,
            role: None,
            pr_url: None,
            gate: None,
            plan_path: None,
            claim_status: None,
            session_ref: None,
        }
    }

    #[test]
    fn op_id_is_monotonic_per_process() {
        assert_ne!(next_op_id(), next_op_id());
    }

    #[test]
    fn event_serializes_without_note_field() {
        let ev = AuditEvent::new(
            AuditEventKind::Claim,
            "ops",
            "lqos-1",
            "inst",
            AuditOutcome::Ok,
            Utc::now(),
        );
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains("\"event\":\"claim\""));
        assert!(!j.contains("note"));
        assert!(!j.contains("prior_instance"));
    }

    #[test]
    fn completion_resolves_its_intent() {
        let events = vec![
            intent("takeover", "demo", "new", Some("old"), "op1"),
            completion("op1"),
        ];
        assert!(dangling_intents(&events, "demo").is_empty());
    }

    #[test]
    fn unmatched_intent_is_dangling() {
        let events = vec![intent("takeover", "demo", "new", Some("old"), "op1")];
        assert_eq!(dangling_intents(&events, "demo").len(), 1);
        assert!(dangling_intents(&events, "other").is_empty());
    }

    #[test]
    fn takeover_disposition() {
        let i = intent("takeover", "demo", "new", Some("old"), "op1");
        assert_eq!(
            classify_intent(&i, Some(&rec("new"))),
            IntentDisposition::Applied
        );
        assert_eq!(
            classify_intent(&i, Some(&rec("old"))),
            IntentDisposition::NotApplied
        );
        assert_eq!(
            classify_intent(&i, Some(&rec("stranger"))),
            IntentDisposition::Indeterminate
        );
        assert_eq!(classify_intent(&i, None), IntentDisposition::Indeterminate);
    }

    #[test]
    fn release_disposition() {
        let i = intent("release", "demo", "owner", None, "op2");
        assert_eq!(classify_intent(&i, None), IntentDisposition::Applied);
        assert_eq!(
            classify_intent(&i, Some(&rec("owner"))),
            IntentDisposition::NotApplied
        );
        assert_eq!(
            classify_intent(&i, Some(&rec("stranger"))),
            IntentDisposition::Indeterminate
        );
    }
}
