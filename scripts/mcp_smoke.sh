#!/usr/bin/env bash
set -euo pipefail

port="${MCP_SMOKE_PORT:-3055}"
server_bin="${MCP_SERVER_BIN:-target/debug/codex-bridge}"
mock_upstream_bin="${MCP_MOCK_UPSTREAM_BIN:-target/debug/examples/mock_upstream}"
run_root="$(mktemp -d)"
workspace="$run_root/workspace"
# Deliberately preserve a lexical `./` in the server argument. Project roots
# must be normalized before SecurePathResolver returns canonical descendants;
# otherwise fresh-turn skill discovery falsely reports PATH_OUTSIDE_WORKSPACE.
workspace_arg="$run_root/./workspace"
server_pid=""

cleanup() {
  if [[ -n "$server_pid" ]]; then
    kill -TERM "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT

mkdir -p "$workspace" "$run_root/home/.agents/skills/operator-smoke"
[[ -x "$mock_upstream_bin" ]]
cat >"$run_root/home/.agents/skills/operator-smoke/SKILL.md" <<'EOF'
---
name: operator-smoke
description: User-home integration skill.
---

# Operator smoke skill
EOF
cat >"$run_root/upstreams.yaml" <<EOF
mcpServers:
  direct-smoke:
    command: "$mock_upstream_bin"
    type: stdio
    mode: direct
  gateway-smoke:
    command: "$mock_upstream_bin"
    type: stdio
    mode: gateway
EOF

HOME="$run_root/home" \
MCP_BIND="127.0.0.1:${port}" \
MCP_AUTH_MODE="either" \
MAX_LEGACY_MCP_SESSIONS="1" \
MCP_UPSTREAM_CONFIG="$run_root/upstreams.yaml" \
RUST_LOG=info \
"$server_bin" "$workspace_arg" >"$run_root/server.out" 2>&1 &
server_pid=$!

for _ in $(seq 1 100); do
  curl -sf "http://127.0.0.1:${port}/health" >/dev/null && break
  sleep 0.1
done
curl -sf "http://127.0.0.1:${port}/health" >/dev/null

token_file="$workspace/.metadata/auth-token"
[[ -s "$token_file" ]]
token="$(tr -d '\r\n' <"$token_file")"
[[ "$token" == cb_* ]]
endpoint="http://127.0.0.1:${port}/${token}/mcp"
bearer_endpoint="http://127.0.0.1:${port}/mcp"

modern_headers=(
  -H 'content-type: application/json'
  -H 'accept: application/json, text/event-stream'
  -H 'mcp-protocol-version: 2026-07-28'
)
meta='"openai/subject":"usr_test","openai/session":"conv_test","io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}'

curl -sS -D "$run_root/modern-init.headers" "${modern_headers[@]}" \
  --data '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"smoke","version":"1"}}}' \
  "$endpoint" >"$run_root/modern-init.json"
! grep -qi '^mcp-session-id:' "$run_root/modern-init.headers"
grep -q 'CodexBridge' "$run_root/modern-init.json"
grep -Eq '"version":"[^"]+\+contract\.[0-9a-f]{12}"' "$run_root/modern-init.json"
grep -q 'first project-bearing turn' "$run_root/modern-init.json"
grep -q 'chatgpt_turn_init' "$run_root/modern-init.json"
grep -q 'exactly once' "$run_root/modern-init.json"
grep -q 'previous_turn_ref' "$run_root/modern-init.json"
grep -q 'multiple rounds of tool calls' "$run_root/modern-init.json"
grep -q 'Repeat this loop as many times as needed' "$run_root/modern-init.json"
grep -q 'Do not leave actionable TODOs' "$run_root/modern-init.json"
grep -q 'Verification is part of implementation' "$run_root/modern-init.json"
grep -q "Match the user's requested task mode" "$run_root/modern-init.json"
grep -q 'Do not create commits, branches, tags, pushes, releases, deployments' "$run_root/modern-init.json"
grep -q 'clearly unrelated pre-existing failure' "$run_root/modern-init.json"
grep -q 'project-local AGENTS/rule source is marked' "$run_root/modern-init.json"

# Either-mode keeps the path-token endpoint while also accepting a standard
# bearer-authenticated /mcp endpoint.
curl -sS "${modern_headers[@]}" -H "authorization: Bearer ${token}" \
  --data '{"jsonrpc":"2.0","id":1000,"method":"initialize","params":{"protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"bearer-smoke","version":"1"}}}' \
  "$bearer_endpoint" >"$run_root/bearer-init.json"
grep -q 'CodexBridge' "$run_root/bearer-init.json"
invalid_bearer_status="$(curl -sS -o "$run_root/invalid-bearer.json" -w '%{http_code}' \
  "${modern_headers[@]}" -H 'authorization: Bearer definitely-wrong-token' \
  --data '{"jsonrpc":"2.0","id":1002,"method":"initialize","params":{"protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"bad-bearer","version":"1"}}}' \
  "$bearer_endpoint")"
[[ "$invalid_bearer_status" == 401 ]]
grep -q 'AUTH_FAILED' "$run_root/invalid-bearer.json"

curl -sS "${modern_headers[@]}" -H 'mcp-method: tools/list' \
  --data "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{\"_meta\":{$meta}}}" \
  "$endpoint" >"$run_root/tools.json"

expected_tools='["apply_patch","chatgpt_turn_init","exec_command","gateway_gateway_smoke","glob","grep","list_directory","read_file","recall","remember","skills_list","skills_read","tree","update_plan","upstream_direct_smoke__mock_echo","view_image","write_stdin"]'
jq -e --argjson expected "$expected_tools" '(.result.tools | map(.name) | sort) == ($expected | sort)' "$run_root/tools.json" >/dev/null
jq -e '.result.tools[] | select(.name == "chatgpt_turn_init") | (.description | contains("idempotent")) and (.description | contains("stop_current_turn")) and (.inputSchema.properties.previous_turn_ref.type == ["string","null"]) and (.outputSchema.properties.status.enum == ["synchronized","soft_error"]) and (.outputSchema.properties.agent_action.enum == ["continue","stop_current_turn"]) and (.outputSchema.properties.soft_error.type == ["object","null"]) and (.outputSchema.properties.turn_ref.type == ["string","null"]) and (.outputSchema.properties.previous_turn_ref.type == ["string","null"]) and (.outputSchema.properties.instruction_hash.type == ["string","null"]) and (.outputSchema.properties.state_hash.type == ["string","null"]) and (.outputSchema.properties.instructions_changed.type == "boolean") and (.outputSchema.properties.state_changed.type == "boolean") and (.outputSchema.properties.turn_reused.type == "boolean") and (.outputSchema.properties.brief.type == ["string","null"]) and (.outputSchema.properties.state_update.type == ["string","null"])' "$run_root/tools.json" >/dev/null
jq -e '.result.tools[] | select(.name == "skills_list") | .inputSchema.properties.path.type == ["string","null"]' "$run_root/tools.json" >/dev/null
jq -e '.result.tools[] | select(.name == "skills_read") | .inputSchema.properties.path.type == ["string","null"]' "$run_root/tools.json" >/dev/null
jq -e '.result.tools[] | select(.name == "recall") | .inputSchema.properties.offset.type == "integer" and .inputSchema.properties.max_results.type == ["integer","null"] and .inputSchema.properties.snapshot_hash.type == ["string","null"] and .inputSchema.properties.include_plan.type == "boolean" and .inputSchema.properties.extensions.type == "object" and .inputSchema.properties.extensions.additionalProperties != false and ((.inputSchema.required // []) | index("extensions") | not)' "$run_root/tools.json" >/dev/null
jq -e '.result.tools[] | select(.name == "exec_command") | .inputSchema.properties.stdin.type == ["string","null"] and .inputSchema.properties.close_stdin.type == "boolean" and .inputSchema.properties.extensions.type == "object" and .inputSchema.properties.extensions.additionalProperties != false and ((.inputSchema.required // []) | index("extensions") | not)' "$run_root/tools.json" >/dev/null
jq -e '.result.tools[] | select(.name == "write_stdin") | .inputSchema.properties.since_output_offset.type == ["integer","null"] and .inputSchema.properties.wait_for_exit_ms.type == ["integer","null"] and .inputSchema.properties.extensions.type == "object" and .inputSchema.properties.extensions.additionalProperties != false and ((.inputSchema.required // []) | index("extensions") | not)' "$run_root/tools.json" >/dev/null
jq -e '.result.tools[] | select(.name == "remember") | (.description | contains("costly to rediscover")) and (.description | contains("memory_set") | not)' "$run_root/tools.json" >/dev/null
jq -e '.result.tools[] | select(.name == "grep") | .outputSchema.properties.incomplete.type == "boolean" and .outputSchema.properties.skipped_files.type == "integer" and .outputSchema.properties.traversal_limit_hit.type == "boolean"' "$run_root/tools.json" >/dev/null
jq -e '.result.tools[] | select(.name == "exec_command") | (.description | contains("Shell:")) and (.description | contains("Default backend:"))' "$run_root/tools.json" >/dev/null
for removed in exec run_command search_files write_file read_files file_info project_info project_status memory_set plan_get task_add clock_now git_status git_commit; do
  ! grep -q "\"$removed\"" "$run_root/tools.json"
