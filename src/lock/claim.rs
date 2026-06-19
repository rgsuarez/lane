//! The claim algorithm (§S2.5 + fail-closed remediation): lane-first, atomic-visible,
//! force-safe.
//!
//! Acquires the lane mutex, recovers + reconciles the audit log, classifies the existing
//! lock through the guarded reader (a symlinked/wrong-owner same-lane lock fails closed
//! even under `--force`; only a malformed *regular* record is force-takeable), scans
//! sibling targets for overlap under the target mutex (always — even under `--force`),
//! then writes atomically: a free lane via exclusive `hard_link`; a takeover via fsync'd
//! `intent{op:takeover}` → `rename`-over → `completion{op:takeover}`. Refusals and
//! malformed rejections emit best-effort `claim_refused` / `malformed` audit events that
//! never alter the primary exit code / reason.

use std::path::Path;

use chrono::{DateTime, Utc};

use crate::error::{LaneError, RefusedReason};
use crate::lock::audit::{next_op_id, AuditEvent, AuditEventKind, AuditOutcome, AuditSink};
use crate::lock::mutex::LaneMutex;
use crate::lock::paths::LaneRoot;
use crate::lock::target::Target;
use crate::lock::{
    audit_refusal, combine_warnings, reconcile_for_mutation, record, scan_overlap,
    test_hold_after_lane_mutex, ttl_to_duration, validate_instance, validate_name, validate_note,
    validate_ttl, write_temp, ClaimParams, ClaimSuccess, CommandError, FsOps, DEFAULT_TTL_HOURS,
};
use crate::model::ClaimRecord;

/// How an existing `<lane>.lock` resolves under the decision table (codifies I3/I9).
enum Decision {
    /// No (readable) prior — create a fresh record via exclusive `hard_link`.
    Create,
    /// Replace a prior record via `rename`-over (write-ahead audited).
    Takeover {
        prior_instance: Option<String>,
        was_malformed: bool,
    },
}

/// Claim a lane. See the module docs for the full sequence and invariants.
pub fn claim_core(
    root: &LaneRoot,
    p: &ClaimParams,
    now: DateTime<Utc>,
    fs: &dyn FsOps,
    audit: &dyn AuditSink,
) -> Result<ClaimSuccess, CommandError> {
    validate_name("repo", &p.repo)?;
    validate_name("lane", &p.lane)?;
    validate_instance(&p.instance)?;
    let ttl = p.ttl_hours.unwrap_or(DEFAULT_TTL_HOURS);
    validate_ttl(ttl)?;
    if let Some(n) = &p.note {
        validate_note(n)?;
    }

    // WRITE PATH: create state dirs under the object guard.
    root.ensure_write_dirs(&p.repo, fs)?;
    let uid = root.expected_uid();

    // Canonical target (rejects relative/dotdot/root-ancestry).
    let target = match &p.target {
        Some(t) => Some(Target::resolve(t, p.home.as_deref(), root.path())?),
        None => None,
    };

    // Recover any crash-truncated audit fragment + full-stream validate before mutating.
    crate::lock::audit::recover_if_needed(
        &root.audit_path(&p.repo),
        &p.repo,
        &p.lane,
        &p.instance,
        now,
        fs,
        uid,
    )?;

    // Lane mutex (held for the whole operation).
    let _lane_guard = LaneMutex::acquire(&root.lane_mutex_path(&p.repo, &p.lane), fs, uid)?;
    test_hold_after_lane_mutex();

    // Reconcile dangling intents for this lane (block on a genuinely indeterminate one).
    let recon = reconcile_for_mutation(root, &p.repo, &p.lane, fs)?;

    let lock_path = root.lock_path(&p.repo, &p.lane);
    let decision = match classify_existing(
        &lock_path,
        root.path(),
        &p.repo,
        &p.lane,
        now,
        p.force,
        fs,
        uid,
    ) {
        Ok(d) => d,
        Err(e) => {
            let audit_warning = audit_refusal(audit, &p.repo, &p.lane, &p.instance, now, &e);
            return Err(CommandError {
                error: e,
                audit_warning,
            });
        }
    };

    // Target mutex (second) + overlap scan — ALWAYS when targeted, even under --force.
    let _target_guard = match &target {
        Some(t) => {
            let g = LaneMutex::acquire(&root.target_mutex_path(&p.repo), fs, uid)?;
            if let Err(e) = scan_overlap(root, &p.repo, &p.lane, t, now, fs) {
                let audit_warning = audit_refusal(audit, &p.repo, &p.lane, &p.instance, now, &e);
                return Err(CommandError {
                    error: e,
                    audit_warning,
                });
            }
            Some(g)
        }
        None => None,
    };

    // Build the new record.
    let expires = now + ttl_to_duration(ttl);
    let target_norm = target.as_ref().map(|t| t.normalized().to_string());
    let record = ClaimRecord {
        schema_version: Some(1),
        lane: p.lane.clone(),
        repo: p.repo.clone(),
        instance: p.instance.clone(),
        pid: Some(std::process::id() as i64),
        target: target_norm.clone(),
        target_normalized: target_norm.clone(),
        note: p.note.clone(),
        claimed_at: now,
        updated_at: now,
        expires_at: expires,
        ttl_hours: ttl,
        linear_key: None,
        branch: None,
        role: None,
        pr_url: None,
        gate: None,
        plan_path: None,
        claim_status: None,
        session_ref: None,
    };

    let temp = write_temp(root, &p.repo, &p.lane, &record, fs, uid)?;

    match decision {
        Decision::Create => {
            // Exclusive create: hard_link fails if the destination exists.
            if let Err(e) = fs.hard_link(&temp, &lock_path) {
                let _ = fs.remove_file(&temp);
                return Err(LaneError::Io(e).into());
            }
            let _ = fs.remove_file(&temp);
            let mut ev = AuditEvent::new(
                AuditEventKind::Claim,
                &p.repo,
                &p.lane,
                &p.instance,
                AuditOutcome::Ok,
                now,
            );
            ev.op_id = Some(next_op_id());
            ev.ttl_hours = Some(ttl);
            ev.target = target_norm;
            let post = audit.append(&ev, false).err().map(|e| e.to_string());
            Ok(ClaimSuccess {
                lane: p.lane.clone(),
                instance: p.instance.clone(),
                expires_at: expires,
                forced: false,
                prior_instance: None,
                audit_warning: combine_warnings(recon, post),
            })
        }
        Decision::Takeover {
            prior_instance,
            was_malformed,
        } => {
            // Write-ahead: fsync an `intent{op:takeover}` BEFORE the destructive rename.
            let op_id = next_op_id();
            let mut intent = AuditEvent::new(
                AuditEventKind::Intent,
                &p.repo,
                &p.lane,
                &p.instance,
                AuditOutcome::Ok,
                now,
            );
            intent.op_id = Some(op_id.clone());
            intent.op = Some("takeover".into());
            intent.forced = Some(p.force);
            intent.prior_instance = prior_instance.clone();
            intent.was_malformed = Some(was_malformed);
            intent.target = target_norm.clone();
            if let Err(e) = audit.append(&intent, true) {
                // Intent failed → abort BEFORE mutating; the prior claim is intact.
                let _ = fs.remove_file(&temp);
                return Err(LaneError::Io(e).into());
            }

            match fs.rename(&temp, &lock_path) {
                Ok(()) => {
                    let mut comp = AuditEvent::new(
                        AuditEventKind::Completion,
                        &p.repo,
                        &p.lane,
                        &p.instance,
                        AuditOutcome::Ok,
                        now,
                    );
                    comp.op_id = Some(op_id);
                    comp.op = Some("takeover".into());
                    comp.forced = Some(p.force);
                    comp.prior_instance = prior_instance.clone();
                    comp.was_malformed = Some(was_malformed);
                    let post = audit.append(&comp, false).err().map(|e| e.to_string());
                    Ok(ClaimSuccess {
                        lane: p.lane.clone(),
                        instance: p.instance.clone(),
                        expires_at: expires,
                        forced: p.force,
                        prior_instance,
                        audit_warning: combine_warnings(recon, post),
                    })
                }
                Err(e) => {
                    // Mutation failed after the intent ⇒ record completion{error}; prior intact.
                    let mut comp = AuditEvent::new(
                        AuditEventKind::Completion,
                        &p.repo,
                        &p.lane,
                        &p.instance,
                        AuditOutcome::Error,
                        now,
                    );
                    comp.op_id = Some(op_id);
                    comp.op = Some("takeover".into());
                    comp.reason = Some(format!("rename failed: {e}"));
                    let _ = audit.append(&comp, true);
                    let _ = fs.remove_file(&temp);
                    Err(LaneError::Io(e).into())
                }
            }
        }
    }
}

