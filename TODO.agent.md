# Project activity watchdog

- [x] Track MCP activity per project and detect stale projects.
  - **Acceptance criteria**
    - Maintain one in-memory map keyed by project key/id whose value is the timestamp of the latest successfully received MCP tool call for that project.
    - A single background timer ticks every minute.
    - On each tick, entries older than five minutes are removed from the map.
    - After removal, an entry is recreated only when another MCP tool call for that project arrives.
    - For each evicted project, inspect its current plan. If any plan item is `pending` or `in_progress`, emit a conspicuous colored log containing project identity, current plan, and last-tool-call time.
    - Empty plans and fully completed plans are evicted without a stuck/halt warning.
    - Existing project/tool behavior and persisted plan semantics remain unchanged.
  - **Affected paths**
    - `src/audit.rs`: project timestamp map, race-safe stale eviction, stuck/halt console renderer/tests.
    - `src/tools/mod.rs`: refresh timestamp immediately after project resolution for native/content tools, upstream scope, and turn init.
    - `src/server.rs`: one cancellable one-minute watchdog task, five-minute stale threshold, plan inspection/logging, lifecycle/tests.
    - `TODO.agent.md`: execution evidence only; no public contract/config change planned.
  - **Risks**
    - Concurrent access/races between request handling and timer.
    - Holding locks across storage I/O or logging.
    - Updating activity for non-tool traffic or before project identity is resolved.
    - Timer lifecycle causing duplicate watchdogs or preventing clean shutdown/tests.
  - **Phases**
    1. **AUDIT** ✅ — `AgentHandler::run` / `run_content` and upstream `audited_call` converge on `AuditLogger::tool_started`; `chatgpt_turn_init` is separate and obtains its `ProjectContext` after `prepare_turn_initialize`. `Storage::plan_get` returns the canonical persisted plan. `server::run` already owns cancellable maintenance tasks. Baseline: `cargo test audit::tests:: --lib -j1` 22/22 and `cargo test server::tests:: --lib -j1` 6/6 in `docker.io/library/rust:latest` (first `rust:bookworm`/login-shell attempts failed before tests because `cargo` was absent from login-shell PATH).
    2. **AUDIT** ✅ — Timestamp must be refreshed after project identity is known but before capacity waits, so rejected/overloaded calls still count as activity. Stale removal must re-check the current entry under the DashMap entry lock to avoid deleting a concurrent refresh. Eviction returns owned data before SQLite plan reads/logging, so no map lock spans storage I/O. Warn only when a canonical plan contains `pending` or `in_progress`; no-plan/completed-only plans remain silent. Cancellation must stop and join the watchdog during server shutdown.
    3. **IMPLEMENT** ✅ — Added a dedicated `last_tool_calls` DashMap keyed by effective project key with monotonic `Instant` plus RFC3339 wall-clock timestamp. Refresh occurs after project resolution and before capacity waits in native `run`, `run_content`, upstream `tool_scope`, and the separate `chatgpt_turn_init` path.
    4. **IMPLEMENT** ✅ — Added one cancellable watchdog task owned by `server::run`: 60-second interval, skipped missed ticks, strict `idle > 300s` conditional eviction, then canonical `plan_get` outside the map lock. `pending` / `in_progress` plans emit `project_stuck`; no-plan/completed-only plans are silent. Shutdown joins the watchdog.
    5. **IMPLEMENT** ✅ — Added regression coverage for exact-five-minute boundary, refresh age reset, stale eviction, re-add only after a new tool call, native dispatch refresh, plan filtering, persisted warning payload, and colored project/timestamp/plan console rendering. Targeted post-change results: `audit::tests` 24/24, `server::tests` 7/7, `native_tool_run_records_project_last_tool_call` 1/1.
    6. **REVIEW/VERIFY** ✅ — Re-read `src/audit.rs`, `src/server.rs`, and `src/tools/mod.rs` in full. `srcwalk trace callers` confirms exactly four production refresh sites (`tool_scope`, `run`, `run_content`, `chatgpt_turn_init`) and the stale consumer is the server sweep. Conditional DashMap entry removal prevents a snapshot/remove race; storage I/O/logging occurs only after the entry is owned and removed. `srcwalk review` shows only the intended four changed paths and `git diff --check` is clean.
    7. **REVIEW/VERIFY** ✅ — Final container gate (`docker.io/library/rust:latest`): `rustup component add rustfmt clippy`, `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -j1 -- -D warnings`, `cargo test --all-targets --all-features -j1`, and `cargo build --bins --examples --all-features -j1` completed as one `&&` chain with exit 0. Library tests: 425 passed / 1 ignored live-Podman probe / 0 failed; main + all integration/example test targets shown by Cargo also passed. Final `srcwalk review` reports only the intended four paths; `git diff --check` exits 0.