done

call_tool() {
  local name="$1" id="$2" arguments="$3" output="$4"
  curl -sS "${modern_headers[@]}" -H 'mcp-method: tools/call' -H "mcp-name: $name" \
    --data "{\"jsonrpc\":\"2.0\",\"id\":$id,\"method\":\"tools/call\",\"params\":{\"name\":\"$name\",\"arguments\":$arguments,\"_meta\":{$meta}}}" \
    "$endpoint" >"$output"
}

call_tool_with_meta() {
  local name="$1" id="$2" arguments="$3" call_meta="$4" output="$5"
  curl -sS "${modern_headers[@]}" -H 'mcp-method: tools/call' -H "mcp-name: $name" \
    --data "{\"jsonrpc\":\"2.0\",\"id\":$id,\"method\":\"tools/call\",\"params\":{\"name\":\"$name\",\"arguments\":$arguments,\"_meta\":{$call_meta}}}" \
    "$endpoint" >"$output"
}

call_tool read_file 3 '{"path":"hello.txt"}' "$run_root/pre-init.json"
jq -e '.result.isError == true and (.result.structuredContent == null)' "$run_root/pre-init.json" >/dev/null
grep -q 'TURN_NOT_INITIALIZED' "$run_root/pre-init.json"
call_tool list_directory 1003 '{"path":"."}' "$run_root/pre-init-list.json"
jq -e '.result.isError == true and (.result.structuredContent == null)' "$run_root/pre-init-list.json" >/dev/null
grep -q 'TURN_NOT_INITIALIZED' "$run_root/pre-init-list.json"
call_tool exec_command 1004 '{"cmd":"true"}' "$run_root/pre-init-exec.json"
jq -e '.result.isError == true and (.result.structuredContent == null)' "$run_root/pre-init-exec.json" >/dev/null
grep -q 'TURN_NOT_INITIALIZED' "$run_root/pre-init-exec.json"
call_tool upstream_direct_smoke__mock_echo 1005 '{"message":"pre-init"}' "$run_root/pre-init-direct-upstream.json"
jq -e '.result.isError == true and (.result.structuredContent == null)' "$run_root/pre-init-direct-upstream.json" >/dev/null
grep -q 'TURN_NOT_INITIALIZED' "$run_root/pre-init-direct-upstream.json"
call_tool gateway_gateway_smoke 1006 '{"function":"mock_echo","arguments":{"message":"pre-init"}}' "$run_root/pre-init-gateway.json"
jq -e '.result.isError == true and (.result.structuredContent == null)' "$run_root/pre-init-gateway.json" >/dev/null
grep -q 'TURN_NOT_INITIALIZED' "$run_root/pre-init-gateway.json"

partial_meta='"openai/subject":"usr_test","io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}'
call_tool_with_meta chatgpt_turn_init 1001 '{}' "$partial_meta" "$run_root/partial-openai-context.json"
jq -e '.result.isError == true and .result.structuredContent == null and (.result.content[0].text | startswith("INCOMPLETE_OPENAI_CONTEXT:"))' "$run_root/partial-openai-context.json" >/dev/null

