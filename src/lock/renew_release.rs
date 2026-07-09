//! Renew, release, and the read-only status/list cores (§S2.7 / §S2.11 + fail-closed
//! remediation).
//!
//! renew and release are **strictly owner-only** (no `--force`). Both hold the lane mutex
//! so they never interleave with a claim-takeover, recover + full-stream-validate the
//! audit log first, and reconcile dangling intents for the lane (blocking on a genuinely
//! indeterminate one). All claim-state reads go through the guarded shared reader. renew
//! renames over an existing record (atomic, non-destructive). release is write-ahead
//! audited (`intent{op:release}` → remove → `completion{op:release}`). status/list are
//! read-only and never mutate (R4); status additionally surfaces dangling-intent
//! reconciliation as an `audit_warning`.

use chrono::{DateTime, Utc};

use crate::error::{LaneError, RefusedReason};
use crate::lock::audit::{next_op_id, AuditEvent, AuditEventKind, AuditOutcome, AuditSink};
use crate::lock::mutex::LaneMutex;
use crate::lock::paths::LaneRoot;
use crate::lock::target::Target;
use crate::lock::{
    combine_warnings, read_liveness, reconcile_for_mutation, reconcile_for_status, record,
    scan_overlap, ttl_to_duration, validate_instance, validate_name, validate_note, validate_ttl,
    write_temp, FsOps, HandoffParams, HandoffSuccess, ReleaseParams, ReleaseSuccess, RenewParams,
    RenewSuccess, StatusData,
};
use crate::model::ClaimStatus;

/// Renew an owned lease. Refuses if missing (`not_held`), not the owner (`not_owner`),
/// or already lapsed (`expired` — re-`claim` instead of renewing).
pub fn renew_core(
    root: &LaneRoot,
    p: &RenewParams,
    now: DateTime<Utc>,
    fs: &dyn FsOps,
    audit: &dyn AuditSink,
) -> Result<RenewSuccess, LaneError> {
    validate_name("repo", &p.repo)?;
    validate_name("lane", &p.lane)?;
    validate_instance(&p.instance)?;

    root.ensure_write_dirs(&p.repo, fs)?;
    let uid = root.expected_uid();
    crate::lock::audit::recover_if_needed(
        &root.audit_path(&p.repo),
        &p.repo,
        &p.lane,
        &p.instance,
        now,
        fs,
        uid,
    )?;

    let _lane_guard = LaneMutex::acquire(&root.lane_mutex_path(&p.repo, &p.lane), fs, uid)?;
    let recon = reconcile_for_mutation(root, &p.repo, &p.lane, fs)?;
    let lock_path = root.lock_path(&p.repo, &p.lane);

    // Guarded shared reader: missing ⇒ not_held; malformed/identity/symlink ⇒ fail closed.
    let rec = record::read_claim(&lock_path, root.path(), uid, fs)?
        .ok_or(LaneError::Refused(RefusedReason::NotHeld))?;
    if rec.instance != p.instance {
        return Err(LaneError::Refused(RefusedReason::NotOwner));
    }
    if now >= rec.expires_at {
        return Err(LaneError::Refused(RefusedReason::Expired));
    }
    let ttl = p.ttl_hours.unwrap_or(rec.ttl_hours);
    validate_ttl(ttl)?;

    // Targeted renew: re-validate overlap under the target mutex (lane→target order).
    let stored_target = rec.target_normalized.clone().or_else(|| rec.target.clone());
    let _target_guard = match &stored_target {
        Some(tn) => {
            let g = LaneMutex::acquire(&root.target_mutex_path(&p.repo), fs, uid)?;
            let t = Target::from_normalized(tn);
            scan_overlap(root, &p.repo, &p.lane, &t, now, fs)?;
            Some(g)
        }
        None => None,
    };

    let mut newrec = rec.clone();
    newrec.schema_version = Some(1);
    newrec.updated_at = now;
    newrec.expires_at = now + ttl_to_duration(ttl);
    newrec.ttl_hours = ttl;

    let temp = write_temp(root, &p.repo, &p.lane, &newrec, fs, uid)?;
    if let Err(e) = fs.rename(&temp, &lock_path) {
        let _ = fs.remove_file(&temp);
        return Err(LaneError::Io(e));
    }

    let mut ev = AuditEvent::new(
        AuditEventKind::Renew,
        &p.repo,
        &p.lane,
        &p.instance,
        AuditOutcome::Ok,
        now,
    );
    ev.op_id = Some(next_op_id());
    ev.ttl_hours = Some(ttl);
    let post = audit.append(&ev, false).err().map(|e| e.to_string());

    Ok(RenewSuccess {
        lane: p.lane.clone(),
        expires_at: newrec.expires_at,
        audit_warning: combine_warnings(recon, post),
    })
}

