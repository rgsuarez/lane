# lane — EXHAUSTIVE HANDOFF for the next session

> **Read this first.** It is a complete transfer of context from the sessions that built
> Slices 0a/1/2 of `lane`. The mission is unchanged and **not complete**: realize the full
> north star (a local app that replaces Vantage in daily agent-work orchestration). You are
> picking up at **Slice 3**. Nothing here is speculative unless marked "planned".

---

## 0. Boot sequence for you (the next agent)

1. You should be running with **cwd = `~/projects/lane`** (→ `/Users/richie/projects-local/lane`, a real dir on Mac SSD; `claude-yolo` works from here).
2. Read, in order: this file → `AGENTS.md` (standing orders) → `docs/lane_SPEC.md` → `docs/LANE_SYSTEM_DIAGRAM_SPEC.md` (accurate as-built+planned brief) → the master plan `~/.claude/plans/plan-mode-build-the-gleaming-donut.md` (the "gleaming-donut" plan; Slice 3 spec is §11 line 154 + the design in §9).
3. Confirm state: `git -C ~/projects/lane status --short --branch` (expect `## main...origin/main`, plus the untracked journal/handoff files); `git rev-parse main` = `8634f30…`.
4. Run the gates to confirm green baseline: `cargo fmt --check && cargo build && cargo test && cargo test --all-features && cargo clippy --all-targets -- -D warnings` (expect **117 passed**).
5. Do **not** start coding a new slice without a Commander-approved Execution-Mode prompt + (per house style) a Codex GO on the slice plan.

---

## 1. The goal (north star) — unchanged

Verbatim (plan §1 / `docs/lane_SPEC.md` §1):