call_tool chatgpt_turn_init 4 '{"project_key":"production-stress-test"}' "$run_root/project.json"
jq -e '.result.structuredContent.initialized == true and .result.structuredContent.workspace_state == "new" and .result.structuredContent.reused_existing_binding == false and .result.structuredContent.previous_turn_ref == null and .result.structuredContent.instructions_changed == true and .result.structuredContent.state_changed == true and .result.structuredContent.turn_reused == false and (.result.structuredContent.instruction_hash | type == "string") and (.result.structuredContent.state_hash | type == "string") and (.result.structuredContent.turn_ref | startswith("r_")) and (.result.structuredContent.brief | type == "string") and .result.structuredContent.state_update == null' "$run_root/project.json" >/dev/null
[[ -d "$workspace/production-stress-test" ]]
first_turn_ref="$(jq -r '.result.structuredContent.turn_ref' "$run_root/project.json")"
first_instruction_hash="$(jq -r '.result.structuredContent.instruction_hash' "$run_root/project.json")"
first_state_hash="$(jq -r '.result.structuredContent.state_hash' "$run_root/project.json")"
[[ ${#first_turn_ref} -eq 24 ]]
grep -q 'YOLO semantics' "$run_root/project.json"
grep -q 'gateway_gateway_smoke' "$run_root/project.json"
grep -q 'Do not call it again during this same user turn' "$run_root/project.json"
grep -F -q "[ref:${first_turn_ref}]" "$run_root/project.json"

# Simulate the next user turn: pass the nearest prior CodexBridge ref explicitly.
# Instructions and state are unchanged, so the server returns only a compact receipt.
call_tool chatgpt_turn_init 44 "$(jq -cn --arg previous "$first_turn_ref" '{previous_turn_ref:$previous}')" "$run_root/project-existing.json"
jq -e --arg previous "$first_turn_ref" --arg instructions "$first_instruction_hash" --arg state "$first_state_hash" '.result.structuredContent.workspace_state == "existing" and .result.structuredContent.reused_existing_binding == true and .result.structuredContent.previous_turn_ref == $previous and .result.structuredContent.turn_ref != $previous and .result.structuredContent.instruction_hash == $instructions and .result.structuredContent.state_hash == $state and .result.structuredContent.instructions_changed == false and .result.structuredContent.state_changed == false and .result.structuredContent.turn_reused == false and .result.structuredContent.brief == null and .result.structuredContent.state_update == null' "$run_root/project-existing.json" >/dev/null
second_turn_ref="$(jq -r '.result.structuredContent.turn_ref' "$run_root/project-existing.json")"
jq -e --slurpfile first "$run_root/project.json" '.result.structuredContent.effective_project_key == $first[0].result.structuredContent.effective_project_key' "$run_root/project-existing.json" >/dev/null
grep -F -q "[ref:${second_turn_ref}]" "$run_root/project-existing.json"

# An accidental second init in the same turn carries the same previous ref.
# It must be idempotent and return the already-created second turn.
call_tool chatgpt_turn_init 46 "$(jq -cn --arg previous "$first_turn_ref" '{previous_turn_ref:$previous}')" "$run_root/project-duplicate.json"
jq -e --arg previous "$first_turn_ref" --arg current "$second_turn_ref" '.result.structuredContent.previous_turn_ref == $previous and .result.structuredContent.turn_ref == $current and .result.structuredContent.turn_reused == true and .result.structuredContent.instructions_changed == false and .result.structuredContent.state_changed == false and .result.structuredContent.brief == null and .result.structuredContent.state_update == null' "$run_root/project-duplicate.json" >/dev/null

# Once bound, omitting the previous ref returns an MCP-success soft stop rather
# than a tool execution error or silently creating an ambiguous additional turn.
call_tool chatgpt_turn_init 47 '{}' "$run_root/project-missing-parent.json"
jq -e '.result.isError != true and .result.structuredContent.status == "soft_error" and .result.structuredContent.agent_action == "stop_current_turn" and .result.structuredContent.initialized == false and .result.structuredContent.turn_ref == null and .result.structuredContent.soft_error.code == "PREVIOUS_TURN_REF_REQUIRED" and .result.structuredContent.soft_error.retry_on_next_user_turn == true and (.result.content[0].text | startswith("STOP_CURRENT_TURN:"))' "$run_root/project-missing-parent.json" >/dev/null

# Mutate only durable project state during the second turn. The next turn must
# return a compact state delta without re-sending the instruction brief.
call_tool update_plan 82 '{"plan":[{"step":"state-only-smoke","status":"in_progress"}],"explanation":"state hash smoke"}' "$run_root/state-only-plan.json"
call_tool chatgpt_turn_init 83 "$(jq -cn --arg previous "$second_turn_ref" '{previous_turn_ref:$previous}')" "$run_root/project-state-changed.json"
jq -e --arg previous "$second_turn_ref" --arg instructions "$first_instruction_hash" --arg old_state "$first_state_hash" '.result.structuredContent.previous_turn_ref == $previous and .result.structuredContent.instruction_hash == $instructions and .result.structuredContent.state_hash != $old_state and .result.structuredContent.instructions_changed == false and .result.structuredContent.state_changed == true and .result.structuredContent.turn_reused == false and .result.structuredContent.brief == null and (.result.structuredContent.state_update | type == "string")' "$run_root/project-state-changed.json" >/dev/null
third_turn_ref="$(jq -r '.result.structuredContent.turn_ref' "$run_root/project-state-changed.json")"
state_only_hash="$(jq -r '.result.structuredContent.state_hash' "$run_root/project-state-changed.json")"
grep -q 'state-only-smoke' "$run_root/project-state-changed.json"
grep -q 'recall pagination and include_plan=true' "$run_root/project-state-changed.json"
grep -F -q "[ref:${third_turn_ref}]" "$run_root/project-state-changed.json"

# Duplicate continuation replay must return the snapshot committed for that turn,
# even if durable state changes after the original response. Restore the state
# afterward so the following instruction-only change remains isolated.
call_tool update_plan 87 '{"plan":[{"step":"state-after-original-response","status":"in_progress"}],"explanation":"retry replay mutation"}' "$run_root/replay-mutated-plan.json"
call_tool chatgpt_turn_init 88 "$(jq -cn --arg previous "$second_turn_ref" '{previous_turn_ref:$previous}')" "$run_root/project-replay-after-state-change.json"
jq -e --arg current "$third_turn_ref" --arg state "$state_only_hash" '.result.structuredContent.turn_ref == $current and .result.structuredContent.turn_reused == true and .result.structuredContent.state_hash == $state and .result.structuredContent.instructions_changed == false and .result.structuredContent.state_changed == true and .result.structuredContent.brief == null and (.result.structuredContent.state_update | type == "string")' "$run_root/project-replay-after-state-change.json" >/dev/null
grep -q 'state-only-smoke' "$run_root/project-replay-after-state-change.json"
! grep -q 'state-after-original-response' "$run_root/project-replay-after-state-change.json"
call_tool update_plan 89 '{"plan":[{"step":"state-only-smoke","status":"in_progress"}],"explanation":"state hash smoke"}' "$run_root/replay-restore-plan.json"

call_tool upstream_direct_smoke__mock_echo 41 '{"message":"direct-ok"}' "$run_root/upstream-direct.json"
grep -q 'echo: direct-ok' "$run_root/upstream-direct.json"
call_tool gateway_gateway_smoke 42 '{"function":"mock_echo","arguments":{"message":"gateway-ok"}}' "$run_root/upstream-gateway.json"
grep -q 'echo: gateway-ok' "$run_root/upstream-gateway.json"

patch="$(cat <<'EOF'
*** Begin Patch
*** Add File: hello.txt
+hello
*** Add File: AGENTS.md
+SMOKE_AGENT_INSTRUCTION
*** Add File: .agents/skills/demo/SKILL.md
+---
+name: demo
+description: Use for integration smoke checks.
+---
+
+# Demo skill
*** Add File: .agents/skills/demo/references/api.md
+SKILL_RESOURCE_OK
*** Add File: services/api/src/lib.rs
+pub fn api() {}
*** Add File: services/api/AGENTS.md
+NESTED_API_AGENT_RULE
*** Add File: services/api/.gitignore
+ignored.txt
*** Add File: services/api/ignored.txt
+NESTED_IGNORE_SHOULD_HIDE
*** Add File: services/api/visible.txt
+NESTED_IGNORE_VISIBLE
*** Add File: services/api/.agents/skills/team/deploy/SKILL.md
+---
+name: nested-deploy
+description: CANONICAL_NESTED_SKILL
+---
+
+# Nested deploy skill
*** Add File: services/api/.codex/skills/alias-deploy/SKILL.md
+---
+name: nested-deploy
+description: ALIAS_SHOULD_LOSE
+---
*** Add File: services/api/.codex/skills/legacy-only/SKILL.md
+---
+name: legacy-compat
+description: CODEX_ALIAS_COMPAT_SKILL
+---
*** Add File: services/web/.agents/skills/web-only/SKILL.md
+---
+name: web-only
+description: SIBLING_SKILL_SHOULD_NOT_APPEAR
+---
*** Add File: node_modules/smoke-visible.txt
+EXPLICIT_IGNORED_DIR_LISTING_OK
*** Add File: excluded-by-git-info.tmp
+SHOULD_NOT_BE_DISCOVERED
*** Add File: .git/info/exclude
+excluded-by-git-info.tmp
*** Add File: src/search-smoke.rs
+before-context
+SEARCH_NEEDLE
+after-context
*** Add File: docs/search-smoke.md
+SEARCH_NEEDLE
*** End Patch
EOF
)"
call_tool apply_patch 5 "$(jq -cn --arg input "$patch" '{input:$input}')" "$run_root/patch.json"
jq -e '.result.structuredContent.applied == true' "$run_root/patch.json" >/dev/null

# A later user turn observes the new root AGENTS.md and root skill catalogue.
# The instruction hash changes while the saved-state hash remains stable.
call_tool chatgpt_turn_init 48 "$(jq -cn --arg previous "$third_turn_ref" '{previous_turn_ref:$previous}')" "$run_root/project-context-changed.json"
jq -e --arg previous "$third_turn_ref" --arg old_instructions "$first_instruction_hash" --arg state "$state_only_hash" '.result.structuredContent.previous_turn_ref == $previous and .result.structuredContent.instructions_changed == true and .result.structuredContent.state_changed == false and .result.structuredContent.turn_reused == false and .result.structuredContent.instruction_hash != $old_instructions and .result.structuredContent.state_hash == $state and (.result.structuredContent.brief | type == "string") and .result.structuredContent.state_update == null' "$run_root/project-context-changed.json" >/dev/null
fourth_turn_ref="$(jq -r '.result.structuredContent.turn_ref' "$run_root/project-context-changed.json")"
grep -q 'SMOKE_AGENT_INSTRUCTION' "$run_root/project-context-changed.json"
grep -q 'integration smoke checks' "$run_root/project-context-changed.json"
grep -q 'state-only-smoke' "$run_root/project-context-changed.json"
grep -F -q "[ref:${fourth_turn_ref}]" "$run_root/project-context-changed.json"

# Nested AGENTS instructions are not part of the root turn brief. The first
# mutation in that scope must disclose them and fail before writing; retrying the
# same operation after disclosure is allowed.
nested_patch="$(cat <<'EOF'
*** Begin Patch
*** Update File: services/api/src/lib.rs
@@
-pub fn api() {}
+pub fn api() { /* nested */ }
*** End Patch
EOF
)"
call_tool apply_patch 90 "$(jq -cn --arg input "$nested_patch" '{input:$input}')" "$run_root/nested-agents-gate.json"
jq -e '.result.isError == true and .result.structuredContent == null and (.result.content[0].text | startswith("AGENTS_SCOPE_REQUIRED:"))' "$run_root/nested-agents-gate.json" >/dev/null
grep -q 'NESTED_API_AGENT_RULE' "$run_root/nested-agents-gate.json"
call_tool apply_patch 91 "$(jq -cn --arg input "$nested_patch" '{input:$input}')" "$run_root/nested-agents-retry.json"
jq -e '.result.structuredContent.applied == true' "$run_root/nested-agents-retry.json" >/dev/null
call_tool read_file 92 '{"path":"services/api/src/lib.rs"}' "$run_root/nested-agents-read.json"
grep -q 'nested' "$run_root/nested-agents-read.json"

# Nested .gitignore is scoped to services/api and participates in recursive
# discovery just like root ignore rules.
call_tool glob 93 '{"pattern":"services/api/**/*.txt"}' "$run_root/nested-ignore-glob.json"
grep -q 'services/api/visible.txt' "$run_root/nested-ignore-glob.json"
! grep -q 'services/api/ignored.txt' "$run_root/nested-ignore-glob.json"

# A branch from the same ChatGPT subject can inherit the same effective project
# from the previous ref without resupplying project_key. A new conversation gets
# a full brief even when both stored hashes themselves did not change.
branch_meta='"openai/subject":"usr_test","openai/session":"conv_branch","io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}'
call_tool_with_meta chatgpt_turn_init 49 "$(jq -cn --arg previous "$fourth_turn_ref" '{previous_turn_ref:$previous}')" "$branch_meta" "$run_root/project-branch.json"
jq -e --slurpfile main "$run_root/project-context-changed.json" --arg previous "$fourth_turn_ref" '.result.structuredContent.workspace_state == "joined" and .result.structuredContent.previous_turn_ref == $previous and .result.structuredContent.instructions_changed == true and .result.structuredContent.state_changed == true and (.result.structuredContent.brief | type == "string") and .result.structuredContent.effective_project_key == $main[0].result.structuredContent.effective_project_key' "$run_root/project-branch.json" >/dev/null

blank_envelope_patch="$(printf '\n\n*** Begin Patch\n*** Add File: blank-envelope.txt\n+BLANK_ENVELOPE_OK\n*** End Patch\n\n')"
call_tool apply_patch 73 "$(jq -cn --arg input "$blank_envelope_patch" '{input:$input}')" "$run_root/blank-envelope-patch.json"
jq -e '.result.structuredContent.applied == true' "$run_root/blank-envelope-patch.json" >/dev/null
call_tool read_file 74 '{"path":"blank-envelope.txt"}' "$run_root/blank-envelope-read.json"
grep -q 'BLANK_ENVELOPE_OK' "$run_root/blank-envelope-read.json"

call_tool read_file 6 '{"path":"hello.txt"}' "$run_root/read.json"
grep -q 'hello' "$run_root/read.json"

# A minified/generated file may contain one logical line larger than the
# presentation budget. read_file must continue inside that same line instead of
# silently skipping its remainder.
call_tool exec_command 60 '{"cmd":"head -c 300000 /dev/zero | base64 -w0 > long-line.txt; printf END >> long-line.txt"}' "$run_root/long-line-write.json"
call_tool read_file 61 '{"path":"long-line.txt"}' "$run_root/long-line-first.json"
jq -e '.result.structuredContent.truncated == true and .result.structuredContent.next_offset == 0 and (.result.structuredContent.next_line_byte_offset > 0)' "$run_root/long-line-first.json" >/dev/null
line_cursor="$(jq -r '.result.structuredContent.next_line_byte_offset' "$run_root/long-line-first.json")"
call_tool read_file 62 "$(jq -cn --argjson cursor "$line_cursor" '{path:"long-line.txt",offset:0,line_byte_offset:$cursor}')" "$run_root/long-line-second.json"
jq -e --argjson cursor "$line_cursor" '.result.structuredContent.line_byte_offset == $cursor and .result.structuredContent.truncated == false' "$run_root/long-line-second.json" >/dev/null
grep -q 'END' "$run_root/long-line-second.json"

# The default 256-KiB window remains conservative, but callers can explicitly
# request a larger bounded window for generated/minified files. This should
# finish the same ~400-KiB logical line in one call without changing global
# output configuration.
call_tool read_file 1010 '{"path":"long-line.txt","max_bytes":1048576}' "$run_root/long-line-wide.json"
jq -e '.result.structuredContent.truncated == false and .result.structuredContent.next_offset == null and .result.structuredContent.next_line_byte_offset == null' "$run_root/long-line-wide.json" >/dev/null
grep -q 'END' "$run_root/long-line-wide.json"

# A file may be much larger than MAX_WRITE_BYTES and still be readable through
# ranged read_file windows. A separate sparse file exceeds grep's 64-MiB scan
# ceiling and must be reported as incomplete rather than a false complete miss.
call_tool exec_command 94 '{"cmd":"head -c 9000000 /dev/zero | base64 -w0 > huge-search.txt; truncate -s 70000000 too-large-search.txt; printf HUGE_SEARCH_NEEDLE >> too-large-search.txt"}' "$run_root/huge-file-write.json"
call_tool read_file 95 '{"path":"huge-search.txt","limit":1}' "$run_root/huge-file-read.json"
jq -e '.result.structuredContent.bytes > 8388608 and .result.structuredContent.shown_lines == 1 and .result.structuredContent.truncated == true' "$run_root/huge-file-read.json" >/dev/null
call_tool grep 96 '{"pattern":"HUGE_SEARCH_NEEDLE","path":"too-large-search.txt"}' "$run_root/huge-file-grep.json"
jq -e '.result.structuredContent.incomplete == true and .result.structuredContent.skipped_files >= 1 and .result.structuredContent.truncated == false and .result.structuredContent.next_offset == null and (.result.structuredContent.continuation | contains("skipped"))' "$run_root/huge-file-grep.json" >/dev/null

call_tool list_directory 7 '{}' "$run_root/list.json"
grep -q 'hello.txt' "$run_root/list.json"
call_tool list_directory 78 '{"path":".","max_results":1}' "$run_root/list-page-1.json"
jq -e '.result.structuredContent.count == 1 and .result.structuredContent.truncated == true and .result.structuredContent.next_offset == 1' "$run_root/list-page-1.json" >/dev/null
first_list_name="$(jq -r '.result.structuredContent.entries[0].name' "$run_root/list-page-1.json")"
call_tool list_directory 79 '{"path":".","offset":1,"max_results":1}' "$run_root/list-page-2.json"
second_list_name="$(jq -r '.result.structuredContent.entries[0].name' "$run_root/list-page-2.json")"
[[ "$first_list_name" != "$second_list_name" ]]
call_tool tree 8 '{"path":".","depth":4,"max_entries":50}' "$run_root/tree.json"
grep -q 'SKILL.md' "$run_root/tree.json"
call_tool tree 80 '{"path":".","depth":1,"max_entries":50}' "$run_root/tree-depth-1.json"
! grep -q 'SKILL.md' "$run_root/tree-depth-1.json"
call_tool tree 81 '{"path":".","depth":4,"max_entries":1}' "$run_root/tree-page-1.json"
jq -e '.result.structuredContent.entries | length == 1' "$run_root/tree-page-1.json" >/dev/null
jq -e '.result.structuredContent.truncated == true and .result.structuredContent.next_offset == 1' "$run_root/tree-page-1.json" >/dev/null
call_tool glob 9 '{"pattern":"**/*.md"}' "$run_root/glob.json"
grep -q 'AGENTS.md' "$run_root/glob.json"
call_tool grep 10 '{"pattern":"SMOKE_AGENT_INSTRUCTION","path":"."}' "$run_root/grep.json"
grep -q 'AGENTS.md' "$run_root/grep.json"

# Shared ignore rules hide default ignored trees and .git/info/exclude from
# recursive discovery, while an explicit list of an ignored directory remains
# useful when the caller names it directly.
call_tool glob 63 '{"pattern":"**/*.txt"}' "$run_root/ignored-glob.json"
! grep -q 'smoke-visible.txt' "$run_root/ignored-glob.json"
call_tool glob 64 '{"pattern":"*.tmp"}' "$run_root/git-info-glob.json"
jq -e '.result.structuredContent.paths == []' "$run_root/git-info-glob.json" >/dev/null
call_tool list_directory 65 '{"path":"node_modules"}' "$run_root/ignored-explicit-list.json"
grep -q 'smoke-visible.txt' "$run_root/ignored-explicit-list.json"

# Include filters and context are verified through structured MCP output, not
# only through the pure search helper tests.
call_tool grep 66 '{"pattern":"SEARCH_NEEDLE","path":".","include":"**/*.rs","context":1}' "$run_root/grep-context.json"
jq -e '.result.structuredContent.count == 1 and .result.structuredContent.matches[0].path == "src/search-smoke.rs" and .result.structuredContent.matches[0].context_before == ["before-context"] and .result.structuredContent.matches[0].context_after == ["after-context"]' "$run_root/grep-context.json" >/dev/null
call_tool glob 67 '{"pattern":"src/[broken"}' "$run_root/invalid-glob.json"
jq -e '.result.isError == true and .result.structuredContent == null and (.result.content[0].text | startswith("INVALID_INPUT:"))' "$run_root/invalid-glob.json" >/dev/null
call_tool grep 68 '{"pattern":"[broken","path":"."}' "$run_root/invalid-regex.json"
jq -e '.result.isError == true and .result.structuredContent == null and (.result.content[0].text | startswith("INVALID_INPUT:"))' "$run_root/invalid-regex.json" >/dev/null

rollback_patch="$(cat <<'EOF'
*** Begin Patch
*** Add File: rollback-a.txt
+old
*** End Patch
EOF
)"
call_tool apply_patch 46 "$(jq -cn --arg input "$rollback_patch" '{input:$input}')" "$run_root/rollback-seed.json"
failing_patch="$(cat <<'EOF'
*** Begin Patch
*** Update File: rollback-a.txt
@@
-old
+new
*** Delete File: missing-for-rollback.txt
*** End Patch
EOF
)"
call_tool apply_patch 47 "$(jq -cn --arg input "$failing_patch" '{input:$input}')" "$run_root/rollback-fail.json"
jq -e '.result.isError == true' "$run_root/rollback-fail.json" >/dev/null
call_tool read_file 48 '{"path":"rollback-a.txt"}' "$run_root/rollback-read.json"
grep -q 'old' "$run_root/rollback-read.json"
! grep -q 'new' "$run_root/rollback-read.json"

