# AGENTS.md — `lane` standing orders

Durable rules for any agent working in this repo. Read before editing. The full design
is in `docs/lane_SPEC.md` and the master plan
(`~/.claude/plans/plan-mode-build-the-gleaming-donut.md`).

**Current status & next work (read on cold boot):** as-built = Slices 0a/1/2 (offline
locking core; `main` @ the latest commit; 117 tests). Full context + the next steps are in
`session-journals/2026-06-21-002-NEXT-SESSION-HANDOFF.md` (build journal: the `…-001-…`
file). Next slice = **3 (lifecycle + pairing + git-worktree automation + zeos skill-wrap)**.
The north star (a local app that *replaces Vantage in daily orchestration*) is **not yet
realized** — much remains (Slices 0b/3/4/5; Vantage exit criteria 0/6).

## North star

`lane` is a **portable, Linear-first, offline-capable local agent-work orchestration
app**. It owns machine-local logistics: lane claims, worktree coordination, session
visibility, advisor/executor pairing, gates, local audit, and closeout. It is separate
from zeos and callable by zeos via a skill-wrap. It is **Vantage-migration-aware, never
Vantage-compatible** — the core runtime never calls Vantage.

## Source-of-truth boundaries

| Layer | Owns |
|---|---|
| **Linear** | Planning (issues/projects/status/assignee/labels) |
| **GitHub** | Code / CI / review / deploy |
| **1Password** | Secrets (`op` CLI; never values on disk) |
| **`lane`** | Machine-local logistics (claims, sessions, pairing, gates, local audit) |
| **zeos** | Operator-OS wrapper / memory; calls `lane` via a skill |
| **Vantage** | Legacy archive / migration source / reference only |

## The locking core is permanently offline

`claim / renew / release / handoff / status / list / board` MUST work with **no** Linear /
GitHub / 1Password / Vantage / homebox / overseer / tmux / network / daemon / DB / async.
They touch only local files under `$LANE_ROOT` (override with `--lane-root`; absolute, local
filesystem only — NFS is rejected because advisory locks are unreliable there). `board`'s
worktree enrichment is OPT-IN (`--worktrees git`); the default spawns nothing and touches
nothing outside `$LANE_ROOT`.

This does **not** prohibit *adapter* modules (Git worktree, Linear GraphQL, 1Password,
tmux/overseer liveness). Those live **outside the locking core** and never make the core
itself reach the network. Slice 3 added the first one: `src/git/` (local `git` shell-outs
under a bounded wait) plus the `start`/`close` COMPOSITION verbs in `src/lifecycle.rs`,
which orchestrate core primitives + the adapter and hold NO core mutex across a git spawn.
The core (`src/lock/`) has no dependency on either.

## Locking safety rules (do not weaken)

- **Exactly one active owner per lane** (OS advisory lock via `File::try_lock`; the kernel
  releases on process exit/crash — no stale leases, no PID/timestamp body).
- **No overlapping active targets in a repo** (canonical ancestor/descendant check).
- **Never auto-steal an active claim.** Takeover only when expired or `--force`.
- **`--force` exists only on `claim`** and **never bypasses the target-overlap scan**.
  renew/release are strictly owner-only (no `--force`).
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

`0` ok (incl. `release`/`close`/`status` of an absent lane). `1` refused
(`active_held`/`not_owner`/`target_overlap`/`mutex_busy`/`expired`/`not_held`/
`dirty_worktree`). `2` `identity`/`malformed`/`io`/`non_local_root` (plus Clap usage
errors, human-only). Under `--json`, exactly one versioned envelope is the sole stdout for
every post-parse exit path. `LaneError::{exit_code,reason}` is the single authoritative
mapping (`src/error.rs`).

## Secrets policy

1Password (`op`) is the secret provider. Never read secret files, never log secrets /
references / retrieval mechanics, never put secrets in lock files or the audit log. The
claim `note` is non-secret but is still excluded from the audit log.

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

No daemon / DB / async / HTTP / network *in the locking core*; no real git-worktree or tmux
*in the core*; no cross-host locking; no Slice 0b doctrine edits. (Adapters that add those
capabilities are future gated slices, not permanently banned.)
