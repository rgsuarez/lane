//! `start`/`close` lifecycle composition — OUTSIDE the locking core.
//!
//! These verbs COMPOSE the offline locking core (`claim_core`, `renew_core`,
//! `release_core`) with the git adapter (`src/git/`). The core itself never calls git and
//! never spawns a process; this layer orchestrates the two and emits through the SAME
//! single-envelope path (`lock::emit`) as every other verb — never a duplicate envelope.
//!
//! `start` is claim-first: the claim (with `target` = the worktree path) is the
//! reservation, taken BEFORE any git mutation, so two racing `start`s resolve at the
//! lock and the loser never mutates git. Read-only validation (repo/branch/path
//! prechecks) runs before the claim by design — failing fast keeps typo-class errors out
//! of the audit log. On a git failure the composition CLEANS UP: the branch it created
//! is deleted (so a retry is never blocked by the branch-exists precheck) and the claim
//! is released through the write-ahead-audited core primitive; the git failure stays the
//! primary error, and a compensation fault is REPORTED through the `audit_warning`
//! surface, never silent.
//!
//! `close --remove-worktree` renews FIRST (owner + expiry guard, plus a full-TTL lease
//! extension so the claim cannot lapse into expiry-takeover mid-teardown), probes before
//! removing (an already-deleted worktree is skipped, not an error), never passes
//! `--force` to git (a dirty worktree REFUSES, exit 1, claim intact), and releases last
//! (`release_core`'s internal mutex re-verifies ownership as the final authority). NO
//! core mutex is ever held across a git spawn: each core primitive acquires and releases
//! its own mutex internally, and the git calls run between them.

use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};

use crate::cli::{CloseArgs, StartArgs};
use crate::config::{LaneConfig, ROLE_LINEAR_API};
use crate::error::{LaneError, RefusedReason};
use crate::git::{GitAdapter, GitError, GitRunner, StdGitRunner};
use crate::linear::transport::{LinearTransport, UreqTransport};
use crate::linear::{api, draft, publish};
use crate::lock::audit::{AuditEvent, AuditEventKind, AuditOutcome, AuditSink, StdAuditSink};
use crate::lock::claim::claim_core;
use crate::lock::paths::LaneRoot;
use crate::lock::renew_release::{release_core, renew_core};
use crate::lock::{
    emit, record, require_instance, resolve_root, validate_name, ClaimParams, CommandError, FsOps,
    Outcome, ReleaseParams, RenewParams, VerbData,
};
use crate::model::{ClaimRecord, ClaimStatus, Role};
use crate::secrets::{OpRunner, SecretResolver, SecretValue, StdOpRunner};

/// Map a git-adapter error to the authoritative error type. A dirty-worktree refusal is
/// a safe exit-1 refusal (`dirty_worktree`); every other git failure (plumbing, timeout,
/// spawn, precheck, flag-smuggling) is an exit-2 io-class error carrying the detail.
/// `pub(crate)`: the hook composition layer (`src/hook.rs`) maps its git faults through
/// the SAME single translation — never a second mapping.
pub(crate) fn git_to_lane(e: GitError) -> LaneError {
    match e {
        GitError::DirtyWorktree { .. } => LaneError::Refused(RefusedReason::DirtyWorktree),
        other => LaneError::Io(std::io::Error::other(other.to_string())),
    }
}

/// Join optional warning strings into one `audit_warning`-style line.
fn join_warnings(parts: Vec<Option<String>>) -> Option<String> {
    let v: Vec<String> = parts.into_iter().flatten().collect();
    if v.is_empty() {
        None
    } else {
        Some(v.join("; "))
    }
}

/// `lane start` runner (production: real git runner, real clock).
pub fn run_start(args: &StartArgs) -> i32 {
    run_start_at(args, Utc::now(), &StdGitRunner::new())
}

pub(crate) fn run_start_at(args: &StartArgs, now: DateTime<Utc>, runner: &dyn GitRunner) -> i32 {
    let repo = Some(args.repo.clone());
    let lane = Some(args.lane.clone());
    let fs = crate::lock::StdFs;
    let home = std::env::var("HOME").ok();
    let result = (|| -> Result<(Outcome, Option<VerbData>, Option<String>), CommandError> {
        let instance = require_instance(args.instance.clone())?;
        validate_name("repo", &args.repo)?;
        validate_name("lane", &args.lane)?;

        // Derivations. The lane-name grammar (validated above) is a single flag-safe path
        // component, so the lowercased lane is safe as both a branch name and a path leaf.
        let git_repo = &args.git_repo;
        if !git_repo.is_absolute() {
            return Err(LaneError::Identity(format!(
                "--git-repo must be an absolute path, got {}",
                git_repo.display()
            ))
            .into());
        }
        let lane_lower = args.lane.to_ascii_lowercase();
        let branch = args.branch.clone().unwrap_or_else(|| lane_lower.clone());
        let worktree: PathBuf = match &args.worktree {
            Some(w) => {
                if !w.is_absolute() {
                    return Err(LaneError::Identity(format!(
                        "--worktree must be an absolute path, got {}",
                        w.display()
                    ))
                    .into());
                }
                w.clone()
            }
            // Sibling-of-repo convention, LOWERCASED leaf: the stored (case-folded)
            // normalized target then equals the on-disk path exactly.
            None => {
                let file_name = git_repo
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                git_repo.with_file_name(format!("{file_name}-{lane_lower}"))
            }
        };
        let base = args.base.clone().unwrap_or_else(|| "HEAD".to_string());

        // Step 0 — read-only git prechecks BEFORE the claim (typo-class failures never
        // reach the audit log). The branch name is validated the way git itself does
        // (check-ref-format), so an invalid override fails here with a clear error.
        let git = GitAdapter::new(runner);
        git.check_branch_name(git_repo, &branch)
            .map_err(git_to_lane)?;
        let precheck = git
            .precheck_start(git_repo, &worktree, &branch, home.as_deref())
            .map_err(git_to_lane)?;

        // Step 1 — CLAIM FIRST (the reservation; two racing starts resolve here and the
        // loser never mutates git). The record carries the lifecycle facts from birth.
        let root = resolve_root(args.lane_root.clone(), home.as_deref(), &fs)?;
        let sink = StdAuditSink::new(root.audit_path(&args.repo), root.expected_uid());
        let params = ClaimParams {
            repo: args.repo.clone(),
            lane: args.lane.clone(),
            instance: instance.clone(),
            target: Some(worktree.to_string_lossy().to_string()),
            home: home.clone(),
            ttl_hours: args.ttl_hours,
            note: args.note.clone(),
            branch: Some(branch.clone()),
            linear_key: args.linear_key.clone(),
            role: Some(Role::Executor),
            claim_status: Some(ClaimStatus::Active),
            ..Default::default()
        };
        let claim = claim_core(&root, &params, now, &fs, &sink)?;

        // Step 2 — git mutations UNDER the claim (no core mutex is held here; claim_core
        // has returned). TWO OWNED STEPS, not `worktree add -b`: branch creation is its
        // own tracked call, so the compensation path knows AS A FACT whether this run
        // created the branch — it never infers creation from a later branch_exists,
        // which would race an external git process creating the same name and could
        // delete a branch that was never ours. Git receives the derived on-disk path,
        // never target_normalized.
        let mut branch_created = false;
        let git_result: Result<(), GitError> = (|| {
            git.create_branch(git_repo, &branch, &base)?;
            branch_created = true;
            git.worktree_add_existing(git_repo, &worktree, &branch)
        })();
        if let Err(git_err) = git_result {
            let warning = compensate_start_failure(
                &root,
                &args.repo,
                &args.lane,
                &instance,
                git_repo,
                &branch,
                &worktree,
                branch_created,
                &git,
                &fs,
                &sink,
                claim.audit_warning.clone(),
                now,
            );
            return Err(CommandError {
                error: git_to_lane(git_err),
                audit_warning: warning,
            });
        }

        let data = VerbData::Start {
            lane: claim.lane,
            instance: claim.instance,
            expires_at: claim.expires_at,
            worktree: worktree.to_string_lossy().to_string(),
            branch,
            base,
            warning: precheck.device_warning.clone(),
        };
        // ADVISORY, after the worktree exists and the claim already succeeded: `start` is the
        // verb that CREATES the condition - `git worktree add` materializes tracked files only,
        // so the just-created worktree has no install directory and its gates cannot run yet.
        // Reporting it here, where the fresh path is known exactly, is the earliest possible
        // moment. It never installs and never fails the verb.
        let readiness = crate::lock::workspace_readiness_warning(&worktree);
        Ok((
            Outcome::Ok,
            Some(data),
            join_warnings(vec![claim.audit_warning, readiness]),
        ))
    })();
    emit(args.json, "start", repo, lane, result)
}