move_patch="$(cat <<'EOF'
*** Begin Patch
*** Update File: rollback-a.txt
*** Move to: rollback-moved.txt
@@
-old
+moved
*** End Patch
EOF
)"
call_tool apply_patch 49 "$(jq -cn --arg input "$move_patch" '{input:$input}')" "$run_root/move.json"
jq -e '.result.structuredContent.applied == true' "$run_root/move.json" >/dev/null
call_tool read_file 50 '{"path":"rollback-moved.txt"}' "$run_root/move-read.json"
grep -q 'moved' "$run_root/move-read.json"
call_tool read_file 51 '{"path":"rollback-a.txt"}' "$run_root/move-old-path.json"
jq -e '.result.isError == true' "$run_root/move-old-path.json" >/dev/null

# A CRLF file patched with LF-authored hunks keeps CRLF endings; untouched
# lines are byte-preserved instead of being renormalized to one ending.
crlf_patch="$(cat <<'EOF'
*** Begin Patch
*** Update File: crlf.txt
@@
-crlf-two
+crlf-two-edited
*** End Patch
EOF
)"
call_tool exec_command 1016 '{"cmd":"printf '\''crlf-one\\r\\ncrlf-two\\r\\n'\'' > crlf.txt; od -c crlf.txt | head -2","yield_time_ms":5000}' "$run_root/crlf-seed.json"
jq -e '.result.structuredContent.exit_code == 0' "$run_root/crlf-seed.json" >/dev/null
call_tool apply_patch 1017 "$(jq -cn --arg input "$crlf_patch" '{input:$input}')" "$run_root/crlf-patch.json"
jq -e '.result.structuredContent.applied == true' "$run_root/crlf-patch.json" >/dev/null
call_tool exec_command 1018 '{"cmd":"printf '\''crlf-one\\r\\ncrlf-two-edited\\r\\n'\'' > expected.bin; cmp -s crlf.txt expected.bin && printf CRLF_BYTE_IDENTICAL || printf CRLF_MISMATCH","yield_time_ms":5000}' "$run_root/crlf-verify.json"
grep -q 'CRLF_BYTE_IDENTICAL' "$run_root/crlf-verify.json"
! grep -q 'CRLF_MISMATCH' "$run_root/crlf-verify.json"

