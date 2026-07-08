//! `lane check` — read-only claim-coverage verdict (Slice 3.5, ZER-84).
//!
//! Answers one question: **does an ACTIVE claim owned by THIS instance cover the queried
//! path?** Exit 0 = covered-and-mine. Refusals (exit 1): `uncovered` (no covering active
//! claim — expired-only and target-less near-misses are named in the message),
//! `foreign_owner` (a covering active claim is held by a DIFFERENT instance — the
//! collision case the guard exists to surface), `no_identity` (no `--instance` /
//! `$LANE_INSTANCE`; identity is never guessed). Integrity/malformed stay exit 2.
//!
//! Read-only by law (§S2.13 read path): the scan is [`list_core`] (guarded reads,
//! fail-closed), there is NO audit interaction, NO mutation, NO subprocess; fully
//! offline. The scan defaults to ALL repo namespaces — claim targets are machine-global
//! absolute paths, and namespace inference from a git toplevel basename is wrong inside
//! worktrees. The refusal message is the product surface: it carries the exact fix
//! command with real values, composed here and carried by [`LaneError::RefusedMsg`]
//! through the single `emit` print path.

use chrono::{DateTime, Utc};

use crate::error::{LaneError, RefusedReason};
use crate::lock::paths::LaneRoot;
use crate::lock::renew_release::list_core;
use crate::lock::target::Target;
use crate::lock::{validate_instance, FsOps};
use crate::model::StaleState;

/// Inputs to [`check_core`].
pub struct CheckParams {
    /// Optional namespace filter — narrows the scan AND the suggested fix command.
    /// `None` (the safe hook default) scans every namespace under the lane root.
    pub repo: Option<String>,
    /// The raw path to check (absolute, or `~`/`$HOME`-prefixed; the runner has already
    /// absolutized cwd-relative input).
    pub path: String,
    /// The caller identity (already resolved and validated).
    pub instance: String,
    pub home: Option<String>,
}

/// A successful (covered-and-mine) verdict.
pub struct CheckSuccess {
    /// The normalized queried path.
    pub path: String,
    /// The covering claim's coordinates.
    pub repo: String,
    pub lane: String,
    pub instance: String,
    pub target: String,
    pub expires_at: DateTime<Utc>,
    /// A foreign active claim ALSO covering the path (cross-namespace co-coverage —
    /// exactly the collision this tool surfaces; reported, not fatal).
    pub warning: Option<String>,
}

/// Resolve the caller identity for `check`: flag, then `$LANE_INSTANCE`.
pub(crate) fn resolve_check_identity(arg: Option<String>) -> Result<String, LaneError> {
    resolve_check_identity_from(arg, std::env::var("LANE_INSTANCE").ok())
}

/// Pure resolver (testable without process env). ABSENCE — including an empty or
/// whitespace-only value, following the `resolve_raw_root` empty-env precedent — is a
/// safe `no_identity` refusal (exit 1): that is the guard's uncovered-identity case. A
/// PRESENT but invalid identity stays `Identity` (exit 2) like every other verb.
pub(crate) fn resolve_check_identity_from(
    arg: Option<String>,
    env_val: Option<String>,
) -> Result<String, LaneError> {
    let candidate = arg.or(env_val);
    let s = match candidate {
        Some(s) if !s.trim().is_empty() => s,
        _ => {
            return Err(LaneError::RefusedMsg {
                reason: RefusedReason::NoIdentity,
                msg: "no caller identity; pass --instance <id> or export LANE_INSTANCE=<id>".into(),
            })
        }
    };
    validate_instance(&s)?;
    Ok(s)
}

/// One covering claim, kept for the verdict.
struct Covering {
    repo: String,
    lane: String,
    instance: String,
    target: String,
    depth: usize,
    expires_at: DateTime<Utc>,
}

/// Segment depth of a normalized path (tie-break key: deepest target wins).
fn depth_of(normalized: &str) -> usize {
    normalized.split('/').filter(|s| !s.is_empty()).count()
}

