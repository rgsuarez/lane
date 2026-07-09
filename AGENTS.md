# AGENTS.md — `lane` standing orders

Durable rules for any agent working in this repo. Read before editing. The full design
is in `docs/lane_SPEC.md` and the master plan
(`~/.claude/plans/plan-mode-build-the-gleaming-donut.md`).

**Current status & next work (read on cold boot):** as-built = Slices 0a/1/2/3/3.5/**4**
(offline locking core; git worktree adapter + `start`/`close`/`handoff`; commit guard
`check` + `hook`, ZER-84; **Linear read adapter + 1Password + gated closeout writes,
ZER-85: `lane pull`, `board --linear api`, `close --draft-closeout|--post-closeout`,
root adapter audit, claim-generation guard — 295 tests at the Slice-4 baseline**). lane
was **adopted machine-wide 2026-07-07** (Linear ZER-82; consumer doctrine in eleetai
CLAUDE.md § MULTI-AGENT WORKTREE POLICY). Trust `git log` + `session-journals/` (newest
first) over any stale pointer. Next = **0b (doctrine edits, gated) and 5 (migration
tooling + installer)**. The north star (a local app that *replaces Vantage in daily
orchestration*) is **not yet realized** — Vantage exit criteria remain open. Known flake
quarantine: Linear ZER-83 (`tests/lock_concurrency.rs` release-profile timing) — do not
entangle.
**PERMANENT DESCOPE (2026-07-08 Commander directive): no tmux, no zeos, no pairing
runtime, no overseer — zeos is retired.** Never resurrect `lane pair`, skill-wraps, or
tmux integration.

## North star

`lane` is a **portable, Linear-first, offline-capable local agent-work orchestration
app**. It owns machine-local logistics: lane claims, worktree coordination, session
visibility, commit-time coverage enforcement, gates, local audit, and closeout. It is a
**standalone CLI invoked directly by agent sessions** — no wrapper, no skill layer, no
tmux (zeos retired 2026-07; pairing permanently descoped 2026-07-08). It is
**Vantage-migration-aware, never Vantage-compatible** — the core runtime never calls
Vantage.

## Source-of-truth boundaries

| Layer | Owns |
|---|---|
| **Linear** | Planning (issues/projects/status/assignee/labels) |
| **GitHub** | Code / CI / review / deploy |
| **1Password** | Secrets (`op` CLI; never values on disk) |
| **`lane`** | Machine-local logistics (claims, sessions, commit guard, gates, local audit) |
| **Vantage** | Legacy archive / migration source / reference only |

## The locking core is permanently offline

`claim / renew / release / handoff / status / list / board` MUST work with **no** Linear /
GitHub / 1Password / Vantage / homebox / overseer / tmux / network / daemon / DB / async.
They touch only local files under `$LANE_ROOT` (override with `--lane-root`; absolute, local
filesystem only — NFS is rejected because advisory locks are unreliable there). `board`'s
worktree enrichment is OPT-IN (`--worktrees git`); the default spawns nothing and touches
nothing outside `$LANE_ROOT`.

This does **not** prohibit *adapter* modules (Git worktree, Linear GraphQL, 1Password,
heartbeat-file liveness). Those live **outside the locking core** and never make the core
itself reach the network. Slice 3 added the first one: `src/git/` (local `git` shell-outs
under a bounded wait) plus the `start`/`close` COMPOSITION verbs in `src/lifecycle.rs`,
which orchestrate core primitives + the adapter and hold NO core mutex across a git spawn.
Slice 4 added `src/config` ($LANE_ROOT/config.toml, object-guarded read), `src/secrets`
(`op` CLI spawns under a 60s bounded wait; `env:` pointer fallback) and `src/linear`
(sync GraphQL over `ureq` — the crate's ONE allowlisted network dependency — plus the
TTL cache, closeout draft composer, and the adapter-owned per-lane publish lock). The
core (`src/lock/`) has no dependency on any of them: `tests/no_network_guard.rs` enforces
the manifest law (FORBIDDEN + justified ADAPTER_ONLY allowlist) AND a source scan proving
`src/lock/**` + `src/hook.rs` never import adapter modules or network-capable crates.
Network verbs (`pull`, `board --linear api`, `close --draft/post-closeout`) are opt-in
by invocation; every local verb is byte-identical with the adapters unused.

## Locking safety rules (do not weaken)

- **Exactly one active owner per lane** (OS advisory lock via `File::try_lock`; the kernel
  releases on process exit/crash — no stale leases, no PID/timestamp body).
- **No overlapping active targets in a repo** (canonical ancestor/descendant check).
- **Never auto-steal an active claim.** Takeover only when expired or `--force`.
- **`--force` exists only on `claim`** and **never bypasses the target-overlap scan**.
  renew/release are strictly owner-only (no `--force`). Since Slice 4, release is
  additionally **generation-guardable** (`ReleaseParams.expected_claimed_at`): the
  `close` composition binds to the claim generation `(repo, lane, instance,
  claimed_at)` it read, so a same-instance release+reclaim race can never have its
  successor claim released (or its worktree removed) by a stale close. The plain
  `release` verb passes `None` — byte-identical behavior.
- **Every force / takeover / release is write-ahead audited** (fsync'd `intent` before the
  destructive mutation; `completion` after). A post-mutation audit failure is reported as
  success-with-`audit_warning`, never "mutation failed".
- **Identity-inconsistent or malformed records fail closed** (exit 2). A symlink/non-regular
  object where a state file/dir is expected fails closed.
- **All claim-state and audit reads go through the single guarded reader**
  (`record::read_guarded`): it first validates the **whole ancestor directory chain beneath
  the canonical root** via `paths::guard_dir_chain` (every existing interior component must
  be a real, same-owner directory — an interior symlink / non-dir / wrong-owner fails closed,
  exit 2), then the leaf (reject symlink / non-regular / wrong-owner; verify the opened fd's
  `(dev, ino)` matches the lstat'd pair). `guard_dir_chain` is the SINGLE chain guard — never
  duplicate it, and never use `Path::is_dir`/`exists`/`metadata`/`canonicalize` (symlink-
  following) as a trust decision on a state component. Directory enumerations (`read_dir`)
  guard the chain first. No caller may `read_to_string` claim state directly. **`--force`
  never bypasses the object guard** (only a malformed *regular* same-lane record is
  force-takeable; a symlinked one fails closed).
- **Audit integrity:** every audit open/append/recovery routes through the object guard;
  recovery validates the ENTIRE complete stream (any malformed complete record fails closed
  before a mutation); only a final non-newline fragment is quarantined. A dangling `intent`
  (no matching `completion`) is reconciled against the lock files (the source of truth):
  applied/not-applied → structured `audit_warning`; **indeterminate → mutations fail closed**.
  A completion is **never fabricated**; there is no auto-repair beyond trailing-fragment recovery.
- **A refusal/malformed audit failure never changes the primary outcome** — the original
  exit code / `Reason` stands; the audit failure is surfaced as `audit_warning`/stderr.
- **Writes are atomic-visible**: free-lane via exclusive `hard_link`; takeover/renew via
  `rename`-over (never unlink-first), so a crash never leaves a torn or missing lock.

## Exit codes & JSON envelope

`0` ok (incl. `release`/`close`/`status` of an absent lane, and `hook status`/`hook
uninstall` of a not-installed repo). `1` refused
(`active_held`/`not_owner`/`target_overlap`/`mutex_busy`/`expired`/`not_held`/
`dirty_worktree`/`uncovered`/`foreign_owner`/`no_identity`/`hook_compose_refused`/
`no_linear_key`).
`2` `identity`/`malformed`/`io`/`non_local_root`/`secret_unavailable`/`network` (plus
Clap usage errors, human-only).
Under `--json`, exactly one versioned envelope is the sole stdout for every post-parse
exit path. `LaneError::{exit_code,reason}` is the single authoritative mapping
(`src/error.rs`); context-rich refusals ride `LaneError::RefusedMsg { reason, msg }`
(closed reason code in the envelope, composed fix text on human stderr) — never a second
print path.

## Commit guard (Slice 3.5 — `check` + `hook`)

- **`lane check` is read-only by law:** scans via `list_core` (guarded reads), zero
  audit interaction, zero mutation, spawns nothing, fully offline. Default scan = ALL
  namespaces (claim targets are machine-global paths; a worktree's toplevel basename is
  NOT the repo namespace). Identity is required — absence refuses `no_identity` (exit 1),
  an invalid identity stays `identity` (exit 2). Coverage is DIRECTIONAL
  (`Target::covers`: claim equal-or-ancestor of path) with the `target_normalized` →
  `target` fallback mirroring `scan_overlap`.
- **`lane hook` lives outside the locking core** (like `src/git/` + `src/lifecycle.rs`):
  takes NO core locks, never touches `$LANE_ROOT`, and the generated hook NEVER
  auto-claims. Compose, never clobber: marked-block append/replace only
  (`# >>> lane hook vN >>>` … `# <<< lane hook <<<`); managed `core.hooksPath`, symlink,
  dormant (non-executable), non-text, oversize, or marker-damaged files are refused
  (`hook_compose_refused`) with manual instructions. Writes are temp-in-same-dir +
  chmod 0755 + atomic rename. The canonical script is `BLOCK_TEMPLATE` in `src/hook.rs`;
  in a composed hook every path `return`s (never `exit 0` — that would skip the host's
  later gates). Modes: `git config lane.hook.mode` = `advise` (default) | `enforce`;
  `LANE_HOOK_BYPASS=1` bypasses LOUDLY; missing-binary/exit-2 postures are
  warn-and-pass in advise, fail-closed in enforce — never silent-open. Residual to keep
  documented: `git commit --no-verify` skips pre-commit (consumer doctrine forbids it
  for agents). Rollout doctrine: `docs/HOOK_ROLLOUT.md`.

## Secrets policy

1Password (`op`) is the secret provider. Never read secret files, never log secrets /
references / retrieval mechanics, never put secrets in lock files or the audit log. The
claim `note` is non-secret but is still excluded from the audit log.

As built (Slice 4, `src/secrets`): role keys in `$LANE_ROOT/config.toml
[secrets.roles]` map to opaque references dispatched by scheme (`op://…` → `op read
--no-newline` under a 60s bounded wait; `env:VARNAME` → the sanctioned env pointer).
`SecretValue` has no Display/Serialize/Clone and a redacted Debug; `expose()` is called
only at the Authorization-header construction. `op` stderr is classified then DROPPED
(it can name vaults/items); every resolution appends one `secret_requested` event
(role key + outcome + ts — never a value or reference) to the ROOT adapter audit
(`$LANE_ROOT/audit.log`), which core recovery never reads. Closeout drafts are
whitelist-by-construction (no `instance`, no `note`, no local paths) with a
defense-in-depth scrub before any post. The transport refuses non-https `api_url`
except loopback test fixtures.

## Test commands (all must be green before commit)

```
cargo fmt --check
cargo build
cargo test
cargo test --all-features
cargo clippy --all-targets -- -D warnings
```

Concurrency/crash tests spawn the real binary as separate processes (exactly-one-winner,
SIGKILL crash-release, mutex/audit contention). Fault tests inject the `FsOps`/`AuditSink`
seams. Write-path tests put `$LANE_ROOT` under `$HOME` so the local-filesystem device
check passes.

## GitOps gates

- Branch per slice from `main` HEAD; explicit-path staging.
- **Stop before commit; commit only on explicit operator go.** No push / PR / remote
  without authorization. **No co-author trailer** in commits.

## Prohibited scope (locking core)

No daemon / DB / async / HTTP / network *in the locking core*; no real git-worktree work
*in the core*; no cross-host locking; no Slice 0b doctrine edits. (Adapters that add such
capabilities are future gated slices — EXCEPT tmux/zeos/pairing/overseer integration,
which is PERMANENTLY banned per the 2026-07-08 Commander directive.)