# Two simultaneous patches starting from the same old content must not both
# report success. Project-scoped mutation serialization ensures one observes the
# other's committed result instead of silently last-writer-wins clobbering it.
concurrent_seed="$(cat <<'EOF'
*** Begin Patch
*** Add File: concurrent.txt
+base
*** End Patch
EOF
)"
call_tool apply_patch 105 "$(jq -cn --arg input "$concurrent_seed" '{input:$input}')" "$run_root/concurrent-seed.json"
patch_left="$(cat <<'EOF'
*** Begin Patch
*** Update File: concurrent.txt
@@
-base
+left
*** End Patch
EOF
)"
patch_right="$(cat <<'EOF'
*** Begin Patch
*** Update File: concurrent.txt
@@
-base
+right
*** End Patch
EOF
)"
(call_tool apply_patch 106 "$(jq -cn --arg input "$patch_left" '{input:$input}')" "$run_root/concurrent-left.json") &
left_pid=$!
(call_tool apply_patch 107 "$(jq -cn --arg input "$patch_right" '{input:$input}')" "$run_root/concurrent-right.json") &
right_pid=$!
wait "$left_pid"
wait "$right_pid"
successes="$(jq -s '[.[] | select(.result.structuredContent.applied == true)] | length' "$run_root/concurrent-left.json" "$run_root/concurrent-right.json")"
[[ "$successes" == 1 ]]
call_tool read_file 108 '{"path":"concurrent.txt"}' "$run_root/concurrent-read.json"
jq -e '.result.structuredContent.content | test("left|right")' "$run_root/concurrent-read.json" >/dev/null

call_tool skills_list 11 '{}' "$run_root/skills.json"
grep -q 'integration smoke checks' "$run_root/skills.json"
grep -q 'operator-smoke' "$run_root/skills.json"
gateway_skill_name="$(jq -r '.result.structuredContent.skills[] | select(.scope == "gateway") | .name' "$run_root/skills.json")"
[[ -n "$gateway_skill_name" ]]
[[ "$gateway_skill_name" == __mcp_gateway_* ]]
call_tool skills_read 12 '{"name":"DEMO","limit":65536}' "$run_root/skill-read.json"
grep -q '# Demo skill' "$run_root/skill-read.json"
call_tool skills_read 13 '{"name":"demo","resource":"references/api.md"}' "$run_root/skill-resource.json"
grep -q 'SKILL_RESOURCE_OK' "$run_root/skill-resource.json"
call_tool skills_read 75 '{"name":"demo","resource":"./references/api.md"}' "$run_root/skill-resource-curdir.json"
grep -q 'SKILL_RESOURCE_OK' "$run_root/skill-resource-curdir.json"
# Nested repo skill discovery mirrors Codex CWD ancestry when path is supplied.
# The canonical .agents root wins a same-level .codex compatibility alias, while
# a unique .codex skill remains available. Sibling repo roots stay out of scope.
call_tool skills_list 84 '{"path":"services/api/src/lib.rs"}' "$run_root/nested-skills.json"
grep -q 'CANONICAL_NESTED_SKILL' "$run_root/nested-skills.json"
grep -q 'CODEX_ALIAS_COMPAT_SKILL' "$run_root/nested-skills.json"
! grep -q 'ALIAS_SHOULD_LOSE' "$run_root/nested-skills.json"
! grep -q 'SIBLING_SKILL_SHOULD_NOT_APPEAR' "$run_root/nested-skills.json"
call_tool skills_read 85 '{"name":"nested-deploy","path":"services/api/src/lib.rs"}' "$run_root/nested-skill-read.json"
grep -q '# Nested deploy skill' "$run_root/nested-skill-read.json"
call_tool skills_read 86 '{"name":"legacy-compat","path":"services/api/src/lib.rs"}' "$run_root/compat-skill-read.json"
grep -q 'CODEX_ALIAS_COMPAT_SKILL' "$run_root/compat-skill-read.json"
call_tool skills_read 43 "$(jq -cn --arg name "$gateway_skill_name" '{name:$name,limit:65536}')" "$run_root/gateway-skill.json"
grep -q 'mock_echo' "$run_root/gateway-skill.json"
call_tool skills_read 97 "$(jq -cn --arg name "$gateway_skill_name" '{name:$name,resource:"functions/mock_echo.json"}')" "$run_root/gateway-function-resource.json"
jq -e '.result.structuredContent.resource == "functions/mock_echo.json" and (.result.structuredContent.content | contains("mock_echo")) and (.result.structuredContent.content | contains("input_schema"))' "$run_root/gateway-function-resource.json" >/dev/null
call_tool skills_read 45 '{"name":"missing-smoke-skill"}' "$run_root/missing-skill.json"
jq -e '.result.isError == true and .result.structuredContent == null and (.result.content[0].text | startswith("FILE_NOT_FOUND:"))' "$run_root/missing-skill.json" >/dev/null
grep -q 'Available examples:' "$run_root/missing-skill.json"

