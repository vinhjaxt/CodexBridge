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