- [x] Final integration verification.
  - **Depends on**: Project activity watchdog task.
  - **Acceptance criteria**
    - Combined final repository state satisfies all behavior above.
    - No required checks remain unexecuted; unrelated baseline failures, if any, are distinguished with evidence.
    - `TODO.agent.md` and persisted plan reflect the actual final state.
  - **Phases**
    1. **AUDIT** ✅ — Final behavior matches the request: effective-project-key map, refresh on MCP tool calls, one-minute sweep, strict older-than-five-minutes eviction, persisted-plan inspection, colored stuck/halt output, and re-add only on a later MCP tool call.
    2. **AUDIT** ✅ — Final call graph still has four production refresh boundaries and one watchdog consumer; `server::run` owns exactly one watchdog task and cancels/joins it during shutdown.
    3. **IMPLEMENT** ✅ — No integration defects were found, so no source changes were required after the full verification gate.
    4. **IMPLEMENT** ✅ — Not applicable: no source changes occurred after the full fmt/clippy/test/build gate, so no verification evidence was invalidated.
    5. **IMPLEMENT** ✅ — All task containers used `--rm`; final `podman ps -a --filter name=tmp-wdg3c7-` returned no matching resources. Final worktree contains only `src/audit.rs`, `src/server.rs`, `src/tools/mod.rs`, and this checklist.
    6. **REVIEW/VERIFY** ✅ — Re-read all modified source files in full, reviewed the complete source diff and final `srcwalk review`, and confirmed `git diff --check` exits 0 against the initially clean worktree.
    7. **REVIEW/VERIFY** ✅ — Integrated regression gate remains valid (exit 0); final container/resource check is empty and no task command session remains running.

# Test audit — native tool contract coverage

- [x] Prune redundant output-schema coverage at the native tool-contract owner.
  - **Acceptance criteria**
    - Apply `/shared/SKILL-TEST-AUDIT.md` in audit mode without broad speculative cleanup.
    - Remove only tests whose failure is already caught by a stronger test at the same owner boundary.
    - Preserve all independent public MCP schema, registry, protocol, security, platform, persistence, and lifecycle contracts.
    - Do not add or preserve production seams solely to support implementation-coupled tests.
  - **Candidate evidence**
    - Candidate: `tools::tests::every_public_tool_has_an_output_schema` in `src/tools/mod.rs`.
    - Actual failure detected: any `PUBLIC_TOOL_NAMES` route has `output_schema == None`.
    - Stronger keeper: `tools::tests::every_public_tool_has_object_input_and_output_contracts` iterates the same public routes and requires every output schema to resolve to `type == "object"`; a missing output schema produces `None` and fails the same assertion.
    - Owner: `AgentHandler::native_router` composes `NativeToolRegistry::build` and assigns `typed_output_schema`; no test-only production seam is involved.
    - History: the duplicate test dates to initial commit `be84038`; no dedicated bug-regression history was found.
    - Deletion unlocked: one redundant unit test only; no production/support deletion.
    - Risk: loss of the duplicate test's specialized failure message. Functional regression detection remains at the same owner boundary.
    - Focused validation: run both candidate and keeper before deletion; after deletion run the keeper plus the full `tools::tests` owner suite.
  - **Retained false positive**
    - `windows_taskkill_program_for_test` is a test-only seam, but it currently supports a Windows security/platform invariant that `taskkill.exe` is launched by absolute System32 path. Native Windows runtime proof is unavailable locally, so this audit will not weaken or rewrite that contract speculatively.
  - **Affected paths**
    - `src/tools/mod.rs`: delete only the redundant output-schema-presence test.
    - `TODO.agent.md`: durable audit evidence and verification results.
  - **Risks**
    - Accidentally dropping the only proof that all public tools expose an output schema.
    - Expanding a narrow audit into unrelated test cleanup.
    - Treating cross-compile evidence as native Windows runtime proof.
  - **Phases**
    1. **AUDIT** ✅ — Read the requested skill completely from the available path `/shared/SKILL-TEST-AUDIT.md`; starting worktree was clean; persisted project plan was empty; root instructions and `srcwalk guide` were consumed.
    2. **AUDIT** ✅ — Inspected the native registry/schema owner and overlapping tests. Baseline container proof: candidate 1/1 pass and stronger keeper 1/1 pass in `docker.io/library/rust:latest`. Candidate evidence above satisfies the skill's deletion fields.
    3. **IMPLEMENT** ✅ — Deleted only `every_public_tool_has_an_output_schema` from `src/tools/mod.rs`.
    4. **IMPLEMENT** ✅ — Preserved all neighboring contract tests and production schema/registry code unchanged; current diff has no production-code hunk.
    5. **IMPLEMENT** ✅ — `srcwalk review` shows the only source hunk is the removed test; no production seam or support helper becomes unused because the candidate exercised `AgentHandler::native_router` directly.
    6. **REVIEW/VERIFY** ✅ — Post-edit container proof: stronger keeper 1/1 passed, then full `tools::tests` owner suite passed 44/44 with 0 failures.
    7. **REVIEW/VERIFY** ✅ — Final `tmp-ta83-gate` container exited 0 after `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -j1 -- -D warnings`, `cargo test --all-targets --all-features -j1`, and `cargo build --bins --examples --all-features -j1`. Library result: 424 passed, 1 intentionally ignored live-Podman probe, 0 failed; all integration/main/example targets shown by Cargo passed. Host `git diff --check` also exited 0.
    8. **REVIEW/VERIFY** ✅ — No repository review helper exists. Per skill fallback, performed a separate fresh review pass over the complete final diff for correctness, lost-contract, false-positive, vacuous-test, source-trust, validation-gap, and cleanup risks; no actionable finding. This was not independently executed by a second model/process.
    9. **REVIEW/VERIFY** ✅ — `git diff --numstat`: `src/tools/mod.rs` 0 additions / 15 deletions (test-only), `TODO.agent.md` metadata only. Cleanup block for prefix `tmp-ta83-` completed and confirmed zero remaining task containers; no task-owned volumes/networks/images were created.