/// Classify the existing `<lane>.lock` per the decision table, through the guarded reader.
/// A symlink / non-regular / wrong-owner / TOCTOU same-lane object fails closed **even
/// under `--force`** (force never bypasses the object guard); only a malformed *regular*
/// record (or an identity-inconsistent one) is force-takeable.
#[allow(clippy::too_many_arguments)]
fn classify_existing(
    path: &Path,
    root: &Path,
    repo: &str,
    lane: &str,
    now: DateTime<Utc>,
    force: bool,
    fs: &dyn FsOps,
    expected_uid: u32,
) -> Result<Decision, LaneError> {
    // Object guard (interior-symlink chain + symlink/non-regular/wrong-owner/TOCTOU) runs
    // before any parse — so --force cannot bypass it.
    let text = match record::read_guarded(path, root, expected_uid, fs)? {
        None => return Ok(Decision::Create),
        Some(t) => t,
    };
    match serde_json::from_str::<ClaimRecord>(&text) {
        Ok(rec) => {
            let ident_ok = rec.repo == repo && rec.lane == lane;
            if !ident_ok {
                if force {
                    Ok(Decision::Takeover {
                        prior_instance: Some(rec.instance),
                        was_malformed: true,
                    })
                } else {
                    Err(LaneError::Identity(format!(
                        "existing claim {} is identity-inconsistent",
                        path.display()
                    )))
                }
            } else if now < rec.expires_at {
                if force {
                    Ok(Decision::Takeover {
                        prior_instance: Some(rec.instance),
                        was_malformed: false,
                    })
                } else {
                    Err(LaneError::Refused(RefusedReason::ActiveHeld))
                }
            } else {
                // Expired — takeable without --force.
                Ok(Decision::Takeover {
                    prior_instance: Some(rec.instance),
                    was_malformed: false,
                })
            }
        }
        Err(_) => {
            if force {
                Ok(Decision::Takeover {
                    prior_instance: None,
                    was_malformed: true,
                })
            } else {
                Err(LaneError::Malformed {
                    path: path.to_path_buf(),
                    detail: "unparseable existing claim".into(),
                })
            }
        }
    }
}
