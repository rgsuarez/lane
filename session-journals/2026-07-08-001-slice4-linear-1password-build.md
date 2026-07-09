# 2026-07-08 — Slice 4: Linear read adapter + 1Password + gated writes — build journal

**Linear:** ZER-85 (Zero Echelon, P3) · **branch:** `slice-4-linear-1password` off `main`
@ `2c060be` · **plan:** `/Users/richie/.claude/plans/streamed-inventing-moore.md`
(Plan-Mode authored; Codex advisor GO round 4, 2026-07-08 — 7 redlines adopted).

## Why this slice exists

Through 3.5 lane is provably offline but blind: no view of its planning SoT (Linear)
and no safe secret path. Slice 4 connects both — read-only Linear GraphQL + `op`-resolved
secrets + the first operator-gated write — without the locking core gaining a network
path, and with the network guard deliberately amended rather than weakened.

## What was built

1. **Errors** (`src/error.rs`): additive `no_linear_key` (exit 1, refusal),
   `secret_unavailable` + `network` (exit 2). Single-mapping law intact.
2. **`src/proc.rs`**: the git adapter's bounded drain/poll/kill loop extracted
   (byte-oriented; git decodes lossily, secrets strictly — a lossy secret is silent
   corruption). Git delegates; existing bounded-wait tests keep covering it.
3. **`src/config.rs`**: first reader of `$LANE_ROOT/config.toml` (new `toml` dep,
   justified). Read through the OBJECT-GUARDED reader — `[linear] api_url` controls
   where a credential is sent, so a symlinked config fails closed.
4. **`src/secrets/`**: `OpRunner` seam (mirrors `GitRunner`); `StdOpRunner` (60s bound —
   Touch ID); scheme dispatch `op://` / `env:`; pre-spawn reference validation; `op`
   stderr classified then dropped; `SecretValue` (no Display/Serialize/Clone, redacted
   Debug). Every resolution appends `secret_requested` (with the new `secret_role`
   field — NOT `role`, which the claim schema owns) to the new ROOT-level adapter audit
   (`LaneRoot::root_audit_path`), structurally invisible to core recovery.
5. **`src/linear/`**: `transport` (sync `ureq` — the ONE allowlisted network dep;
   rustls/ring, zero async runtime by `cargo tree`; raw `Authorization` header, no
   `Bearer`; https-or-loopback URL policy; statuses classified body-free; 10s bound),
   `api` (variables-only GraphQL: viewer issues / issue-by-key / preflight uuid+marker /
   commentCreate), `cache` (TTL'd disposable derived state; silent refetch; stderr-only
   write warnings), `draft` (whitelist-by-construction closeout composer + deterministic
   `lane-closeout: <lane>@<claimed_at>` marker + defense-in-depth scrub), `publish`
   (adapter-owned per-lane advisory lock reusing `LaneMutex` semantics — never the core
   lane mutex).
6. **Verbs:** `lane pull` (identity-free read; standard schema-1 envelope via
   `VerbData::Pull` carrying the adapter-neutral `model::PullIssue` DTO; fresh cache
   serves with zero secret + zero network); `lane board --linear api` (opt-in
   `LinearSource` mirroring `WorktreeSource`; fail-soft `ApiLinearProvider`; new
   additive `Provenance::Live`; human table gains STATE/TITLE columns); `lane close
   --draft-closeout` (pure preview) and `--post-closeout` (the explicit operator go).
7. **Gated-write hardening (Codex redlines):** publish lock acquired BEFORE secret
   resolution and all mutation (`mutex_busy` loser provably touches nothing);
   generation re-checks at lock entry, before worktree removal (ALL close modes), and
   immediately before the create; single in-lock preflight = the dedupe authority
   (timeout-after-create reruns dedupe instead of duplicating); release generation-
   guarded via additive `ReleaseParams.expected_claimed_at` (core edit that strictly
   strengthens owner-only release; the plain verb passes `None`, unchanged).
