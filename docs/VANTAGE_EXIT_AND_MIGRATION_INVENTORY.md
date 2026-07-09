# Vantage Exit & Migration Inventory (Slice 0a)

**Status:** inventory, no code, no mutations. Produced by Slice 0a of the approved plan
`/Users/richie/.claude/plans/plan-mode-build-the-gleaming-donut.md`.
**Companion:** [`lane_SPEC.md`](./lane_SPEC.md).

> **Goal:** extract the useful lessons, preserve audit history, and **remove Vantage from daily agent-work orchestration** on `general`. The app is **Vantage-migration-aware, never Vantage-compatible**.
>
> **Discipline (hard):** this inventory captures **file paths + doctrine topics only — never secret values, never 1Password references/labels, never credential-retrieval mechanics.** Secret-bearing files (`~/.vantage/token`, `~/.vantage/master.age`, `~/.vantage/env/`, `~/.zeos/tokens`, `~/.secrets/*`) are listed **by path category only; contents were never read.**

---

## 1. Topology recap (verified read-only)

- **Vantage the *service* runs on homebox** — `vantaged` on `homebox:7777` from `/opt/vantage/bin/`, Postgres `vantage` schema at `homebox:5433`, token `/var/lib/vantage/token` (root-owned), SolidStart dashboard at `homebox:7777`.
- **`general` is a Vantage *client* / dev host** — `~/projects/vantage` → symlink → `~/projects-local/vantage-recovery` (a 2026-05-03 storage-incident recovery checkout; siblings `vantage.volume-blocked-20260503-*`, `vantage-recovery`, `recovery-captures/`); a `vantage-probe` launchd companion pushes Mac metrics to homebox.
- **`general` is greenfield for local lane coordination** — `~/.zeos/` holds only `tokens` (secret, not read); no `~/.zeos/coordination`, no `zeos-lane` binary, no lock dirs.

## 2. Surface inventory — disposition per capability