> A **portable, Linear-first, offline-capable local agent-work orchestration app.** Linear is
> the planning source of truth (replacing Vantage's LOE model), GitHub is code/CI/review,
> 1Password is the secret provider (replacing the Vantage vault), and `lane` owns the
> machine-local logistics those three do not: **lane claims, worktree coordination, active
> session/heartbeat visibility, advisor/executor pairing, audit history, Git discipline,
> gates, and closeout** — fully offline. **Vantage is removed from daily agent-work
> orchestration**, reduced to migration source + read-only archive + reference.

Two objectives: **(a)** build the local logistics engine; **(b)** strategically **retire Vantage
from daily use**. We have done part of (a) and **none** of (b).

---

## 2. Current state (as-built)

- **Repo:** `rgsuarez/lane` (PRIVATE GitHub). Local: `/Users/richie/projects-local/lane` (`~/projects/lane` is a symlink → `~/projects-local`).
- **Branch/HEAD:** `main` = `origin/main` = **`8634f30`**. Other local branches: `slice-1-board` (`f73340b`), `slice-2-locking` (`e9f30b1`). `origin` → `https://github.com/rgsuarez/lane.git`.
- **Commits:** `d0e2482` docs (Slice 0a) → `f73340b` board (Slice 1) → `e9f30b1` locking core (Slice 2) → `0dc0cd9` diagram spec → `8634f30` description fix.
- **Tests:** **117 passing**, 0 failing. fmt/clippy clean. `cargo test --all-features` also 117.
- **Language/runtime:** Rust, edition 2021, `rust-toolchain.toml` pins channel `stable`. Single native CLI binary (`lane`), lib + thin bin. **One** non-dev dependency: `unicode-normalization` (NFC for target overlap). dev-deps: `tempfile`, `assert_cmd`, `predicates`.
- **What works today:** the offline locking core — `claim / renew / release / status / list / board`, with audit, target-overlap safety, TTL leases, crash/race safety, and a security-hardened fail-closed posture. **No network, no daemon, no DB, no async** (enforced by `no_network_guard`).

---

## 3. Slice roadmap — done vs not

| Slice | Scope | Status |
|---|---|---|
| 0a | Spec + Vantage-exit/migration inventory (docs) | ✅ `d0e2482` |
| 0b | Doctrine edits (global `~/.claude/CLAUDE.md`: LOE-filing→Linear, secrets→1Password) | ❌ TODO (gated; ~30-day compat window) |
| 1 | Read-only `board` aggregator | ✅ `f73340b` — Linear/worktree/overseer providers are **fixtures/stubs**; only claims are real |
| 2 | Offline locking core | ✅✅ `e9f30b1` — done beyond spec (2 adversarial review passes; 117 tests) |
| 3 | Lifecycle + pairing: `start/pair/handoff/close`, tmux, **git worktree/branch automation**, zeos skill-wrap | ❌ **YOU START HERE** |
| 4 | Linear read adapter (+ gated writes) + 1Password (`op`) | ❌ TODO |
| 5 | Vantage-LOE→Linear migration + installer | ❌ TODO |

**Vantage exit criteria (plan §12) — 0 of 6 met:** Linear not live (1), 1Password not live (2), local app does claims but not sessions/pairing (3), GitHub→Linear auto-status unverified (4), no migration archive (5), cost/cron/topology undispositioned (6). **Vantage is still the daily runtime.** Note the global `~/.claude/CLAUDE.md` still mandates Vantage LOE filing every session (Slice 0b not done) — that doctrine governs how we *track* work right now.

---

## 4. Architecture as-built (every module)

Single crate. `src/lib.rs` exposes `pub mod {board, cli, error, lock, model, output}`.

### `src/error.rs`
- `LaneError` (the **single** authoritative error type): `Refused(RefusedReason)`, `Identity(String)`, `NonLocalRoot(String)`, `Malformed { path, detail }`, `Io(io::Error)`.
- `RefusedReason`: `ActiveHeld, NotOwner, TargetOverlap, MutexBusy, Expired, NotHeld`.
- `Reason` (closed, serde snake_case — the 10 JSON reasons): `active_held, not_owner, target_overlap, mutex_busy, expired, not_held, identity, malformed, non_local_root, io`.
- `LaneError::exit_code()` → `1` for `Refused(_)`, `2` for everything else. `LaneError::reason()` → the matching `Reason`. **This is the sole exit-code/reason mapping — do not duplicate it.** `impl Error + From<io::Error>`.

### `src/lock/mod.rs` (the hub)
- **`FsOps` trait + `StdFs`** — the injectable filesystem seam: `rename`, `hard_link`, `remove_file`, `device_of`, `owner_uid`. Production = `StdFs` (plain std). Tests inject faults.
- Param/result structs: `ClaimParams`, `RenewParams`, `ReleaseParams`, `ClaimSuccess`, `RenewSuccess`, `ReleaseSuccess`, `StatusData`.
- Validation: `validate_name` (`^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`, reject `.`/`..`), `validate_instance` (non-empty, ≤128, no control chars), `validate_ttl` (finite, >0, ≤720), `validate_note` (≤1024). `DEFAULT_TTL_HOURS=12`, `MAX_TTL_HOURS=720`. `ttl_to_duration`.
- Shared write helper `write_temp` (complete 0600 temp + fsync); `scan_overlap` (sibling target-overlap under target mutex; guarded reads; malformed sibling fails closed).
- **Reconciliation:** `reconcile_for_mutation` (blocks on `indeterminate`), `reconcile_for_status` (warns, never blocks), `describe_dangling`, `combine_warnings`.
- **Refusal audit:** `audit_refusal` (best-effort `claim_refused`/`malformed` event; **returns `Some(warning)` iff the append failed** — never changes the primary error).
- **Output:** `Outcome` enum (`ok/refused/released/not_held/error`), `VerbData` (untagged), `Envelope` (`schema_version: 1`), `CommandError { error: LaneError, audit_warning: Option<String> }` (carries a refusal-audit warning to `emit` without altering exit/reason; `From<LaneError>`), `emit` (one envelope per post-parse exit path; JSON or human), `error_stderr` (human error+warning lines), `human_success`.
- **CLI runners:** `run_claim/run_renew/run_release/run_status/run_list` (+ `_at(now)` variants for tests). `read_liveness()` → `Unknown` (Slice 2 has no liveness).
- Test-only hook: `test_hold_after_lane_mutex()` (`#[cfg(debug_assertions)]`, sleeps `$LANE_TEST_HOLD_LANE_MUTEX_MS` while holding the lane mutex — used by concurrency tests).

### `src/lock/paths.rs` (the trust anchor + object guards)
- `LaneRoot { path, canon_existing, expected_uid }`. `resolve(raw, home, fs)`: require absolute → canonicalize the **longest existing ancestor** (first-run safe) → **local-FS check** (resolved device must equal `$HOME`'s device, via `fs.device_of`; else `NonLocalRoot`) → `expected_uid` = `$HOME`'s uid. Layout builders: `repo_dir/locks_dir/mutexes_dir/lock_path/lane_mutex_path/target_mutex_path/audit_path/temp_path`.
- `ensure_write_dirs` (write path): creates `<repo>/`, `locks/`, `mutexes/` one component at a time via `ensure_dir`/`validate_existing_dir`, tolerating first-run `AlreadyExists` races; symlink/non-dir/wrong-owner → fail closed; same-owner wrong mode repaired to `0700`. `ensure_dir_guarded` (used by audit recovery).
- `open_or_create_writer(path, mode, fs, uid)` — **the object guard for files**: absent → `create_new` (mode); existing → `symlink_metadata` (reject symlink/non-regular/wrong-owner) → `open` → `fstat` and verify `(dev, ino)` matches the lstat'd pair (std-only TOCTOU guard; **no `O_NOFOLLOW`/libc**) → repair same-owner wrong mode. `DIR_MODE=0700`.
- **`guard_dir_chain(root, dir, fs, uid) -> Presence` (Fix A)** — the **single** read-only ancestor-chain guard: for every existing component strictly beneath the canonical root up to `dir`, non-following `symlink_metadata`, require real same-owner directory; reject interior symlink/non-dir/wrong-owner or out-of-root path (`Identity`); `Absent` at first missing component. `Presence { Present, Absent }`. **Never use `Path::is_dir`/`exists`/`metadata`/`canonicalize` as a trust decision on a state component.**
- `resolve_raw_root` (flag/`$LANE_ROOT`/`$HOME`-default → absolute).

### `src/lock/target.rs`
- `Target { normalized, segments }`. `resolve(raw, home, lane_root)`: expand `~`/`$HOME`; require absolute; reject `.`/`..`; `canonicalize_conservative` (longest existing ancestor realpath + NFC + ASCII-lowercase the unresolved tail + **reject non-ASCII unresolved tail in v1**); reject `/`, exactly `$HOME`, and any path that contains or is contained by `LANE_ROOT` (both directions). `from_normalized` (rebuild from a stored sibling target). `overlaps` = equal OR ancestor/descendant on segment vectors.

### `src/lock/mutex.rs`
- `LaneMutex` (RAII) — exclusive **OS advisory lock** via `std::fs::File::try_lock` (stable ≥1.89). Jittered exponential backoff, ≤3s → `MutexBusy` (exit 1). FD held for the critical section; **drop/process-death closes the fd → kernel releases** (no stale lease, no PID/timestamp body). Mutex files are persistent lock targets (never unlinked). Opened via `open_or_create_writer`. **Lock order: lane → target** (deadlock-free).

### `src/lock/record.rs` (the single read path)
- `read_guarded(path, root, expected_uid, fs)` — chain-guard `path.parent()` then leaf-guard the file (`symlink_metadata` reject symlink/non-regular/wrong-owner → open → `fstat` dev+ino TOCTOU → read). Transient `NotFound`/missing ancestor → `Ok(None)`. **No caller may `read_to_string` claim state directly.**
- `read_claim` = `read_guarded` + parse + `check_identity` (`repo`==grandparent dir, `lane`==filename stem). Used by board/list/status/overlap/classify/audit-read.

### `src/lock/audit.rs`
- `AuditEvent` fields: `ts, op_id, op, event, repo, lane, instance, outcome, forced, prior_instance, was_malformed, reason, target, ttl_hours, recovered_path, recovered_bytes`. **Never** logs `note`/secrets/PII. `op` ∈ `{takeover, release}` (intent/completion discriminator for reconciliation).
- `AuditEventKind`: `claim, claim_refused, renew, release, intent, completion, takeover, malformed, audit_recovery`. `AuditOutcome`: `ok, refused, error`. `next_op_id()` = `<unix_nanos>-<pid>-<atomic-counter>`.
- `AuditSink` trait + `StdAuditSink` (guarded append: `open_or_create_writer` + `flock` + seek-to-end + write line + optional fsync). Destructive ops write an **fsync'd `intent` before** the mutation, `completion` after.
- `recover_if_needed` — under the audit lock: object-guard the log; **validate EVERY complete newline-terminated record** (any malformed complete record → fail closed, exit 2); only a final non-newline **fragment** is quarantined to `audit.recovered/<op_id>.frag` (guarded, fsync'd) + truncated + an `audit_recovery` event appended. `read_validated_events` (read-only stream). `dangling_intents` (intents with no matching completion `op_id`). `classify_intent` → `IntentDisposition { Applied, NotApplied, Indeterminate }` (takeover: lock==new→applied, ==prior→not_applied, else→indeterminate; release: absent→applied, ==owner→not_applied, else→indeterminate).

### `src/lock/claim.rs`
- `claim_core` sequence: validate → `ensure_write_dirs` → resolve target → `recover_if_needed` → acquire lane mutex → `test_hold` hook → `reconcile_for_mutation` (indeterminate blocks) → `classify_existing` (decision table) → if targeted: acquire target mutex + `scan_overlap` → build record → `write_temp` → **free-lane: exclusive `hard_link`** (fails if dest exists) / **takeover: `intent{op:takeover}` fsync → `rename`-over → `completion{op:takeover}`**. Refusal/malformed → `audit_refusal` + `CommandError`. Post-mutation audit failure → success + `audit_warning`.
- `classify_existing` (guarded; decision table): absent→Create; active+no-force→`active_held`; active+force→takeover; expired→takeover (no force needed); malformed *regular*+no-force→`malformed`; malformed *regular*+force→takeover; identity-inconsistent regular without force→`identity`; **symlink/non-regular/wrong-owner → `identity` even under `--force`** (force never bypasses the object guard); sibling target overlap → `target_overlap` even under force.

### `src/lock/renew_release.rs`
- `renew_core` (owner-only, **no `--force`**): missing→`not_held`; not owner→`not_owner`; `now>=expires_at`→`expired` (cannot revive a lapsed lease); targeted → re-acquire target mutex + re-scan overlap; `rename`-over with updated `updated_at/expires_at/ttl_hours`; terminal `renew` event.
- `release_core` (owner-only, no force): absent→`not_held` exit 0; not owner→`not_owner` exit 1; `intent{op:release}` fsync → `remove_file` → `completion{op:release}`. No forced release.
- `status_core` (read-only): guarded read + `classify_stale` + `reconcile_for_status` warning. `list_core` (read-only): guarded descent (`guard_dir_chain` per repo/locks; reject interior symlink) + per-claim guarded read; fails closed on malformed.

### `src/board/` (Slice 1)
- `mod.rs`: `assemble`, `classify_stale` (expired if `now>=expires_at`; orphaned if NotLive; possibly_stale if idle >3h; else active), `BoardInputs` (incl. `expected_uid`), `run_board` (resolves `LaneRoot`). `claims.rs`: `read_claims` via the guarded reader + `guard_dir_chain` descent. `worktrees.rs`/`linear.rs`/`liveness.rs`: provider traits + `Empty/Fixture/Stub` impls — **NO real `git`/network/overseer** (the future-slice swap point). `output/{mod,human,json}`: `Board` (schema_version `0`, unstable).

### `src/cli.rs`, `src/main.rs`, `src/model.rs`
- `cli.rs`: clap; `Command { Board, Claim, Renew, Release, Status, List }` + arg structs; `resolve_lane_root`/`resolve_lane_root_from` (board's absolute-only resolver). `--force` exists **only** on `claim`. `--instance` required for claim/renew/release.
- `main.rs`: parse → dispatch → process exit code (board: `anyhow` + downcast to `LaneError`; lock verbs: runners return `i32`).
- `model.rs`: `ClaimRecord` (20 fields: `schema_version, lane, repo, instance, pid, target, target_normalized, note, claimed_at, updated_at, expires_at, ttl_hours, linear_key, branch, role, pr_url, gate, plan_path, claim_status, session_ref`); `Provenance/Provenanced`; `Role/ClaimStatus/Gate`; `Liveness`; `StaleState`; `SourceKind/SourceFreshness`; `WorktreeInfo`; `LinearIssueLite`; `BoardRow`; `Board`. Slice 2 populates the first 12 fields; the rest are reserved for Slices 3–4.

---

## 5. CLI contract

```
lane claim   <lane> --repo <repo> [--target <abs>] [--ttl-hours <h>] [--note <s>] [--force] [--json] [--lane-root <abs>] [--instance <id>]
lane renew   <lane> --repo <repo> [--ttl-hours <h>] [--json] [--lane-root <abs>] [--instance <id>]   (owner-only; no --force)
lane release <lane> --repo <repo> [--json] [--lane-root <abs>] [--instance <id>]                      (owner-only; no --force)
lane status  <lane> --repo <repo> [--json] [--lane-root <abs>]                                        (read-only)
lane list    [--repo <repo>] [--json] [--lane-root <abs>]                                             (read-only)
lane board   [--repo <repo>] [--json] [--lane-root <abs>] [--linear-fixture <f>] [--worktree-fixture <f>]
```
- **Env:** `LANE_ROOT` (default `~/.lane`, **must be on the same FS device as `$HOME`**), `LANE_INSTANCE` (required for claim/renew/release; never guessed).
- **Exit codes:** `0` ok (incl. release/status of an absent lane); `1` refused; `2` identity/malformed/io/non_local_root (+ clap usage errors, human-only stderr).
- **JSON envelope** (`--json`, exactly one per post-parse exit path): `{schema_version:1, ok, verb, repo?, lane?, outcome, reason?, audit_warning?, data}`. `data` is `null` on refused/error. `board` uses its own `Board` JSON (schema_version `0`).
- On-disk layout: `~/.lane/<repo>/{locks/<lane>.lock, mutexes/<lane>.mutex, mutexes/target.mutex, audit.log, audit.recovered/<op_id>.frag}`. Dirs `0700`, files `0600`.

---

## 6. Security/safety invariants (do not weaken — see `AGENTS.md`)
1. Exactly one active owner per lane (OS advisory lock; crash-releases).
2. No overlapping active targets in a repo (canonical ancestor/descendant).
3. Never auto-steal an active claim (takeover only when expired or `--force`).
4. `--force` only on `claim`; **never** bypasses the object guard or target overlap.
5. All reads through `record::read_guarded` → `guard_dir_chain` (interior chain) + leaf `(dev,ino)` guard; no direct `read_to_string` of state; no symlink-following trust decisions.
6. Atomic-visible writes: free-lane = exclusive `hard_link`; takeover/renew = `rename`-over (never unlink-first).
7. Write-ahead audit: fsync `intent` before destructive mutation; `completion` after; post-mutation audit failure → exit 0 + `audit_warning`. Full-stream validation; fragment-only recovery; **never fabricate a completion**.
8. Dangling-intent reconciliation: indeterminate **blocks** mutations (`Identity`, exit 2); applied/not-applied → warning.
9. A refusal/malformed audit failure never changes the primary exit code/reason (surfaced as `audit_warning`/stderr).
10. Std-only, offline, no network/async/DB/daemon in the core (`no_network_guard` forbids the crates by name).

---

## 7. Test suite (117) + infra
- **Lib unit** (`#[cfg(test)]`): `error`, `target` (overlap/canonicalization), `audit` (op_id, reconciliation/classify), `record` (guard/identity/symlink), `lock::mod` (error_stderr, CommandError, combine_warnings).
- **Integration** (`tests/`): `board_*` + `claim_identity` + `cli_board` + `lane_root` (Slice 1); `no_network_guard`; `lock_lifecycle`, `lock_refusals`, `lock_validation`, `lock_concurrency` (multi-process: exactly-one-winner, overlap race, expired-takeover race, mutex contention, **SIGKILL crash-release**, audit contention), `lock_core_faults` (FsOps/AuditSink injection: intent-fail/mutation-fail/completion-fail→audit_warning, recovery, expiry-boundary, read-no-mutate), `lock_fail_closed` (symlink/malformed/dangling-intent/refusal-audit), `lock_interior_guard` (Fix A interior symlinks), `lock_audit_warning` (Fix B).
- **`tests/common/mod.rs`** helpers: `temp_root()` (**`tempdir_in($HOME)`** so the device check passes — never plain `tempdir()` for write-path tests), `run()`/`spawn_holding()` (real binary via `CARGO_BIN_EXE_lane`), `FaultFs`/`FaultAudit` (the injectable seams). Write-path tests must keep `LANE_ROOT` under `$HOME`.

---

## 8. Review/remediation history (why the core is hardened)
- **Codex round-5 GO** on the Slice 2 *plan* before build.
- **Codex review Pass 1 → NO-GO**, 5 defects: read-only object-guard bypass (symlinked `.lock`), partial audit validation, no dangling-intent reconciliation, missing `claim_refused`/`malformed` events, audit/guard gaps. → Pass 1 remediation.
- **Codex review Pass 2 → 2 defects:** (A) interior state-dir symlinks bypass the leaf-only guard; (B) refusal audit failures silently discarded. → Pass 2 remediation (`guard_dir_chain` + `CommandError`/`audit_warning`). **Approved.**
- Codex review journal: `session-journals/codex-copilot/2026-06-18-lane-slice2-review-no-go.md` (lives in the **eleetai** repo's journals, not here).

---

## 9. Doctrine / how we work
- **Gate discipline:** each step (implement / commit / merge / publish / fix) is a **separately Commander-authorized Execution-Mode prompt** (bounded, opens with `EXECUTION MODE` + cites the plan path; explicit hard-stops; report-and-stop). House style runs slice plans through a **Codex GO** loop first (`/goplan`).
- **GitOps:** branch per slice from `main`; **explicit-path staging** (never `git add -A`); **stop before commit; commit only on explicit operator go**; **NO co-author trailer** (AGENTS.md + Commander directive — overrides the global `~/.claude/CLAUDE.md` co-author default); no push/PR/remote without authorization.
- **gh identities:** default account `eleetai`; `lane`'s owner is `rgsuarez`. To push to `rgsuarez/lane`: `gh auth switch --user rgsuarez` → push → `gh auth switch --user eleetai` (restore). Verify with `gh api user --jq .login`. Never expose credential values/paths/mechanics.
- **Dependencies:** single new dep policy (`unicode-normalization` only). New deps require Commander auth + an **embed-first** check (commodity infra is embedded, not rebuilt). New capabilities (git/Linear/1Password/tmux/overseer) go in **adapter modules outside the locking core** — the core stays offline/std-only.
- **Secrets:** 1Password (`op`) is the provider (planned). Never read secret files, never log secrets/refs/mechanics, never put secrets in lock files or audit.

---

## 10. Exact next steps (Slice 3 first)

**Slice 3 — lifecycle + pairing** (plan §11 line 154 + §9). This is what makes lane usable daily. Suggested decomposition (each its own gate):
1. **Worktree/branch automation** (a NEW adapter module, e.g. `src/git/` — outside the locking core): `git worktree add` + branch `richie/<team>-N-<slug>`; primary checkout read-only; dirty-tree precheck; NFS-target warning. Replace the board's fixture worktree provider with real `git worktree list`.
2. **`lane start`** — resolve issue → create branch+worktree → `claim` the lane+target → launch the executor session. **`lane pair`** — attach an advisor (`role=advisor`, same key, 7-var env + bootstrap injection, active-parent doctrine). **`lane handoff`** — flip `claim_status:handoff` + digest. **`lane close`** — drafted, operator-gated closeout.
3. **tmux** paired advisor/executor runtime (the 7-var session env contract is documented in the plan §6/§9 and the salvage matrix §8).
4. **zeos skill-wrap**: `skills/lane/SKILL.md` (skill-wraps-CLI pattern) so zeos invokes `lane` and consumes its JSON.
   - Keep all of this **outside** the locking core; the core's offline guarantee must hold. Add tests; keep the 117 green.

Then: **Slice 4** (Linear read adapter via GraphQL with the API key resolved from 1Password `op` at call time; gated Linear writes; 1Password integration per plan §6) → **Slice 0b** (doctrine cutover) → **Slice 5** (Vantage→Linear migration, archive-first, `vantage-loe:<id>` stamp; native-binary installer) → **meet the 6 Vantage exit criteria** and cut Vantage from daily orchestration. Cross-machine hard locking is a **later embed-first COA** (Postgres advisory locks / etcd / NATS — never hand-rolled).

---

## 11. Gotchas / landmines
- **Device check in tests:** write-path tests fail with `non_local_root` unless `LANE_ROOT` is on the same FS device as `$HOME`; use `tests/common::temp_root()` (`tempdir_in($HOME)`), not bare `tempdir()`.
- **`tmux paired-lane` ≠ this app.** The portfolio's "paired-lane" tmux discipline and the memory file `feedback-eleetai-lane-execution-discipline.md` are about a *different* "lane" concept — don't conflate with this repo.
- **No README/installer yet** (Slice 5). Fresh-mac install: Xcode CLT → rustup → `gh` + auth as `rgsuarez` → `gh repo clone rgsuarez/lane` → `cargo build && cargo test` → `cargo install --path . --locked` (→ `~/.cargo/bin/lane`). `cargo install` works despite `publish = false`/`version = 0.0.0`.
- **Binary is dynamically linked** (libSystem/CoreFoundation) — copy-install only to matching macOS arch; otherwise build on target.
- **MSRV:** language floor ≈ Rust 1.89 (`File::try_lock`); built/tested on 1.93; toolchain pins channel `stable`.
- **These handoff/journal files are untracked** (not on GitHub). To preserve cross-machine, commit + push them (push needs the `rgsuarez` identity dance).

---

## 12. References
- Master plan: `~/.claude/plans/plan-mode-build-the-gleaming-donut.md`
- In-repo: `AGENTS.md`, `docs/lane_SPEC.md`, `docs/VANTAGE_EXIT_AND_MIGRATION_INVENTORY.md`, `docs/LANE_SYSTEM_DIAGRAM_SPEC.md`
- Journal: `session-journals/2026-06-21-001-slice2-build-review-merge-publish.md`
- Project memory (auto-loads in a lane-cwd session): `~/.claude/projects/-Users-richie-projects-local-lane/memory/` (`project_lane_status.md`, `lane-next-steps.md`)

---

## 13. Ready-to-paste kickoff for the next session
> Read `~/projects/lane/session-journals/2026-06-21-002-NEXT-SESSION-HANDOFF.md` in full, then `AGENTS.md` and the gleaming-donut plan. Confirm baseline (`main` @ `8634f30`, 117 tests green). We are continuing toward the full north star — Vantage replacement — and are starting **Slice 3 (lifecycle + pairing + git-worktree automation + zeos skill-wrap)**. Propose a Slice 3 plan (Plan Mode), run it past Codex for GO, and only then request an Execution-Mode gate. Keep the locking core offline/std-only; new capabilities go in adapter modules outside it; no co-author trailer; explicit-path staging; commit only on my go.
