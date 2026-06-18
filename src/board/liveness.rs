//! Liveness provider. Slice 1 uses a stub returning `Unknown` — NO tmux, NO overseer.
//! Tests inject their own provider by implementing [`LivenessProvider`].

use chrono::{DateTime, Utc};

use crate::model::{ClaimRecord, Liveness, Provenance, Provenanced, SourceFreshness, SourceKind};

/// Reports whether the session behind a claim is live. Real (overseer/tmux) implementations
/// land in a later slice.
pub trait LivenessProvider {
    fn liveness_for(&self, claim: &ClaimRecord) -> Provenanced<Liveness>;
    fn freshness(&self, now: DateTime<Utc>) -> SourceFreshness;
}

/// Always-`Unknown` liveness (the default in Slice 1; no session detection).
pub struct StubLivenessProvider;

impl LivenessProvider for StubLivenessProvider {
    fn liveness_for(&self, _claim: &ClaimRecord) -> Provenanced<Liveness> {
        Provenanced::unknown(Liveness::Unknown)
    }
    fn freshness(&self, now: DateTime<Utc>) -> SourceFreshness {
        SourceFreshness {
            source: SourceKind::Liveness,
            provenance: Provenance::Unknown,
            ok: true,
            fetched_at: now,
            note: "stub liveness (no overseer/tmux in Slice 1)".to_string(),
        }
    }
}
