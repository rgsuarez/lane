# lane commit-guard rollout (Slice 3.5, ZER-84; placement + exit classes revised per ZER-90/ZER-91)

Consumer-facing doctrine for installing the `lane hook` pre-commit guard across the
machine's repos. The guard converts "agents should hold a claim before committing" from
convention into a mechanical gate at the commit choke point: the installed hook runs
`lane check --path "$PWD"` and, in `enforce` mode, refuses an uncovered commit
fail-closed with the exact fix command on stderr.

## Placement law (every install, native and managed)

Insert the lane block **immediately after the secret-scan gate (or first, if there is
no secret scan), and always BEFORE any gate that can exit early.** A success-path early
exit (`exit 0`) above the block makes the guard unreachable dead code: the host hook
stops before reaching it on exactly the commits the guard exists to check. This is not
theoretical — eleetai's lockfile gate ends `exit 0` whenever no `package.json` is
staged (nearly every commit), which silently killed the originally-documented
end-of-file placement (ZER-90). End-of-file placement is only safe when the host hook
has no early exits; placing the block right after the secret scan preserves
secret-scan-first ordering while guaranteeing it runs.

Tooling backstops (heuristic, warning-only): `lane hook install` (native mode) warns
when the hook it composed contains a success-path early exit above the lane block, and
`lane hook status` repeats the warning for composed hooks. Neither can see inside a
manager-owned hook file (husky's `core.hooksPath` points at its generated shim dir, not
the user hook), so for managed installs the smoke test below is the detector.

## Acceptance smoke test (mandatory — this is what caught ZER-90)

After every install, snippet paste, or hook-file refactor, from a hook-live worktree
with no claim identity:

```bash
env -u LANE_INSTANCE git commit --allow-empty -m "lane-guard smoke"
# expect on stderr: a `lane-hook:` line (WARNING in advise; BLOCKED in enforce)
git reset --hard HEAD~1   # drop the smoke commit (advise mode lets it land)
```

No `lane-hook:` line means the guard did not run — the block is unreachable (early exit
above it) or the hook file is inert. Do not report an install complete until this
passes.

## Posture: advise-first soak, per-repo enforce flip

Install lands in **advise** mode (warn, never block). Flip a repo to enforce with:

```bash
git -C <repo> config lane.hook.mode enforce   # no reinstall needed
```

**Enforce-flip criteria (per repo):** ≥5 working days AND ≥30 commits with

1. zero false blocks (every WARNING traced to a genuinely uncovered commit),
2. zero unexplained `lane-hook: WARNING` lines on agent commits (agents actually hold
   claims; v2's distinct no-identity message keeps propagation failures from polluting
   this count), and
3. zero `exit 2, integrity/io` events.

## Rollout order

1. **lane itself** (dogfood; `~/projects/lane` is a symlink to `~/projects-local/lane` — one checkout, one install):
   ```bash
   lane hook install --git-repo /Users/richie/projects-local/lane --repo lane
   ```
   First repo to flip enforce after soak.
2. **Greenfield fleet** — crypto-bot, vantage, verify (verified: no hooks, no
   `core.hooksPath`; native mode, instant). The zeos repo is excluded — zeos is retired
   (2026-07); do not install new tooling into it:
   ```bash
   lane hook install --git-repo <repo> --repo <namespace>
   ```
3. **eleetai LAST, via PR (managed mode).** eleetai runs husky v9
   (`core.hooksPath=.husky/_`); `lane hook install` correctly REFUSES and prints the
   snippet. The consumer change is a PR inserting the output of
   ```bash
   lane hook print --snippet --repo eleetai
   ```
   into `.husky/pre-commit` per the placement law: immediately after the gitleaks
   (secret-scan) gate and BEFORE the lockfile gate, whose success path ends in an early
   `exit 0` — the early exit that made the original end-of-file guidance dead code
   (ZER-90). As shipped (EAI-134), eleetai embeds the block at exactly that Gate-2
   position. The block is `sh -e`/`set -u`-safe under husky's loader and `return`-falls
   through to the remaining gates. Set `lane.hook.mode` in **both** clones'
   `.git/config` (`/Users/richie/projects-local/eleetai` and
   `/Volumes/Workspace/projects/eleetai`) — repo config is per-clone even though the
   tracked hook file is shared history. Finish with the acceptance smoke test in a
   hook-live worktree.