/// Hand off an owned lease: flip `claim_status -> handoff` (and optionally replace the
/// note) so a successor can see the lane is offered, WITHOUT releasing it - the claim
/// stays held and the TTL keeps ticking, so the target stays protected until the
/// successor takes over. Owner-only with the exact renew posture: missing ⇒ `not_held`,
/// not the owner ⇒ `not_owner`, lapsed ⇒ `expired` (a lapsed lease cannot be handed
/// off). Re-handoff of an already-`handoff` claim is idempotent success (refreshes
/// `updated_at`/note). Follows renew's owner-only lane-mutex rename-over pattern but
/// SKIPS the target-mutex/overlap re-scan: handoff changes no target, so there is no
/// overlap surface to re-validate. Non-destructive: one terminal `handoff` audit event,
/// no intent/completion pair.
pub fn handoff_core(
    root: &LaneRoot,
    p: &HandoffParams,
    now: DateTime<Utc>,
    fs: &dyn FsOps,
    audit: &dyn AuditSink,
) -> Result<HandoffSuccess, LaneError> {
    validate_name("repo", &p.repo)?;
    validate_name("lane", &p.lane)?;
    validate_instance(&p.instance)?;
    if let Some(n) = &p.note {
        validate_note(n)?;
    }

    root.ensure_write_dirs(&p.repo, fs)?;
    let uid = root.expected_uid();
    crate::lock::audit::recover_if_needed(
        &root.audit_path(&p.repo),
        &p.repo,
        &p.lane,
        &p.instance,
        now,
        fs,
        uid,
    )?;

    let _lane_guard = LaneMutex::acquire(&root.lane_mutex_path(&p.repo, &p.lane), fs, uid)?;
    let recon = reconcile_for_mutation(root, &p.repo, &p.lane, fs)?;
    let lock_path = root.lock_path(&p.repo, &p.lane);

    // Guarded shared reader: missing ⇒ not_held; malformed/identity/symlink ⇒ fail closed.
    let rec = record::read_claim(&lock_path, root.path(), uid, fs)?
        .ok_or(LaneError::Refused(RefusedReason::NotHeld))?;
    if rec.instance != p.instance {
        return Err(LaneError::Refused(RefusedReason::NotOwner));
    }
    if now >= rec.expires_at {
        return Err(LaneError::Refused(RefusedReason::Expired));
    }

    let mut newrec = rec.clone();
    newrec.schema_version = Some(1);
    newrec.claim_status = Some(ClaimStatus::Handoff);
    newrec.updated_at = now;
    if p.note.is_some() {
        newrec.note = p.note.clone();
    }

    let temp = write_temp(root, &p.repo, &p.lane, &newrec, fs, uid)?;
    if let Err(e) = fs.rename(&temp, &lock_path) {
        let _ = fs.remove_file(&temp);
        return Err(LaneError::Io(e));
    }

    let mut ev = AuditEvent::new(
        AuditEventKind::Handoff,
        &p.repo,
        &p.lane,
        &p.instance,
        AuditOutcome::Ok,
        now,
    );
    ev.op_id = Some(next_op_id());
    let post = audit.append(&ev, false).err().map(|e| e.to_string());

    Ok(HandoffSuccess {
        lane: p.lane.clone(),
        expires_at: newrec.expires_at,
        audit_warning: combine_warnings(recon, post),
    })
}

/// Release an owned lane. Absent ⇒ no-op success (`not_held`, exit 0). Not the owner ⇒
/// refused (`not_owner`, exit 1). There is no force path.
pub fn release_core(
    root: &LaneRoot,
    p: &ReleaseParams,
    now: DateTime<Utc>,
    fs: &dyn FsOps,
    audit: &dyn AuditSink,
) -> Result<ReleaseSuccess, LaneError> {
    validate_name("repo", &p.repo)?;
    validate_name("lane", &p.lane)?;
    validate_instance(&p.instance)?;

    root.ensure_write_dirs(&p.repo, fs)?;
    let uid = root.expected_uid();
    crate::lock::audit::recover_if_needed(
        &root.audit_path(&p.repo),
        &p.repo,
        &p.lane,
        &p.instance,
        now,
        fs,
        uid,
    )?;

    let _lane_guard = LaneMutex::acquire(&root.lane_mutex_path(&p.repo, &p.lane), fs, uid)?;
    let recon = reconcile_for_mutation(root, &p.repo, &p.lane, fs)?;
    let lock_path = root.lock_path(&p.repo, &p.lane);

    let rec = match record::read_claim(&lock_path, root.path(), uid, fs)? {
        None => {
            return Ok(ReleaseSuccess {
                present: false,
                audit_warning: recon,
            })
        }
        Some(r) => r,
    };
    if rec.instance != p.instance {
        return Err(LaneError::Refused(RefusedReason::NotOwner));
    }
    // Generation guard (Slice 4): under the lane mutex, a generation-bound caller
    // (the close composition) refuses when the live record is a SUCCESSOR claim —
    // same lane, same instance, different `claimed_at`. `not_held` is exact: the
    // generation the caller bound to no longer exists. Strictly strengthens the
    // owner-only release law; `None` (the plain verb) is byte-identical to before.
    if let Some(expected) = p.expected_claimed_at {
        if rec.claimed_at != expected {
            return Err(LaneError::Refused(RefusedReason::NotHeld));
        }
    }

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
    intent.op = Some("release".into());
    if let Err(e) = audit.append(&intent, true) {
        // Intent failed → abort before removing; the claim is intact.
        return Err(LaneError::Io(e));
    }

    match fs.remove_file(&lock_path) {
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
            comp.op = Some("release".into());
            let post = audit.append(&comp, false).err().map(|e| e.to_string());
            Ok(ReleaseSuccess {
                present: true,
                audit_warning: combine_warnings(recon, post),
            })
        }
        Err(e) => {
            let mut comp = AuditEvent::new(
                AuditEventKind::Completion,
                &p.repo,
                &p.lane,
                &p.instance,
                AuditOutcome::Error,
                now,
            );
            comp.op_id = Some(op_id);
            comp.op = Some("release".into());
            comp.reason = Some(format!("remove failed: {e}"));
            let _ = audit.append(&comp, true);
            Err(LaneError::Io(e))
        }
    }
}