call_tool exec_command 14 '{"cmd":"printf ready; read line; printf got:%s \"$line\"","yield_time_ms":250,"timeout_ms":10000}' "$run_root/exec.json"
session="$(jq -r '.result.structuredContent.session_id // empty' "$run_root/exec.json")"
[[ -n "$session" ]]
other_meta='"openai/subject":"usr_other","openai/session":"conv_other","io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}'
# A turn reference is continuity metadata for one ChatGPT subject, not a bearer
# capability that lets another user join the referenced effective project.
call_tool_with_meta chatgpt_turn_init 51 "$(jq -cn --arg previous "$fourth_turn_ref" '{previous_turn_ref:$previous}')" "$other_meta" "$run_root/cross-subject-ref.json"
jq -e '.result.isError != true and .result.structuredContent.status == "soft_error" and .result.structuredContent.agent_action == "stop_current_turn" and .result.structuredContent.initialized == false and .result.structuredContent.turn_ref == null and .result.structuredContent.soft_error.code == "TURN_REF_NOT_FOUND" and .result.structuredContent.soft_error.retry_on_next_user_turn == true and (.result.content[0].text | startswith("STOP_CURRENT_TURN:"))' "$run_root/cross-subject-ref.json" >/dev/null
# The harness now simulates that subject's next user turn, where starting its own
# project is valid because it has no prior successful CodexBridge turn reference.
call_tool_with_meta chatgpt_turn_init 52 '{"project_key":"other-workspace"}' "$other_meta" "$run_root/other-project.json"
jq -e '.result.structuredContent.workspace_state == "new" and .result.structuredContent.instructions_changed == true and .result.structuredContent.state_changed == true' "$run_root/other-project.json" >/dev/null
call_tool_with_meta write_stdin 53 "$(jq -cn --arg id "$session" '{session_id:$id,chars:""}')" "$other_meta" "$run_root/cross-project-session.json"
jq -e '.result.isError == true and .result.structuredContent == null and (.result.content[0].text | startswith("FILE_NOT_FOUND:"))' "$run_root/cross-project-session.json" >/dev/null
call_tool write_stdin 15 "$(jq -cn --arg id "$session" '{session_id:$id,chars:"hello\n",close_stdin:true,yield_time_ms:5000}')" "$run_root/stdin.json"
grep -q 'got:hello' "$run_root/stdin.json"
# One-shot stdin + EOF must let stdin-consuming CLIs/subagents finish without a
# second write_stdin round trip or an open-pipe hang.
call_tool exec_command 1008 '{"cmd":"cat","stdin":"SUBAGENT_EOF_OK\n","close_stdin":true,"yield_time_ms":5000}' "$run_root/oneshot-stdin-eof.json"
jq -e '.result.structuredContent.exit_code == 0 and .result.structuredContent.completion_reason == "exited" and .result.structuredContent.session_id == null' "$run_root/oneshot-stdin-eof.json" >/dev/null
grep -q 'SUBAGENT_EOF_OK' "$run_root/oneshot-stdin-eof.json"
call_tool exec_command 1016 '{"cmd":"cat","extensions":{"stdin":"EXTENSION_EOF_OK\n","close_stdin":true},"yield_time_ms":5000}' "$run_root/extension-stdin-eof.json"
jq -e '.result.structuredContent.exit_code == 0 and .result.structuredContent.completion_reason == "exited"' "$run_root/extension-stdin-eof.json" >/dev/null
grep -q 'EXTENSION_EOF_OK' "$run_root/extension-stdin-eof.json"
call_tool exec_command 1017 '{"cmd":"printf UNKNOWN_EXTENSION_OK","extensions":{"future_option":{"nested":true}},"yield_time_ms":5000}' "$run_root/unknown-extension.json"
jq -e '.result.structuredContent.exit_code == 0 and .result.structuredContent.completion_reason == "exited"' "$run_root/unknown-extension.json" >/dev/null
grep -q 'UNKNOWN_EXTENSION_OK' "$run_root/unknown-extension.json"
call_tool exec_command 1018 '{"cmd":"cat","stdin":"TYPED_EXTENSION_PRECEDENCE\n","close_stdin":true,"extensions":{"stdin":123,"close_stdin":"invalid"},"yield_time_ms":5000}' "$run_root/typed-extension-precedence.json"
jq -e '.result.structuredContent.exit_code == 0 and .result.structuredContent.completion_reason == "exited"' "$run_root/typed-extension-precedence.json" >/dev/null
grep -q 'TYPED_EXTENSION_PRECEDENCE' "$run_root/typed-extension-precedence.json"
call_tool exec_command 1019 '{"cmd":"cat","extensions":{"stdin":123},"yield_time_ms":5000}' "$run_root/invalid-exec-extension.json"
jq -e '.result.isError == true and (.result.content[0].text | startswith("INVALID_INPUT:"))' "$run_root/invalid-exec-extension.json" >/dev/null
call_tool write_stdin 1020 '{"session_id":"missing-session","extensions":{"wait_for_exit_ms":"invalid"}}' "$run_root/invalid-stdin-extension.json"
jq -e '.result.isError == true and (.result.content[0].text | startswith("INVALID_INPUT:"))' "$run_root/invalid-stdin-extension.json" >/dev/null
call_tool exec_command 1007 '{"cmd":"limit=$(ulimit -u 2>/dev/null || printf unsupported); if [ \"$limit\" = 128 ]; then printf NPROC_BAD:%s \"$limit\"; exit 99; fi; printf NPROC_OK:%s \"$limit\"","yield_time_ms":5000}' "$run_root/nproc-limit.json"
jq -e '.result.structuredContent.exit_code == 0' "$run_root/nproc-limit.json" >/dev/null
grep -q 'NPROC_OK:' "$run_root/nproc-limit.json"
call_tool write_stdin 54 '{"session_id":"missing-session","chars":""}' "$run_root/missing-session.json"
jq -e '.result.isError == true and .result.structuredContent == null and (.result.content[0].text | startswith("FILE_NOT_FOUND:"))' "$run_root/missing-session.json" >/dev/null
call_tool exec_command 76 '{"cmd":"exit 7","yield_time_ms":1000}' "$run_root/nonzero-exit.json"
jq -e '.result.structuredContent.exit_code == 7 and .result.structuredContent.completion_reason == "exited" and .result.structuredContent.timed_out == false and .result.structuredContent.deadline_exceeded == false' "$run_root/nonzero-exit.json" >/dev/null
# If the deadline and normal process exit are adjacent, the one-second
# completion grace preserves the real exit status while explicitly recording
# that the requested deadline was crossed. It must not synthesize exit -1.
call_tool exec_command 1009 '{"cmd":"sleep 0.3; exit 0","timeout_ms":250,"yield_time_ms":2000}' "$run_root/timeout-boundary.json"
jq -e '.result.structuredContent.exit_code == 0 and .result.structuredContent.completion_reason == "exited" and .result.structuredContent.deadline_exceeded == true and .result.structuredContent.timed_out == false' "$run_root/timeout-boundary.json" >/dev/null
call_tool exec_command 77 '{"cmd":"sleep 2","timeout_ms":250,"yield_time_ms":1500}' "$run_root/process-timeout.json"
jq -e '.result.structuredContent.timed_out == true and .result.structuredContent.deadline_exceeded == true and .result.structuredContent.completion_reason == "timed_out" and (.result.structuredContent.exit_code == null or .result.structuredContent.exit_code >= 0)' "$run_root/process-timeout.json" >/dev/null

# Chunked output is byte-cursored: each response delivers its range once, and
# a lost response can be recovered with since_output_offset instead of
# re-running the command.
call_tool exec_command 1013 '{"cmd":"printf CHUNK_A_; sleep 1.1; printf CHUNK_B","yield_time_ms":300,"timeout_ms":10000}' "$run_root/chunk-start.json"
chunk_session="$(jq -r '.result.structuredContent.session_id // empty' "$run_root/chunk-start.json")"
[[ -n "$chunk_session" ]]
jq -e '.result.structuredContent.output == "CHUNK_A_" and .result.structuredContent.output_offset == 0 and .result.structuredContent.output_next_offset == 8' "$run_root/chunk-start.json" >/dev/null
sleep 1.5
call_tool write_stdin 1014 "$(jq -cn --arg id "$chunk_session" '{session_id:$id}')" "$run_root/chunk-rest.json"
jq -e '.result.structuredContent.completion_reason == "exited" and .result.structuredContent.output == "CHUNK_B" and .result.structuredContent.output_offset == 8 and .result.structuredContent.output_next_offset == 15' "$run_root/chunk-rest.json" >/dev/null
call_tool write_stdin 1015 "$(jq -cn --arg id "$chunk_session" '{session_id:$id,extensions:{since_output_offset:0}}')" "$run_root/chunk-replay.json"
jq -e '.result.structuredContent.completion_reason == "exited" and .result.structuredContent.output == "CHUNK_A_CHUNK_B" and .result.structuredContent.output_offset == 0' "$run_root/chunk-replay.json" >/dev/null
call_tool write_stdin 1021 "$(jq -cn --arg id "$chunk_session" '{session_id:$id,since_output_offset:0,extensions:{since_output_offset:"invalid"}}')" "$run_root/chunk-typed-precedence.json"
jq -e '.result.structuredContent.completion_reason == "exited" and .result.structuredContent.output == "CHUNK_A_CHUNK_B" and .result.structuredContent.output_offset == 0' "$run_root/chunk-typed-precedence.json" >/dev/null

