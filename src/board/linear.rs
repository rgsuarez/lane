//! Linear providers. The DEFAULT stays offline (`NoLinearProvider`); fixtures serve
//! tests; `ApiLinearProvider` (Slice 4) is the OPT-IN live source behind
//! `board --linear api` — fail-soft by construction: any failure degrades the
//! source's freshness note and the board still renders.

use anyhow::Context;
use chrono::{DateTime, Utc};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::{LaneConfig, ROLE_LINEAR_API};
use crate::linear::transport::UreqTransport;
use crate::linear::{api, cache};
use crate::lock::audit::StdAuditSink;
use crate::lock::paths::LaneRoot;
use crate::lock::StdFs;
use crate::model::{LinearIssueLite, Provenance, Provenanced, SourceFreshness, SourceKind};
use crate::secrets::{SecretResolver, SecretValue, StdOpRunner, UNSCOPED};

/// Supplies Linear issue facts by key.
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

/// The live Linear source for `board --linear api` (Slice 4). Mirrors
/// `GitWorktreeProvider`'s fail-soft interior-mutability pattern:
/// - **Lazy**: zero config/secret/network work until the first `issue_for` call —
///   and `assemble` only calls `issue_for` for claims that carry a `linear_key`, so
///   a key-less board under `--linear api` stays byte-identical to `--linear off`.
/// - **Fail-soft**: config/secret/transport failures set `degraded`, land in the
///   freshness note (messages are non-secret by construction), and rows still render.
///   A transport failure short-circuits further network attempts this run.
/// - **Cache-backed**: per-key entries under `$LANE_ROOT/cache/linear/` with
///   per-entry TTL; the merged map is written back once (in `freshness`, which the
///   board assembles after all joins). A miss (no such issue) is memoized, never
///   degraded.
pub struct ApiLinearProvider {
    root: PathBuf,
    expected_uid: u32,
    ttl_seconds: Cell<u64>,
    ctx: RefCell<Option<LiveCtx>>,
    init_attempted: Cell<bool>,
    by_key: RefCell<BTreeMap<String, cache::CacheEnvelope<LinearIssueLite>>>,
    memo: RefCell<BTreeMap<String, Option<LinearIssueLite>>>,
    degraded: Cell<bool>,
    notes: RefCell<Vec<String>>,
    fetched: Cell<u32>,
    cache_hits: Cell<u32>,
    dirty_cache: Cell<bool>,
}

struct LiveCtx {
    api_url: String,
    secret: SecretValue,
    transport: UreqTransport,
}

impl ApiLinearProvider {
    pub fn new(root: &LaneRoot) -> Self {
        Self {
            root: root.path().to_path_buf(),
            expected_uid: root.expected_uid(),
            ttl_seconds: Cell::new(crate::config::DEFAULT_CACHE_TTL_SECONDS),
            ctx: RefCell::new(None),
            init_attempted: Cell::new(false),
            by_key: RefCell::new(BTreeMap::new()),
            memo: RefCell::new(BTreeMap::new()),
            degraded: Cell::new(false),
            notes: RefCell::new(Vec::new()),
            fetched: Cell::new(0),
            cache_hits: Cell::new(0),
            dirty_cache: Cell::new(false),
        }
    }

    fn degrade(&self, note: String) {
        self.degraded.set(true);
        self.notes.borrow_mut().push(note);
    }

    fn by_key_path(&self) -> PathBuf {
        cache::cache_dir(&self.root).join(cache::ISSUES_BY_KEY_FILE)
    }