8. **Guard reshape** (`tests/no_network_guard.rs`): FORBIDDEN + justified
   `ADAPTER_ONLY` allowlist (ureq, toml — disjointness + presence asserted) + the new
   `core_sources_import_no_adapter_or_network_code` source scan over `src/lock/**` +
   `src/hook.rs`.
9. **Docs:** spec §5/§7/§8/§12/§15 as-built deltas; AGENTS.md cold-boot header, adapter
   paragraph, reason lists, generation-guard law, secrets as-built.

## Verification

- Gates: `cargo fmt --check` / `build` / `test` / `test --all-features` /
  `clippy --all-targets -- -D warnings` — all green, exit codes checked. **295 tests /
  32 suites** (baseline 227/27; +68/+5). `cargo tree`: no tokio/async-std/smol/aws-lc;
  rustls+ring only. `tests/lock_concurrency.rs` untouched (ZER-83 quarantine).
- New suites: `secrets_op` (real spawned fake `op` incl. bounded-kill; the
  grandchild-pipe caveat is documented and avoided with `exec`), `linear_transport`
  (REAL UreqTransport against a std TcpListener; raw-auth/no-Bearer proven on the
  wire), `linear_pull` (8: envelope, cache TTL/corrupt/refresh/limit semantics,
  cached-pull-with-op-absent, fail-closed roles, human lines), `board_linear_api`
  (4: inert default, live enrich + cross-run cache, soft degrade, key-less inertia),
  `close_gated_write` (9: draft purity byte-checked, full post flow, timeout-after-
  create dedupe rerun, dirty-refusal-before-any-publish, secret-unavailable-untouched,
  absent-lane, plain-close-without-config/op, flag conflict), plus in-module race
  tests (generation swaps before/during post; concurrent double-post ≤1 create) and
  publish-lock object-guard units (symlinked dir/file fail closed).
- Secret hygiene: sentinel sweeps prove the key appears in NOTHING but the
  Authorization header (not stdout/stderr/envelope/audit/cache/lock).
- Dogfood: built under live claim `lane/zer-85` (instance LG-E1) taken via the real
  binary's `lane start`; renewed at milestones; commit guard passed silently throughout.

## Decisions of record

- **ureq 3 (rustls), minimal features** — the one network-capable dep; embed-first over
  reqwest (embeds tokio even blocking), curl FFI, curl-binary shell-out (argv leaks).
- **Reference-scheme provider dispatch** (`op://` / `env:`) instead of a provider key —
  less config surface; spec-§7 fallback sanctioned.
- **Root-level adapter audit** — one place to audit all secret/external interactions;
  core recovery structurally cannot read it, so adapter events can never fail-close a
  core mutation. `secret_role` field name avoids colliding with the claim `role`.
- **Draft = pure preview; post = explicit flag** — no interactive TTY prompt in an
  agent-invoked CLI; the marker + publish lock + generation guard carry idempotency
  and serialization instead.
- **Generation guard widened to ALL close modes** — a mode-forked guard would be
  incoherent; plain `lane release` keeps `None`.

## Residuals (documented honestly)

- POST-failure error envelopes don't re-show the draft (re-run `--draft-closeout`).
- Marker scan reads the first comment page (100) — a closeout buried deeper on a
  pathologically chatty issue would re-post; the marker makes the duplicate
  self-identifying.
- A reclaim between the final pre-post generation check and Linear committing the
  create is irreducible without cross-host locking (accepted; local truth is kept by
  the generation-guarded release).
- Label/custom-field writes: adapter seam ready, deliberately not wired this slice.

## Follow-ups (not this slice)

- Slice 0b (doctrine edits) and Slice 5 (migration tooling + installer).
- ZER-83 (lock-concurrency release-profile flake) untouched, still open.
- Operator config bootstrap on this machine (`~/.lane/config.toml` with a real
  `linear_api` → `op://` reference) for the live demo.