/// The `start` failure path: compensating cleanup, then release, all faults REPORTED and
/// none escalated (the git failure that got us here stays the primary error). The branch
/// is deleted IF AND ONLY IF this run's own `create_branch` succeeded (`branch_created`,
/// a tracked fact — never an existence probe, which would race an external git process
/// creating the same name and delete a branch that was never ours). A leftover partial
/// worktree directory is named; the claim is released through the write-ahead-audited
/// core primitive, and if THAT fails the still-held claim is named explicitly (safe
/// direction: TTL-bounded, board-visible, releasable).
#[allow(clippy::too_many_arguments)]
fn compensate_start_failure(
    root: &crate::lock::paths::LaneRoot,
    repo: &str,
    lane: &str,
    instance: &str,
    git_repo: &Path,
    branch: &str,
    worktree: &Path,
    branch_created: bool,
    git: &GitAdapter<'_>,
    fs: &dyn crate::lock::FsOps,
    sink: &dyn crate::lock::audit::AuditSink,
    claim_warning: Option<String>,
    now: DateTime<Utc>,
) -> Option<String> {
    let mut warnings: Vec<Option<String>> = vec![claim_warning];
    // Cleanup is gated on branch_created: if our create_branch never succeeded, our
    // worktree_add never ran, so anything at the path is FOREIGN and untouchable.
    if branch_created {
        // Remove a partially-registered worktree FIRST (it keeps its branch checked
        // out, which would block the branch delete) — but only with an OWNERSHIP
        // PROOF: the registered worktree must be attached to the branch THIS run just
        // created (git refuses to attach one branch to two worktrees, so a match is
        // ours). A foreign worktree that raced onto this path is left untouched and
        // named. Every fault is reported; none escalates.
        match git.probe_worktree(worktree) {
            Ok(Some(wt)) if wt.branch.as_deref() == Some(branch) => {
                if let Err(e) = git.worktree_remove(git_repo, worktree) {
                    warnings.push(Some(format!(
                        "cleanup: could not remove partial worktree {}: {e}",
                        worktree.display()
                    )));
                }
            }
            Ok(Some(_)) => {
                warnings.push(Some(format!(
                    "cleanup: a FOREIGN worktree occupies {} (left untouched)",
                    worktree.display()
                )));
            }
            Ok(None) => {}
            Err(e) => {
                warnings.push(Some(format!(
                    "cleanup: could not inspect {}: {e}",
                    worktree.display()
                )));
            }
        }
        if let Err(e) = git.delete_branch(git_repo, branch) {
            warnings.push(Some(format!(
                "cleanup: could not delete branch {branch}: {e}"
            )));
        }
    }
    if worktree.exists() {
        warnings.push(Some(format!(
            "cleanup: a partial worktree directory may remain at {}",
            worktree.display()
        )));
    }
    let rel = ReleaseParams {
        repo: repo.to_string(),
        lane: lane.to_string(),
        instance: instance.to_string(),
        // Compensation releases the claim `start` just created; no generation bind
        // (nothing else can have re-claimed under the still-held flow).
        expected_claimed_at: None,
    };
    match release_core(root, &rel, now, fs, sink) {
        Ok(s) => warnings.push(s.audit_warning),
        Err(rel_err) => warnings.push(Some(format!(
            "compensating release FAILED ({rel_err}); the claim is still held by {instance} and must be released manually"
        ))),
    }
    join_warnings(warnings)
}

/// Injectable dependencies for the `close` composition (production: real git, real
/// `op`, real HTTP; tests substitute any subset). The op/transport seams are only
/// exercised in the gated-closeout modes — a plain close never touches them.
pub(crate) struct CloseDeps<'a> {
    pub git: &'a dyn GitRunner,
    pub op: &'a dyn OpRunner,
    pub transport: &'a dyn LinearTransport,
}

/// `lane close` runner (production: real git/op/HTTP, real clock).
pub fn run_close(args: &CloseArgs) -> i32 {
    let git = StdGitRunner::new();
    let op = StdOpRunner::new();
    let transport = UreqTransport::new();
    run_close_at(
        args,
        Utc::now(),
        &CloseDeps {
            git: &git,
            op: &op,
            transport: &transport,
        },
    )
}

/// Which closeout mode the flags selected (clap enforces exclusivity).
enum CloseoutMode {
    Normal,
    Draft,
    Post,
}

