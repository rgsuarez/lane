# `lane` System Diagram Specification

**Purpose:** Numerical, implementation-aware description for producing architectural,
workflow, security, and migration diagrams of `lane`.

**Repository:** `rgsuarez/lane`  
**As-built baseline:** `main` at `e9f30b1`  
**Implemented:** Slices 0a, 1, and 2  
**Planned:** Slices 3–5 and later optional capabilities

## 0. Diagram notation

Use these styles consistently:

1. **Solid green box or arrow:** implemented and operating in the current codebase.
2. **Dashed blue box or arrow:** planned future integration.
3. **Dotted gray box or arrow:** legacy, migration-only, archive, or reference.
4. **Gold box:** external source of truth.
5. **Purple box:** local persistent state.
6. **Red crossed-out arrow:** explicitly forbidden dependency or data flow.
7. **Dark navy box:** the `lane` application boundary.
8. **Light cyan box:** human or agent actor.

## 1. Executive system model

1. `lane` is a standalone native Rust CLI.
2. It runs locally on an operator-controlled machine.
3. Its core works without network access.
4. It owns machine-local work claims and their audit history.
5. It prevents two local agents from owning the same lane simultaneously.
6. It prevents two local lanes from claiming overlapping filesystem targets.
7. It does not own project planning.
8. Linear is the planned project-management source of truth.
9. It does not own code review or deployment state.
10. GitHub is the code, CI, review, release, and deployment source of truth.
11. It does not store secrets.
12. 1Password is the planned default secret provider.
13. It does not replace agent memory or journals.
14. zeos remains the operator OS, memory, journal, and skill layer.
15. It does not make optional liveness systems authoritative.
16. Overseer may later enrich local claims with session and pair liveness.
17. It does not depend on Vantage during normal runtime.
18. Vantage is migration-aware legacy input and historical archive only.
19. Each machine initially owns only its own local lane state.
20. Cross-machine lock authority is not implemented.

## 2. Current implementation state

1. The private GitHub repository is `https://github.com/rgsuarez/lane`.
2. The authoritative branch is `main`.
3. The as-built baseline for this specification is Slice 3.5 (branched from `main` @ `b5d1361`).
4. Slice 0a delivered the architecture and Vantage-exit documents.
5. Slice 1 delivered the read-only board framework.
6. Slice 2 delivered the offline locking and audit core.
7. Slice 3 delivered the git worktree adapter, the `start`/`close` composition verbs, `handoff`, and the opt-in live board worktree probe.
8. Slice 3.5 delivered the commit guard: the read-only `check` coverage verb and the `hook` pre-commit family (ZER-84).
9. The current CLI implements `board`, `claim`, `renew`, `handoff`, `release`, `status`, `list`, `check`, `start`, `close`, and `hook print|install|status|uninstall`.
10. The codebase has 227 passing tests at the as-built baseline.
11. The core has no HTTP client.
12. The core has no database.
13. The core has no async runtime.
14. The core has no daemon.
15. The core has no background process.
16. The core has no live Linear client.
17. The Git adapter (worktree/branch/hooks-dir/config plumbing) lives OUTSIDE the locking core; the core itself never spawns git.
18. The core has no live 1Password integration.
19. The core has no tmux session spawning.
20. The core has no advisor/executor pairing runtime.
21. The core has no live Overseer integration.
22. The core has no Vantage migration executable.
23. The core has no cross-host locking.

## 3. Source-of-truth ownership

| ID | System | Authoritative for | Not authoritative for |
|---:|---|---|---|
| 3.1 | Linear | Issues, projects, initiatives, priorities, assignees, planning status | Local claims, secrets, code |
| 3.2 | GitHub | Code, branches, commits, PRs, CI, reviews, releases, deployment lineage | Local work ownership |
| 3.3 | 1Password | Secret storage and approved retrieval | Planning, code, local claims |
| 3.4 | `lane` | Machine-local claim ownership, target reservations, leases, local audit | Portfolio planning or secrets |
| 3.5 | zeos | Profiles, skills, private journals, memory, operator continuity | Claim authority |
| 3.6 | Overseer | Optional local pair and process liveness | Claim authority |
| 3.7 | Vantage | Historical LOE archive and migration input | Future planning, claims, sessions, secrets |

## 4. Primary actors

### 4.1 Commander

