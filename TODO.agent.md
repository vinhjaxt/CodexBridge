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
