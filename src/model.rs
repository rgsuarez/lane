//! Data model for the read-only board. Every board field is tagged with its
//! [`Provenance`] so a reader can tell authoritative facts from derived or fixture data.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Where a value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Local claim lock files — the source of truth for "who holds what here".
    Authoritative,
    /// Computed locally from another source (e.g. staleness from timestamps).
    Derived,
    /// Read from a fixture file (Slice 1 stand-in for a real provider).
    Fixture,
    /// Not determinable in this slice (e.g. liveness without overseer/tmux).
    Unknown,
}

/// A value tagged with its [`Provenance`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenanced<T> {
    pub value: T,
    pub provenance: Provenance,
}

impl<T> Provenanced<T> {
    pub fn new(value: T, provenance: Provenance) -> Self {
        Self { value, provenance }
    }
    pub fn authoritative(value: T) -> Self {
        Self::new(value, Provenance::Authoritative)
    }
    pub fn derived(value: T) -> Self {
        Self::new(value, Provenance::Derived)
    }
    pub fn fixture(value: T) -> Self {
        Self::new(value, Provenance::Fixture)
    }
    pub fn unknown(value: T) -> Self {
        Self::new(value, Provenance::Unknown)
    }
}

/// Role a lane plays in a pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Executor,
    Advisor,
}

/// Lifecycle status of a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStatus {
    Active,
    Blocked,
    Handoff,
}

/// Current gate a lane sits at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    Plan,
    Execute,
    Review,
    Smoke,
    Migration,
    Merge,
    Closeout,
}

/// A claim lock record (read from `LANE_ROOT/<repo>/locks/<lane>.lock`).
///
/// Unknown JSON keys are ignored (forward-compatible with future schema additions).
/// `pid` is informational only and never used for liveness.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimRecord {
    /// Lock schema version. Slice 2 writes `Some(1)`; Slice 1 records and fixtures
    /// without the key deserialize as `None`. Evolution rule: additive optional fields
    /// only; never remove/repurpose a field without bumping this and updating readers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u32>,
    pub lane: String,
    pub repo: String,
    pub instance: String,
    #[serde(default)]
    pub pid: Option<i64>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub target_normalized: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    pub claimed_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub ttl_hours: f64,
    #[serde(default)]
    pub linear_key: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub role: Option<Role>,
    #[serde(default)]
    pub pr_url: Option<String>,
    #[serde(default)]
    pub gate: Option<Gate>,
    #[serde(default)]
    pub plan_path: Option<String>,
    #[serde(default)]
    pub claim_status: Option<ClaimStatus>,
    #[serde(default)]
    pub session_ref: Option<String>,
}

/// Liveness of the session behind a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Liveness {
    Live,
    NotLive,
    Unknown,
}

/// Staleness classification of a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleState {
    Active,
    Expired,
    PossiblyStale,
    Orphaned,
}

/// Which input source a [`SourceFreshness`] describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Claims,
    Worktrees,
    Linear,
    Liveness,
}

/// Per-source freshness/provenance, so the board reports what was real vs fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFreshness {
    pub source: SourceKind,
    pub provenance: Provenance,
    pub ok: bool,
    pub fetched_at: DateTime<Utc>,
    pub note: String,
}

/// Minimal worktree facts (derived; fixture in Slice 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub path: String,
    pub branch: Option<String>,
    pub head: Option<String>,
}

/// Minimal Linear issue facts (fixture in Slice 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinearIssueLite {
    pub key: String,
    pub title: String,
    pub state: String,
    pub assignee: Option<String>,
    pub url: String,
}

/// One pulled Linear issue (Slice 4, `lane pull`). ADAPTER-NEUTRAL on purpose:
/// defined here — not in `src/linear` — so the envelope layer (`src/lock`) can carry
/// it in `VerbData::Pull` without referencing the adapter (the core source-scan law
/// in `tests/no_network_guard.rs`). The adapter deserializes GraphQL straight into it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullIssue {
    pub identifier: String,
    pub title: String,
    pub state: String,
    pub state_type: String,
    pub url: String,
    pub updated_at: String,
}

/// One board row, keyed by Linear issue where available. Every claim-sourced fact
/// is provenance-tagged: claim facts are authoritative; `age_secs` and `stale_state`
/// are derived; provider facts carry the provider's provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardRow {
    pub linear_key: Option<Provenanced<String>>,
    pub repo: Provenanced<String>,
    pub lane: Provenanced<String>,
    pub instance: Provenanced<String>,
    pub branch: Option<Provenanced<String>>,
    pub role: Option<Provenanced<Role>>,
    pub gate: Option<Provenanced<Gate>>,
    pub claim_status: Option<Provenanced<ClaimStatus>>,
    pub stale_state: Provenanced<StaleState>,
    pub liveness: Provenanced<Liveness>,
    pub worktree: Option<Provenanced<WorktreeInfo>>,
    pub linear: Option<Provenanced<LinearIssueLite>>,
    pub pr_url: Option<Provenanced<String>>,
    pub expires_at: Provenanced<DateTime<Utc>>,
    pub age_secs: Provenanced<i64>,
}

/// The assembled board. `schema_version` is 0 (unstable) for Slice 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Board {
    pub schema_version: u32,
    pub generated_at: DateTime<Utc>,
    pub rows: Vec<BoardRow>,
    pub sources: Vec<SourceFreshness>,
}