## Upgrading the block (v1 → v2)

v2 (ZER-91) adds a distinct no-identity diagnosis and enforce-mode exit classes —
1 = commit not covered by a claim (violation); 2 = environment (lane missing / no
identity / integrity-io) — matching the `1 = enforcement fail, 2 = environment fail`
taxonomy eleetai's pre-commit documents. Upgrade paths:

- **Native installs:** re-run `lane hook install` — the marked block is replaced in
  place (the splice parses any `vN`), and `lane hook status` reports the installed
  `script_version`.
- **Managed installs:** re-paste a freshly printed snippet at the same position on the
  next hook-file PR. eleetai currently embeds v1 (old taxonomy: every enforce block
  exits 1); advise mode makes the timing non-urgent — the re-paste is what picks up the
  v2 exit classes and the no-identity diagnosis.

## Operating the guard

- **Status:** `lane hook status --git-repo <repo> [--json]` — managed?, installed?,
  version, mode, foreign hook present, lane-on-PATH, unreachable-block warning.
- **Bypass (human/emergency, loud):** `LANE_HOOK_BYPASS=1 git commit …` — prints one
  `lane-hook: BYPASSED` line to stderr and passes, in BOTH modes. Not for agents.
- **Uninstall:** `lane hook uninstall --git-repo <repo>` — removes exactly the marked
  block (foreign hook restored byte-identical) and the `lane.hook.mode` key; refuses if
  the markers are damaged.
- **Upgrade:** re-run `install` — the marked block is replaced in place; an operator's
  `enforce` is never downgraded by a plain re-install.

## Fail postures (never silent-open)

| Situation | advise | enforce (exit class) |
|---|---|---|
| Uncovered / foreign-owner (`lane check` exit 1) | WARNING + pass (relays the fix command) | BLOCKED, **exit 1 — coverage violation** (relays the fix command + bypass) |
| No caller identity (`LANE_INSTANCE` unset/empty) | WARNING naming identity propagation + `export LANE_INSTANCE=` fix + pass | BLOCKED, **exit 2 — environment** (names the export fix; never mislabeled as uncovered) |
| `lane` missing from PATH | WARNING + pass | BLOCKED, **exit 2 — environment** (install/bypass instructions) |
| `lane check` exit ≥2 (integrity/io) | loud WARNING + pass | BLOCKED, **exit 2 — environment** |
| Bypass (`LANE_HOOK_BYPASS=1`) | one loud BYPASSED line + pass | one loud BYPASSED line + pass |

Only a covered commit is silent. **Observability:** the exit class is carried by the
HOOK SCRIPT's own exit status — what sibling gates, hook runners, direct invocation
(`sh .git/hooks/pre-commit`), and CI harnesses observe, and the same layer where
eleetai's pre-existing gates already honor the `1 = enforcement / 2 = environment`
taxonomy. `git commit` itself flattens ANY pre-commit failure to its own exit 1
(verified empirically, git 2.50): automation wrapping bare `git commit` sees only
pass/fail there. Key environment-repair logic (propagate `LANE_INSTANCE`, install
lane, fix `$LANE_ROOT`) on the hook-layer exit code or on the distinct `lane-hook:`
stderr messages — never on `git commit`'s own exit code.

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
- **Whitespace-only `LANE_INSTANCE`** evades the block's emptiness pre-check; `lane
  check` still refuses (`no_identity`, exit 1) and the hook then labels it with the
  coverage-class message/exit under enforce. Degenerate; lane's own stderr line directly
  above names the true cause.
- **The unreachability heuristic is line-lexical** (flags bare `exit` lines and
  `exit 0` fragments including `… && exit 0` / `… || exit 0` / `…; exit 0` one-liners;
  misses `exit "$rc"` and multi-line case-arm bodies; may false-positive on heredoc or
  quoted-string content, or on a conditional `exit 0` a maintainer knows is
  unreachable). It feeds a warning, never a refusal — the smoke test is the
  authoritative check.
- **`hook status` cannot inspect manager-owned hook files** (`core.hooksPath` resolves
  to husky's shim dir, not the pasted user hook), so managed-mode unreachability is
  detected by the acceptance smoke test, not by tooling.
