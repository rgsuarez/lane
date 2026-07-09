//! The single authoritative error type for the locking core and its CLI verbs.
//!
//! `LaneError` maps 1:1 to a process exit code ([`LaneError::exit_code`]) and a JSON
//! [`Reason`] ([`LaneError::reason`]). This is the *only* place either mapping is
//! declared — there is no second exit-code or reason table anywhere in the crate
//! (the §S2.13 contract).
//!
//! Exit codes: `1` for safe refusals, `2` for everything else (identity / non-local
//! root / malformed / io). Clap usage errors are handled by Clap itself (human-only,
//! stderr, exit 2) before `--json` is ever parsed, so they are never an envelope.

use std::fmt;
use std::io;
use std::path::PathBuf;

use serde::Serialize;

/// Why a refusal happened. Every variant maps to exit code `1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusedReason {
    /// An active, non-expired claim is held and `--force` was not given.
    ActiveHeld,
    /// renew/release was attempted by a non-owning instance.
    NotOwner,
    /// A sibling claim holds an equal or ancestor/descendant target.
    TargetOverlap,
    /// The per-lane or target mutex was held past the acquisition window.
    MutexBusy,
    /// renew was attempted against a lapsed (expired) lease (re-`claim` instead).
    Expired,
    /// renew was attempted against a lane that is not currently held.
    NotHeld,
    /// `close --remove-worktree` was refused because git reports the worktree has
    /// modified or untracked files (never removed with `--force`; the claim stays held).
    DirtyWorktree,
    /// `check`: no active claim owned by the caller covers the queried path (includes
    /// expired-only and target-less-only near-misses — the message says which).
    Uncovered,
    /// `check`: the queried path is covered by an active claim held by a DIFFERENT
    /// instance — the collision case the guard exists to surface.
    ForeignOwner,
    /// `check`: no caller identity was provided (`--instance` / `$LANE_INSTANCE`).
    /// ABSENCE is a safe refusal (exit 1); an INVALID identity stays `Identity` (exit 2).
    NoIdentity,
    /// `hook`: lane refuses to compose/modify a git hook file it cannot edit safely
    /// (managed `core.hooksPath`, symlink, non-executable / non-text / oversize file,
    /// damaged markers). One machine reason; the message carries the case detail.
    HookComposeRefused,
    /// `close --draft-closeout`/`--post-closeout`: the claim records no `linear_key`,
    /// so there is no Linear issue to draft against (run plain `close`, or re-claim
    /// with `--linear-key`). Normally constructed as `RefusedMsg` with the fix text.
    NoLinearKey,
}

impl RefusedReason {
    /// The JSON `reason` code for this refusal (the closed envelope enum).
    fn to_reason(self) -> Reason {
        match self {
            RefusedReason::ActiveHeld => Reason::ActiveHeld,
            RefusedReason::NotOwner => Reason::NotOwner,
            RefusedReason::TargetOverlap => Reason::TargetOverlap,
            RefusedReason::MutexBusy => Reason::MutexBusy,
            RefusedReason::Expired => Reason::Expired,
            RefusedReason::NotHeld => Reason::NotHeld,
            RefusedReason::DirtyWorktree => Reason::DirtyWorktree,
            RefusedReason::Uncovered => Reason::Uncovered,
            RefusedReason::ForeignOwner => Reason::ForeignOwner,
            RefusedReason::NoIdentity => Reason::NoIdentity,
            RefusedReason::HookComposeRefused => Reason::HookComposeRefused,
            RefusedReason::NoLinearKey => Reason::NoLinearKey,
        }
    }
}

/// The single error type returned by every fallible locking-core path.
#[derive(Debug)]
pub enum LaneError {
    /// A safe, expected refusal (exit 1).
    Refused(RefusedReason),
    /// A safe refusal (exit 1) carrying a fully-composed, non-secret context message.
    /// `reason` is the closed machine code for the envelope; `msg` is the human fix text
    /// (Display prints `refused: {msg}` — for `check`/`hook` the message IS the exact fix
    /// command with real values). The closed [`Reason`] enum stays payload-free.
    RefusedMsg { reason: RefusedReason, msg: String },
    /// Identity-inconsistent on-disk state, interior-state symlink, or a post-parse
    /// validation failure (exit 2).
    Identity(String),
    /// The resolved lane root is not on the local filesystem (exit 2).
    NonLocalRoot(String),
    /// A claim, audit, or config record could not be parsed, or violated its on-disk
    /// shape (exit 2).
    Malformed { path: PathBuf, detail: String },
    /// Any underlying I/O error (exit 2).
    Io(io::Error),
    /// A secret could not be resolved: `op` missing / not signed in / role unmapped /
    /// env pointer unset / empty or non-UTF-8 value (exit 2). The message names the
    /// role key and the actionable fix — NEVER a reference, a value, or `op` stderr.
    SecretUnavailable(String),
    /// A network-verb transport failure: connect/TLS/timeout, non-2xx HTTP, or a
    /// GraphQL-level error (exit 2). The message is a closed classification — never a
    /// response-body dump. Local verbs can never produce this.
    Network(String),
}