- [x] Final integration verification for test audit.
  - **Depends on**: redundant output-schema coverage task.
  - **Acceptance criteria**
    - Final repository state retains one strong owner-boundary proof for every public tool's object input/output contract.
    - No audit edit weakens unrelated tests or production behavior.
    - Required checks are bound to the final worktree state and no task-owned process/container/resource remains.
  - **Phases**
    1. **AUDIT** ✅ — Final ledger satisfies the retention bar: one same-owner duplicate was removed; the stronger public object-schema keeper remains; the Windows taskkill test seam was retained because it protects a distinct platform/security invariant that was not safely replaceable without native Windows runtime proof.
    2. **AUDIT** ✅ — Re-read the surviving keeper after all edits: it still iterates every `PUBLIC_TOOL_NAMES` route returned by `AgentHandler::native_router` and fails when either input/output contract does not resolve to an object. Registry/schema production code remains untouched.
    3. **IMPLEMENT** ✅ — Final review found no in-scope correction beyond the single intended deletion, so no late source edit was required.
    4. **IMPLEMENT** ✅ — Unrelated production and test behavior is untouched; final worktree contains only `src/tools/mod.rs` plus this audit checklist.
    5. **IMPLEMENT** ✅ — Not applicable after the full gate: no Rust source changed after that gate, so its evidence was not invalidated by later checklist-only edits.
    6. **REVIEW/VERIFY** ✅ — Complete final diff from the clean starting worktree is one 15-line unit-test deletion plus audit metadata; `git diff --check` exits 0.
    7. **REVIEW/VERIFY** ✅ — Targeted post-edit proof remains keeper 1/1 and `tools::tests` 44/44. Broader final gate exits 0: fmt, clippy with `-D warnings`, all-target/all-feature tests (library 424 passed / 1 intentionally ignored / 0 failed plus all shown integration/main/example targets), and bins/examples build.
    8. **REVIEW/VERIFY** ✅ — Separate same-process review fallback found no correctness, lost-contract, vacuous-test, source-trust, validation-gap, or cleanup finding; no repository review helper or permitted second-model/subagent review was used.
    9. **REVIEW/VERIFY** ✅ — Final pre-reconciliation status is only `TODO.agent.md` and `src/tools/mod.rs` modified; task prefix `tmp-ta83-` was cleaned to zero remaining containers. Persisted plan and project-modification handoff are reconciled immediately after this checklist readback.

# Watchdog environment override (2026-10-03)

- [x] Honor `CODEXBRIDGE_INTERRUPT_IGNORE=plan` for stale-project warnings.
  - Acceptance: exact env value `plan` emits `project_stuck` for missing, empty, and completed plans; unset/other values retain pending/in-progress filtering; idle eviction and payload remain unchanged.
  - Affected: `src/server.rs` and this checklist. Risk: accidental broad warning behavior or test env races; use injected boolean in the testable inspection boundary.
  - AUDIT (2): Existing sweep reads canonical persisted plan after eviction; no-plan and completed-only previously silent. Override is read at sweep time and only changes emission filtering.
  - IMPLEMENT (3): Added exact-value environment check at sweep and passed override into inspection; preserved the original predicate without override. Added regression for all three formerly silent plan states.
  - REVIEW/VERIFY (4): complete: cargo fmt --check, clippy all-targets/features -D warnings, full test suite (426 library tests), build bins/examples passed in ephemeral Rust containers.
