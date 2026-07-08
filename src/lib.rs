//! `lane` — Linear-first local agent-work orchestration.
//!
//! Slice 1 (this build) is **read-only and offline**: the `lane board` command
//! reads claim records from local lock files (authoritative) and joins fixture/stub
//! providers for worktrees, Linear, and liveness. There is no network path, no tmux,
//! no Vantage, and no 1Password in this slice. See `docs/lane_SPEC.md`.
#![forbid(unsafe_code)]

pub mod board;
pub mod cli;
pub mod error;
pub mod git;
pub mod hook;
pub mod lifecycle;
pub mod lock;
pub mod model;
pub mod output;

pub use board::{assemble, classify_stale, BoardInputs};
pub use error::{LaneError, Reason, RefusedReason};
pub use model::{
    Board, BoardRow, ClaimRecord, Liveness, Provenance, Provenanced, SourceFreshness, SourceKind,
    StaleState,
};