1. Selects or approves work.
2. Authorizes mutations.
3. Approves forced takeover.
4. Approves Git operations at required gates.
5. Approves Linear writes.
6. Approves migration and cutover.
7. Resolves ambiguous recovery conditions.

### 4.2 Executor agent

1. May be Claude, Codex, Gemini, or another executor.
2. Receives an issue or work identifier.
3. Operates within a specific repository.
4. Uses an isolated target or worktree.
5. presents an explicit instance identity to `lane`.
6. Claims, renews, and releases local work.

### 4.3 Advisor agent

1. Reviews or guides an executor.
2. Will eventually attach to the same Linear issue.
3. Will eventually carry `role=advisor`.
4. Will eventually reference an active executor.
5. Will participate in handoff and closeout.
6. Advisor pairing is planned, not implemented.

### 4.4 zeos

1. Maintains operator identity and profiles.
2. Maintains journals and memory.
3. Provides skills and workflow doctrine.
4. Will invoke `lane` through a skill wrapper.
5. Will consume structured JSON responses.
6. Does not own or modify claim authority independently.

## 5. CLI boundary

### 5.1 Implemented commands

1. `lane claim <lane> --repo <repo>`
2. `lane renew <lane> --repo <repo>`
3. `lane release <lane> --repo <repo>`
4. `lane handoff <lane> --repo <repo>` (Slice 3: owner-only status flip, core verb)
5. `lane status <lane> --repo <repo>`
6. `lane list [--repo <repo>]`
7. `lane board [--repo <repo>]` (`--worktrees git` opt-in live worktree probe)
8. `lane start <lane> --repo <repo> --git-repo <path>` (Slice 3: composition verb)
9. `lane close <lane> --repo <repo> [--remove-worktree]` (Slice 3: composition verb)
10. `lane check [--path <p>] [--repo <repo>]` (Slice 3.5: read-only claim-coverage verdict; identity required, all-namespace scan by default)
11. `lane hook print|install|status|uninstall` (Slice 3.5: git pre-commit guard family; composes with foreign hooks, refuses managed `core.hooksPath` dirs with a paste-in snippet; lane's first commit-time-enforcement surface)

### 5.2 Planned commands

1. `lane pull`
2. `lane plan`
3. `lane pair`
4. `lane migrate`

### 5.3 CLI validation

1. Repo identifiers must match the supported single-component identifier grammar.
2. Lane identifiers must match the supported single-component identifier grammar.
3. `.` is rejected.
4. `..` is rejected.
5. Instance identity must be explicit.
6. Instance identity comes from `--instance` or `LANE_INSTANCE`.
7. Identity is never guessed from the PID, username, shell, or terminal.
8. TTL must be finite.
9. TTL must be greater than zero.
10. TTL must not exceed 720 hours.
11. Notes must satisfy local validation.
12. A target must be absolute.
13. `LANE_ROOT` must be absolute.

### 5.4 Output contract

Every post-parse JSON response uses one envelope:

```json
{
  "schema_version": 1,
  "ok": true,
  "verb": "claim",
  "repo": "example",
  "lane": "TEAM-123",
  "outcome": "ok",
  "reason": null,
  "audit_warning": null,
  "data": {}
}
```

Outcomes:

1. `ok`
2. `refused`
3. `released`
4. `not_held`
5. `error`

Exit codes:

1. Exit `0`: operation succeeded or absence is harmless.
2. Exit `1`: safe operational refusal.
3. Exit `2`: malformed state, identity violation, filesystem failure, or security failure.

Closed reason values:

1. `active_held`
2. `not_owner`
3. `target_overlap`
4. `mutex_busy`
5. `expired`
6. `not_held`
7. `dirty_worktree`
8. `uncovered` (Slice 3.5: `check` — no active caller-owned claim covers the path)
9. `foreign_owner` (Slice 3.5: `check` — a covering active claim belongs to another instance)
10. `no_identity` (Slice 3.5: `check` — no `--instance`/`LANE_INSTANCE`; absence refuses, invalid stays `identity`)
11. `hook_compose_refused` (Slice 3.5: `hook` — managed hooksPath / symlink / dormant / non-text / oversize / damaged markers)
12. `identity`
13. `malformed`
14. `non_local_root`
15. `io`

Context-rich refusals (`uncovered`, `foreign_owner`, `no_identity`, `hook_compose_refused`) are carried by `LaneError::RefusedMsg { reason, msg }`: the envelope `reason` stays the closed code above; the human-mode stderr message is composed at the call site with real values (the exact fix command).

## 6. Local state root

### 6.1 Location

Default:

```text
~/.lane/
```

Override:

```text
LANE_ROOT=/absolute/local/path
```

### 6.2 Resolution

1. Accept the CLI override, environment override, or default.
2. Require an absolute path.
3. Find the longest existing ancestor.
4. Canonicalize that existing ancestor.
5. Preserve a non-existent tail for first-run creation.
6. Compare the resolved root device to `$HOME`.
7. Reject a different filesystem device as `non_local_root`.
8. Record the expected owner UID from `$HOME`.
9. Allow symlinks only in the prefix above the canonical root.
10. Reject symlinks beneath the root.

### 6.3 Filesystem layout

```text
~/.lane/
├── config.toml
└── <repo>/
    ├── locks/
    │   ├── <lane>.lock
    │   └── <lane>.lock.<operation-id>.tmp
    ├── mutexes/
    │   ├── <lane>.mutex
    │   └── target.mutex
    ├── audit.log
    └── audit.recovered/
        └── <operation-id>.frag
```

Permissions:

1. State directories use `0700`.
2. State files use `0600`.
3. Same-owner incorrect modes may be repaired on write paths.
4. Wrong-owner objects fail closed.
5. Read-only verbs do not create or chmod state.

## 7. Filesystem security boundary

### 7.1 Directory-chain guard

For every existing component beneath the canonical root:

1. Use non-following metadata.
2. Reject a symlink.
3. Reject a non-directory.
4. Reject a wrong-owner directory.
5. Reject a path outside the root.
6. Return `Absent` at the first missing component.
7. Never create directories on read paths.
8. Never chmod directories on read paths.
9. Never use symlink-following `Path::is_dir` as a trust decision.

### 7.2 Leaf-file guard

For every state file:

1. Inspect the path with non-following metadata.
2. Reject a symlink.
3. Reject a non-regular file.
4. Reject a wrong-owner file.
5. Open the file.
6. Read metadata from the open descriptor.
7. Compare device and inode with the pre-open metadata.
8. Reject a mismatch as a possible replacement race.
9. Read only after the identity check succeeds.

### 7.3 Security result

1. A symlinked claim fails closed.
2. A symlinked repo directory fails closed.
3. A symlinked locks directory fails closed.
4. A symlinked audit log fails closed.
5. A wrong-owner state object fails closed.
6. `--force` does not bypass this guard.
7. External content is never accepted as authoritative lane state.

## 8. Claim record

Path:

```text
~/.lane/<repo>/locks/<lane>.lock
```

This is authoritative for:

> Who owns this lane on this machine right now?

Fields:

1. `schema_version`
2. `lane`
3. `repo`
4. `instance`
5. `pid`
6. `target`
7. `target_normalized`
8. `note`
9. `claimed_at`
10. `updated_at`
11. `expires_at`
12. `ttl_hours`
13. `linear_key`
14. `branch`
15. `role`
16. `pr_url`
17. `gate`
18. `plan_path`
19. `claim_status`
20. `session_ref`

Current Slice 2 fields include:

1. Schema version.
2. Lane.
3. Repo.
4. Instance.
5. Informational PID.
6. Target.
7. Normalized target.
8. Note.
9. Claim timestamp.
10. Update timestamp.
11. Expiration timestamp.
12. TTL.

Future slices populate:

1. Linear key.
2. Branch.
3. Executor or advisor role.
4. PR URL.
5. Workflow gate.
6. Plan path.
7. Claim status.
8. Session reference.

The PID is informational and is never the liveness authority.

## 9. Mutex model

### 9.1 Per-lane mutex

Path:

```text
~/.lane/<repo>/mutexes/<lane>.mutex
```

Behavior:

1. Open under the object guard.
2. Acquire an OS advisory exclusive lock.
3. Retry with jittered exponential backoff.
4. Stop retrying after three seconds.
5. Return `mutex_busy` if still held.
6. Hold the file descriptor for the critical section.
7. Release automatically when the descriptor closes.
8. Release automatically when the process crashes.
9. Do not store lease timestamps in mutex files.
10. Do not implement PID-based mutex recovery.

### 9.2 Per-repo target mutex

Path:

```text
~/.lane/<repo>/mutexes/target.mutex
```

Purpose:

1. Serialize target-overlap scans.
2. Prevent two concurrent claims from both passing an overlap check.
3. Prevent targeted renew from resurrecting a conflicting target.

Lock order:

```text
lane mutex → target mutex
```

This fixed order prevents a lock-ordering cycle.

## 10. Target canonicalization

1. A target represents a worktree or protected filesystem area.
2. The target must be absolute.
3. `/` is prohibited.
4. Exactly `$HOME` is prohibited.
5. `.` components are prohibited.
6. `..` components are prohibited.
7. A target containing `LANE_ROOT` is prohibited.
8. A target contained by `LANE_ROOT` is prohibited.
9. Resolve the longest existing ancestor.
10. Resolve existing symlinks in that ancestor.
11. NFC-normalize unresolved path components.
12. Lowercase unresolved ASCII components.
13. Reject unresolved non-ASCII components in v1.
14. Store the canonical representation as `target_normalized`.

Two targets overlap when:

1. They are equal.
2. The first is an ancestor of the second.
3. The second is an ancestor of the first.

Example conflict:

```text
/work/app
/work/app/frontend
```

## 11. Free-lane claim sequence

1. Agent invokes `lane claim`.
2. CLI parses arguments.
3. Validator checks repo.
4. Validator checks lane.
5. Validator checks instance.
6. Validator checks TTL.
7. Validator checks note.
8. Resolve and validate the lane root.
9. Validate local filesystem ownership.
10. Create or validate required write directories.
11. Canonicalize the target when present.
12. Recover any trailing audit fragment.
13. Validate every complete audit record.
14. Acquire the per-lane mutex.
15. Reconcile dangling intents.
16. Guard-read the same-lane claim path.
17. Determine that no claim exists.
18. Acquire the target mutex when targeted.
19. Guard-read sibling claims.
20. Scan sibling targets for overlap.
21. Construct the new claim record.
22. Write the claim to a temporary file.
23. Fsync the temporary file.
24. Create the final lock using an exclusive hard link.
25. The hard link fails if another process already created the lane.
26. Remove the temporary file.
27. Append the `claim` audit event.
28. Return success.
29. Drop the target mutex guard.
30. Drop the lane mutex guard.
31. The OS releases both advisory locks.

The exclusive hard link is the exactly-one-winner primitive for a free lane.

## 12. Existing-lane decision table

1. No existing record → create.
2. Active valid record without `--force` → `active_held`.
3. Active valid record with `--force` → takeover candidate.
4. Expired valid record → takeover candidate without force.
5. Malformed regular record without force → `malformed`.
6. Malformed regular record with force → takeover candidate.
7. Identity-inconsistent regular record without force → `identity`.
8. Identity-inconsistent regular record with force → malformed takeover candidate.
9. Symlinked record → `identity`, even with force.
10. Wrong-owner record → `identity`, even with force.
11. Non-regular record → `identity`, even with force.
12. Overlapping sibling target → `target_overlap`, even with force.

`--force` bypasses only the active same-lane refusal or malformed regular-record refusal.
It never bypasses filesystem identity or target-overlap safety.

## 13. Takeover sequence

1. Acquire the lane mutex.
2. Validate and recover the audit stream.
3. Reconcile previous dangling operations.
4. Guard-read and classify the existing claim.
5. Acquire the target mutex when targeted.
6. Recheck all sibling target overlaps.
7. Create and fsync the replacement temporary record.
8. Generate an operation ID.
9. Append `intent{op=takeover}`.
10. Fsync the intent.
11. If intent append fails, abort before mutation.
12. Atomically rename the new record over the old record.
13. Append `completion{op=takeover}`.
14. Return the new and prior owner information.
15. If completion append fails after mutation, the claim remains successful.
16. Surface the completion failure as `audit_warning`.

## 14. Renew sequence

1. Agent invokes `lane renew`.
2. Require explicit instance identity.
3. Validate the lane root.
4. Validate or create write directories.
5. Recover and validate the audit stream.
6. Acquire the lane mutex.
7. Reconcile dangling operations.
8. Guard-read the claim.
9. Missing claim → `not_held`.
10. Different owner → `not_owner`.
11. Expired claim → `expired`.
12. Validate the requested or existing TTL.
13. Acquire the target mutex when targeted.
14. Re-run target-overlap detection.
15. Copy the claim record.
16. Update `updated_at`.
17. Update `expires_at`.
18. Update `ttl_hours`.
19. Write and fsync a temporary claim.
20. Atomically rename it over the existing claim.
21. Append a `renew` audit event.
22. Return the new expiration time.

TTL rules:

1. Default TTL is 12 hours.
2. Maximum TTL is 720 hours.
3. Renew cannot revive an already expired lease.

## 15. Release sequence

1. Agent invokes `lane release`.
2. Require explicit instance identity.
3. Validate the lane root.
4. Recover and validate the audit stream.
5. Acquire the lane mutex.
6. Reconcile dangling operations.
7. Guard-read the claim.
8. Missing claim → harmless `not_held`, exit `0`.
9. Different owner → `not_owner`, exit `1`.
10. Generate an operation ID.
11. Append `intent{op=release}`.
12. Fsync the intent.
13. If intent append fails, leave the claim intact.
14. Remove the claim file.
15. Append `completion{op=release}`.
16. Return `released`.
17. If completion append fails, release remains successful.
18. Surface the failure as `audit_warning`.

There is no forced release path.

## 16. Audit model

Path:

```text
~/.lane/<repo>/audit.log
```

Properties:

1. Append-only JSONL.
2. One JSON object per line.
3. Locked during append.
4. Newline-terminated events.
5. No secret values.
6. No claim notes.
7. Destructive operations use write-ahead intent and completion records.

Audit event fields:

1. `ts`
2. `op_id`
3. `op`
4. `event`
5. `repo`
6. `lane`
7. `instance`
8. `outcome`
9. `forced`
10. `prior_instance`
11. `was_malformed`
12. `reason`
13. `target`
14. `ttl_hours`
15. `recovered_path`
16. `recovered_bytes`

Audit event kinds:

1. `claim`
2. `claim_refused`
3. `renew`
4. `release`
5. `handoff`
6. `intent`
7. `completion`
8. `takeover`
9. `malformed`
10. `audit_recovery`

Audit outcomes:

1. `ok`
2. `refused`
3. `error`

Operation ID:

```text
<unix-nanoseconds>-<pid>-<atomic-counter>
```

## 17. Audit recovery

### 17.1 Trailing-fragment recovery

1. Lock `audit.log`.
2. Read the complete file.
3. Locate the final newline.
4. Parse every complete newline-terminated record.
5. Fail closed if any complete record is malformed.
6. Allow only a final non-newline fragment to be recoverable.
7. Create `audit.recovered/` under the object guard.
8. Write the fragment to `<operation-id>.frag`.
9. Fsync the recovery fragment.
10. Truncate `audit.log` to the final valid newline.
11. Fsync `audit.log`.
12. Append `audit_recovery`.
13. Never delete or rewrite earlier valid audit records.

### 17.2 Dangling-intent reconciliation

A dangling intent is:

```text
intent exists + matching completion does not exist
```

Takeover classification:

1. Claim matches the new owner → `applied`.
2. Claim matches the prior owner → `not_applied`.
3. Claim matches neither → `indeterminate`.

Release classification:

1. Claim is absent → `applied`.
2. Claim still matches releasing owner → `not_applied`.
3. Claim belongs to another owner → `indeterminate`.

Behavior:

1. Applied state produces an audit warning.
2. Not-applied state produces an audit warning.
3. Indeterminate state blocks mutations.
4. Read-only status may surface the warning.
5. No completion record is fabricated.
6. No automatic repair is performed.

## 18. Refusal and audit-failure behavior

1. Active-held refusals attempt a `claim_refused` event.
2. Target-overlap refusals attempt a `claim_refused` event.
3. Malformed claim rejections attempt a `malformed` event.
4. Identity rejections may attempt a `malformed` event where applicable.
5. Audit failure never changes the primary outcome.
6. The original exit code remains authoritative.
7. The original reason remains authoritative.
8. JSON output includes `audit_warning`.
9. Human output prints the primary error.
10. Human stderr also prints an audit-warning line.
11. A successful refusal audit produces no warning.

## 19. Read-only commands

### 19.1 `lane status`

1. Resolve the canonical local root.
2. Guard the complete directory chain.
3. Guard-read one claim.
4. Classify staleness.
5. Read the audit stream.
6. Reconcile dangling intents.
7. Return claim state and warnings.
8. Do not create state.
9. Do not chmod state.
10. Do not append audit events.

### 19.2 `lane list`

1. Resolve the canonical local root.
2. Guard repo and locks directories.
3. Enumerate claim files.
4. Guard-read every claim.
5. Fail closed on unsafe or malformed state.
6. Classify each record.
7. Sort by repo and lane.
8. Perform no mutation.

### 19.3 `lane board`

The board joins four provider categories:

1. Claims.
2. Worktrees.
3. Linear issues.
4. Liveness.

Current provider state:

1. Claims are real and authoritative.
2. Worktrees are fixture-based or empty.
3. Linear issues are fixture-based or absent.
4. Liveness is an `unknown` stub.

Future provider state:

1. Worktrees come from `git worktree list`.
2. Linear issues come from a read-only GraphQL adapter.
3. Liveness comes from Overseer, heartbeat files, or tmux.

Every board value carries provenance:

1. `authoritative`
2. `derived`
3. `fixture`
4. `unknown`

Staleness classifications:

1. `active`
2. `expired`
3. `possibly_stale`
4. `orphaned`

Rules:

1. `now >= expires_at` → expired.
2. Session known not live → orphaned.
3. No update for more than three hours → possibly stale.
4. Otherwise → active.

## 20. Planned full workflow

Use dashed blue arrows for this section.

1. Commander or agent chooses a Linear issue.
2. The Linear issue key becomes the universal work identifier.
3. `lane pull` lists assigned issues.
4. `lane plan` associates an approved plan path.
5. `lane start` checks Git status and repository policy.
6. `lane start` creates a purpose-named branch.
7. `lane start` creates an isolated Git worktree.
8. `lane claim` reserves the issue and target locally.
9. `lane start` launches an executor session.
10. `lane pair` attaches an advisor.
11. zeos stores private memory and journals.
12. Overseer optionally reports session and pair liveness.
13. The executor implements work inside the isolated worktree.
14. The executor periodically runs `lane renew`.
15. Commits are pushed to GitHub.
16. A GitHub PR is created.
17. GitHub CI and reviews determine readiness.
18. GitHub integration links the PR to Linear.
19. Linear status moves through GitHub integration where supported.
20. `lane handoff` creates a transition digest when ownership changes.
21. `lane close` drafts a sanitized Linear closeout.
22. Commander approves the Linear mutation.
23. `lane release` removes the local claim.
24. Local audit remains in `audit.log`.
25. GitHub retains the code and review history.
26. Linear retains the planning history.
27. Vantage is not called during normal runtime.

## 21. Planned 1Password integration

1. `lane` uses the `op` CLI rather than implementing a vault.
2. Configuration stores logical secret roles.
3. Logical roles map to opaque references.
4. Secret values are never stored in claim files.
5. Secret values are never stored in audit logs.
6. Secret values are never printed.
7. `op run` may inject a value into a child environment.
8. Missing authentication causes secret-requiring work to fail closed.
9. Local claim operations continue working offline.
10. `lane` may log `secret_requested`.
11. The event contains the logical role and timestamp, not the value.
12. 1Password retains its own access audit.
13. Environment pointers or macOS Keychain may be fallback providers.
14. 1Password remains the default provider.

## 22. Vantage migration and exit

Show Vantage as a dotted gray legacy box outside the active runtime.

1. Vantage currently contains historical LOEs.
2. The migration tool reads LOEs without making them runtime dependencies.
3. A single-PR LOE maps to a Linear issue.
4. A multi-issue LOE maps to a Linear project and issues.
5. A portfolio theme maps to a Linear initiative.
6. Migrated items receive `vantage-loe:<id>`.
7. Historical Vantage rows remain read-only.
8. Historical records are not deleted.
9. New planning moves to Linear.
10. New secrets move to 1Password.
11. New local claims move to `lane`.
12. New code state remains in GitHub.
13. Vantage does not provide runtime claim authority.
14. Vantage does not provide steady-state secret authority.
15. Vantage does not provide future session control.
16. Cost attribution is retained separately or retired.
17. Cron-agent capabilities are retained separately or retired.
18. Topology capabilities are retained separately or retired.
19. Dashboard capabilities are replaced or retired.
20. Vantage eventually becomes archive and reference only.

## 23. Cross-machine model

Each host has an independent root:

```text
general → ~/.lane/
liquid  → ~/.lane/
host-N  → ~/.lane/
```

Rules:

1. A host is authoritative only for its local claims.
2. Lock files are not copied between machines as authority.
3. Linear supplies team-visible planning state.
4. A future board may aggregate remote state read-only.
5. Hard cross-machine locking is not implemented.
6. Future hard locking must embed a proven shared system.
7. Candidate systems include PostgreSQL advisory locks.
8. Candidate systems include etcd.
9. Candidate systems include NATS-based coordination.
10. A custom distributed-lock protocol is prohibited without an explicit exception.

## 24. Diagram node inventory

### 24.1 Actor nodes

1. Commander
2. Executor agent
3. Advisor agent

### 24.2 Local application nodes

4. zeos
5. `lane` CLI
6. Input validator
7. JSON/human renderer
8. Lane-root resolver
9. Directory-chain guard
10. Leaf-file guard
11. Lane mutex
12. Target canonicalizer
13. Target mutex
14. Claim engine
15. Renew engine
16. Release engine
17. Shared record reader
18. Audit engine
19. Audit reconciler
20. Board assembler
21. Git adapter (Slice 3: worktree/branch/hooks-dir/config plumbing, bounded-wait spawns)
22. Lifecycle composer (Slice 3: `start`/`close` orchestration over core + git adapter)
23. Coverage engine (Slice 3.5: `check` — directional target coverage over guarded reads)
24. Hook manager (Slice 3.5: `hook print|install|status|uninstall` — marker-block composition)

### 24.3 Local state nodes

25. Claim files
26. Temporary claim files
27. Lane mutex files
28. Target mutex file
29. Audit log
30. Audit recovery fragments
31. Config file
32. Consumer repo pre-commit hook file (Slice 3.5: lane-marked block; NOT under `LANE_ROOT`)

### 24.4 External system nodes

33. Linear
34. GitHub
35. 1Password
36. Overseer
37. Git worktrees
38. Vantage archive
39. Other-host `lane`
40. Future shared lock service

## 25. Implemented edges

Use solid green arrows.

1. Commander → executor agent.
2. Executor agent → `lane` CLI.
3. `lane` CLI → input validator.
4. Input validator → lane-root resolver.
5. Lane-root resolver → directory-chain guard.
6. Directory-chain guard → leaf-file guard.
7. `lane claim` → claim engine.
8. Claim engine → lane mutex.
9. Claim engine → target canonicalizer.
10. Claim engine → target mutex.
11. Claim engine → shared record reader.
12. Claim engine → temporary claim file.
13. Claim engine → claim file.
14. Claim engine → audit engine.
15. `lane renew` → renew engine.
16. Renew engine → lane mutex.
17. Renew engine → target mutex.
18. Renew engine → shared record reader.
19. Renew engine → claim file.
20. Renew engine → audit engine.
21. `lane release` → release engine.
22. Release engine → lane mutex.
23. Release engine → shared record reader.
24. Release engine → claim file.
25. Release engine → audit engine.
26. Shared record reader → claim files.
27. Audit engine → audit log.
28. Audit engine → audit recovery fragments.
29. Audit reconciler → audit log.
30. Audit reconciler → claim files.
31. `lane status` → shared record reader.
32. `lane status` → audit reconciler.
33. `lane list` → shared record reader.
34. `lane board` → board assembler.
35. Board assembler → shared record reader.
36. Claim engine → JSON/human renderer.
37. Renew engine → JSON/human renderer.
38. Release engine → JSON/human renderer.
39. Status/list engine → JSON/human renderer.
40. JSON/human renderer → executor agent.
41. `lane start`/`lane close` → lifecycle composer (Slice 3).
42. Lifecycle composer → claim/renew/release engines (no core mutex held across a git spawn).
43. Lifecycle composer → git adapter → Git worktrees (Slice 3).
44. Board assembler → git adapter (Slice 3: opt-in `--worktrees git` live probe).
45. `lane check` → coverage engine (Slice 3.5).
46. Coverage engine → target canonicalizer.
47. Coverage engine → shared record reader (all namespaces by default; zero audit, zero mutation).
48. `lane hook install|status|uninstall` → hook manager (Slice 3.5).
49. Hook manager → git adapter (toplevel, hooks dir, `core.hooksPath`, `lane.hook.mode` config).
50. Hook manager → consumer repo pre-commit hook file (atomic temp+rename; marker-block surgery only).
51. Consumer `git commit` → installed pre-commit hook → `lane check` (advise warns, enforce fails closed; `LANE_HOOK_BYPASS=1` loud bypass).

## 26. Planned edges

Use dashed blue arrows.

1. zeos → `lane` CLI.
2. Advisor agent → `lane pair`.
3. Board assembler → Overseer.
4. Board assembler → Linear.
5. Git worktrees → GitHub branch and PR.
6. GitHub → Linear status integration.
7. `lane` Linear adapter → 1Password.
8. 1Password → temporary child environment.
9. `lane close` → operator-gated Linear write.
10. Vantage archive → migration tool.
11. Migration tool → Linear.
12. Multiple local `lane` installations → read-only aggregate board.
13. Multiple hosts → future shared lock service, only if approved.

## 27. Forbidden edges

Use red crossed-out arrows.

1. `lane` locking core ✕ Vantage runtime.
2. `lane` locking core ✕ Linear for claim authority.
3. `lane` locking core ✕ GitHub for claim authority.
4. `lane` locking core ✕ 1Password for claim authority.
5. `lane` locking core ✕ network dependency.
6. Host A claim files ✕ Host B claim files as copied authority.
7. Linear ✕ secret values.
8. Audit log ✕ secret values.
9. Claim file ✕ secret values.
10. `lane` ✕ custom database for the MVP.
11. `lane` ✕ custom message bus.
12. `lane` ✕ custom distributed lock protocol.
13. Overseer ✕ claim authority.
14. zeos journals ✕ public Linear comments.
15. Raw transcripts ✕ Linear.

## 28. Recommended master diagram

Create a 16:9 landscape diagram with three panels.

### 28.1 Panel A — Ecosystem and ownership

Top row:

```text
Commander → Executor Agent ↔ Advisor Agent
```

Middle:

```text
zeos → lane CLI
```

Right-side external sources of truth:

```text
Linear | GitHub | 1Password | Overseer
```

Bottom-right legacy:

```text
Vantage Archive
```

Add ownership captions:

```text
Planning = Linear
Code/CI/Review = GitHub
Secrets = 1Password
Local ownership = lane
Memory/Journals = zeos
Liveness = Overseer
Archive = Vantage
```

### 28.2 Panel B — Local `lane` engine

Large navy boundary:

```text
lane — Offline Local Coordination Core
```

Internal flow:

```text
CLI
 → Validation
 → LaneRoot Resolution
 → Path Guards
 → Lane Mutex
 → Existing-Claim Decision
 → Target Mutex + Overlap Scan
 → Atomic Mutation
 → Audit + Reconciliation
 → Output
```

Persistent-state row:

```text
locks/ | mutexes/ | audit.log | audit.recovered/ | config.toml
```

### 28.3 Panel C — End-to-end work lifecycle

```text
Linear Issue
 → Plan
 → Branch + Worktree
 → Local Claim
 → Executor + Advisor Work
 → Renew
 → GitHub PR + CI
 → Handoff or Close
 → Release
```

Use solid arrows only for currently implemented claim/status/renew/release/audit
operations. Use dashed arrows for Linear, live Git, pairing, tmux, 1Password, and
closeout.

## 29. Recommended secondary diagrams

Create four supporting diagrams in addition to the master architecture:

1. **Claim sequence diagram**
   - Actor, CLI, guards, mutexes, claim file, audit log.
2. **Crash-recovery state diagram**
   - Intent, mutation, completion, dangling classifications.
3. **Source-of-truth diagram**
   - Linear, GitHub, 1Password, lane, zeos, Overseer, Vantage.
4. **Migration timeline**
   - Vantage archive → Linear/1Password/lane → Vantage exit.

## 30. Suggested diagram title and palette

Title:

> `lane`: Linear-First, Offline Agent-Work Orchestration

Subtitle:

> Local claims and audit without network dependencies; external systems retain their own sources of truth.

Palette:

1. `lane` boundary: dark navy.
2. Implemented core: green.
3. Planned integration: blue.
4. External sources of truth: gold.
5. Local persistent state: purple.
6. Legacy Vantage: gray.
7. Security failures and forbidden edges: red.
8. Humans and agents: light cyan.

## 31. Authoritative references

1. [`lane_SPEC.md`](./lane_SPEC.md) — original Slice 0a target architecture.
2. [`AGENTS.md`](../AGENTS.md) — current durable safety and operating rules.
3. [`VANTAGE_EXIT_AND_MIGRATION_INVENTORY.md`](./VANTAGE_EXIT_AND_MIGRATION_INVENTORY.md) — migration and retirement model.
4. `src/lock/` — current Slice 2 as-built locking and audit implementation.
5. `src/board/` — current Slice 1 board implementation.

This document is the diagram-generation brief. It deliberately distinguishes current
implemented behavior from planned target-state behavior.