- [x] Final integration for watchdog override.
  - AUDIT (2): Confirmed original five-minute threshold and refresh paths unchanged in reviewed diff.
  - IMPLEMENT (3): No test/build failures required corrections.
  - REVIEW/VERIFY (4): reviewed srcwalk diff; full fmt/clippy/test gate and separate fmt/build gate exited 0; git diff --check clean; temporary containers exited with --rm.

# Exec default timeout (2026-10-03)

- [x] Raise the built-in `EXEC_DEFAULT_TIMEOUT_MS` default to five minutes.
  - Acceptance: change only the built-in default from `120_000` ms to `300_000` ms; preserve env override precedence, `EXEC_MAX_TIMEOUT_MS=3_600_000`, validation, and process timeout semantics.
  - Affected: `src/config.rs` and this checklist only. Risk is limited to unintended adjacent config changes.
  - AUDIT (2): Current source defines the built-in at `ConfigBuilder::build`; `EXEC_DEFAULT_TIMEOUT_MS` env values still override it, and the existing max/default validation is separate.
  - AUDIT (2): Starting worktree is clean; no default-value-specific test exists, so focused config tests plus source/diff review are sufficient for this one-literal behavior change.
  - IMPLEMENT (3): Changed only the built-in `EXEC_DEFAULT_TIMEOUT_MS` fallback in `ConfigBuilder::build` from `120_000` to `300_000` ms.
  - IMPLEMENT (3): preserve `EXEC_MAX_TIMEOUT_MS` and all timeout enforcement logic unchanged.
  - IMPLEMENT (3): do not add unrelated source, test, or configuration changes.
  - REVIEW/VERIFY (4): Re-read the changed config region; `srcwalk review` reports one changed production symbol (`ConfigBuilder::build`) and no other source hunk.
  - REVIEW/VERIFY (4): Ephemeral `docker.io/library/rust:latest` container ran `cargo test config::tests:: --lib -j1`: 20 passed, 0 failed.
  - REVIEW/VERIFY (4): The same container ran `cargo fmt --all --check` successfully before the focused tests; final diff/whitespace check is part of integration below.
  - REVIEW/VERIFY (4): Functional acceptance is satisfied; final checklist/plan reconciliation proceeds in the integration task.

- [x] Final integration for exec default timeout.
  - Depends on: five-minute default task.
  - Acceptance: final worktree contains exactly the requested functional change, required focused checks pass, and no task-owned process/container remains.
  - AUDIT (2): Final combined-state review shows only `TODO.agent.md` and `src/config.rs` modified; the production diff is exactly one literal replacement at `ConfigBuilder::build`.
  - AUDIT (2): `EXEC_MAX_TIMEOUT_MS` remains `3_600_000`; env/override precedence, validation, and process timeout enforcement are unchanged.
  - IMPLEMENT (3): No correction was required after review or focused tests.
  - IMPLEMENT (3): No extra functional changes were added; no tests or other source files were modified.
  - IMPLEMENT (3): Cleanup block for prefix `tmp-tmo5-` completed; final container listing is empty and no task-owned volume/network/image was created.
  - REVIEW/VERIFY (4): Re-read all of `src/config.rs` in bounded source ranges and reviewed the final `srcwalk review`; no adjacent behavior change found.
  - REVIEW/VERIFY (4): Final focused evidence remains `cargo fmt --all --check` plus `cargo test config::tests:: --lib -j1` (20 passed, 0 failed); `git diff --check` exits 0.
  - REVIEW/VERIFY (4): Persisted plan/handoff is reconciled after this checklist update.
  - REVIEW/VERIFY (4): Completion readback follows immediately before final response.

# Exec default timeout — 30 minutes (2026-10-03)

- [x] Raise the built-in `EXEC_DEFAULT_TIMEOUT_MS` default from five to thirty minutes.
  - Acceptance: change only the built-in fallback from `300_000` ms to `1_800_000` ms; preserve env override precedence, `EXEC_MAX_TIMEOUT_MS=3_600_000`, validation, and process timeout semantics.
  - Affected: `src/config.rs` and this checklist only. Risk: unintended adjacent config edits.
  - AUDIT (2): Current source still has the prior `300_000` ms fallback in `ConfigBuilder::build`; max timeout remains `3_600_000` ms.
  - AUDIT (2): Starting worktree contains only the prior timeout/checklist changes from the immediately preceding task; no unrelated files are dirty.
  - IMPLEMENT (3): Changed only the built-in fallback in `ConfigBuilder::build` from `300_000` ms to `1_800_000` ms.
  - IMPLEMENT (3): preserve max timeout and environment/override behavior unchanged.
  - IMPLEMENT (3): make no unrelated source/test/config changes.
  - REVIEW/VERIFY (4): Re-read the changed config region; `srcwalk review` reports the same single production symbol/hunk and `EXEC_MAX_TIMEOUT_MS` remains `3_600_000` ms.
  - REVIEW/VERIFY (4): Ephemeral Rust container ran `cargo fmt --all --check` and `cargo test config::tests:: --lib -j1`; 20 tests passed, 0 failed.
  - REVIEW/VERIFY (4): `git diff --check` passes; functional acceptance is satisfied and final reconciliation proceeds below.

