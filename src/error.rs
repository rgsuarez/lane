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
}

/// The single error type returned by every fallible locking-core path.
#[derive(Debug)]
pub enum LaneError {
    /// A safe, expected refusal (exit 1).
    Refused(RefusedReason),
    /// Identity-inconsistent on-disk state, interior-state symlink, or a post-parse
    /// validation failure (exit 2).
    Identity(String),
    /// The resolved lane root is not on the local filesystem (exit 2).
    NonLocalRoot(String),
    /// A claim or audit record could not be parsed, or violated its on-disk shape (exit 2).
    Malformed { path: PathBuf, detail: String },
    /// Any underlying I/O error (exit 2).
    Io(io::Error),
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
    Identity,
    Malformed,
    NonLocalRoot,
    Io,
}

impl LaneError {
    /// Process exit code: `1` for refusals, `2` for everything else.
    pub fn exit_code(&self) -> i32 {
        match self {
            LaneError::Refused(_) => 1,
            LaneError::Identity(_)
            | LaneError::NonLocalRoot(_)
            | LaneError::Malformed { .. }
            | LaneError::Io(_) => 2,
        }
    }

    /// The JSON `reason` for the error envelope.
    pub fn reason(&self) -> Reason {
        match self {
            LaneError::Refused(RefusedReason::ActiveHeld) => Reason::ActiveHeld,
            LaneError::Refused(RefusedReason::NotOwner) => Reason::NotOwner,
            LaneError::Refused(RefusedReason::TargetOverlap) => Reason::TargetOverlap,
            LaneError::Refused(RefusedReason::MutexBusy) => Reason::MutexBusy,
            LaneError::Refused(RefusedReason::Expired) => Reason::Expired,
            LaneError::Refused(RefusedReason::NotHeld) => Reason::NotHeld,
            LaneError::Identity(_) => Reason::Identity,
            LaneError::NonLocalRoot(_) => Reason::NonLocalRoot,
            LaneError::Malformed { .. } => Reason::Malformed,
            LaneError::Io(_) => Reason::Io,
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
            LaneError::Identity(m) => write!(f, "identity error: {m}"),
            LaneError::NonLocalRoot(m) => {
                write!(f, "lane root is not on a local filesystem: {m}")
            }
            LaneError::Malformed { path, detail } => {
                write!(f, "malformed record at {}: {detail}", path.display())
            }
            LaneError::Io(e) => write!(f, "io error: {e}"),
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
    }

    #[test]
    fn reason_serializes_snake_case() {
        let j = serde_json::to_string(&Reason::ActiveHeld).unwrap();
        assert_eq!(j, "\"active_held\"");
        let j = serde_json::to_string(&Reason::NonLocalRoot).unwrap();
        assert_eq!(j, "\"non_local_root\"");
    }
}