/// Read-only status of one lane plus any dangling-intent reconciliation warning. Absent ⇒
/// `{present:false}` (not_held); present ⇒ the record + its stale classification. Never mutates.
pub fn status_core(
    root: &LaneRoot,
    repo: &str,
    lane: &str,
    now: DateTime<Utc>,
    fs: &dyn FsOps,
) -> Result<(StatusData, Option<String>), LaneError> {
    let lock_path = root.lock_path(repo, lane);
    let record = record::read_claim(&lock_path, root.path(), root.expected_uid(), fs)?;
    // Surface dangling-intent reconciliation (read-only; never blocks).
    let warning = reconcile_for_status(root, repo, lane, record.as_ref(), fs)?;
    let data = match record {
        None => StatusData {
            present: false,
            record: None,
            stale_state: None,
        },
        Some(rec) => {
            let stale = crate::board::classify_stale(&rec, now, read_liveness());
            StatusData {
                present: true,
                record: Some(rec),
                stale_state: Some(stale),
            }
        }
    };
    Ok((data, warning))
}

/// Read-only listing of all claims (optionally filtered to one repo). A malformed,
/// identity-inconsistent, or symlinked record fails closed (exit 2). Never mutates.
pub fn list_core(
    root: &LaneRoot,
    repo_filter: Option<&str>,
    now: DateTime<Utc>,
    fs: &dyn FsOps,
) -> Result<Vec<StatusData>, LaneError> {
    let uid = root.expected_uid();
    let mut out: Vec<StatusData> = Vec::new();
    let rd = match std::fs::read_dir(root.path()) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(LaneError::Io(e)),
    };
    for entry in rd {
        let entry = entry.map_err(LaneError::Io)?;
        let ft = entry.file_type().map_err(LaneError::Io)?;
        // An interior symlink directly under the root fails closed; stray non-dirs skip.
        if ft.is_symlink() {
            return Err(LaneError::Identity(format!(
                "interior state symlink (refusing to follow): {}",
                entry.path().display()
            )));
        }
        if !ft.is_dir() {
            continue;
        }
        let repo_name = entry.file_name().to_string_lossy().to_string();
        if let Some(f) = repo_filter {
            if f != repo_name {
                continue;
            }
        }
        // Guarded chain (rejects a symlinked repo/locks; never follows). Absent → skip.
        let locks_dir = entry.path().join("locks");
        match crate::lock::paths::guard_dir_chain(root.path(), &locks_dir, fs, uid)? {
            crate::lock::paths::Presence::Absent => continue,
            crate::lock::paths::Presence::Present => {}
        }
        for lock in std::fs::read_dir(&locks_dir).map_err(LaneError::Io)? {
            let lock = lock.map_err(LaneError::Io)?;
            let path = lock.path();
            if path.extension().and_then(|e| e.to_str()) != Some("lock") {
                continue;
            }
            if let Some(rec) = record::read_claim(&path, root.path(), uid, fs)? {
                let stale = crate::board::classify_stale(&rec, now, read_liveness());
                out.push(StatusData {
                    present: true,
                    record: Some(rec),
                    stale_state: Some(stale),
                });
            }
        }
    }
    out.sort_by(|a, b| {
        let ka = a.record.as_ref().map(|r| (&r.repo, &r.lane));
        let kb = b.record.as_ref().map(|r| (&r.repo, &r.lane));
        ka.cmp(&kb)
    });
    Ok(out)
}
