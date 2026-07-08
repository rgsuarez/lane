//! Human-readable table for the board. Each value carries a provenance tag in `[..]`:
//! `A`=authoritative, `D`=derived, `F`=fixture, `U`=unknown.

use std::fmt::Write;

use crate::model::{Board, Gate, Liveness, Provenance, SourceKind, StaleState};

/// Render the board as a human-readable table + a per-source provenance footer.
pub fn render(board: &Board) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "LANE BOARD  generated={}  schema=v{}",
        board.generated_at.to_rfc3339(),
        board.schema_version
    );

    if board.rows.is_empty() {
        let _ = writeln!(out, "(no lanes)");
    } else {
        let _ = writeln!(
            out,
            "{:<13} {:<18} {:<13} {:<18} {:<12} {:<30} {:<12} {:<16} TITLE",
            "KEY", "LANE", "REPO", "STALE", "LIVE", "BRANCH", "GATE", "STATE"
        );
        for r in &board.rows {
            let key = match &r.linear_key {
                Some(p) => format!("{}[{}]", p.value, prov(p.provenance)),
                None => "-".to_string(),
            };
            let lane = format!("{}[{}]", r.lane.value, prov(r.lane.provenance));
            let repo = format!("{}[{}]", r.repo.value, prov(r.repo.provenance));
            let stale = format!(
                "{}[{}]",
                stale_str(r.stale_state.value),
                prov(r.stale_state.provenance)
            );
            let live = format!(
                "{}[{}]",
                live_str(r.liveness.value),
                prov(r.liveness.provenance)
            );
            let branch = match &r.branch {
                Some(p) => format!("{}[{}]", p.value, prov(p.provenance)),
                None => "-".to_string(),
            };
            let gate = match &r.gate {
                Some(g) => format!("{}[{}]", gate_str(g.value), prov(g.provenance)),
                None => "-".to_string(),
            };
            // The Linear join (fixture or live): one provenance tag for the issue,
            // shown on STATE; TITLE is last and deliberately untruncated.
            let (issue_state, issue_title) = match &r.linear {
                Some(p) => (
                    format!("{}[{}]", p.value.state, prov(p.provenance)),
                    p.value.title.clone(),
                ),
                None => ("-".to_string(), "-".to_string()),
            };
            let _ = writeln!(
                out,
                "{:<13} {:<18} {:<13} {:<18} {:<12} {:<30} {:<12} {:<16} {}",
                key, lane, repo, stale, live, branch, gate, issue_state, issue_title
            );
        }
    }

    let _ = writeln!(out, "\nsources:");
    for s in &board.sources {
        let _ = writeln!(
            out,
            "  {:<9} provenance={:<14} ok={:<5} note={}",
            src_str(s.source),
            prov_full(s.provenance),
            s.ok,
            s.note
        );
    }
    out
}

fn prov(p: Provenance) -> &'static str {
    match p {
        Provenance::Authoritative => "A",
        Provenance::Derived => "D",
        Provenance::Fixture => "F",
        Provenance::Live => "L",
        Provenance::Unknown => "U",
    }
}

fn prov_full(p: Provenance) -> &'static str {
    match p {
        Provenance::Authoritative => "authoritative",
        Provenance::Derived => "derived",
        Provenance::Fixture => "fixture",
        Provenance::Live => "live",
        Provenance::Unknown => "unknown",
    }
}

fn stale_str(s: StaleState) -> &'static str {
    match s {
        StaleState::Active => "active",
        StaleState::Expired => "expired",
        StaleState::PossiblyStale => "possibly-stale",
        StaleState::Orphaned => "orphaned",
    }
}

fn live_str(l: Liveness) -> &'static str {
    match l {
        Liveness::Live => "live",
        Liveness::NotLive => "not-live",
        Liveness::Unknown => "unknown",
    }
}

fn gate_str(g: Gate) -> &'static str {
    match g {
        Gate::Plan => "plan",
        Gate::Execute => "execute",
        Gate::Review => "review",
        Gate::Smoke => "smoke",
        Gate::Migration => "migration",
        Gate::Merge => "merge",
        Gate::Closeout => "closeout",
    }
}

fn src_str(k: SourceKind) -> &'static str {
    match k {
        SourceKind::Claims => "claims",
        SourceKind::Worktrees => "worktrees",
        SourceKind::Linear => "linear",
        SourceKind::Liveness => "liveness",
    }
}