/// The Slice-4 generation re-check: the live record must be the SAME claim
/// generation the close bound to at step 0 — same `instance`, same `linear_key`,
/// exact `claimed_at` — and unexpired. Run before the gated publish path proceeds
/// (a takeover, expiry, or same-instance release+reclaim aborts with nothing done).
fn verify_generation(
    lock_path: &Path,
    root: &LaneRoot,
    fs: &dyn FsOps,
    bound: &ClaimRecord,
    instance: &str,
    now: DateTime<Utc>,
) -> Result<(), LaneError> {
    match record::read_claim(lock_path, root.path(), root.expected_uid(), fs)? {
        None => Err(LaneError::Refused(RefusedReason::NotHeld)),
        Some(cur) => {
            if cur.instance != instance {
                return Err(LaneError::Refused(RefusedReason::NotOwner));
            }
            if cur.claimed_at != bound.claimed_at {
                // A successor generation of the same lane (same-instance
                // release+reclaim): the claim this close bound to no longer exists.
                return Err(LaneError::Refused(RefusedReason::NotHeld));
            }
            if cur.linear_key != bound.linear_key {
                return Err(LaneError::Identity(
                    "claim's linear_key changed mid-close; re-run".to_string(),
                ));
            }
            if cur.expires_at <= now {
                return Err(LaneError::Refused(RefusedReason::Expired));
            }
            Ok(())
        }
    }
}

/// Post-mode context threaded from preflight to the publish step. Holds the
/// publish-lock guard so the ENTIRE mutation-and-publish section stays serialized
/// (the guard drops after release, at the end of the closure).
struct CloseoutCtx {
    draft_body: String,
    key: String,
    issue_uuid: String,
    already_posted: bool,
    secret: SecretValue,
    api_url: String,
    _publish_guard: publish::PublishGuard,
}