/// The closed set of JSON `reason` values (snake_case). Present in the envelope iff
/// `outcome ∈ {refused, error}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    ActiveHeld,
    NotOwner,
    TargetOverlap,
    MutexBusy,
    Expired,
    NotHeld,
    DirtyWorktree,
    Uncovered,
    ForeignOwner,
    NoIdentity,
    HookComposeRefused,
    NoLinearKey,
    Identity,
    Malformed,
    NonLocalRoot,
    Io,
    SecretUnavailable,
    Network,
}

impl LaneError {
    /// Process exit code: `1` for refusals, `2` for everything else.
    pub fn exit_code(&self) -> i32 {
        match self {
            LaneError::Refused(_) | LaneError::RefusedMsg { .. } => 1,
            LaneError::Identity(_)
            | LaneError::NonLocalRoot(_)
            | LaneError::Malformed { .. }
            | LaneError::Io(_)
            | LaneError::SecretUnavailable(_)
            | LaneError::Network(_) => 2,
        }
    }

    /// The JSON `reason` for the error envelope.
    pub fn reason(&self) -> Reason {
        match self {
            LaneError::Refused(r) => r.to_reason(),
            LaneError::RefusedMsg { reason, .. } => reason.to_reason(),
            LaneError::Identity(_) => Reason::Identity,
            LaneError::NonLocalRoot(_) => Reason::NonLocalRoot,
            LaneError::Malformed { .. } => Reason::Malformed,
            LaneError::Io(_) => Reason::Io,
            LaneError::SecretUnavailable(_) => Reason::SecretUnavailable,
            LaneError::Network(_) => Reason::Network,
        }
    }
}

impl fmt::Display for LaneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LaneError::Refused(RefusedReason::ActiveHeld) => {
                write!(f, "lane is actively held (use --force to take over)")
            }
            LaneError::Refused(RefusedReason::NotOwner) => {
                write!(f, "refused: caller does not own this lane")
            }
            LaneError::Refused(RefusedReason::TargetOverlap) => {
                write!(f, "refused: target overlaps an active claim")
            }
            LaneError::Refused(RefusedReason::MutexBusy) => {
                write!(f, "refused: lane is busy (mutex held)")
            }
            LaneError::Refused(RefusedReason::Expired) => {
                write!(f, "refused: lease has expired (re-claim instead of renew)")
            }
            LaneError::Refused(RefusedReason::NotHeld) => {
                write!(f, "refused: lane is not held")
            }
            LaneError::Refused(RefusedReason::DirtyWorktree) => {
                write!(
                    f,
                    "refused: worktree has modified or untracked files (close without --remove-worktree, or clean it first)"
                )
            }
            // Generic fallbacks for the context-rich refusals (defensive exhaustiveness;
            // `check`/`hook` normally construct `RefusedMsg` with the composed fix text).
            LaneError::Refused(RefusedReason::Uncovered) => {
                write!(f, "refused: path is not covered by an active claim")
            }
            LaneError::Refused(RefusedReason::ForeignOwner) => {
                write!(
                    f,
                    "refused: path is covered by a claim held by another instance"
                )
            }
            LaneError::Refused(RefusedReason::NoIdentity) => {
                write!(
                    f,
                    "refused: no caller identity; pass --instance <id> or export LANE_INSTANCE=<id>"
                )
            }
            LaneError::Refused(RefusedReason::HookComposeRefused) => {
                write!(f, "refused: cannot compose the git hook safely")
            }
            LaneError::Refused(RefusedReason::NoLinearKey) => {
                write!(f, "refused: claim records no linear_key")
            }
            LaneError::RefusedMsg { msg, .. } => write!(f, "refused: {msg}"),
            LaneError::Identity(m) => write!(f, "identity error: {m}"),
            LaneError::NonLocalRoot(m) => {
                write!(f, "lane root is not on a local filesystem: {m}")
            }
            LaneError::Malformed { path, detail } => {
                write!(f, "malformed record at {}: {detail}", path.display())
            }
            LaneError::Io(e) => write!(f, "io error: {e}"),
            LaneError::SecretUnavailable(m) => write!(f, "secret unavailable: {m}"),
            LaneError::Network(m) => write!(f, "network error: {m}"),
        }
    }
}

