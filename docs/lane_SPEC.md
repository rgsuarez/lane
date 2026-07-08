# `lane` — Specification (Slice 0a)

**Status:** design spec, no code. Produced by Slice 0a of the approved plan
`/Users/richie/.claude/plans/plan-mode-build-the-gleaming-donut.md`.
**Companion:** [`VANTAGE_EXIT_AND_MIGRATION_INVENTORY.md`](./VANTAGE_EXIT_AND_MIGRATION_INVENTORY.md).
**Target machine:** `general` (Mac mini M4, macOS). Portable to liquid/others later.

> This document defines *what we build*. It is **Vantage-migration-aware, never Vantage-compatible**: the `lane` runtime never calls Vantage for claims, planning, secrets, sessions, or closeout.

---

## 1. North star

A **portable, Linear-first, offline-capable local agent-work orchestration app**. **Linear** is the planning source of truth, **GitHub** is the code/CI/review SoT, **1Password** is the secret provider, and **`lane`** owns the machine-local logistics those three do not: lane claims, worktree coordination, active session/heartbeat visibility, advisor/executor pairing, audit history, Git discipline, gates, and closeout. The core **works fully offline**; network services only enrich planning and closeout.

## 2. Product boundary

**Standalone local app**, own repo (`~/projects-local/lane`), **separate from zeos**, callable by zeos via a skill-wrap. Not a zeos subcomponent, not a Vantage module, not a hybrid. Rationale: separation of concerns (it is the local successor to Vantage's *lane/session runtime* only — planning→Linear, secrets→1Password); portability (one native binary, env-overridable root); offline-first reliability (claims must work with nothing else up); clean zeos integration via the proven skill-wraps-CLI pattern.

## 3. Name, runtime, install

- **Name:** `lane`. **Config/state root:** `~/.lane/` with **`LANE_ROOT`** env override (absolute-only; test isolation + portability).
- **Runtime:** **Rust**, shipped as a **single native CLI binary** (self-contained; macOS does not support fully static libSystem linking — "native self-contained," not "static"). Chosen for: no `node_modules` cross-platform landmine, fast startup for a hot CLI, first-class atomic file-locking/concurrency.
- **No daemon** (MVP). Claims are files; liveness is read at query time; the board is a read-time aggregator. A daemon/dashboard is a later embed-first COA only.
- **Install:** `general` first (`cargo install --path .` or copied binary on PATH) + a zeos `skills/lane/SKILL.md` wrapper. Later: copy the binary to liquid/others; per-host `LANE_ROOT`; **GitHub remote deferred** (decided before the installer slice).

## 4. CLI surface

```
lane board     # read-only issue-keyed aggregate (Linear + worktrees + claims + liveness)
lane pull      # list assigned Linear issues to choose from (read-only)
lane plan      # record plan_path for a lane
lane start     # create branch+worktree + claim + register pair (gated git steps)
lane claim     # atomic claim of a lane (offline-capable)
lane renew     # extend a held claim (owner-only)
lane release   # release a held claim (owner-only)
lane status    # read-only status of one lane (--json)
lane list      # read-only listing of claims (all namespaces, or one --repo)
lane check     # read-only: does an active caller-owned claim cover a path? (Slice 3.5)
lane hook      # git pre-commit guard: print|install|status|uninstall (Slice 3.5)
lane pair      # attach advisor to executor (same Linear key)
lane handoff   # flip claim_status:handoff + write digest
lane close     # release + draft Linear closeout (gated write)
lane migrate   # Vantage-LOE → Linear, archive-first (gated batch)
```
Verbs mirror the proven `vantage lane` + `zeos-lane` vocabulary. `--json` on all read verbs.
`hook` is lane's first commit-time-enforcement surface (a deliberate Slice 3.5 boundary
expansion, ZER-84): the installed pre-commit hook runs `lane check` and — in `enforce`
mode — refuses an uncovered commit fail-closed, converting the "first agent needs no
claim" convention into a mechanical gate at the commit choke point.

## 5. Config & data model

**Authoritative, local, files (work offline):**
- Claim records — `~/.lane/<repo>/locks/<lane>.lock` (JSON). SoT for "who holds what on this machine."
- Audit log — `~/.lane/<repo>/audit.log` (append-only JSONL).
- Config — `~/.lane/config.toml`.

