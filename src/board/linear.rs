//! Linear provider. Slice 1 is OFFLINE: fixture JSON only, or no provider at all.
//! There is no network client in this slice (see the `no_network_guard` test).

use anyhow::Context;
use chrono::{DateTime, Utc};
use std::path::Path;

use crate::model::{LinearIssueLite, Provenance, Provenanced, SourceFreshness, SourceKind};

/// Supplies Linear issue facts by key. A real GraphQL implementation lands in a later slice.
pub trait LinearProvider {
    fn issue_for(&self, key: &str) -> Option<Provenanced<LinearIssueLite>>;
    fn freshness(&self, now: DateTime<Utc>) -> SourceFreshness;
}

/// No Linear data — the default in Slice 1 (offline; no network path compiled in).
pub struct NoLinearProvider;

impl LinearProvider for NoLinearProvider {
    fn issue_for(&self, _key: &str) -> Option<Provenanced<LinearIssueLite>> {
        None
    }
    fn freshness(&self, now: DateTime<Utc>) -> SourceFreshness {
        SourceFreshness {
            source: SourceKind::Linear,
            provenance: Provenance::Unknown,
            ok: true,
            fetched_at: now,
            note: "offline: no Linear source (Slice 1)".to_string(),
        }
    }
}

/// Linear issues loaded from a fixture JSON array (offline).
pub struct FixtureLinearProvider {
    issues: Vec<LinearIssueLite>,
}

impl FixtureLinearProvider {
    pub fn new(issues: Vec<LinearIssueLite>) -> Self {
        Self { issues }
    }
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read linear fixture {}", path.display()))?;
        let issues: Vec<LinearIssueLite> = serde_json::from_str(&text)
            .with_context(|| format!("parse linear fixture {}", path.display()))?;
        Ok(Self { issues })
    }
}

impl LinearProvider for FixtureLinearProvider {
    fn issue_for(&self, key: &str) -> Option<Provenanced<LinearIssueLite>> {
        self.issues
            .iter()
            .find(|i| i.key == key)
            .cloned()
            .map(Provenanced::fixture)
    }
    fn freshness(&self, now: DateTime<Utc>) -> SourceFreshness {
        SourceFreshness {
            source: SourceKind::Linear,
            provenance: Provenance::Fixture,
            ok: true,
            fetched_at: now,
            note: format!("{} fixture issue(s)", self.issues.len()),
        }
    }
}