- [x] Final integration for 30-minute exec default timeout.
  - Depends on: thirty-minute default task.
  - Acceptance: final production diff reflects the requested 30-minute default only; focused checks pass; no task-owned process/container remains.
  - AUDIT (2): Final combined-state review shows only `TODO.agent.md` and `src/config.rs` modified; the production diff is exactly one literal replacement from HEAD (`120_000` to `1_800_000`).
  - AUDIT (2): `EXEC_MAX_TIMEOUT_MS` remains `3_600_000`; environment/override precedence, validation, diagnostic reporting, and process timeout semantics are unchanged.
  - IMPLEMENT (3): No correction was required after final review or focused tests.
  - IMPLEMENT (3): No extra functional changes or tests were added.
  - IMPLEMENT (3): Cleanup block for prefix `tmp-tmo30-` completed; final container listing is empty and no task-owned volumes/networks/images remain.
  - REVIEW/VERIFY (4): Re-read `src/config.rs` across the complete file after the final source edit and reviewed `srcwalk review`; no adjacent behavior change found.
  - REVIEW/VERIFY (4): `cargo fmt --all --check` passed; `cargo test config::tests:: --lib -j1` passed 20/20; final `git diff --check` passed.
  - REVIEW/VERIFY (4): Persisted plan/handoff reconciliation and completion readback follow immediately.

# write_stdin poll/yield defaults (2026-10-03)

- [x] Adjust write_stdin yield ceilings and polling guidance.
  - Acceptance: keep `MIN_YIELD_MS=250`; set `MAX_YIELD_MS=120_000`; set `MAX_POLL_YIELD_MS=40_000`; keep `MAX_INITIAL_YIELD_MS=20_000` unchanged; update the public `write_stdin` description to explicitly prohibit using shell/bash `sleep` instead of `write_stdin` wait/polling.
  - Affected: `src/tools/process.rs`, focused description regression in `src/tools/mod.rs`, and this checklist. Preserve process lifetime timeout semantics and all unrelated tool contracts.
  - AUDIT (2): Current constants are `250`, `30_000`, `20_000`, with poll max aliased to initial max; existing tests assert 30s/20s and must be updated coherently.
  - AUDIT (2): Current public `write_stdin` description explains repeated polling but does not explicitly forbid shell sleep; starting worktree is clean.
  - IMPLEMENT (3): Set `MAX_YIELD_MS=120_000` and `MAX_POLL_YIELD_MS=40_000`; kept `MIN_YIELD_MS=250` and `MAX_INITIAL_YIELD_MS=20_000`; updated the adjacent poll-bound comment.
  - IMPLEMENT (3): Updated the public `write_stdin` description to explicitly forbid shell/bash sleep as a wait/poll substitute and added a direct native-router description regression assertion.
  - IMPLEMENT (3): preserve initial exec yield cap, process execution deadlines, input/signal/replay semantics, and unrelated public schemas.
  - REVIEW/VERIFY (4): Re-read both modified Rust source files across their complete contents (including the previously truncated `src/tools/mod.rs` window) and reviewed the changed regions plus `srcwalk review`; no adjacent behavior change found.
  - REVIEW/VERIFY (4): Ephemeral `rust:latest` container: `cargo fmt --all --check` passed; process tests passed 57/57 runnable with 1 intentional live-Podman ignore; write_stdin description regression passed; public-description bound regression passed; `cargo clippy --lib --all-features -- -D warnings` passed.
  - REVIEW/VERIFY (4): `git diff --check` passed before verification; final diff/cleanup/plan reconciliation proceeds in integration below.