# A finished response that is presentation-truncated must keep its recovery
# handle even if a client first polls without a replay cursor. The subsequent
# explicit replay must still recover the retained output instead of requiring
# the command to be re-run.
call_tool exec_command 1022 '{"cmd":"printf RECOVERY_START_; i=0; while [ $i -lt 400 ]; do printf payload-%04d- $i; i=$((i+1)); done; printf _RECOVERY_END","yield_time_ms":5000,"max_output_tokens":8}' "$run_root/finished-truncated-start.json"
recovery_session="$(jq -r '.result.structuredContent.session_id // empty' "$run_root/finished-truncated-start.json")"
[[ -n "$recovery_session" ]]
jq -e '.result.structuredContent.completion_reason == "exited" and .result.structuredContent.truncated == true' "$run_root/finished-truncated-start.json" >/dev/null
call_tool write_stdin 1023 "$(jq -cn --arg id "$recovery_session" '{session_id:$id}')" "$run_root/finished-truncated-cursorless.json"
jq -e --arg id "$recovery_session" '.result.structuredContent.completion_reason == "exited" and .result.structuredContent.output == "" and .result.structuredContent.session_id == $id and (.result.structuredContent.continuation | contains("remains retained for replay"))' "$run_root/finished-truncated-cursorless.json" >/dev/null
call_tool write_stdin 1024 "$(jq -cn --arg id "$recovery_session" '{session_id:$id,extensions:{since_output_offset:0}}')" "$run_root/finished-truncated-replay.json"
jq -e '.result.structuredContent.completion_reason == "exited" and .result.structuredContent.output_offset == 0 and .result.structuredContent.session_id == null' "$run_root/finished-truncated-replay.json" >/dev/null
grep -q 'RECOVERY_START_' "$run_root/finished-truncated-replay.json"
grep -q '_RECOVERY_END' "$run_root/finished-truncated-replay.json"

# Replay from a cursor inside an evicted middle region must resume at the first
# retained tail byte and explicitly disclose the omitted gap. The response must
# never fabricate stale head bytes as if the retained buffer were contiguous.
call_tool exec_command 1026 '{"cmd":"printf E4_HEAD_UNIQUE; head -c 5000000 /dev/zero | tr \\0 X; printf E4_TAIL_UNIQUE","yield_time_ms":5000,"max_output_tokens":128}' "$run_root/evicted-middle-start.json"
evicted_session="$(jq -r '.result.structuredContent.session_id // empty' "$run_root/evicted-middle-start.json")"
[[ -n "$evicted_session" ]]
jq -e '.result.structuredContent.completion_reason == "exited" and .result.structuredContent.truncated == true' "$run_root/evicted-middle-start.json" >/dev/null
call_tool write_stdin 1027 "$(jq -cn --arg id "$evicted_session" '{session_id:$id,since_output_offset:2500000,max_output_tokens:128}')" "$run_root/evicted-middle-replay.json"
jq -e '.result.structuredContent.completion_reason == "exited" and .result.structuredContent.output_offset > 2500000 and .result.structuredContent.truncated == true' "$run_root/evicted-middle-replay.json" >/dev/null
grep -q 'buffered bytes omitted' "$run_root/evicted-middle-replay.json"
grep -q 'E4_TAIL_UNIQUE' "$run_root/evicted-middle-replay.json"
! grep -q 'E4_HEAD_UNIQUE' "$run_root/evicted-middle-replay.json"

# Real PTY path: allocate a terminal, verify the session is interactive, resize
# it while sending input, and collect the rendered terminal snapshot.
call_tool exec_command 109 '{"cmd":"printf PTY_READY; read line; echo PTY_GOT:$line","tty":true,"rows":20,"cols":70,"yield_time_ms":250,"timeout_ms":10000}' "$run_root/pty-start.json"
pty_session="$(jq -r '.result.structuredContent.session_id // empty' "$run_root/pty-start.json")"
[[ -n "$pty_session" ]]
jq -e '.result.structuredContent.tty == true and (.result.structuredContent.terminal_snapshot | type == "object")' "$run_root/pty-start.json" >/dev/null
call_tool write_stdin 110 "$(jq -cn --arg id "$pty_session" '{session_id:$id,chars:"hello\n",rows:30,cols:100,close_stdin:true,yield_time_ms:5000}')" "$run_root/pty-finish.json"
jq -e '.result.structuredContent.tty == true and .result.structuredContent.completion_reason == "exited" and (.result.structuredContent.exit_code != null)' "$run_root/pty-finish.json" >/dev/null
grep -q 'PTY_GOT:hello' "$run_root/pty-finish.json"

# Process-control signals operate on a live process tree and produce a terminal
# result rather than leaving a dangling session.
call_tool exec_command 111 '{"cmd":"sleep 30","yield_time_ms":250,"timeout_ms":10000}' "$run_root/signal-start.json"
signal_session="$(jq -r '.result.structuredContent.session_id // empty' "$run_root/signal-start.json")"
[[ -n "$signal_session" ]]
call_tool write_stdin 112 "$(jq -cn --arg id "$signal_session" '{session_id:$id,signal:"interrupt",extensions:{wait_for_exit_ms:5000}}')" "$run_root/signal-finish.json"
jq -e '.result.structuredContent.completion_reason == "signaled" and .result.structuredContent.requested_signal == "interrupt" and .result.structuredContent.timed_out == false and .result.structuredContent.session_id == null and (.result.structuredContent.exit_code == null or .result.structuredContent.exit_code >= 0)' "$run_root/signal-finish.json" >/dev/null

# Graceful shutdown can be evidenced in one lifecycle call: deliver SIGTERM,
# wait for the child to run its handler, drain the final log line, and return
# the terminal status instead of forcing a later polling/inspection round.
call_tool exec_command 1011 '{"cmd":"trap '\''printf GRACEFUL_SHUTDOWN_OK\\n; exit 0'\'' TERM; printf SERVICE_READY\\n; while :; do sleep 1; done","yield_time_ms":250,"timeout_ms":10000}' "$run_root/graceful-start.json"
graceful_session="$(jq -r '.result.structuredContent.session_id // empty' "$run_root/graceful-start.json")"
[[ -n "$graceful_session" ]]
grep -q 'SERVICE_READY' "$run_root/graceful-start.json"
call_tool write_stdin 1012 "$(jq -cn --arg id "$graceful_session" '{session_id:$id,signal:"terminate",wait_for_exit_ms:5000}')" "$run_root/graceful-finish.json"
jq -e '.result.structuredContent.completion_reason == "exited" and .result.structuredContent.requested_signal == "terminate" and .result.structuredContent.exit_code == 0 and .result.structuredContent.session_id == null and .result.structuredContent.timed_out == false' "$run_root/graceful-finish.json" >/dev/null
grep -q 'GRACEFUL_SHUTDOWN_OK' "$run_root/graceful-finish.json"

# Output can be bounded by an approximate token window independently of the
# byte-retention cap; the result must disclose that truncation happened.
call_tool exec_command 113 '{"cmd":"head -c 200000 /dev/zero | base64 -w0","yield_time_ms":5000,"max_output_tokens":128}' "$run_root/token-window.json"
jq -e '.result.structuredContent.truncated == true and (.result.structuredContent.original_token_count > 128) and (.result.structuredContent.output | length > 0)' "$run_root/token-window.json" >/dev/null

# A session that exits after the initial yield must not accept later mutation or
# signals. It may already have been cleaned from the registry; both safe outcomes
# are acceptable and neither can target a reused PID.
call_tool exec_command 98 '{"cmd":"sleep 0.4; printf finished","yield_time_ms":250,"timeout_ms":5000}' "$run_root/finish-later.json"
finished_session="$(jq -r '.result.structuredContent.session_id // empty' "$run_root/finish-later.json")"
[[ -n "$finished_session" ]]
sleep 1
call_tool write_stdin 99 "$(jq -cn --arg id "$finished_session" '{session_id:$id,chars:"x",signal:"interrupt"}')" "$run_root/finished-session-mutation.json"
jq -e '.result.isError == true and .result.structuredContent == null and ((.result.content[0].text | startswith("PROCESS_FINISHED:")) or (.result.content[0].text | startswith("FILE_NOT_FOUND:")))' "$run_root/finished-session-mutation.json" >/dev/null