Dispositions: **keep-as-lesson** (port the idea into `lane`) · **replace** (Linear / 1Password / GitHub / `lane board`) · **retire** (stand down) · **archive** (read-only history) · **separate** (out of `lane`'s scope; retire-or-standalone-ops in a later COA).

| # | Vantage surface | Where (path / topic — no secrets) | Disposition | Migration mechanic / note |
|---|---|---|---|---|
| 1 | **LOEs** (lines_of_effort, state machine, priority) | homebox Postgres `vantage.lines_of_effort` + `*_events`; verbs in `vantage-recovery/docs/agents/GUIDE.md` | **replace (planning) + archive (data)** | LOE→Linear (§4); export read-only; retain rows as audit history |
| 2 | **LOE single-owner checkout** (transactional one-owner, checkin bridge note) | same | **keep-as-lesson** | Becomes `lane claim` exactly-one-winner + handoff note |
| 3 | **Lane bootstraps** (`.vantage/bootstrap-<role>-<sid>.md`, doctrine-extract injection) | `vantage-recovery/AGENTS.md:355-377`; `<repo>/.vantage/` | **keep-as-lesson** | Reimplemented locally as `lane` bootstrap injection (markdown, `.git/info/exclude`) |
| 4 | **`claude-multi` / `codex-multi`** launchers (worktree-isolated executor/advisor entry) | `vantage-recovery/CLAUDE.md:42-56,173-204` | **keep-as-lesson + replace** | `lane start`/`lane pair` spawn tmux locally; no Vantage HTTP control |
| 5 | **tmux `-L vantage-lanes` socket** convention (isolated socket; macOS firewall mitigation) | `vantage-recovery/AGENTS.md:299-320` | **keep-as-lesson** | `lane` uses its own isolated tmux socket; same degrade-not-die posture |
| 6 | **7-var session env contract** (`VANTAGE_SESSION_ID`, `_LOE_ID`, `_AGENT_ROLE`, …) | `vantage-recovery/AGENTS.md:322-337` | **keep-as-lesson** | Re-expressed as `LANE_*` env (role, linear_key, repo, branch, instance, parent, bootstrap) |
| 7 | **Run start/end audit** (`agent_runs`, run start/end/heartbeat) | homebox Postgres `vantage.agent_runs`; `GUIDE.md:218-225` | **keep-as-lesson (local) + separate (fleet)** | Lane-work audit → local JSONL; cron-agent run audit stays a Vantage concern |
| 8 | **Secret-vault references** (age-encrypted vault, ACL bindings, `secret_access_log`) | homebox Postgres `vantage.secrets_vault`/`secret_bindings`/`secret_access_log`; client token `~/.vantage/token`, master `~/.vantage/master.age` (**path category only**) | **replace (→1Password)** | 1Password `op` is the provider (SPEC §7); Vantage vault **read-only during migration only**; no `lane` path reads it |
| 9 | **Dashboard links** (SolidStart 7-screen) | `http://homebox:7777/` (Bridge/Overlook/Agents/Costs/Topology/Secrets/Journal) | **replace** | Planning visibility → Linear UI; local lane status → `lane board`. No rebuilt local web dashboard |
| 10 | **Cron-agent assumptions** (templates `ceo-wake` etc., adopt/confirm/release, plist/unit generation) | `vantage-recovery/docs/agents/GUIDE.md:42-204`; `docs/SYSTEM_ARCHITECTURE.md:146-193` | **separate (retire / standalone ops / later COA)** | Out of `lane` scope; `lane` governs *interactive* sessions, not cron jobs |
| 11 | **Topology references** (machines table, git/overseer/launchd/systemd/topology scanners) | `docs/SYSTEM_ARCHITECTURE.md:102,219-235` | **separate (retire / standalone ops / later COA)** | `lane` may *read* liveness later; never owns topology |
| 12 | **Global LOE-filing doctrine** (mandatory Vantage LOE filing every session; Vantage governance; secret-in-Vantage rule) | `~/.claude/CLAUDE.md` §"LOE Filing Doctrine", §"Vantage — Portfolio Control Plane", §"AI Model Defaults" (secret refs); `~/.claude/docs/cross-pair-overseer.md`, `~/.claude/docs/loe-automation-mode.md` | **replace (doctrine)** | Rewrite LOE-filing → Linear-issue filing; secret refs → 1Password. **This is Slice 0b (separately gated).** Inventory only here |
| 13 | **Multi-tenant model** (`project_id` + `operator_project_id`) | `vantage-recovery/docs/agents/GUIDE.md:98-162` | **discard (MVP)** | `lane` is single-operator, single-machine |

## 3. Doctrine-reference inventory for Slice 0b (edit checklist — NOT edited in 0a)

Files that reference Vantage LOE/lane/secret doctrine and will need rewriting in the separately-gated Slice 0b (paths + topics only):

| File / surface | Topic to change | Edit order |
|---|---|---|
| `~/.claude/CLAUDE.md` § "LOE Filing Doctrine — Autonomous Bug & Improvement Capture" | LOE filing → Linear issue filing | 1 (highest blast radius) |
| `~/.claude/CLAUDE.md` § "Vantage — Portfolio Control Plane" | Reclassify Vantage → archive/migration/reference; remove as planning/secret SoT | 2 |
| `~/.claude/CLAUDE.md` § "AI Model Defaults" + secret-retrieval references | Secrets → 1Password (`op`); Vantage vault read-only-during-migration | 3 |
| `~/.claude/docs/cross-pair-overseer.md`, `~/.claude/docs/loe-automation-mode.md` | Lane/LOE-automation phrasing → `lane` app + Linear | 4 |
| zeos `infrastructure/skills/*`, `kernel/`, `modules/`, `docs/` (LOE/lane mentions) | Point at `lane` skill-wrap; remove Vantage-lane assumptions | 5 |
| Project-local `CLAUDE.md`/`AGENTS.md` that reference Vantage LOEs | Per-project, after global settles | 6 |
| `~/.vantage/`, `~/.zeos/` (path category only) | Inventory only — runtime state, not doctrine; do not edit | n/a |

## 4. Migration mechanics (LOE → Linear, archive-first)

1. **Export read-only** from Vantage (`vantage_list_loes` / `vantage_get_loe`) — no Vantage writes.
2. **Map by scope:** single-PR LOE → Linear **Issue** (`TEAM-N`, the universal work ID); multi-PR/multi-issue LOE → **Project** + sub-issues; portfolio theme → **Initiative**. Priority → Linear priority + label.
3. **Stamp** each Linear item `vantage-loe:<id>` + a backref to the archived LOE.
4. **Retain Vantage LOE rows read-only on homebox — never delete.** They remain audit history.
5. **Legacy-intake gate:** do **not** disable/stub `vantage_create_loe` until the Linear path is **live, tested, and Commander approves cutover.** ~30-day compatibility window with both visible.

## 5. Vantage exit criteria (Vantage no longer required in daily orchestration when ALL hold)

1. **Linear live for planning** — issues/projects in use; GitHub auto-status verified per target repo.
2. **1Password live for secrets** — `op` bootstrapped on general; work-secret roles resolve via references; no workflow path reads the Vantage vault.
3. **Local app live for claims/sessions** — `lane` claim/status/renew/release + pairing in use; **offline mode verified**.
4. **GitHub integration verified** — branch→PR→issue auto-link/auto-status confirmed.
5. **Migration archive exported + indexed** — LOEs exported, mapped to Linear (`vantage-loe:<id>`), retained read-only.
6. **Cost / cron-agent fleet / topology each explicitly retained-as-standalone-ops or retired** — a written disposition per capability, not a silent dependency.

Until #1–#6, Vantage may run **read-only as a migration/archive source**; after, it is reference-only.

## 6. Ratified residual dispositions (exit criterion 6 — closed 2026-07-09, ZER-87)

Commander-ratified 2026-07-09 (delegation recorded in the LANEGAP campaign anchor). One line of consequence per capability:

| Capability | Disposition | Operative consequence |
|---|---|---|
| Cost / resource attribution | **RETAINED as standalone ops** (on Vantage, unchanged) | Global doctrine § Resources & Cost stays live; ZER-52 tracks provider revival; cost polling survives the exit |
| Cron-agent fleet | **RETAINED as standalone ops** (on Vantage, unchanged) | Wake loops, reporters, verify scheduled smokes keep running as Vantage agents; launchd `vantage.com.*` plists stay |
| Topology / probes / machines | **RETIRED** | No probe infrastructure exists (verified 2026-07-09: none on Mac launchd, none on homebox systemd); the machines table is not a consumer surface |

The two retentions deliberately SURVIVE the daily-orchestration exit; any future full-Vantage retirement must re-home them first. Recorded in global `~/.claude/CLAUDE.md` § Vantage same day.

### §3 checklist status (2026-07-09)

Rows 1–3: done by Operation CLEAN BREAK (2026-07-07). Row 4: both docs ARCHIVED to `~/.claude/docs/archive/` and their doctrine sections replaced with retired stubs (ZER-89). Row 5: OBE — zeos retired wholesale 2026-07 (permanent descope); no re-pointing, trees remain read-only reference. Row 6: workspace `~/projects-local/CLAUDE.md` satellite pointer fixed 2026-07-09; per-project residue handled as encountered. Row 7: unchanged (runtime state, not doctrine).