pub(crate) fn run_close_at(args: &CloseArgs, now: DateTime<Utc>, deps: &CloseDeps) -> i32 {
    let repo = Some(args.repo.clone());
    let lane = Some(args.lane.clone());
    let fs = crate::lock::StdFs;
    let home = std::env::var("HOME").ok();
    let runner = deps.git;
    let result = (|| -> Result<(Outcome, Option<VerbData>, Option<String>), CommandError> {
        let instance = require_instance(args.instance.clone())?;
        validate_name("repo", &args.repo)?;
        validate_name("lane", &args.lane)?;
        let root = resolve_root(args.lane_root.clone(), home.as_deref(), &fs)?;
        let sink = StdAuditSink::new(root.audit_path(&args.repo), root.expected_uid());
        let mode = if args.draft_closeout {
            CloseoutMode::Draft
        } else if args.post_closeout {
            CloseoutMode::Post
        } else {
            CloseoutMode::Normal
        };
        let close_data = |released: bool,
                          worktree_removed: bool,
                          skipped: bool,
                          draft: Option<String>,
                          posted: Option<bool>,
                          already: Option<bool>,
                          url: Option<String>| {
            VerbData::Close {
                lane: args.lane.clone(),
                released,
                worktree_removed,
                skipped_missing_worktree: skipped,
                closeout_draft: draft,
                closeout_posted: posted,
                closeout_already_posted: already,
                closeout_comment_url: url,
            }
        };

        // Step 0 — absence check FIRST (guarded read, no mutex): close-of-absent mirrors
        // release exactly (`not_held`, exit 0), BEFORE any renew attempt (renew's own
        // not_held is a refusal, the wrong contract here). Later steps re-verify under
        // the core mutexes, so this unguarded read cannot be the final authority.
        // Draft/Post modes get the same contract: an unheld lane has nothing true to
        // draft, so nothing is composed and nothing is posted.
        let lock_path = root.lock_path(&args.repo, &args.lane);
        let rec = match record::read_claim(&lock_path, root.path(), root.expected_uid(), &fs)? {
            None => {
                let data = close_data(false, false, false, None, None, None, None);
                return Ok((Outcome::NotHeld, Some(data), None));
            }
            Some(r) => r,
        };

        let mut worktree_removed = false;
        let mut skipped_missing = false;
        let mut warnings: Vec<Option<String>> = Vec::new();

        // Step 0.5 — LOCAL compose (Draft|Post): linear_key gate, whitelist composer,
        // reference scrub. Pure-local: no secret, no network, no mutation.
        let mut closeout: Option<CloseoutCtx> = None;
        if !matches!(mode, CloseoutMode::Normal) {
            let key = match rec.linear_key.clone() {
                Some(k) => k,
                None => {
                    return Err(CommandError::from(LaneError::RefusedMsg {
                        reason: RefusedReason::NoLinearKey,
                        msg: format!(
                            "claim {}/{} records no linear_key; run plain `lane close`, or re-claim/`lane start` with --linear-key",
                            args.repo, args.lane
                        ),
                    }));
                }
            };
            let draft_body = draft::compose_closeout(&rec, now);
            if let Some(violation) = draft::scrub_violation(&draft_body, None) {
                return Err(CommandError::from(LaneError::Io(std::io::Error::other(
                    violation,
                ))));
            }
            if matches!(mode, CloseoutMode::Draft) {
                // PURE PREVIEW: zero mutation, zero spawns, zero network.
                let data = close_data(false, false, false, Some(draft_body), None, None, None);
                return Ok((Outcome::Ok, Some(data), None));
            }

            // ---- Post mode ----
            // Step 0.6 — guarded publish lock BEFORE any secret resolution or local
            // mutation: a busy loser (`mutex_busy`) and a poisoned lock object (exit 2)
            // both provably touch nothing.
            let publish_guard = publish::acquire(&root, &args.repo, &args.lane, &fs)?;
            // Step 0.7 — generation re-check #1 (pre-lock races caught here). A vanished
            // claim or a same-instance SUCCESSOR generation (both `NotHeld`) is the same
            // graceful exit-0 not_held contract as step 0 / the renew race — nothing has
            // been posted yet, and the successor (if any) is left intact. A foreign
            // takeover (`NotOwner`) or lapsed lease (`Expired`) stays an honest refusal.
            match verify_generation(&lock_path, &root, &fs, &rec, &instance, now) {
                Ok(()) => {}
                Err(LaneError::Refused(RefusedReason::NotHeld)) => {
                    let data = close_data(false, false, false, None, None, None, None);
                    return Ok((Outcome::NotHeld, Some(data), join_warnings(warnings)));
                }
                Err(error) => {
                    return Err(CommandError {
                        error,
                        audit_warning: join_warnings(warnings),
                    });
                }
            }
            // Step 0.8 — resolve the API key (audited to the ROOT adapter audit).
            let config = LaneConfig::load(root.path(), root.expected_uid(), &fs)?;
            let root_sink = StdAuditSink::new(root.root_audit_path(), root.expected_uid());
            let resolver = SecretResolver {
                config: &config,
                runner: deps.op,
                sink: &root_sink,
                repo: &args.repo,
                lane: &args.lane,
                instance: &instance,
            };
            let (secret_result, audit_warn) = resolver.resolve(ROLE_LINEAR_API, now);
            warnings.push(audit_warn);
            let secret = match secret_result {
                Ok(s) => s,
                Err(error) => {
                    return Err(CommandError {
                        error,
                        audit_warning: join_warnings(warnings),
                    });
                }
            };
            if let Some(violation) = draft::scrub_violation(&draft_body, Some(&secret)) {
                return Err(CommandError {
                    error: LaneError::Io(std::io::Error::other(violation)),
                    audit_warning: join_warnings(warnings),
                });
            }
            // Step 0.9 — ONE preflight read: issue UUID + marker scan. Inside the
            // publish lock this scan IS the dedupe authority. Bad key / offline are
            // caught here, before ANY mutation.
            let marker = draft::closeout_marker(&args.lane, rec.claimed_at);
            let preflight = match api::preflight_issue(
                deps.transport,
                &config.linear.api_url,
                &secret,
                &key,
                &marker,
            ) {
                Ok(Some(p)) => p,
                Ok(None) => {
                    return Err(CommandError {
                        error: LaneError::Network(format!(
                            "linear issue `{key}` not found (check the claim's linear_key)"
                        )),
                        audit_warning: join_warnings(warnings),
                    });
                }
                Err(error) => {
                    return Err(CommandError {
                        error,
                        audit_warning: join_warnings(warnings),
                    });
                }
            };
            closeout = Some(CloseoutCtx {
                draft_body,
                key,
                issue_uuid: preflight.uuid,
                already_posted: preflight.already_posted,
                secret,
                api_url: config.linear.api_url.clone(),
                _publish_guard: publish_guard,
            });
        }
        let post_mode = closeout.is_some();

        if args.remove_worktree || post_mode {
            // Step 1 — RENEW FIRST: enforces owner + expiry (a lapsed lease is not yours
            // to tear down) AND extends the lease by the claim's own TTL, so the claim
            // cannot lapse into expiry-takeover while git runs. A NotHeld here means the
            // claim vanished between the step-0 read and the renew (a release race):
            // map it to the same harmless not_held exit-0 contract as step 0.
            let renew = RenewParams {
                repo: args.repo.clone(),
                lane: args.lane.clone(),
                instance: instance.clone(),
                ttl_hours: None,
            };
            let renewed = match renew_core(&root, &renew, now, &fs, &sink) {
                Ok(s) => s,
                Err(LaneError::Refused(RefusedReason::NotHeld)) => {
                    // In post mode this is a post-generation-check race: nothing is
                    // held, so nothing is closed and nothing is posted.
                    let data = close_data(false, false, false, None, None, None, None);
                    return Ok((Outcome::NotHeld, Some(data), join_warnings(warnings)));
                }
                // Carry the accumulated warnings (post mode may have pushed a
                // secret_requested audit-degradation warning before this point) —
                // `e.into()` would hardcode audit_warning: None and drop it.
                Err(e) => {
                    return Err(CommandError {
                        error: e,
                        audit_warning: join_warnings(warnings),
                    })
                }
            };
            warnings.push(renewed.audit_warning);
        }
        // From here on, every failure path must CARRY the accumulated warnings (the
        // renew may have succeeded with audit degradation; hiding that in a later
        // git-error envelope would misreport the completed lease extension).
        let fail = |e: crate::git::GitError, warnings: Vec<Option<String>>| CommandError {
            error: git_to_lane(e),
            audit_warning: join_warnings(warnings),
        };

        if args.remove_worktree {
            // Step 2 — probe, then remove (no core mutex held across these spawns).
            let stored = rec.target_normalized.clone().or_else(|| rec.target.clone());
            if let Some(t) = stored {
                let git = GitAdapter::new(runner);
                // Resolve the path to REMOVE: the stored (case-folded) target directly;
                // on a miss, the same real-case fallback the board provider uses, so an
                // operator-overridden mixed-case worktree on a case-sensitive volume is
                // still found (never released-but-left-behind). Ambiguity never guesses.
                let stored_path = Path::new(&t).to_path_buf();
                let mut remove_path: Option<(std::path::PathBuf, Option<String>)> = None;
                match git.probe_worktree(&stored_path) {
                    Err(e) => return Err(fail(e, warnings)),
                    Ok(Some(info)) => remove_path = Some((stored_path.clone(), info.branch)),
                    Ok(None) => {
                        if let (Some(parent), Some(tail)) =
                            (stored_path.parent(), stored_path.file_name())
                        {
                            let tail = tail.to_string_lossy().to_string();
                            // A readdir FAILURE must not read as "worktree missing": a
                            // permission or io fault here would otherwise release the
                            // claim while the worktree survives unprotected. Only a
                            // clean NotFound (parent gone) counts as genuinely absent.
                            let entries: Vec<String> = match std::fs::read_dir(parent) {
                                Ok(rd) => rd
                                    .filter_map(|e| e.ok())
                                    .map(|e| e.file_name().to_string_lossy().to_string())
                                    .collect(),
                                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                                Err(e) => {
                                    return Err(CommandError {
                                        error: LaneError::Io(e),
                                        audit_warning: join_warnings(warnings),
                                    });
                                }
                            };
                            match crate::git::resolve_real_case(&entries, &tail) {
                                crate::git::CaseMatch::One(real) if real != tail => {
                                    let real_path = parent.join(real);
                                    match git.probe_worktree(&real_path) {
                                        Err(e) => return Err(fail(e, warnings)),
                                        Ok(Some(info)) => {
                                            remove_path = Some((real_path, info.branch))
                                        }
                                        Ok(None) => {}
                                    }
                                }
                                crate::git::CaseMatch::Ambiguous => {
                                    // FAIL CLOSED: releasing here would drop protection
                                    // while an unidentified worktree survives on disk.
                                    // The operator resolves the case-ambiguous pair
                                    // manually; the claim stays held (and renewed).
                                    return Err(CommandError {
                                        error: LaneError::Identity(format!(
                                            "ambiguous case-insensitive worktree matches for {}; resolve manually (claim kept)",
                                            stored_path.display()
                                        )),
                                        audit_warning: join_warnings(warnings),
                                    });
                                }
                                _ => {}
                            }
                        }
                    }
                }
                match remove_path {
                    None => {
                        // Already deleted, or not a worktree: skip the git spawn and
                        // fall through to release — a claim must never become
                        // un-closable over a directory that no longer exists.
                        skipped_missing = true;
                    }
                    Some((wt, probed_branch)) => {
                        // Worktree IDENTITY guard: when the claim records the branch its
                        // lifecycle created, the entity about to be removed must be ON
                        // that branch. The stored target is case-folded, so an UNRELATED
                        // worktree can resolve at that path (a folded-case collision on a
                        // case-sensitive volume, or manual surgery replacing the worktree
                        // between start and close); git refuses one branch on two
                        // worktrees, so a branch match proves the worktree is this
                        // claim's. Mismatch fails closed: worktree kept, claim kept (and
                        // renewed). A branchless claim records no identity to prove and
                        // keeps path-identity semantics.
                        if let Some(expected) = rec.branch.as_deref() {
                            if probed_branch.as_deref() != Some(expected) {
                                return Err(CommandError {
                                    error: LaneError::Identity(format!(
                                        "worktree at {} is on branch '{}', not the claim's branch '{}'; refusing to remove (claim kept)",
                                        wt.display(),
                                        probed_branch.as_deref().unwrap_or("<detached>"),
                                        expected
                                    )),
                                    audit_warning: join_warnings(warnings),
                                });
                            }
                        }
                        // Defense-in-depth ownership + GENERATION re-verify immediately
                        // before the destructive spawn: the renew extended the lease, so
                        // only a deliberate `claim --force` (different owner) or a
                        // same-instance release+reclaim (successor GENERATION, Slice 4)
                        // can have changed the record — and neither's fresh worktree may
                        // be destroyed by this stale close. A guarded read costs one stat.
                        match record::read_claim(&lock_path, root.path(), root.expected_uid(), &fs)
                        {
                            Ok(Some(cur))
                                if cur.instance == instance && cur.claimed_at == rec.claimed_at => {
                            }
                            // A clean different owner is an ordinary refusal.
                            Ok(Some(cur)) if cur.instance != instance => {
                                return Err(CommandError {
                                    error: LaneError::Refused(RefusedReason::NotOwner),
                                    audit_warning: join_warnings(warnings),
                                });
                            }
                            // Same instance, successor generation: the claim this close
                            // bound to is gone — never remove the successor's worktree.
                            Ok(Some(_)) => {
                                return Err(CommandError {
                                    error: LaneError::Refused(RefusedReason::NotHeld),
                                    audit_warning: join_warnings(warnings),
                                });
                            }
                            // Vanished mid-destructive-path: stop, do not remove.
                            Ok(None) => {
                                return Err(CommandError {
                                    error: LaneError::Refused(RefusedReason::NotHeld),
                                    audit_warning: join_warnings(warnings),
                                });
                            }
                            // Fail-closed reader errors (malformed, symlinked,
                            // wrong-owner state) PROPAGATE as exit 2 — masking state
                            // corruption as an ownership refusal would violate the
                            // core reader contract.
                            Err(e) => {
                                return Err(CommandError {
                                    error: e,
                                    audit_warning: join_warnings(warnings),
                                });
                            }
                        }
                        let main_repo = match git.main_repo_of(&wt) {
                            Ok(p) => p,
                            Err(e) => return Err(fail(e, warnings)),
                        };
                        if let Err(e) = git.worktree_remove(&main_repo, &wt) {
                            return Err(fail(e, warnings));
                        }
                        worktree_removed = true;
                    }
                }
            }
        }

        // Step 2.5 — the gated write (Post): post-or-dedupe INSIDE the publish lock.
        // The in-lock preflight marker scan is the dedupe authority: present ⇒ Linear
        // already carries this generation's closeout (an earlier run's timeout landed,
        // or a concurrent winner posted) ⇒ skip the create. A POST failure keeps the
        // claim held (and renewed) — the rerun re-scans and posts only if absent.
        let mut closeout_draft_out: Option<String> = None;
        let mut closeout_posted: Option<bool> = None;
        let mut closeout_already: Option<bool> = None;
        let mut closeout_url: Option<String> = None;
        if let Some(ctx) = &closeout {
            closeout_draft_out = Some(ctx.draft_body.clone());
            let root_sink = StdAuditSink::new(root.root_audit_path(), root.expected_uid());
            if ctx.already_posted {
                closeout_already = Some(true);
            } else {
                // Final generation re-check immediately before the remote write: core
                // verbs don't take the publish lock, so a same-instance release+reclaim
                // could still have raced the renew/removal window. One guarded read
                // closes the whole local window; the residual (a reclaim between this
                // read and Linear committing the create) is irreducible without
                // cross-host locking — the marker makes such a comment self-identifying
                // and the generation-guarded release keeps local state truthful.
                match verify_generation(&lock_path, &root, &fs, &rec, &instance, now) {
                    Ok(()) => {}
                    // Successor/vanished BEFORE any post: graceful not_held (nothing
                    // published; the successor, if any, is intact). Consistent with the
                    // step-0/step-0.7/renew races.
                    Err(LaneError::Refused(RefusedReason::NotHeld)) => {
                        let data = close_data(
                            false,
                            worktree_removed,
                            skipped_missing,
                            None,
                            None,
                            None,
                            None,
                        );
                        return Ok((Outcome::NotHeld, Some(data), join_warnings(warnings)));
                    }
                    Err(error) => {
                        return Err(CommandError {
                            error,
                            audit_warning: join_warnings(warnings),
                        });
                    }
                }
                match api::post_comment(
                    deps.transport,
                    &ctx.api_url,
                    &ctx.secret,
                    &ctx.issue_uuid,
                    &ctx.draft_body,
                ) {
                    Ok(comment) => {
                        closeout_posted = Some(true);
                        closeout_url = comment.url;
                        let mut event = AuditEvent::new(
                            AuditEventKind::LinearWrite,
                            &args.repo,
                            &args.lane,
                            &instance,
                            AuditOutcome::Ok,
                            now,
                        );
                        event.linear_key = Some(ctx.key.clone());
                        if let Err(e) = root_sink.append(&event, true) {
                            warnings
                                .push(Some(format!("adapter audit degraded (linear_write): {e}")));
                        }
                    }
                    Err(error) => {
                        let mut event = AuditEvent::new(
                            AuditEventKind::LinearWrite,
                            &args.repo,
                            &args.lane,
                            &instance,
                            AuditOutcome::Error,
                            now,
                        );
                        event.linear_key = Some(ctx.key.clone());
                        if let Err(e) = root_sink.append(&event, true) {
                            warnings
                                .push(Some(format!("adapter audit degraded (linear_write): {e}")));
                        }
                        return Err(CommandError {
                            error,
                            audit_warning: join_warnings(warnings),
                        });
                    }
                }
            }
        }

        // Step 3 — release LAST, GENERATION-GUARDED (`release_core`'s internal mutex
        // re-verifies ownership AND the claim generation; it is the final authority —
        // a successor claim of the same lane survives a stale close, in every mode).
        let rel = ReleaseParams {
            repo: args.repo.clone(),
            lane: args.lane.clone(),
            instance,
            expected_claimed_at: Some(rec.claimed_at),
        };
        let released = match release_core(&root, &rel, now, &fs, &sink) {
            Ok(s) => s,
            // Generation mismatch at release. With NO external side effect committed
            // this run (non-post modes, or post mode that only deduped/never posted),
            // a same-instance successor is the graceful exit-0 not_held contract — the
            // successor is preserved (release_core refused it), exit code stays 0 so
            // teardown automation that keys on nonzero is unaffected. But if we DID
            // post this run, a successor appearing before release is a genuine anomaly
            // (a closeout is now on Linear for a generation that's gone): surface it as
            // the error, exit nonzero.
            Err(LaneError::Refused(RefusedReason::NotHeld))
                if closeout_posted != Some(true) && closeout_already != Some(true) =>
            {
                let data = close_data(
                    false,
                    worktree_removed,
                    skipped_missing,
                    None,
                    None,
                    None,
                    None,
                );
                return Ok((Outcome::NotHeld, Some(data), join_warnings(warnings)));
            }
            Err(error) => {
                return Err(CommandError {
                    error,
                    audit_warning: join_warnings(warnings),
                });
            }
        };
        warnings.push(released.audit_warning);

        let outcome = if released.present {
            Outcome::Released
        } else {
            Outcome::NotHeld
        };
        let data = close_data(
            released.present,
            worktree_removed,
            skipped_missing,
            closeout_draft_out,
            closeout_posted,
            closeout_already,
            closeout_url,
        );
        Ok((outcome, Some(data), join_warnings(warnings)))
        // `closeout` (holding the publish-lock guard) drops HERE — after release.
    })();
    emit(args.json, "close", repo, lane, result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::GitOutput;
    use crate::lock::audit::{AuditEvent, AuditEventKind, AuditSink};
    use crate::lock::paths::LaneRoot;
    use crate::lock::StdFs;

    /// A fake runner scripted for the compensation path: `show-ref` says the branch
    /// EXISTS (the failed add created it) and `branch -D` succeeds; calls are logged.
    struct CompensationGit {
        log: std::cell::RefCell<Vec<String>>,
        fail_delete: bool,
    }
    impl GitRunner for CompensationGit {
        fn run(&self, args: &[&str]) -> Result<GitOutput, GitError> {
            self.log.borrow_mut().push(args.join(" "));
            let joined = args.join(" ");
            if joined.contains("show-ref") {
                return Ok(GitOutput {
                    code: Some(0), // branch exists
                    stdout: String::new(),
                    stderr: String::new(),
                });
            }
            if joined.contains("branch -D") {
                return if self.fail_delete {
                    Ok(GitOutput {
                        code: Some(1),
                        stdout: String::new(),
                        stderr: "cannot delete".into(),
                    })
                } else {
                    Ok(GitOutput {
                        code: Some(0),
                        stdout: String::new(),
                        stderr: String::new(),
                    })
                };
            }
            Ok(GitOutput {
                code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    /// A sink that fails every append with the given kind (release's write-ahead intent,
    /// for the double-fault case) and forwards the rest to the real sink.
    struct FailOn {
        inner: StdAuditSink,
        kind: AuditEventKind,
    }
    impl AuditSink for FailOn {
        fn append(&self, event: &AuditEvent, fsync: bool) -> std::io::Result<()> {
            if event.event == self.kind {
                return Err(std::io::Error::other("injected audit failure"));
            }
            self.inner.append(event, fsync)
        }
    }

    fn setup(root_dir: &Path) -> (LaneRoot, StdAuditSink) {
        let home = std::env::var("HOME").unwrap();
        let root = LaneRoot::resolve(root_dir, Some(&home), &StdFs).unwrap();
        let sink = StdAuditSink::new(root.audit_path("ops"), root.expected_uid());
        (root, sink)
    }

    fn claim_demo(root: &LaneRoot, sink: &dyn AuditSink) {
        let params = ClaimParams {
            repo: "ops".into(),
            lane: "demo".into(),
            instance: "a".into(),
            home: std::env::var("HOME").ok(),
            ..Default::default()
        };
        claim_core(root, &params, Utc::now(), &StdFs, sink).unwrap();
    }

    #[test]
    fn compensation_deletes_created_branch_and_releases_the_claim() {
        let home = std::env::var("HOME").unwrap();
        let tmp = tempfile::Builder::new()
            .prefix("lane-lc-")
            .tempdir_in(&home)
            .unwrap();
        let (root, sink) = setup(tmp.path());
        claim_demo(&root, &sink);
        assert!(root.lock_path("ops", "demo").exists(), "claim on disk");

        let runner = CompensationGit {
            log: std::cell::RefCell::new(Vec::new()),
            fail_delete: false,
        };
        let git = GitAdapter::new(&runner);
        let warning = compensate_start_failure(
            &root,
            "ops",
            "demo",
            "a",
            Path::new("/repo"),
            "demo-branch",
            Path::new("/repo-demo-nonexistent"),
            true,
            &git,
            &StdFs,
            &sink,
            None,
            Utc::now(),
        );
        assert!(
            !root.lock_path("ops", "demo").exists(),
            "the claim was compensating-released"
        );
        let log = runner.log.borrow().join("\n");
        assert!(log.contains("branch -D demo-branch"), "branch deleted");
        assert!(
            warning.is_none() || !warning.as_deref().unwrap_or("").contains("FAILED"),
            "clean compensation carries no failure text, got {warning:?}"
        );
    }

    #[test]
    fn compensation_double_fault_reports_the_residual_claim() {
        let home = std::env::var("HOME").unwrap();
        let tmp = tempfile::Builder::new()
            .prefix("lane-lc-")
            .tempdir_in(&home)
            .unwrap();
        let (root, real_sink) = setup(tmp.path());
        claim_demo(&root, &real_sink);

        // The release's write-ahead intent append fails -> release_core aborts before
        // removing -> the claim survives and the warning NAMES it.
        let sink = FailOn {
            inner: StdAuditSink::new(root.audit_path("ops"), root.expected_uid()),
            kind: AuditEventKind::Intent,
        };
        let runner = CompensationGit {
            log: std::cell::RefCell::new(Vec::new()),
            fail_delete: false,
        };
        let git = GitAdapter::new(&runner);
        let warning = compensate_start_failure(
            &root,
            "ops",
            "demo",
            "a",
            Path::new("/repo"),
            "demo-branch",
            Path::new("/repo-demo-nonexistent"),
            true,
            &git,
            &StdFs,
            &sink,
            None,
            Utc::now(),
        )
        .expect("double fault produces a warning");
        assert!(
            warning.contains("compensating release FAILED"),
            "warning names the failed compensation: {warning}"
        );
        assert!(
            warning.contains("still held by a"),
            "warning names the residual claim holder: {warning}"
        );
        assert!(
            root.lock_path("ops", "demo").exists(),
            "the claim survives the failed release (TTL-bounded, releasable)"
        );
    }

    #[test]
    fn compensation_never_deletes_a_branch_this_run_did_not_create() {
        // THE RACE PIN: an external git process created the same branch name after our
        // precheck; our create_branch therefore failed (branch_created=false). The
        // compensation must NOT delete that foreign branch, no matter what an existence
        // probe would say.
        let home = std::env::var("HOME").unwrap();
        let tmp = tempfile::Builder::new()
            .prefix("lane-lc-")
            .tempdir_in(&home)
            .unwrap();
        let (root, sink) = setup(tmp.path());
        claim_demo(&root, &sink);

        let runner = CompensationGit {
            log: std::cell::RefCell::new(Vec::new()),
            fail_delete: false,
        };
        let git = GitAdapter::new(&runner);
        let _ = compensate_start_failure(
            &root,
            "ops",
            "demo",
            "a",
            Path::new("/repo"),
            "demo-branch",
            Path::new("/repo-demo-nonexistent"),
            false, // this run did NOT create the branch
            &git,
            &StdFs,
            &sink,
            None,
            Utc::now(),
        );
        let log = runner.log.borrow().join("\n");
        assert!(
            !log.contains("branch -D"),
            "a branch this run did not create is NEVER deleted, log: {log}"
        );
        assert!(
            !root.lock_path("ops", "demo").exists(),
            "the claim is still compensating-released"
        );
    }

    #[test]
    fn compensation_never_removes_a_foreign_worktree_at_the_path() {
        // Our branch was created, our worktree add failed, and a FOREIGN worktree
        // (attached to some other branch) occupies the path: it is left untouched
        // and named; only OUR branch is deleted.
        let home = std::env::var("HOME").unwrap();
        let tmp = tempfile::Builder::new()
            .prefix("lane-lc-")
            .tempdir_in(&home)
            .unwrap();
        let (root, sink) = setup(tmp.path());
        claim_demo(&root, &sink);

        struct ForeignGit(std::cell::RefCell<Vec<String>>);
        impl GitRunner for ForeignGit {
            fn run(&self, args: &[&str]) -> Result<GitOutput, GitError> {
                let joined = args.join(" ");
                self.0.borrow_mut().push(joined.clone());
                let reply = |out: &str| {
                    Ok(GitOutput {
                        code: Some(0),
                        stdout: out.to_string(),
                        stderr: String::new(),
                    })
                };
                if joined.contains("is-inside-work-tree") {
                    return reply("true");
                }
                if joined.contains("branch --show-current") {
                    return reply("someone-elses-branch");
                }
                reply("")
            }
        }
        let runner = ForeignGit(std::cell::RefCell::new(Vec::new()));
        let git = GitAdapter::new(&runner);
        let warning = compensate_start_failure(
            &root,
            "ops",
            "demo",
            "a",
            Path::new("/repo"),
            "demo-branch",
            Path::new("/repo-demo-foreign"),
            true,
            &git,
            &StdFs,
            &sink,
            None,
            Utc::now(),
        )
        .expect("foreign occupation is named");
        assert!(warning.contains("FOREIGN worktree"), "named: {warning}");
        let log = runner.0.borrow().join("\n");
        assert!(
            !log.contains("worktree remove"),
            "a foreign worktree is NEVER removed: {log}"
        );
        assert!(
            log.contains("branch -D demo-branch"),
            "our own branch is still deleted: {log}"
        );
    }

    #[test]
    fn cleanup_failure_is_reported_but_never_escalated() {
        let home = std::env::var("HOME").unwrap();
        let tmp = tempfile::Builder::new()
            .prefix("lane-lc-")
            .tempdir_in(&home)
            .unwrap();
        let (root, sink) = setup(tmp.path());
        claim_demo(&root, &sink);

        let runner = CompensationGit {
            log: std::cell::RefCell::new(Vec::new()),
            fail_delete: true,
        };
        let git = GitAdapter::new(&runner);
        let warning = compensate_start_failure(
            &root,
            "ops",
            "demo",
            "a",
            Path::new("/repo"),
            "demo-branch",
            Path::new("/repo-demo-nonexistent"),
            true,
            &git,
            &StdFs,
            &sink,
            None,
            Utc::now(),
        )
        .expect("cleanup failure produces a warning");
        assert!(
            warning.contains("could not delete branch"),
            "cleanup fault named: {warning}"
        );
        assert!(
            !root.lock_path("ops", "demo").exists(),
            "the release still ran (cleanup faults never block it)"
        );
    }

    // ------------------------------------------------------------------
    // Slice 4 — gated-closeout generation/serialization races (in-process:
    // these interpose between composition steps, which the spawned binary can't).
    // ------------------------------------------------------------------

    use crate::linear::transport::TransportError;
    use crate::proc::{ProcError, ProcOutput};
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A no-op fake `op` (the tests use `op://` roles; the secret value is canned).
    struct FakeOp;
    impl OpRunner for FakeOp {
        fn run(&self, _args: &[&str]) -> Result<ProcOutput, ProcError> {
            Ok(ProcOutput {
                code: Some(0),
                stdout: b"fake-close-key".to_vec(),
                stderr: Vec::new(),
            })
        }
    }

    /// A scriptable, thread-safe fake Linear transport. Preflights always report the
    /// marker ABSENT — the races below resolve through the GENERATION checks and the
    /// publish lock, never through marker dedupe (which the integration suite covers
    /// with real marker round-trips against the loopback fixture).
    struct FakeLinear {
        creates: AtomicU32,
        preflights: AtomicU32,
        /// Called on the FIRST preflight (interposition hook: swap generations, etc.).
        on_first_preflight: Option<Box<dyn Fn() + Sync + Send>>,
        /// Called just before a create is recorded (interposition hook).
        on_create: Option<Box<dyn Fn() + Sync + Send>>,
    }
    impl FakeLinear {
        fn new() -> Self {
            Self {
                creates: AtomicU32::new(0),
                preflights: AtomicU32::new(0),
                on_first_preflight: None,
                on_create: None,
            }
        }
    }
    impl LinearTransport for FakeLinear {
        fn post_json(
            &self,
            _url: &str,
            _auth: &SecretValue,
            body: &serde_json::Value,
        ) -> Result<serde_json::Value, TransportError> {
            let query = body["query"].as_str().unwrap_or_default();
            if query.contains("commentCreate") {
                if let Some(hook) = &self.on_create {
                    hook();
                }
                self.creates.fetch_add(1, Ordering::SeqCst);
                return Ok(json!({ "data": { "commentCreate": {
                    "success": true, "comment": { "url": "https://linear.app/c/1" } } } }));
            }
            let n = self.preflights.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                if let Some(hook) = &self.on_first_preflight {
                    hook();
                }
            }
            Ok(json!({ "data": { "issue": { "id": "uuid-1",
                "comments": { "nodes": [] } } } }))
        }
    }

    fn close_args(lane: &str, instance: &str, root: &Path) -> CloseArgs {
        CloseArgs {
            lane: lane.to_string(),
            repo: "ops".to_string(),
            remove_worktree: false,
            draft_closeout: false,
            post_closeout: true,
            json: true,
            lane_root: Some(root.to_path_buf()),
            instance: Some(instance.to_string()),
        }
    }

    fn write_close_config(root: &Path) {
        std::fs::write(
            root.join("config.toml"),
            "[secrets.roles]\nlinear_api = \"op://V/I/f\"\n[linear]\napi_url = \"https://ignored.invalid/graphql\"\n",
        )
        .unwrap();
    }

    fn claim_keyed(root: &LaneRoot, sink: &dyn AuditSink, lane: &str, instance: &str) {
        let params = ClaimParams {
            repo: "ops".into(),
            lane: lane.into(),
            instance: instance.into(),
            home: std::env::var("HOME").ok(),
            linear_key: Some("ZER-1".into()),
            ..Default::default()
        };
        claim_core(root, &params, Utc::now(), &StdFs, sink).unwrap();
    }

    /// Swap the lane's claim for a SUCCESSOR generation (same instance, fresh
    /// claimed_at) by editing the lock JSON — the same-instance release+reclaim race
    /// compressed into one step.
    fn swap_generation(root: &LaneRoot, lane: &str) {
        let path = root.lock_path("ops", lane);
        let text = std::fs::read_to_string(&path).unwrap();
        let mut rec: serde_json::Value = serde_json::from_str(&text).unwrap();
        let bumped = (Utc::now() + chrono::Duration::seconds(7)).to_rfc3339();
        rec["claimed_at"] = json!(bumped);
        std::fs::write(&path, rec.to_string()).unwrap();
    }

    #[test]
    fn release_generation_guard_unit() {
        let home = std::env::var("HOME").unwrap();
        let tmp = tempfile::Builder::new()
            .prefix("lane-gen-")
            .tempdir_in(&home)
            .unwrap();
        let (root, sink) = setup(tmp.path());
        claim_demo(&root, &sink);
        let rec = record::read_claim(
            &root.lock_path("ops", "demo"),
            root.path(),
            root.expected_uid(),
            &StdFs,
        )
        .unwrap()
        .unwrap();

        // Stale generation ⇒ not_held refusal; the claim survives.
        let stale = ReleaseParams {
            repo: "ops".into(),
            lane: "demo".into(),
            instance: "a".into(),
            expected_claimed_at: Some(rec.claimed_at + chrono::Duration::seconds(5)),
        };
        match release_core(&root, &stale, Utc::now(), &StdFs, &sink) {
            Err(LaneError::Refused(RefusedReason::NotHeld)) => {}
            Err(other) => panic!("wrong refusal: {other}"),
            Ok(_) => panic!("stale generation must not release"),
        }
        assert!(root.lock_path("ops", "demo").exists(), "claim survives");

        // Exact generation ⇒ released. (None-behavior is covered by every
        // pre-existing release test in the suite.)
        let exact = ReleaseParams {
            repo: "ops".into(),
            lane: "demo".into(),
            instance: "a".into(),
            expected_claimed_at: Some(rec.claimed_at),
        };
        let ok = release_core(&root, &exact, Utc::now(), &StdFs, &sink).unwrap();
        assert!(ok.present);
    }

    #[test]
    fn post_closeout_generation_swap_before_post_aborts_with_zero_creates() {
        let home = std::env::var("HOME").unwrap();
        let tmp = tempfile::Builder::new()
            .prefix("lane-swap-")
            .tempdir_in(&home)
            .unwrap();
        let (root, sink) = setup(tmp.path());
        write_close_config(root.path());
        claim_keyed(&root, &sink, "demo", "a");

        let mut transport = FakeLinear::new();
        let root_dir = root.path().to_path_buf();
        transport.on_first_preflight = Some(Box::new(move || {
            // Interpose AFTER the publish lock + generation check #1, BEFORE any
            // renew/post: the same-instance reclaim race.
            let home = std::env::var("HOME").unwrap();
            let r = LaneRoot::resolve(&root_dir, Some(&home), &StdFs).unwrap();
            swap_generation(&r, "demo");
        }));
        let git = CompensationGit {
            log: std::cell::RefCell::new(Vec::new()),
            fail_delete: false,
        };
        let deps = CloseDeps {
            git: &git,
            op: &FakeOp,
            transport: &transport,
        };
        let args = close_args("demo", "a", root.path());
        let code = run_close_at(&args, Utc::now(), &deps);
        // A same-instance successor detected BEFORE any post is the graceful exit-0
        // not_held contract (nothing published; the successor is intact) — consistent
        // with the step-0 / renew races.
        assert_eq!(code, 0, "graceful not_held, nothing posted");
        assert_eq!(
            transport.creates.load(Ordering::SeqCst),
            0,
            "nothing was published"
        );
        assert!(
            root.lock_path("ops", "demo").exists(),
            "the successor claim is untouched"
        );
    }

    #[test]
    fn post_closeout_generation_swap_during_create_never_releases_successor() {
        let home = std::env::var("HOME").unwrap();
        let tmp = tempfile::Builder::new()
            .prefix("lane-swap2-")
            .tempdir_in(&home)
            .unwrap();
        let (root, sink) = setup(tmp.path());
        write_close_config(root.path());
        claim_keyed(&root, &sink, "demo", "a");

        let mut transport = FakeLinear::new();
        let root_dir = root.path().to_path_buf();
        transport.on_create = Some(Box::new(move || {
            // Interpose between the final pre-post generation check and release:
            // the create lands, then release must refuse the successor.
            let home = std::env::var("HOME").unwrap();
            let r = LaneRoot::resolve(&root_dir, Some(&home), &StdFs).unwrap();
            swap_generation(&r, "demo");
        }));
        let git = CompensationGit {
            log: std::cell::RefCell::new(Vec::new()),
            fail_delete: false,
        };
        let deps = CloseDeps {
            git: &git,
            op: &FakeOp,
            transport: &transport,
        };
        let args = close_args("demo", "a", root.path());
        let code = run_close_at(&args, Utc::now(), &deps);
        assert_eq!(code, 1, "generation-guarded release refuses");
        assert_eq!(
            transport.creates.load(Ordering::SeqCst),
            1,
            "exactly one create happened before the swap was detected"
        );
        assert!(
            root.lock_path("ops", "demo").exists(),
            "the successor claim survives the stale close"
        );
    }

    #[test]
    fn concurrent_post_closeouts_publish_at_most_once() {
        let home = std::env::var("HOME").unwrap();
        let tmp = tempfile::Builder::new()
            .prefix("lane-race-")
            .tempdir_in(&home)
            .unwrap();
        let (root, sink) = setup(tmp.path());
        write_close_config(root.path());
        claim_keyed(&root, &sink, "demo", "a");

        let transport = FakeLinear::new();
        let root_path = root.path().to_path_buf();
        let codes = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..2 {
                let transport = &transport;
                let root_path = root_path.clone();
                let codes = &codes;
                scope.spawn(move || {
                    let git = CompensationGit {
                        log: std::cell::RefCell::new(Vec::new()),
                        fail_delete: false,
                    };
                    let deps = CloseDeps {
                        git: &git,
                        op: &FakeOp,
                        transport,
                    };
                    let args = close_args("demo", "a", &root_path);
                    let code = run_close_at(&args, Utc::now(), &deps);
                    codes.lock().unwrap().push(code);
                });
            }
        });
        let creates = transport.creates.load(Ordering::SeqCst);
        assert!(creates <= 1, "at most one create, saw {creates}");
        let codes = codes.into_inner().unwrap();
        assert!(codes.contains(&0), "one racer released cleanly: {codes:?}");
        assert!(
            !root.lock_path("ops", "demo").exists(),
            "the lane is released exactly once"
        );
    }
}