    /// One-shot lazy init: config, by-key cache load, secret resolution (audited to
    /// the root adapter audit). Returns true iff the live context is ready.
    fn ensure_ctx(&self, now: DateTime<Utc>) -> bool {
        if self.degraded.get() {
            return false;
        }
        if self.ctx.borrow().is_some() {
            return true;
        }
        if self.init_attempted.get() {
            return false;
        }
        self.init_attempted.set(true);
        let config = match LaneConfig::load(&self.root, self.expected_uid, &StdFs) {
            Ok(c) => c,
            Err(e) => {
                self.degrade(e.to_string());
                return false;
            }
        };
        self.ttl_seconds.set(config.linear.cache_ttl_seconds);
        if let Ok(text) = std::fs::read_to_string(self.by_key_path()) {
            if let Ok(map) = serde_json::from_str(&text) {
                *self.by_key.borrow_mut() = map;
            }
        }
        let runner = StdOpRunner::new();
        // `self.root` came from a resolved LaneRoot, so this join equals
        // `LaneRoot::root_audit_path` (the root-level adapter audit).
        let sink = StdAuditSink::new(self.root.join("audit.log"), self.expected_uid);
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
        let (result, warn) = resolver.resolve(ROLE_LINEAR_API, now);
        if let Some(w) = warn {
            self.notes.borrow_mut().push(w);
        }
        let secret = match result {
            Ok(s) => s,
            Err(e) => {
                self.degrade(e.to_string());
                return false;
            }
        };
        *self.ctx.borrow_mut() = Some(LiveCtx {
            api_url: config.linear.api_url,
            secret,
            transport: UreqTransport::new(),
        });
        true
    }

    fn cache_lookup(&self, key: &str, now: DateTime<Utc>) -> Option<LinearIssueLite> {
        let map = self.by_key.borrow();
        let envelope = map.get(key)?;
        let age = now.signed_duration_since(envelope.fetched_at);
        let ttl = chrono::Duration::seconds(self.ttl_seconds.get().min(i64::MAX as u64) as i64);
        if age < chrono::Duration::zero() || age > ttl {
            return None;
        }
        Some(envelope.payload.clone())
    }
}

impl LinearProvider for ApiLinearProvider {
    fn issue_for(&self, key: &str) -> Option<Provenanced<LinearIssueLite>> {
        if let Some(memoized) = self.memo.borrow().get(key) {
            return memoized
                .clone()
                .map(|i| Provenanced::new(i, Provenance::Live));
        }
        let now = Utc::now();
        // The TTL needs config; load it (and everything else) before the cache read.
        // A degraded init still allows nothing — cache freshness without config TTL
        // would be a guess.
        if !self.ensure_ctx(now) {
            return None;
        }
        if let Some(issue) = self.cache_lookup(key, now) {
            self.cache_hits.set(self.cache_hits.get() + 1);
            self.memo
                .borrow_mut()
                .insert(key.to_string(), Some(issue.clone()));
            return Some(Provenanced::new(issue, Provenance::Live));
        }
        let outcome = {
            let ctx = self.ctx.borrow();
            let ctx = ctx.as_ref().expect("ensure_ctx returned true");
            api::fetch_issue_by_key(&ctx.transport, &ctx.api_url, &ctx.secret, key)
        };
        match outcome {
            Ok(Some(found)) => {
                self.fetched.set(self.fetched.get() + 1);
                let issue = LinearIssueLite {
                    key: found.issue.identifier,
                    title: found.issue.title,
                    state: found.issue.state,
                    assignee: found.assignee,
                    url: found.issue.url,
                };
                self.by_key.borrow_mut().insert(
                    key.to_string(),
                    cache::CacheEnvelope {
                        fetched_at: now,
                        payload: issue.clone(),
                    },
                );
                self.dirty_cache.set(true);
                self.memo
                    .borrow_mut()
                    .insert(key.to_string(), Some(issue.clone()));
                Some(Provenanced::new(issue, Provenance::Live))
            }
            Ok(None) => {
                self.memo.borrow_mut().insert(key.to_string(), None);
                None
            }
            Err(e) => {
                self.degrade(e.to_string());
                None
            }
        }
    }

    fn freshness(&self, now: DateTime<Utc>) -> SourceFreshness {
        // `assemble` builds the freshness vector after all joins — the natural
        // write-back point for the merged by-key cache.
        if self.dirty_cache.get() {
            if let Some(w) = cache::write_raw(&self.by_key_path(), &*self.by_key.borrow()) {
                self.notes.borrow_mut().push(w);
            }
            self.dirty_cache.set(false);
        }
        let mut note = format!(
            "{} fetched, {} cached",
            self.fetched.get(),
            self.cache_hits.get()
        );
        let extra = self.notes.borrow();
        if !extra.is_empty() {
            note.push_str("; ");
            note.push_str(&extra.join("; "));
        }
        SourceFreshness {
            source: SourceKind::Linear,
            provenance: Provenance::Live,
            ok: !self.degraded.get(),
            fetched_at: now,
            note,
        }
    }
}
