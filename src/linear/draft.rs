//! Closeout draft composer — WHITELIST-BY-CONSTRUCTION redaction (spec §12).
//!
//! The composer may read ONLY these claim fields: `linear_key`, `lane`, `repo`,
//! `branch`, `pr_url`, `gate`, `claimed_at` — plus the close timestamp. Deliberately
//! excluded: `instance` (session identity never reaches Linear), `note` (free text
//! lane cannot vouch for), `target`/`target_normalized` (local machine paths),
//! everything else. The [`scrub_violation`] check is defense-in-depth behind that
//! construction, not the primary guarantee.

use chrono::{DateTime, Utc};

use crate::model::ClaimRecord;
use crate::secrets::SecretValue;

/// The deterministic, non-secret idempotency marker for one claim GENERATION
/// (`lane` + `claimed_at`): reruns of the same close dedupe against it; a later
/// re-claim of the same lane has a fresh `claimed_at` and posts fresh.
pub fn closeout_marker(lane: &str, claimed_at: DateTime<Utc>) -> String {
    format!("lane-closeout: {lane}@{}", claimed_at.to_rfc3339())
}

/// Compose the redacted closeout comment (Markdown), ending with the marker footer.
pub fn compose_closeout(rec: &ClaimRecord, now: DateTime<Utc>) -> String {
    let mut body = format!("**lane closeout** — `{}/{}`", rec.repo, rec.lane);
    if let Some(key) = &rec.linear_key {
        body.push_str(&format!(" ({key})"));
    }
    body.push_str("\n\n");
    if let Some(branch) = &rec.branch {
        body.push_str(&format!("- branch: `{branch}`\n"));
    }
    if let Some(pr) = &rec.pr_url {
        body.push_str(&format!("- PR: {pr}\n"));
    }
    if let Some(gate) = rec.gate {
        body.push_str(&format!("- gate: {}\n", format!("{gate:?}").to_lowercase()));
    }
    body.push_str(&format!(
        "- held: {} → {}\n",
        rec.claimed_at.to_rfc3339(),
        now.to_rfc3339()
    ));
    body.push_str(&format!(
        "\n{}\n",
        closeout_marker(&rec.lane, rec.claimed_at)
    ));
    body
}

/// Defense-in-depth scrub: a draft containing an `op://` reference or the resolved
/// secret value is an invariant breach (unreachable by construction) — the caller
/// aborts and posts nothing.
pub fn scrub_violation(draft: &str, secret: Option<&SecretValue>) -> Option<&'static str> {
    if draft.contains("op://") {
        return Some("closeout draft contains an op:// reference");
    }
    if let Some(s) = secret {
        if draft.contains(s.expose()) {
            return Some("closeout draft contains the resolved secret value");
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ClaimStatus, Gate, Role};

    fn record() -> ClaimRecord {
        let now = Utc::now();
        ClaimRecord {
            schema_version: Some(1),
            lane: "zer-85".into(),
            repo: "lane".into(),
            instance: "SECRET-CALLSIGN".into(),
            pid: Some(42),
            target: Some("/Users/someone/projects-local/lane-zer-85".into()),
            target_normalized: Some("/users/someone/projects-local/lane-zer-85".into()),
            note: Some("private operator note".into()),
            claimed_at: now,
            updated_at: now,
            expires_at: now,
            ttl_hours: 12.0,
            linear_key: Some("ZER-85".into()),
            branch: Some("slice-4-linear-1password".into()),
            role: Some(Role::Executor),
            pr_url: Some("https://github.com/x/y/pull/9".into()),
            gate: Some(Gate::Closeout),
            plan_path: Some("/Users/someone/.claude/plans/p.md".into()),
            claim_status: Some(ClaimStatus::Active),
            session_ref: Some("session-ref-9".into()),
        }
    }

    #[test]
    fn whitelist_fields_only() {
        let rec = record();
        let body = compose_closeout(&rec, Utc::now());
        // Whitelisted facts present…
        assert!(body.contains("`lane/zer-85`"));
        assert!(body.contains("(ZER-85)"));
        assert!(body.contains("`slice-4-linear-1password`"));
        assert!(body.contains("https://github.com/x/y/pull/9"));
        assert!(body.contains("gate: closeout"));
        assert!(body.contains("lane-closeout: zer-85@"));
        // …and NOTHING else: no instance, no note, no paths, no session ref.
        assert!(!body.contains("SECRET-CALLSIGN"));
        assert!(!body.contains("private operator note"));
        assert!(!body.contains("projects-local"));
        assert!(!body.contains("plans/p.md"));
        assert!(!body.contains("session-ref-9"));
    }

    #[test]
    fn marker_is_generation_deterministic() {
        let rec = record();
        let m1 = closeout_marker(&rec.lane, rec.claimed_at);
        let m2 = closeout_marker(&rec.lane, rec.claimed_at);
        assert_eq!(m1, m2);
        let other = closeout_marker(&rec.lane, rec.claimed_at + chrono::Duration::seconds(1));
        assert_ne!(m1, other, "a new generation must carry a new marker");
        let body = compose_closeout(&rec, Utc::now());
        assert!(body.trim_end().ends_with(&m1), "marker is the footer");
    }

    #[test]
    fn scrub_catches_references_and_values() {
        assert!(scrub_violation("clean body", None).is_none());
        assert!(scrub_violation("op://Vault/Item/field", None).is_some());
        let secret = SecretValue::new("hunter2");
        assert!(scrub_violation("… hunter2 …", Some(&secret)).is_some());
        assert!(scrub_violation("clean", Some(&secret)).is_none());
    }
}
