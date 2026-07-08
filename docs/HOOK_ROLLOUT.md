# lane commit-guard rollout (Slice 3.5, ZER-84)

Consumer-facing doctrine for installing the `lane hook` pre-commit guard across the
machine's repos. The guard converts "agents should hold a claim before committing" from
convention into a mechanical gate at the commit choke point: the installed hook runs
`lane check --path "$PWD"` and, in `enforce` mode, refuses an uncovered commit
fail-closed with the exact fix command on stderr.

## Posture: advise-first soak, per-repo enforce flip

Install lands in **advise** mode (warn, never block). Flip a repo to enforce with:

```bash
git -C <repo> config lane.hook.mode enforce   # no reinstall needed
```

**Enforce-flip criteria (per repo):** ≥5 working days AND ≥30 commits with

1. zero false blocks (every WARNING traced to a genuinely uncovered commit),
2. zero unexplained `lane-hook: WARNING` lines on agent commits (agents actually hold claims), and
3. zero `exit 2, integrity/io` events.

## Rollout order

1. **lane itself** (dogfood; `~/projects/lane` is a symlink to `~/projects-local/lane` — one checkout, one install):
   ```bash
   lane hook install --git-repo /Users/richie/projects-local/lane --repo lane
   ```
   First repo to flip enforce after soak.
2. **Greenfield fleet** — crypto-bot, vantage, verify, zeos (verified: no hooks, no
   `core.hooksPath`; native mode, instant):
   ```bash
   lane hook install --git-repo <repo> --repo <namespace>
   ```
3. **eleetai LAST, via PR (managed mode).** eleetai runs husky v9
   (`core.hooksPath=.husky/_`); `lane hook install` correctly REFUSES and prints the
   snippet. The consumer change is a PR appending the output of
   ```bash
   lane hook print --snippet --repo eleetai
   ```
   at the **END** of `.husky/pre-commit` — after the gitleaks and lockfile gates, so
   secret-scan-first ordering is preserved. The block is `sh -e`/`set -u`-safe under
   husky's loader and falls through to nothing (it is the last gate). Set
   `lane.hook.mode` in **both** clones' `.git/config`
   (`/Users/richie/projects-local/eleetai` and `/Volumes/Workspace/projects/eleetai`) —
   repo config is per-clone even though the tracked hook file is shared history.

## Operating the guard

- **Status:** `lane hook status --git-repo <repo> [--json]` — managed?, installed?,
  version, mode, foreign hook present, lane-on-PATH.
- **Bypass (human/emergency, loud):** `LANE_HOOK_BYPASS=1 git commit …` — prints one
  `lane-hook: BYPASSED` line to stderr and passes. Not for agents.
- **Uninstall:** `lane hook uninstall --git-repo <repo>` — removes exactly the marked
  block (foreign hook restored byte-identical) and the `lane.hook.mode` key; refuses if
  the markers are damaged.
- **Upgrade:** re-run `install` — the marked block is replaced in place; an operator's
  `enforce` is never downgraded by a plain re-install.

## Fail postures (never silent-open)

| Situation | advise | enforce |
|---|---|---|
| Uncovered / foreign-owner / no-identity (`lane check` exit 1) | WARNING + pass (relays the fix command) | BLOCKED (relays the fix command + bypass) |
| `lane` missing from PATH | WARNING + pass | BLOCKED with install/bypass instructions |
| `lane check` exit 2 (integrity/io) | loud WARNING + pass | BLOCKED |

## Honest residuals

- **`git commit --no-verify` skips pre-commit.** The guard is a mechanical default, not
  adversarially airtight. Consumer doctrine forbids `--no-verify` for agents; a
  push-time or CI-side coverage backstop (the eleetai pre-push gitleaks pattern) is the
  eventual hard floor — out of scope for Slice 3.5.
- **eleetai fresh agent worktrees have ALL husky hooks inert** (the git-ignored
  `.husky/_/` shim dir is regenerated only by `npm install`/`prepare`, and
  `scripts/agent-worktree.sh --launch` never runs it). This pre-existing consumer bug
  silences gitleaks today and will silence the lane guard identically; the fix
  (bootstrap husky in `agent-worktree.sh`) is consumer-side and tracked as its own
  eleetai issue.
