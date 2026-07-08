# 2026-07-07 — Slice 3.5: commit-guard adapter (`lane check` + `lane hook`) — build journal

**Linear:** ZER-84 (Zero Echelon, P2) · **branch:** `slice-3.5-commit-guard` off `main` @ `b5d1361` · **plan:** `/Users/richie/.claude/plans/build-the-plan-to-robust-wave.md` (Commander-approved).

> Journal-gap note: the Slice 3 session (merged as PR #1, `b5d1361`, 2026-07-05) left NO
> session journal — this is the first journal since `2026-06-21-002-NEXT-SESSION-HANDOFF.md`.
> The AGENTS.md cold-boot header was one slice stale until this slice fixed it.

## Why this slice exists

lane's adoption (ZER-82, 2026-07-07) enforces exactly-one-winner at the **claim** level,
but nothing forced a session to hold a claim before `git commit` — doctrine's "first
agent in the canonical checkout needs no claim" left the 2026-04-27 HEAD-switch incident
class reproducible by two sessions that each believe they're first. This slice closes it
mechanically at the commit choke point.

## What was built

1. **Unified rich-refusal mechanism** (`src/error.rs`): `LaneError::RefusedMsg { reason, msg }`
   (exit 1; closed `Reason` in the envelope; composed fix text via the single Display/emit
   path). `RefusedReason`/`Reason` grew four unit variants: `uncovered`, `foreign_owner`,
   `no_identity`, `hook_compose_refused`. `RefusedReason` keeps `Copy`.
2. **Directional coverage** (`src/lock/target.rs::covers`): claim equal-or-ANCESTOR of
   path (the bidirectional `overlaps` is unsuitable for coverage).
3. **`lane check`** (`src/lock/check.rs` + runner in `src/lock/mod.rs`): read-only
   verdict "does an ACTIVE claim owned by THIS instance cover this path?" — `list_core`
   scan (all namespaces by default; `--repo` narrows scan + fix command),
   `target_normalized → target` fallback mirroring `scan_overlap`, active =
   `stale_state != Expired`, verdict precedence mine → foreign → uncovered with tiered
   fix hints (expired-own: re-claim WITHOUT `--force`; target-less-own: re-claim WITH
   `--force` — same-instance re-claim of an active lane refuses `active_held`). Identity
   required: absence = `no_identity` refusal (exit 1), invalid = `identity` (exit 2).
4. **Git adapter helpers** (`src/git/mod.rs`): `toplevel`, `hooks_dir`
   (`rev-parse --path-format=absolute --git-path hooks`), `config_get/set/unset` — same
   `require_flag_safe` + bounded-wait discipline; `lifecycle::git_to_lane` → `pub(crate)`.
5. **`lane hook print|install|status|uninstall`** (`src/hook.rs`, outside the core):
   canonical POSIX-sh block (`sh -e`/`set -u` safe; function-wrapped, `return`s so a
   composed hook falls through to later gates; `LANE_HOOK_BYPASS=1` loud bypass; mode
   from `git config lane.hook.mode`, default advise; missing-binary/exit-2 =
   warn-and-pass in advise, fail-closed in enforce; never silent-open). Install: managed
   `core.hooksPath` → refusal WITH the paste-in snippet (husky detection); native →
   fresh file / marked-block append (lane LAST) / marker splice (idempotent re-install;
   never downgrades an operator's enforce); symlink / dormant non-executable / non-UTF-8
   / oversize / damaged markers → refused untouched; atomic temp+rename writes, 0755.
   Status: read-only, exit 0 incl. not-installed. Uninstall: surgical block removal,
   foreign remainder byte-identical, config key unset (absent tolerated).
6. **Docs:** AGENTS.md (stale cold-boot header fixed; new Commit-guard section; reason
   list), `docs/lane_SPEC.md` §4/§8/§15, `docs/LANE_SYSTEM_DIAGRAM_SPEC.md`
   §2/§5.1/§5.4/§24/§25/§26 (including Slice-3 staleness it inherited),
   `docs/HOOK_ROLLOUT.md` (new — advise-first soak, enforce-flip criteria, eleetai
   managed path, honest residuals).

## Verification

- Gates: `cargo fmt --check` / `build` / `test` / `test --all-features` /
  `clippy --all-targets -- -D warnings` — all green. **227 tests / 27 suites** (baseline
  174; +53). No new dependencies (`no_network_guard` untouched).
- New integration: `tests/lock_check.rs` (16 — covered/subdir/directional/uncovered/
  foreign/expired/expired+foreign/no-identity/invalid-instance/target-less-hint/
  repo-filter/cross-namespace-warning/malformed-fail-closed/human/relative/default-cwd),
  `tests/hook_install.rs` (10), `tests/hook_commit.rs` (9 — real `git commit` through the
  installed hook: advise/enforce/bypass/covered/foreign/missing-binary/exit-2/worktree-
  via-common-dir/composed-foreign-gate), `tests/hook_uninstall.rs` (4). All
  timing-independent: expiry is fabricated by patching `expires_at` in the lock JSON —
  ZER-83's wall-clock-window pattern is the named anti-pattern and
  `tests/lock_concurrency.rs` was not touched.
- Harness additions (`tests/common/mod.rs`): `run_hook` (no `--lane-root` injection),
  `hook_test_path` (fresh binary first on PATH), `scratch_commit` (hermetic `-c`
  identity/signing overrides — the machine's global `commit.gpgsign=true` would
  otherwise break scratch commits), `scratch_git`.
- Dogfood: this build itself ran under a live claim (`lane/zer-84`, instance
  `claude-zer-84`, target = this checkout) taken via the real binary before the first edit.

## Decisions of record

- **`RefusedMsg` wrapper over payload-carrying `RefusedReason`** (keeps `Copy`, one
  mechanism for check + hook, §S2.13 single-mapping intact).
- **All-namespace default scan** (RL-1): namespace inference from a worktree toplevel
  basename is wrong by construction; `--repo` only narrows.
- **Absent identity refuses in the verb itself** (RL-2): otherwise an identity-less
  session passes on someone else's covering claim — the exact collision the guard exists
  to prevent.
- **Managed hooksPath = refuse + snippet, never write** (tracked hook files are repo
  content; generated shim dirs get clobbered). eleetai adoption is a PR, not an install.
- **`--no-verify` residual documented honestly** (RL-3): mechanical default, not
  adversarially airtight; CI-side backstop is a later slice.

## Follow-ups (not this slice)

- Consumer installs per `docs/HOOK_ROLLOUT.md` (lane → greenfield fleet → eleetai PR).
- File eleetai issue: fresh agent worktrees have ALL husky hooks inert (missing
  `.husky/_/` bootstrap in `scripts/agent-worktree.sh`) — pre-existing, silences
  gitleaks today.
- ZER-83 (lock-concurrency release-profile flake) untouched, still open.