call_tool exec_command 17 '{"cmd":"printf iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg== | base64 -d > pixel.png"}' "$run_root/image-write.json"
call_tool view_image 18 '{"path":"pixel.png"}' "$run_root/image.json"
grep -q 'image/png' "$run_root/image.json"
call_tool exec_command 69 '{"cmd":"printf not-an-image > fake.png"}' "$run_root/fake-image-write.json"
call_tool view_image 70 '{"path":"fake.png"}' "$run_root/fake-image.json"
jq -e '.result.isError == true and .result.structuredContent == null and (.result.content[0].text | startswith("INVALID_INPUT:"))' "$run_root/fake-image.json" >/dev/null
call_tool view_image 71 '{"path":"missing.png"}' "$run_root/missing-image.json"
jq -e '.result.isError == true and .result.structuredContent == null and (.result.content[0].text | startswith("FILE_NOT_FOUND:"))' "$run_root/missing-image.json" >/dev/null
call_tool view_image 72 '{"path":"../outside.png"}' "$run_root/traversal-image.json"
jq -e '.result.isError == true and .result.structuredContent == null and (.result.content[0].text | startswith("PATH_OUTSIDE_WORKSPACE:"))' "$run_root/traversal-image.json" >/dev/null

call_tool remember 19 '{"key":"  smoke  ","value":"remembered"}' "$run_root/remember.json"
jq -e '.result.structuredContent.key == "smoke"' "$run_root/remember.json" >/dev/null
call_tool recall 20 '{"key":" smoke "}' "$run_root/recall.json"
grep -q 'remembered' "$run_root/recall.json"
call_tool update_plan 21 '{"plan":[{"step":"smoke","status":"completed"}],"explanation":"   "}' "$run_root/plan.json"
jq -e '.result.structuredContent.plan.items[0].status == "completed" and .result.structuredContent.plan.explanation == null' "$run_root/plan.json" >/dev/null
call_tool remember 100 '{"key":"alpha","value":"one"}' "$run_root/remember-alpha.json"
call_tool remember 101 '{"key":"beta","value":"two"}' "$run_root/remember-beta.json"
call_tool remember 102 '{"key":"gamma","value":"three"}' "$run_root/remember-gamma.json"
call_tool recall 103 '{"max_results":2,"include_plan":true}' "$run_root/recall-page-1.json"
jq -e '.result.structuredContent.notes | length == 2' "$run_root/recall-page-1.json" >/dev/null
jq -e '.result.structuredContent.truncated == true and .result.structuredContent.next_offset == 2 and (.result.structuredContent.plan.items[0].step == "smoke") and (.result.structuredContent.continuation | contains("offset=2"))' "$run_root/recall-page-1.json" >/dev/null
recall_snapshot_hash="$(jq -r '.result.structuredContent.snapshot_hash' "$run_root/recall-page-1.json")"
call_tool recall 1025 '{"offset":2,"max_results":2}' "$run_root/recall-missing-snapshot.json"
jq -e '.result.isError == true and (.result.content[0].text | startswith("INVALID_INPUT:")) and (.result.content[0].text | contains("snapshot_hash is required"))' "$run_root/recall-missing-snapshot.json" >/dev/null
call_tool recall 107 "$(jq -cn --arg hash "$recall_snapshot_hash" '{offset:2,max_results:2,snapshot_hash:$hash,extensions:{snapshot_hash:123}}')" "$run_root/recall-typed-precedence.json"
jq -e '.result.isError != true and .result.structuredContent.offset == 2 and (.result.structuredContent.notes | length) >= 1' "$run_root/recall-typed-precedence.json" >/dev/null
call_tool recall 108 '{"offset":2,"max_results":2,"extensions":{"snapshot_hash":123}}' "$run_root/recall-invalid-extension.json"
jq -e '.result.isError == true and (.result.content[0].text | startswith("INVALID_INPUT:"))' "$run_root/recall-invalid-extension.json" >/dev/null
call_tool recall 104 "$(jq -cn --arg hash "$recall_snapshot_hash" '{offset:2,max_results:2,include_plan:true,extensions:{snapshot_hash:$hash}}')" "$run_root/recall-page-2.json"
jq -e '.result.structuredContent.offset == 2 and (.result.structuredContent.notes | length) >= 1 and (.result.structuredContent.plan.items[0].status == "completed")' "$run_root/recall-page-2.json" >/dev/null
call_tool remember 105 '{"key":"delta","value":"four"}' "$run_root/remember-delta.json"
call_tool recall 106 "$(jq -cn --arg hash "$recall_snapshot_hash" '{offset:2,max_results:2,extensions:{snapshot_hash:$hash}}')" "$run_root/recall-stale.json"
jq -e '.result.isError == true and (.result.content[0].text | startswith("PAGINATION_STALE:"))' "$run_root/recall-stale.json" >/dev/null

# Keep an interactive process live while the daemon receives SIGTERM. The
# daemon shutdown path must terminate it, wait for its process/drain tasks, and
# publish server_stopped with zero remaining processes before exiting.
call_tool exec_command 200 '{"cmd":"printf SHUTDOWN_PROCESS_READY\\n; trap '\''exit 0'\'' TERM; while :; do sleep 1; done","yield_time_ms":250,"timeout_ms":30000}' "$run_root/shutdown-process.json"
shutdown_session="$(jq -r '.result.structuredContent.session_id // empty' "$run_root/shutdown-process.json")"
[[ -n "$shutdown_session" ]]
grep -q 'SHUTDOWN_PROCESS_READY' "$run_root/shutdown-process.json"

bad_status="$(curl -sS -o "$run_root/bad.json" -w '%{http_code}' -H 'content-type: application/json' --data '{}' "http://127.0.0.1:${port}/incorrect-token-value/mcp")"
[[ "$bad_status" == 401 ]]
grep -q 'AUTH_FAILED' "$run_root/bad.json"

curl -sS -D "$run_root/legacy-init.headers" \
  -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
  --data '{"jsonrpc":"2.0","id":24,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"legacy-smoke","version":"1"}}}' \
  "$endpoint" >"$run_root/legacy-init.sse"
legacy_session="$(awk 'BEGIN{IGNORECASE=1} /^mcp-session-id:/{gsub("\r",""); print $2}' "$run_root/legacy-init.headers")"
[[ -n "$legacy_session" ]]
grep -q '2025-03-26' "$run_root/legacy-init.sse"

# Legacy sessions are bounded independently from modern stateless requests.
legacy_busy_status="$(curl -sS -o "$run_root/legacy-busy.json" -w '%{http_code}' \
  -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
  --data '{"jsonrpc":"2.0","id":25,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"legacy-overflow","version":"1"}}}' \
  "$endpoint")"
[[ "$legacy_busy_status" == 503 ]]
grep -q 'SERVER_BUSY' "$run_root/legacy-busy.json"

cleanup
server_pid=""
logs="$workspace/.metadata/logs"
[[ -s "$logs/tool-calls.log" ]]
[[ -s "$logs/plans.log" ]]
[[ -s "$logs/rust-agent.log" ]]
grep -q '"tool":"chatgpt_turn_init"' "$logs/tool-calls.log"
grep -q '"tool":"update_plan"' "$logs/tool-calls.log"
grep -q '"event":"plan_updated"' "$logs/plans.log"
grep -q '"event":"mcp_request_started"' "$logs/rust-agent.log"
grep -F -q 'printf ready; read line' "$logs/tool-calls.log"
! grep -q '\[REDACTED_CONTENT\]' "$logs/tool-calls.log"
! grep -R -F -q "$token" "$logs" "$run_root/server.out"
! grep -R -E -q 'usr_test|conv_test' "$logs" "$run_root/server.out"
grep -q '"event":"server_stopped"' "$logs/rust-agent.log"
jq -e 'select(.event == "server_stopped") | .remaining_processes == 0' "$logs/rust-agent.log" >/dev/null
project_dirs="$(find "$workspace" -mindepth 1 -maxdepth 1 -type d ! -name .metadata -printf '%f\n' | sort)"
[[ "$project_dirs" == $'other-workspace\nproduction-stress-test' ]]

printf 'CodexBridge MCP smoke test PASS\n'