- [x] Final integration for write_stdin poll/yield defaults.
  - Depends on: yield/description task.
  - Acceptance: final diff implements only the requested limits/guidance plus focused tests; all targeted checks pass; no task-owned process/container remains.
  - AUDIT (2): Final combined-state review shows only `TODO.agent.md`, `src/tools/process.rs`, and the focused `src/tools/mod.rs` regression changed; production behavior changes are limited to the requested yield constants/comment and `write_stdin` description.
  - AUDIT (2): `MAX_INITIAL_YIELD_MS` remains `20_000`; `ProcessRegistry::start` execution-deadline selection/enforcement is unchanged, so the existing process lifetime timeout policy is preserved.
  - IMPLEMENT (3): No corrections were required after source review, focused tests, or clippy.
  - IMPLEMENT (3): No unrelated production/schema changes were added; the only `src/tools/mod.rs` change is the focused description regression test.
  - IMPLEMENT (3): Cleanup for prefix `tmp-yield40-` completed; final container listing is empty and no task-owned volume/network/image remains.
  - REVIEW/VERIFY (4): Final `srcwalk review` and constant discovery confirm `MIN_YIELD_MS=250`, `MAX_YIELD_MS=120_000`, `MAX_INITIAL_YIELD_MS=20_000`, and `MAX_POLL_YIELD_MS=40_000` with the expected clamp call sites.
  - REVIEW/VERIFY (4): Focused evidence remains fmt pass, process tests 57/57 runnable pass with 1 intentional ignored live-Podman test, two description regressions pass, clippy `--lib --all-features -D warnings` pass, and final `git diff --check` pass.
  - REVIEW/VERIFY (4): Persisted plan/handoff reconciliation and completion readback follow immediately.

# write_stdin polling efficiency wording (2026-10-03)

- [x] Refine the public write_stdin guidance to explain why shell sleep is wasteful.
  - Acceptance: make clear that `write_stdin` itself performs the requested wait while polling, so a separate shell/bash `sleep` before polling consumes an extra tool call without adding useful waiting behavior; preserve all runtime semantics and yield constants.
  - Affected: `src/tools/process.rs`, focused regression wording in `src/tools/mod.rs`, and this checklist only.
  - AUDIT (2): Existing description prohibits shell/bash sleep but motivates it mainly by output/status collection rather than explicitly by avoiding a redundant tool call.
  - AUDIT (2): Starting worktree contains only the immediately preceding yield/description task changes; preserve them exactly apart from this wording refinement.
  - IMPLEMENT (3): Description now states that an extra shell/bash `sleep` before `write_stdin` wastes a tool call because `write_stdin` itself performs the requested wait via `wait_for_exit_ms`/`yield_time_ms` while returning process output/status.
  - IMPLEMENT (3): Focused regression renamed/updated to assert both the redundant-tool-call wording and the fact that `write_stdin` already performs the wait.
  - IMPLEMENT (3): preserve all constants, clamp behavior, schemas, and process lifetime semantics.
  - REVIEW/VERIFY (4): Re-read both changed regions and `srcwalk review`; no runtime/control-flow change beyond the prior yield-limit task.
  - REVIEW/VERIFY (4): Ephemeral `rust:latest`: `cargo fmt --all --check`, focused description regression, public-description bound regression, and `cargo clippy --lib --all-features -- -D warnings` all passed; `git diff --check` passed.

- [x] Final integration for write_stdin polling efficiency wording.
  - Depends on: wording refinement task.
  - Acceptance: only wording/test/checklist change relative to the preceding state; focused verification and diff checks pass.
  - AUDIT (2): Final review confirms the yield constants remain `250`, `120_000`, `20_000`, and `40_000`; this follow-up changed only the public description wording, its focused regression wording/name, and this checklist relative to the preceding state.
  - IMPLEMENT (3): No correction was required after focused tests/review; runtime polling, execution timeout, schema, and clamp behavior remain unchanged.
  - REVIEW/VERIFY (4): Final `srcwalk review`, literal discovery of both efficiency phrases, `git diff --check`, and task-container absence all pass; focused fmt/tests/clippy evidence remains valid after the final wording edit.

# Native project fallback creation gate (2026-10-10)