impl std::error::Error for LaneError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LaneError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for LaneError {
    fn from(e: io::Error) -> Self {
        LaneError::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_table() {
        assert_eq!(LaneError::Refused(RefusedReason::ActiveHeld).exit_code(), 1);
        assert_eq!(LaneError::Refused(RefusedReason::NotOwner).exit_code(), 1);
        assert_eq!(
            LaneError::Refused(RefusedReason::TargetOverlap).exit_code(),
            1
        );
        assert_eq!(LaneError::Refused(RefusedReason::MutexBusy).exit_code(), 1);
        assert_eq!(LaneError::Refused(RefusedReason::Expired).exit_code(), 1);
        assert_eq!(LaneError::Refused(RefusedReason::NotHeld).exit_code(), 1);
        assert_eq!(
            LaneError::Refused(RefusedReason::DirtyWorktree).exit_code(),
            1
        );
        assert_eq!(LaneError::Refused(RefusedReason::Uncovered).exit_code(), 1);
        assert_eq!(
            LaneError::Refused(RefusedReason::ForeignOwner).exit_code(),
            1
        );
        assert_eq!(LaneError::Refused(RefusedReason::NoIdentity).exit_code(), 1);
        assert_eq!(
            LaneError::Refused(RefusedReason::HookComposeRefused).exit_code(),
            1
        );
        assert_eq!(
            LaneError::RefusedMsg {
                reason: RefusedReason::Uncovered,
                msg: "x".into()
            }
            .exit_code(),
            1
        );
        assert_eq!(
            LaneError::Refused(RefusedReason::NoLinearKey).exit_code(),
            1
        );
        assert_eq!(LaneError::Identity("x".into()).exit_code(), 2);
        assert_eq!(LaneError::NonLocalRoot("x".into()).exit_code(), 2);
        assert_eq!(
            LaneError::Malformed {
                path: PathBuf::from("/x"),
                detail: "y".into()
            }
            .exit_code(),
            2
        );
        assert_eq!(LaneError::Io(io::Error::other("z")).exit_code(), 2);
        assert_eq!(LaneError::SecretUnavailable("x".into()).exit_code(), 2);
        assert_eq!(LaneError::Network("x".into()).exit_code(), 2);
    }

    #[test]
    fn reason_matches_variant() {
        assert_eq!(
            LaneError::Refused(RefusedReason::TargetOverlap).reason(),
            Reason::TargetOverlap
        );
        assert_eq!(
            LaneError::Refused(RefusedReason::NotHeld).reason(),
            Reason::NotHeld
        );
        assert_eq!(LaneError::Identity("x".into()).reason(), Reason::Identity);
        assert_eq!(
            LaneError::NonLocalRoot("x".into()).reason(),
            Reason::NonLocalRoot
        );
        assert_eq!(
            LaneError::RefusedMsg {
                reason: RefusedReason::ForeignOwner,
                msg: "held by other".into()
            }
            .reason(),
            Reason::ForeignOwner
        );
        assert_eq!(
            LaneError::Refused(RefusedReason::NoIdentity).reason(),
            Reason::NoIdentity
        );
        assert_eq!(
            LaneError::Refused(RefusedReason::NoLinearKey).reason(),
            Reason::NoLinearKey
        );
        assert_eq!(
            LaneError::SecretUnavailable("x".into()).reason(),
            Reason::SecretUnavailable
        );
        assert_eq!(LaneError::Network("x".into()).reason(), Reason::Network);
    }

    #[test]
    fn reason_serializes_snake_case() {
        let j = serde_json::to_string(&Reason::ActiveHeld).unwrap();
        assert_eq!(j, "\"active_held\"");
        let j = serde_json::to_string(&Reason::NonLocalRoot).unwrap();
        assert_eq!(j, "\"non_local_root\"");
        let j = serde_json::to_string(&Reason::DirtyWorktree).unwrap();
        assert_eq!(j, "\"dirty_worktree\"");
        let j = serde_json::to_string(&Reason::Uncovered).unwrap();
        assert_eq!(j, "\"uncovered\"");
        let j = serde_json::to_string(&Reason::ForeignOwner).unwrap();
        assert_eq!(j, "\"foreign_owner\"");
        let j = serde_json::to_string(&Reason::NoIdentity).unwrap();
        assert_eq!(j, "\"no_identity\"");
        let j = serde_json::to_string(&Reason::HookComposeRefused).unwrap();
        assert_eq!(j, "\"hook_compose_refused\"");
        let j = serde_json::to_string(&Reason::NoLinearKey).unwrap();
        assert_eq!(j, "\"no_linear_key\"");
        let j = serde_json::to_string(&Reason::SecretUnavailable).unwrap();
        assert_eq!(j, "\"secret_unavailable\"");
        let j = serde_json::to_string(&Reason::Network).unwrap();
        assert_eq!(j, "\"network\"");
    }

    #[test]
    fn refused_msg_display_carries_the_composed_text() {
        let e = LaneError::RefusedMsg {
            reason: RefusedReason::Uncovered,
            msg: "no active claim covers /x; fix: lane claim demo --repo ops --target /x".into(),
        };
        assert_eq!(
            e.to_string(),
            "refused: no active claim covers /x; fix: lane claim demo --repo ops --target /x"
        );
    }
}