**Derived / cache (non-authoritative, network-enriched):** worktree registry (`git worktree list`), Linear issue read-cache (TTL'd — **Linear is SoT**), session/heartbeat (overseer join or heartbeat-file mtime).

**Claim record schema** (ports `zeos-lane` fields + Vantage session concepts):
```
lane, repo, instance, pid(info-only), target, target_normalized, note,
claimed_at, updated_at, expires_at, ttl_hours,
linear_key, branch, role(executor|advisor), pr_url,
gate(plan|execute|review|smoke|migration|merge|closeout),
plan_path, claim_status(active|blocked|handoff), session_ref
```

## 6. Source-of-truth split

| Layer | Owns | SoT for |
|---|---|---|
| **Linear** | Issues/Projects/Initiatives, status, assignee, labels, planning comments | **Planning** |
| **GitHub** | Branches, PRs, CI, reviews, releases | **Code / CI / review / deploy** |
| **1Password** | Secret storage + retrieval (`op`) | **Secrets** |
| **`lane`** | Claims, worktree registry, session/heartbeat, pairing, gates, closeout, local audit | **Machine-local logistics** |
| **zeos** | Kernel/profile identity, journals, memory; skill-wrap calling `lane` | **Callable operator OS / memory wrapper** |
| **Vantage (homebox)** | Historical LOE archive; migration source; design reference | **Legacy archive / optional unrelated ops only** |
| **overseer (optional)** | Pair registry, heartbeats — single host | **Liveness join (per host)** |

## 7. 1Password integration design

- **Dependency: the `op` CLI** (over the SDK) — zero in-process credential handling, native Touch ID / service-account auth, `op run`/`op read` inject secrets into a child env without printing them, embed-first commodity choice.
- **Naming (no secret labels in docs/config):** secrets resolved by **logical role keys** in `~/.lane/config.toml` mapped to opaque `op://<vault>/<item>/<field>` references. Store **references, never values, never the human label**. (E.g. a `linear_api` role → an `op://` reference.)
- **Bootstrap:** `op signin` (Touch ID) for operator sessions, or a **service-account token** via env for headless contexts — never written to disk by `lane`.
- **Failure mode (fail-closed, offline-safe):** missing `op` / not signed in / missing reference → secret-requiring actions (e.g. Linear writes) fail closed with a clear message; **all local logistics keep working** (they need no secret).
- **Audit:** `lane` logs a **"secret requested" event (role key + ts, never the value)**; 1Password keeps its own access audit.
- **Never print values:** retrieval is always `op run -- <cmd>` / `op read` into a child env; a guard refuses to log a resolved secret. **Fallback** providers (env-var pointer, macOS Keychain) are config-selectable; **1Password is preferred/default.**

## 8. Local lane model

- **Claim:** atomic exactly-one-winner (`O_CREAT|O_EXCL`); lane name = Linear key (`lqos-148`) or slug; refuse on active same-name or target-overlapping lane.
- **Target/worktree overlap:** path-canonical (NFC, case-fold, symlink realpath), ancestor/descendant refusal, app-level mutex. Worktree path is the target.
- **TTL/renew/release:** hours-scale (default 12); owner-only renew/release; never auto-steal an active lane; race-safe expiry takeover under a per-lane mutex; `--force` operator override (logged).
- **Identity:** explicit `--instance <journal-stem>` / `LANE_INSTANCE`, never guessed.
- **Liveness (join, not stored in claim):** overseer `get_worker_heartbeats` if present, else heartbeat-file mtime + tmux check. `pid` is info-only.
- **Pair/advisor (ported from Vantage):** `role:executor|advisor` on the claim; advisor references the same Linear key; 7-var env contract + bootstrap-markdown injection; active-parent doctrine (advisor refuses a non-active parent); handoff flips `claim_status:handoff` + writes a digest.
- **Audit:** append-only JSONL `{event: claim|renew|release|force|takeover|handoff|secret_requested, lane, linear_key, instance, role, ts}`.
- **Stale/orphan:** `EXPIRED` (past TTL) / `possibly-stale` (active, idle >3h) / `orphaned` (active, no live session) — surfaced by `lane board`; **release always operator-gated**, never auto-stolen.
- **Commit guard (Slice 3.5):** `lane check` answers "does an ACTIVE claim owned by THIS instance equal-or-ancestor-cover this path?" (read-only, offline, all-namespace scan by default — namespace inference from a worktree's toplevel basename is wrong by construction; identity required, never guessed). `lane hook install` writes a marked pre-commit block into the repo's RESOLVED hooks dir (one install covers all worktrees), composing with — never clobbering — foreign hooks; a managed `core.hooksPath` (husky et al.) is refused with the exact paste-in snippet. Modes via `git config lane.hook.mode`: `advise` (warn, default) / `enforce` (fail closed); `LANE_HOOK_BYPASS=1` is the loud human bypass. The hook never auto-claims and never mutates lane state. Residual: `git commit --no-verify` skips pre-commit — consumer doctrine forbids it for agents; a CI-side backstop is a later slice.

## 9. Offline / local-only mode (REQUIRED — first-class constraint)

`claim | status | list | renew | release` (+ local pairing/handoff/audit) **work with NO Linear, NO GitHub, NO 1Password, NO homebox, NO Vantage.** They touch only local files under `LANE_ROOT`. Network services (Linear reads, GitHub-integration status movement, 1Password fetch for Linear writes) **enrich planning/closeout only** and fail closed without blocking local logistics. Verified by an explicit offline test (§16).

## 10. Git discipline

Branch `richie/<team>-N-<slug>` (Linear GitHub-integration format → auto-links PR + auto-moves status); one worktree per issue `~/projects/<repo>-<team>-N-<slug>`; primary checkout read-only; exact-path staging; dirty-tree pre-checks before any write; PR/merge gates (review + browser smoke where UI-visible); **GitHub stays code/CI/review SoT**. **NFS landmine:** heavy repos are NFS-symlinked to homebox and `claude-yolo` cannot start under `~/mnt/homebox-storage/`; `lane` detects an NFS-resident target and warns. App + lane root live on Mac SSD.

## 11. Cross-machine stance

**Local-only authority in v1** — each host authoritative for its own lanes. Team-visible state = **Linear only**. A read-only cross-host *view* may aggregate transiently — **never ad-hoc remote lock copying as authority.** Hard cross-host locking is a later, explicitly-approved COA that **embeds** a real shared store (Postgres advisory locks / etcd / NATS), never hand-rolled.

## 12. Linear integration + security/privacy

**Linear:** reads free (issues/projects/states/assignees); **status movement via the GitHub integration** (branch/PR-driven, not an agent write); **labels / custom fields / comments = drafted + operator-gated writes**; migration/import = gated batch. No Linear CLI/MCP on general → **net-new thin GraphQL adapter**, API key resolved from **1Password** at call time, never persisted.

**Never leaves the machine / never posted to Linear:** secret values + 1Password references/labels + retrieval mechanics; raw transcripts / chain-of-thought; journals, memory, pair notes; env values, connection strings, DB fingerprints; customer PII; any gated/unverified state asserted "shipped." Redaction runs before any Linear draft. The tooling performs **no secret-bearing reads** and never echoes a resolved secret.

## 13. `zeos-lane` salvage matrix (design input — what we port)

> `zeos-lane` (liquid's Node tool) is **reference design only** — absent on general.

| Feature | Disposition | Note |
|---|---|---|
| Atomic `O_CREAT\|O_EXCL` exactly-one-winner | **PORT (Rust)** | Proven core |
| Target overlap (NFC + case-fold + symlink realpath, ancestor/descendant refusal, app mutex) | **PORT** | Worktree/shared-file isolation |
| Hours-TTL, never auto-steal active, race-safe expiry takeover, `--force` | **PORT** | Keep exact safety guarantees |
| Explicit identity (`--instance`, never guessed); `--app`→`--repo` namespace | **PORT** | |
| `list` UX | **PORT + EXTEND** | Add `--json` + `status <lane>` verb |
| Breadcrumb audit | **REPLACE** | Structured JSONL |
| Per-host locks, no cross-host | **ACKNOWLEDGE; local-first** | Cross-host later; no remote lock copying |
| Node implementation | **DISCARD code / PORT design** | Rust reimplement |

## 14. Vantage salvage matrix (summary)

Full per-surface enumeration with migration mechanics is in
[`VANTAGE_EXIT_AND_MIGRATION_INVENTORY.md`](./VANTAGE_EXIT_AND_MIGRATION_INVENTORY.md). Summary: **PORT** the LOE single-owner-checkout semantics + lane runtime + session control plane + run-audit (local); **REPLACE** the LOE planning model (→Linear), the secret vault (→1Password), the dashboard (→Linear UI + `lane board`); **ARCHIVE** existing LOE data read-only; **RETIRE-or-SEPARATE** cost / cron-agent fleet / topology (later COAs, not core); **DISCARD** the multi-tenant model (single-operator MVP).

## 15. Implementation slices (reference)

0a (this doc + the inventory) → 0b doctrine edits (gated) → 1 read-only `lane board` → 2 locking core + offline mode → 3 lane lifecycle + pairing + zeos skill-wrap → **3.5 commit-guard adapter (`check` + `hook`, ZER-84)** → 4 Linear read adapter + 1Password + gated writes → 5 migration tooling + installer. Daemon/dashboard + hard cross-host locking are later embed-first COAs.

## 16. Test strategy

Unit (claim overlap, TTL math, JSON schema, redaction, `op` reference parsing — mocked); integration (temp `LANE_ROOT`, fixture locks/worktrees, **fake Linear fixtures, mocked `op`**); **offline-mode test** (no Linear/GitHub/1Password/homebox/Vantage → claim/status/renew/release pass); concurrency (N-agent exactly-one-winner + orphan detection); read-only Linear (mock GraphQL, drafts never auto-write); Git/worktree hygiene (NFS-path warning); migration (fixture LOE export → Linear mapping, idempotent, `vantage-loe:<id>` stamp).

## 17. Open decisions

GitHub remote (deferred to installer slice); liveness source (overseer vs heartbeat files); Linear team→repo map; 1Password vault/item naming convention (references only). Name `lane`, path `~/projects-local/lane`, Rust, config root `~/.lane` are confirmed.