- [x] Gate implicit project creation unless operator explicitly enables native-identity fallback.
  - **Acceptance**: `CODEXBRIDGE_ALLOW_NATIVE_PROJECT_FALLBACK=false` (default) rejects first `chatgpt_turn_init` without a usable project alias or same-subject parent turn reference when no existing binding; rejects without new checkout/metadata directories, binding or turn ref. `true` preserves old native-hash fallback. Existing bound conversation and valid parent inheritance remain usable without an alias. Tools outside init remain gated by `TURN_NOT_INITIALIZED` regardless of flag; preserve error/retry contract and unrelated behavior.
  - **Affected**: `src/config.rs`, `src/project.rs`, `src/server.rs`, `src/tools/mod.rs`, `tests/project_identity_contract.rs`, `tests/state_contract.rs`, `docs.md`, `TODO.agent.md`. Risk: accidentally blocking valid recovery/rejoin or creating directories before gate, inconsistent config/env behavior, contract drift.
  - **AUDIT (2)**: `ProjectRequestContext`/`SharedState::tool_scope` use `resolve_initialized`, which rejects unbound calls before `ensure_layout`. `prepare_turn_initialize` resolves stored native binding / same-subject `previous_turn_ref`; `prepare_initialize_inner` alone selects native hash if none of binding/inheritance/alias exists. `commit_initialize_with_turn_ref` is the directory and storage mutation boundary; gate must occur during prepare. ConfigBuilder already has strict bool parsing and precedence; server owns the sole production resolver construction.
  - **AUDIT (2)**: Starting worktree clean. Checked `TODO.agent.md` against `recall(_plan_readback_)` (no saved plan). Baseline `project::tests` and `config::tests` passed before production edits in `tmp-gate10-baseline`. Traced `prepare_initialize_inner` callers (`prepare_initialize`, `prepare_turn_initialize`), `resolve` legacy path, and successful named/turn-ref recovery; no other tool path can initiate a new binding.
  - **IMPLEMENT (3)**: Added strict bool env `CODEXBRIDGE_ALLOW_NATIVE_PROJECT_FALLBACK` (default `false`, opt-in `true`) in ConfigBuilder with diagnostics and validation/precedence tests; production server injects the flag into ProjectResolver.
  - **IMPLEMENT (3)**: Rejected unbound native-hash fallback in both `prepare_initialize_inner` and legacy `resolve` with retryable `PROJECT_KEY_REQUIRED` before `ensure_layout` or SQLite mutation. Existing named binding, cross-conversation same-subject valid turn-ref inheritance, and already-bound conversation recovery are preserved. `resolve_initialized` still emits `TURN_NOT_INITIALIZED` for uninitialized native/upstream tools.
  - **IMPLEMENT (3)**: Added no-directory/no-binding regressions for stateless and MCP-session identities, opt-in implicit creation/strict reopen, named/turn-ref continuation, and config flags. Updated legacy tests that intentionally rely on implicit native identity to explicitly opt in. Documented flag in `docs.md` and public `chatgpt_turn_init` description.
  - **REVIEW/VERIFY (4)**: Focused after initial change: `project::tests` 23/23 and `config::tests` 21/21 passed. Full regression initially exposed old implicit-creation expectations in `tests/project_identity_contract.rs`, `tests/state_contract.rs`, and one project test assertion; corrected the test fixtures/erroneous assertion without relaxing strict default.
  - **REVIEW/VERIFY (4)**: Final ephemeral `tmp-gate10-gatecheck` Rust container exit 0 on `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -j1 -- -D warnings`, `cargo test --all-targets --all-features -j1`, `cargo build --bins --examples --all-features -j1`. Live Podman test remains intentionally ignored by ordinary test suite.
  - **REVIEW/VERIFY (4)**: Re-read modified behavioral paths, exact added tests, complete diff, `srcwalk review`, `srcwalk trace callers prepare_initialize_inner`, and gate-before-layout logic; existing bindings and turn-ref resolution bypass only the new native-creation gate, not the regular project-turn gate. No unrelated functional change.
  - **REVIEW/VERIFY (4)**: Final `git diff --check` passed; task-specific Podman run containers use `--rm`. Required integrated cleanup and persisted plan reconciliation follow in final integration task.

- [x] Final integration for native project fallback gate.
  - **Depends on**: fallback creation gate task.
  - **Acceptance**: final combined behavior passes relevant project/config/turn protocol regressions plus source/diff review; no task containers/processes remain.
  - **AUDIT (2)**: Final combined path: ConfigBuilder bool env → server constructor → ProjectResolver gate in `prepare_initialize_inner` and `resolve`; tool init still uses `prepare_turn_initialize` and commit only after successful prepare. Native/upstream tools still use `resolve_initialized` and reject unbound requests before layout creation.
  - **AUDIT (2)**: Scoped `srcwalk trace callers prepare_initialize_inner` confirms two entry paths (prepare_initialize, prepare_turn_initialize); original `resolve` is separately gated. Tested all relevant acceptance branches, and source review found no lost turn-ref/rejoin semantics or deleted side effects.
  - **IMPLEMENT (3)**: Full suite surfaced previously implicit native-hash setup in `tests/project_identity_contract.rs` and `tests/state_contract.rs`; those legacy expectations now explicitly opt into fallback. One misplaced regression assertion was corrected before final verification.
  - **IMPLEMENT (3)**: The public init tool description and docs advertise the strict default and opt-in behavior; no public signature/argument changes. Config diagnostics reveal the flag's effective value.
  - **IMPLEMENT (3)**: No source modifications after final combined fmt/clippy/test/build gate, so integrated evidence was not invalidated by checklist-only edits.
  - **REVIEW/VERIFY (4)**: Final `tmp-gate10-gatecheck` ephemeral Rust container exit code 0 across `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -j1 -- -D warnings`, `cargo test --all-targets --all-features -j1`, and `cargo build --bins --examples --all-features -j1` (one intentionally ignored live-Podman test). Earlier full-gate failures were in fixtures that depended on old implicit fallback; corrected and final full gate is green.
  - **REVIEW/VERIFY (4)**: Read behavioral source and tests after edits, inspected final `srcwalk review` and complete `git diff` (8 files), with no unrelated production changes or hidden creation paths found.
  - **REVIEW/VERIFY (4)**: Executed required cleanup for `tmp-gate10-`; final Podman containers/volumes/networks/images all empty. `git diff --check` passed; only intended 8 files modified. No running task sessions remain.
  - **REVIEW/VERIFY (4)**: Reconciled checklist. Persistence plan and project handoff are cleared/recorded after this integration entry readback; no further required implementation/test remains.

