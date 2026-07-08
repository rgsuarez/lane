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
pub mod cache;
pub mod transport;

use chrono::{DateTime, Utc};

use crate::cli::PullArgs;
use crate::config::{LaneConfig, ROLE_LINEAR_API};
use crate::lock::audit::StdAuditSink;
use crate::lock::{emit, home_env, resolve_root, CommandError, Outcome, StdFs, VerbData};
use crate::model::PullIssue;
use crate::secrets::{SecretResolver, StdOpRunner, UNSCOPED};
use transport::UreqTransport;

/// What `lane pull` caches: the fetch limit alongside the issues, so a later request
/// for MORE than the cached fetch refetches instead of silently under-serving.
#[derive(serde::Serialize, serde::Deserialize)]
struct CachedPull {
    limit: u32,
    issues: Vec<PullIssue>,
}

/// `lane pull` — the runner (verb-in-module, the `hook::run_hook` precedent).
pub fn run_pull(args: &PullArgs) -> i32 {
    run_pull_at(args, Utc::now())
}

fn run_pull_at(args: &PullArgs, now: DateTime<Utc>) -> i32 {
    let fs = StdFs;
    let result = (|| -> Result<(Outcome, Option<VerbData>, Option<String>), CommandError> {
        let home = home_env();
        let root = resolve_root(args.lane_root.clone(), home.as_deref(), &fs)?;
        let config = LaneConfig::load(root.path(), root.expected_uid(), &fs)?;
        let cache_path = cache::cache_dir(root.path()).join(cache::VIEWER_ISSUES_FILE);

        if !args.refresh {
            let hit: Option<cache::CacheEnvelope<CachedPull>> =
                cache::read_fresh(&cache_path, config.linear.cache_ttl_seconds, now);
            // Serve the cache only when it covers the requested limit — no secret is
            // resolved and no network is touched on this path (a fresh cache keeps
            // `pull` working with `op` absent entirely).
            if let Some(envelope) = hit {
                if envelope.payload.limit >= args.limit {
                    let mut issues = envelope.payload.issues;
                    issues.truncate(args.limit as usize);
                    return Ok((
                        Outcome::Ok,
                        Some(VerbData::Pull {
                            issues,
                            source: "cache",
                            fetched_at: envelope.fetched_at,
                        }),
                        None,
                    ));
                }
            }
        }

        // Live fetch: resolve the API key (audited), POST, refresh the cache.
        let runner = StdOpRunner::new();
        let sink = StdAuditSink::new(root.root_audit_path(), root.expected_uid());
        // Identity is observability-only here (pull is an identity-free read verb):
        // an exported LANE_INSTANCE enriches the audit event, absence stays "-".
        let instance = std::env::var("LANE_INSTANCE")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| UNSCOPED.to_string());
        let resolver = SecretResolver {
            config: &config,
            runner: &runner,
            sink: &sink,
            repo: UNSCOPED,
            lane: UNSCOPED,
            instance: &instance,
        };
        let (secret_result, audit_warning) = resolver.resolve(ROLE_LINEAR_API, now);
        let secret = secret_result.map_err(|error| CommandError {
            error,
            audit_warning: audit_warning.clone(),
        })?;

        let transport = UreqTransport::new();
        let issues =
            api::fetch_viewer_issues(&transport, &config.linear.api_url, &secret, args.limit)
                .map_err(|error| CommandError {
                    error,
                    audit_warning: audit_warning.clone(),
                })?;

        // Cache write is best-effort and stderr-only (D4): the envelope's
        // audit_warning slot stays reserved for audit degradation.
        if let Some(w) = cache::write(
            &cache_path,
            &CachedPull {
                limit: args.limit,
                issues: issues.clone(),
            },
            now,
        ) {
            eprintln!("lane: warning: {w}");
        }

        Ok((
            Outcome::Ok,
            Some(VerbData::Pull {
                issues,
                source: "api",
                fetched_at: now,
            }),
            audit_warning,
        ))
    })();
    emit(args.json, "pull", None, None, result)
}
