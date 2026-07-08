//! Linear GraphQL adapter (Slice 4, spec §12) — an ADAPTER, outside the locking core.
//!
//! Law of this module:
//! - **Reads are free** (issues/states/assignees for `lane pull` and opt-in board
//!   enrichment); **writes are drafted + operator-gated** (the `lane close`
//!   closeout comment) — never automatic.
//! - Sync-only HTTP (`ureq`, the crate's one allowlisted network dependency); no
//!   async runtime exists anywhere in the tree.
//! - The API key is resolved through [`crate::secrets`] at call time and rides ONLY
//!   the `Authorization` header — never disk, logs, envelopes, or errors.
//! - Linear stays the source of truth: the read cache under `$LANE_ROOT/cache/linear`
//!   is TTL'd, disposable, derived state that no trust decision ever consults.
//! - The locking core never imports this module (enforced by the source scan in
//!   `tests/no_network_guard.rs`); every local verb works with this module unused.

pub mod api;
pub mod transport;