# Restore chatgpt_turn_init public description (2026-10-10)

- [x] Restore the exact previous public tool description.
  - **Acceptance**: The `chatgpt_turn_init` description matches the user-supplied text byte-for-byte; no arguments, schemas, handler flow, runtime config, or other tool descriptions change.
  - **Affected**: `src/tools/mod.rs` description only; `TODO.agent.md` audit notes. Risk: accidental changes to quoted text or unrelated source.
  - **AUDIT (2)**: Initial worktree was clean; the native fallback gate is already committed in HEAD. `srcwalk show src/tools/mod.rs:756-762` confirms the added fallback wording is contained in the single description string. `_plan_readback_` was empty. This is metadata-only; runtime behavior and API shape are unchanged.
  - **AUDIT (2)**: Use the user-provided exact string as the expected output and compare it literally after editing. Review the scoped diff and run formatting plus a focused public-description contract test; broader runtime tests are not required for unchanged code paths.
  - **IMPLEMENT (3)**: Replaced precisely the `chatgpt_turn_init` public description with the user-supplied original string.
  - **IMPLEMENT (3)**: Direct source comparison of the string is exact: 1,140 characters; no signature, route/handler, runtime logic, or config edits.
  - **IMPLEMENT (3)**: Public documentation for the native fallback gate remains unchanged; user explicitly requested only the tool description to be restored.
  - **REVIEW/VERIFY (4)**: Direct literal comparison of the `chatgpt_turn_init` description against the user's quoted text returned exact_match=true (1,140 characters). `srcwalk show src/tools/mod.rs:756-762` confirms that string is the only touched Rust line.
  - **REVIEW/VERIFY (4)**: Ephemeral Podman `rust:latest` container exited 0 for `rustup component add rustfmt`, `cargo fmt --all --check`, and `cargo test --lib -j1 init_schema_exposes_turn_reference_chain` (1/1 passed).
  - **REVIEW/VERIFY (4)**: `srcwalk review` identifies only one non-functional string edit in `src/tools/mod.rs:759`; `git diff --check` is clean; function signatures/control flow are unchanged. No broader runtime suite needed for unchanged logic.
  - **REVIEW/VERIFY (4)**: `podman ps -a --filter name=tmp-desc1010-` shows no retained task containers; only `src/tools/mod.rs` and this checklist differ from HEAD.
- [x] Final integration for restored init description.
  - **Depends on**: Description restoration.
  - **Acceptance**: Combined worktree preserves previous fallback implementation and only reverts the requested description change; formatting, focused contract check, and diff review pass.
  - **AUDIT (2)**: Combined repository diff from HEAD contains only one `src/tools/mod.rs` description replacement and checklist metadata; existing fallback implementation, docs, config, handlers, and tests are preserved.
  - **AUDIT (2)**: This string metadata change does not modify Rust control flow or function signatures, so focused tool schema coverage, exact-text comparison, and formatting suffice; full runtime/E2E retesting would duplicate unaffected behavior proof.
  - **IMPLEMENT (3)**: No further code edits required after the exact-text assertion and focused test.
  - **IMPLEMENT (3)**: No additional configs, descriptions, or public tools changed.
  - **IMPLEMENT (3)**: Ephemeral task container exited with `--rm`; none remained.
  - **REVIEW/VERIFY (4)**: The final Rust description equals the user's supplied original verbatim; exact_match=true and length=1,140.
  - **REVIEW/VERIFY (4)**: Podman `rust:latest` cargo fmt and focused `init_schema_exposes_turn_reference_chain` passed, exit code 0.
  - **REVIEW/VERIFY (4)**: Final `srcwalk review` identifies the single unchanged-behavior `src/tools/mod.rs:759` description hunk; `git diff --check` passed.
  - **REVIEW/VERIFY (4)**: Task-owned containers absent; persisted plan cleared after this integration checklist readback.