/// The coverage verdict.
///
/// Normalizes the query with the SAME [`Target::resolve`] canonicalization claims use
/// (so it inherits the exit-2 rejections: relative, `.`/`..`, `/`, exactly-`$HOME`,
/// lane-root overlap, non-ASCII unresolved tail — none of those can be claim targets
/// either). Scans via [`list_core`] (guarded, fail-closed, sorted); a claim's stored
/// target falls back `target_normalized` → `target` exactly like `scan_overlap`, so
/// Slice-1 records keep covering. ACTIVE means `stale_state != Expired` — an
/// idle-but-unexpired (`PossiblyStale`) claim still covers and still blocks.
pub fn check_core(
    root: &LaneRoot,
    p: &CheckParams,
    now: DateTime<Utc>,
    fs: &dyn FsOps,
) -> Result<CheckSuccess, LaneError> {
    let queried = Target::resolve(&p.path, p.home.as_deref(), root.path())?;
    let rows = list_core(root, p.repo.as_deref(), now, fs)?;

    let mut mine: Vec<Covering> = Vec::new();
    let mut foreign: Vec<Covering> = Vec::new();
    // Near-miss hints, first-in-scan-order: an expired own claim that WOULD cover, and
    // an active own claim with no target at all.
    let mut expired_own: Option<Covering> = None;
    let mut targetless_own: Option<(String, String)> = None;

    for row in rows {
        let Some(rec) = row.record else { continue };
        let active = row.stale_state != Some(StaleState::Expired);
        // Mirror scan_overlap's fallback: Slice-1 records may lack `target_normalized`.
        let norm = rec.target_normalized.clone().or_else(|| rec.target.clone());
        let Some(n) = norm else {
            if active && rec.instance == p.instance && targetless_own.is_none() {
                targetless_own = Some((rec.repo, rec.lane));
            }
            continue;
        };
        if !Target::from_normalized(&n).covers(&queried) {
            continue;
        }
        let c = Covering {
            depth: depth_of(&n),
            repo: rec.repo,
            lane: rec.lane,
            instance: rec.instance,
            target: n,
            expires_at: rec.expires_at,
        };
        if !active {
            if c.instance == p.instance && expired_own.is_none() {
                expired_own = Some(c);
            }
            continue;
        }
        if c.instance == p.instance {
            mine.push(c);
        } else {
            foreign.push(c);
        }
    }

    // Deterministic best-of: deepest target, then latest expiry; stable sort preserves
    // list_core's (repo, lane) order for full ties.
    let best = |v: &mut Vec<Covering>| {
        v.sort_by(|a, b| b.depth.cmp(&a.depth).then(b.expires_at.cmp(&a.expires_at)));
    };
    best(&mut mine);
    best(&mut foreign);

    if let Some(m) = mine.into_iter().next() {
        let warning = foreign.first().map(|f| {
            format!(
                "also covered by active claim {}/{} held by {}",
                f.repo, f.lane, f.instance
            )
        });
        return Ok(CheckSuccess {
            path: queried.normalized().to_string(),
            repo: m.repo,
            lane: m.lane,
            instance: m.instance,
            target: m.target,
            expires_at: m.expires_at,
            warning,
        });
    }

    if let Some(f) = foreign.into_iter().next() {
        return Err(LaneError::RefusedMsg {
            reason: RefusedReason::ForeignOwner,
            msg: format!(
                "{} is covered by {}/{} target {} held by {}; expires {}; coordinate with {} or work under a different path",
                queried.normalized(),
                f.repo,
                f.lane,
                f.target,
                f.instance,
                f.expires_at.to_rfc3339(),
                f.instance,
            ),
        });
    }

    // Uncovered — tiered hint, most actionable first.
    let msg = if let Some(e) = expired_own {
        // Expired is takeable WITHOUT --force (claim.rs classify_existing).
        format!(
            "your claim {}/{} covering {} expired at {}; fix: lane claim {} --repo {} --target {}",
            e.repo,
            e.lane,
            queried.normalized(),
            e.expires_at.to_rfc3339(),
            e.lane,
            e.repo,
            e.target,
        )
    } else if let Some((repo, lane)) = targetless_own {
        // Same-instance re-claim of an ACTIVE lane refuses active_held, so the fix
        // command must carry --force.
        format!(
            "no active claim covers {}; your claim {}/{} has no target; fix: lane claim {} --repo {} --target {} --force",
            queried.normalized(),
            repo,
            lane,
            lane,
            repo,
            queried.normalized(),
        )
    } else {
        let ns = p.repo.as_deref().unwrap_or("<repo>");
        format!(
            "no active claim covers {}; fix: lane claim <lane> --repo {} --target {} (or: lane start <lane> --repo {} --git-repo <repo-path>)",
            queried.normalized(),
            ns,
            queried.normalized(),
            ns,
        )
    };
    Err(LaneError::RefusedMsg {
        reason: RefusedReason::Uncovered,
        msg,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_absent_is_a_no_identity_refusal() {
        for (arg, env) in [
            (None, None),
            (None, Some("".to_string())),
            (None, Some("   ".to_string())),
        ] {
            let e = resolve_check_identity_from(arg, env).unwrap_err();
            assert!(matches!(
                e,
                LaneError::RefusedMsg {
                    reason: RefusedReason::NoIdentity,
                    ..
                }
            ));
            assert_eq!(e.exit_code(), 1);
        }
    }

    #[test]
    fn identity_invalid_stays_exit_2() {
        let e = resolve_check_identity_from(Some("bad\u{7}name".into()), None).unwrap_err();
        assert!(matches!(e, LaneError::Identity(_)));
        assert_eq!(e.exit_code(), 2);
    }

    #[test]
    fn identity_flag_beats_env() {
        let s = resolve_check_identity_from(Some("flag".into()), Some("env".into())).unwrap();
        assert_eq!(s, "flag");
    }

    #[test]
    fn depth_counts_segments() {
        assert_eq!(depth_of("/a/b/c"), 3);
        assert_eq!(depth_of("/"), 0);
    }
}
