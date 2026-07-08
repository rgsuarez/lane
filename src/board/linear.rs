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
/// - **Cache-backed**: per-key entries under `$LANE_ROOT/.cache/linear/` with
///   per-entry TTL; the merged map is written back once (in `freshness`, which the
///   board assembles after all joins). A miss (no such issue) is memoized, never
///   degraded.
pub struct ApiLinearProvider {
    root: PathBuf,
    expected_uid: u32,
    ttl_seconds: Cell<u64>,
    // Config load (no secret, no network) — memoized; gates cache reads.
    config_attempted: Cell<bool>,
    config_ok: Cell<bool>,
    // Secret + transport — resolved LAZILY only on the first cache MISS that needs a
    // live fetch, so a fully-fresh cache serves with zero secret and zero `op` spawn.
    secret_attempted: Cell<bool>,
    ctx: RefCell<Option<LiveCtx>>,
    by_key: RefCell<BTreeMap<String, cache::CacheEnvelope<LinearIssueLite>>>,
    memo: RefCell<BTreeMap<String, Option<LinearIssueLite>>>,
    notes: RefCell<Vec<String>>,
    fetched: Cell<u32>,
    cache_hits: Cell<u32>,
    dirty_cache: Cell<bool>,
    // Freshness health. Starts true; set false by a config/secret failure or ANY
    // per-key fetch error. Never blocks the board or the cache — a soft failure on
    // one key leaves every other key's fresh cache and successful fetch intact.
    ok: Cell<bool>,
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
            config_attempted: Cell::new(false),
            config_ok: Cell::new(false),
            secret_attempted: Cell::new(false),
            ctx: RefCell::new(None),
            by_key: RefCell::new(BTreeMap::new()),
            memo: RefCell::new(BTreeMap::new()),
            notes: RefCell::new(Vec::new()),
            fetched: Cell::new(0),
            cache_hits: Cell::new(0),
            dirty_cache: Cell::new(false),
            ok: Cell::new(true),
        }
    }

    fn soft_fail(&self, note: String) {
        self.ok.set(false);
        self.notes.borrow_mut().push(note);
    }

    fn by_key_path(&self) -> PathBuf {
        cache::cache_dir(&self.root).join(cache::ISSUES_BY_KEY_FILE)
    }

    /// Load config + the by-key cache once (NO secret, NO network, NO `op` spawn).
    /// Memoized. Returns true iff config is usable (so cache reads can be trusted).
    fn ensure_config(&self) -> bool {
        if self.config_attempted.get() {
            return self.config_ok.get();
        }
        self.config_attempted.set(true);
        let config = match LaneConfig::load(&self.root, self.expected_uid, &StdFs) {
            Ok(c) => c,
            Err(e) => {
                self.soft_fail(e.to_string());
                return false;
            }
        };
        self.ttl_seconds.set(config.linear.cache_ttl_seconds);
        if let Ok(text) = std::fs::read_to_string(self.by_key_path()) {
            if let Ok(map) = serde_json::from_str(&text) {
                *self.by_key.borrow_mut() = map;
            }
        }
        self.config_ok.set(true);
        true
    }

    /// Resolve the secret + build the transport once — called LAZILY, only when a cache
    /// miss needs a live fetch. A failure is soft (the board still renders every fresh
    /// cache hit); it just means the missing keys can't be fetched this run. Memoized:
    /// resolved at most once, so a warm cache never spawns `op`.
    fn ensure_secret_ctx(&self, now: DateTime<Utc>) -> bool {
        if self.secret_attempted.get() {
            return self.ctx.borrow().is_some();
        }
        self.secret_attempted.set(true);
        let config = match LaneConfig::load(&self.root, self.expected_uid, &StdFs) {
            Ok(c) => c,
            Err(e) => {
                self.soft_fail(e.to_string());
                return false;
            }
        };
        let runner = StdOpRunner::new();
        let sink = StdAuditSink::new(
            self.root.join(crate::lock::paths::ROOT_AUDIT_FILE),
            self.expected_uid,
        );
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
                self.soft_fail(e.to_string());
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
        // `try_seconds` avoids the panic `Duration::seconds` throws above the
        // millisecond bound (a huge configured TTL) — None ⇒ effectively unbounded.
        let ttl = chrono::Duration::try_seconds(self.ttl_seconds.get().min(i64::MAX as u64) as i64);
        if age < chrono::Duration::zero() || ttl.is_some_and(|ttl| age > ttl) {
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
        // Config first (TTL + the by-key cache) — cheap, no secret, no network. Then
        // try the cache: a fresh hit serves with ZERO secret resolution and ZERO
        // network, so warm-cache board renders never spawn `op` (mirrors `lane pull`).
        if !self.ensure_config() {
            return None;
        }
        if let Some(issue) = self.cache_lookup(key, now) {
            self.cache_hits.set(self.cache_hits.get() + 1);
            self.memo
                .borrow_mut()
                .insert(key.to_string(), Some(issue.clone()));
            return Some(Provenanced::new(issue, Provenance::Live));
        }
        // Cache miss ⇒ a live fetch is needed; resolve the secret now (lazily, once).
        // Failure is soft: this key can't be enriched, but other keys' fresh cache and
        // successful fetches are untouched.
        if !self.ensure_secret_ctx(now) {
            return None;
        }
        let outcome = {
            let ctx = self.ctx.borrow();
            let ctx = ctx.as_ref().expect("ensure_secret_ctx returned true");
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
                // Soft, PER-KEY: a stale/deleted linear_key (Linear reports it via
                // errors[] → Err, not data.issue=null) marks the source not-ok and
                // memoizes a miss for THIS key — it never blanks other keys' fresh
                // cache or successful fetches.
                self.soft_fail(e.to_string());
                self.memo.borrow_mut().insert(key.to_string(), None);
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
            ok: self.ok.get(),
            fetched_at: now,
            note,
        }
    }
}
