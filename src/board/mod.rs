//! Board assembly: read authoritative claims, join provider data, classify staleness.

pub mod claims;
pub mod linear;
pub mod liveness;
pub mod worktrees;

use chrono::{DateTime, Utc};
use std::path::Path;

use crate::cli::{self, BoardArgs};
use crate::model::{
    Board, BoardRow, ClaimRecord, Liveness, Provenance, Provenanced, SourceFreshness, SourceKind,
    StaleState,
};
use crate::output::{self, OutputFormat};

use self::linear::LinearProvider;
use self::liveness::LivenessProvider;
use self::worktrees::WorktreeProvider;

/// A claim is "possibly stale" once it has been idle (no `updated_at` bump) this long.
pub const POSSIBLY_STALE_AFTER_SECS: i64 = 3 * 3600;

/// Pure classification of a claim's staleness given the current time and observed liveness.
pub fn classify_stale(claim: &ClaimRecord, now: DateTime<Utc>, liveness: Liveness) -> StaleState {
    if now >= claim.expires_at {
        StaleState::Expired
    } else if liveness == Liveness::NotLive {
        StaleState::Orphaned
    } else if (now - claim.updated_at).num_seconds() > POSSIBLY_STALE_AFTER_SECS {
        StaleState::PossiblyStale
    } else {
        StaleState::Active
    }
}

/// Inputs to [`assemble`]. Providers are injected so Slice 1 can use fixtures/stubs
/// and later slices can swap real implementations without changing this function.
pub struct BoardInputs<'a> {
    pub lane_root: &'a Path,
    pub repo_filter: Option<&'a str>,
    pub now: DateTime<Utc>,
    pub worktrees: &'a dyn WorktreeProvider,
    pub linear: &'a dyn LinearProvider,
    pub liveness: &'a dyn LivenessProvider,
}

/// Assemble the board from authoritative claims + provider joins. Read-only.
pub fn assemble(inputs: &BoardInputs) -> anyhow::Result<Board> {
    let claims = claims::read_claims(inputs.lane_root, inputs.repo_filter)?;

    let mut sources = vec![SourceFreshness {
        source: SourceKind::Claims,
        provenance: Provenance::Authoritative,
        ok: true,
        fetched_at: inputs.now,
        note: format!(
            "{} claim(s) under {}",
            claims.len(),
            inputs.lane_root.display()
        ),
    }];
    sources.push(inputs.worktrees.freshness(inputs.now));
    sources.push(inputs.linear.freshness(inputs.now));
    sources.push(inputs.liveness.freshness(inputs.now));

    let mut rows: Vec<BoardRow> = claims
        .iter()
        .map(|claim| {
            let liveness = inputs.liveness.liveness_for(claim);
            let stale = classify_stale(claim, inputs.now, liveness.value);
            let worktree = inputs.worktrees.for_claim(claim);
            let linear = claim
                .linear_key
                .as_deref()
                .and_then(|key| inputs.linear.issue_for(key));
            BoardRow {
                linear_key: claim.linear_key.clone().map(Provenanced::authoritative),
                repo: Provenanced::authoritative(claim.repo.clone()),
                lane: Provenanced::authoritative(claim.lane.clone()),
                instance: Provenanced::authoritative(claim.instance.clone()),
                branch: claim.branch.clone().map(Provenanced::authoritative),
                role: claim.role.map(Provenanced::authoritative),
                gate: claim.gate.map(Provenanced::authoritative),
                claim_status: claim.claim_status.map(Provenanced::authoritative),
                stale_state: Provenanced::derived(stale),
                liveness,
                worktree,
                linear,
                pr_url: claim.pr_url.clone().map(Provenanced::authoritative),
                expires_at: Provenanced::authoritative(claim.expires_at),
                age_secs: Provenanced::derived(
                    (inputs.now - claim.claimed_at).num_seconds().max(0),
                ),
            }
        })
        .collect();

    // Deterministic order: rows WITH a Linear key first (sorted by key), missing
    // keys last; ties broken by lane.
    rows.sort_by(|a, b| {
        let ka = a.linear_key.as_ref().map(|p| &p.value);
        let kb = b.linear_key.as_ref().map(|p| &p.value);
        (ka.is_none(), ka, &a.lane.value).cmp(&(kb.is_none(), kb, &b.lane.value))
    });

    Ok(Board {
        schema_version: 0,
        generated_at: inputs.now,
        rows,
        sources,
    })
}

/// Wire Slice-1 providers from CLI args, assemble the board, and print it. Read-only.
pub fn run_board(args: &BoardArgs) -> anyhow::Result<()> {
    let lane_root = cli::resolve_lane_root(args.lane_root.clone())?;
    let now = Utc::now();

    let worktrees: Box<dyn WorktreeProvider> = match &args.worktree_fixture {
        Some(path) => Box::new(worktrees::FixtureWorktreeProvider::from_file(path)?),
        None => Box::new(worktrees::EmptyWorktreeProvider),
    };
    let linear: Box<dyn LinearProvider> = match &args.linear_fixture {
        Some(path) => Box::new(linear::FixtureLinearProvider::from_file(path)?),
        None => Box::new(linear::NoLinearProvider),
    };
    let liveness = liveness::StubLivenessProvider;

    let inputs = BoardInputs {
        lane_root: &lane_root,
        repo_filter: args.repo.as_deref(),
        now,
        worktrees: worktrees.as_ref(),
        linear: linear.as_ref(),
        liveness: &liveness,
    };
    let board = assemble(&inputs)?;

    let fmt = if args.json {
        OutputFormat::Json
    } else {
        OutputFormat::Human
    };
    print!("{}", output::render(&board, fmt)?);
    Ok(())
}
