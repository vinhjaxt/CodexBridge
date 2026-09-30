# CodexBridge

CodexBridge is a Streamable HTTP MCP coding-agent bridge for ChatGPT/Codex-style workflows.

Inspired by: https://github.com/hypnguyen1209/codex-free

## Quick start

### 1. Run CodexBridge and get the MCP server URL

Run the binary against the workspace that will contain your projects:

```bash
./codex-bridge /workspace
```

The workspace argument is optional and defaults to `/workspace`.

On first start, CodexBridge creates an authentication token at `<workspace>/.metadata/auth-token`. With the default settings, the MCP server URL is:

```text
http://<host>:3000/<token>/mcp
```

Use HTTPS through a trusted reverse proxy or tunnel when connecting ChatGPT over the internet.

### 2. Create and connect the ChatGPT plugin

In ChatGPT web:

1. Enable Developer mode if required: **Settings → Apps → Advanced Settings**.
2. Open **Settings → Apps → Create** (or **Workspace settings → Apps → Create**, depending on your workspace).
3. Name the integration **CodexBridge**.
4. Set the MCP endpoint to the server URL from step 1. Do not add separate authentication when using the default path-token URL; the token is already embedded in the endpoint.
5. Select **Scan Tools**, wait for discovery to finish, then select **Create**.
6. Connect/enable **CodexBridge** in ChatGPT.
7. Set its **Permissions** to **Allow all**.

### 3. Create a ChatGPT project

Create a ChatGPT project and use these project instructions, replacing `<project-name>` with the CodexBridge project name you want to use:

```text
Use @CodexBridge for project `<project-name>`.

Before doing any project work, call `chatgpt_turn_init` to initialize or join this CodexBridge project, then follow the returned brief and project instructions. On later turns, follow the CodexBridge turn protocol and automatically pass the previous turn reference.

Task: Work directly in the current project folder and complete the user's request end to end. Keep iterating until the task is finished; do not stop at a partial solution. Resolve ordinary ambiguity from repository evidence instead of asking the user, unless continuing would be unsafe or genuinely impossible.

Before changing anything, inspect the relevant files, code, tests, and current worktree to understand the context. Use `srcwalk` as the primary tool for navigating, finding, and reading source code.

After making changes, re-read every modified file and run the relevant verification or tests. Continue fixing any issues until the requested task is complete or a genuine external blocker prevents further progress.
```

or

```
Use @CodexBridge for project `<project-name>`.

Before any project work, call `chatgpt_turn_init` to initialize/join this CodexBridge project and follow the returned brief/instructions. On later turns, follow the CodexBridge turn protocol and pass the previous turn reference.

Task: Work directly in the current project folder and complete the user's request end to end. Keep iterating until finished; do not stop at a partial solution. Resolve ordinary ambiguity from repository evidence instead of asking the user by deeply analyzing user intent, previous prompts and responses, and the current project state, unless continuing would be unsafe or genuinely impossible.

Before changing anything, inspect relevant files, code, tests, and current worktree, understand the context. Use `srcwalk` as the primary tool for navigating, finding, and reading source code.

For any task that modifies project state (create/update/delete files, code, config, tests, docs, etc.), use `TODO.agent.md` as the durable execution checklist:
- Before editing, first inspect `TODO.agent.md` and compare tasks status with tool `recall({"include_plan":true,"key":"_plan_readback_"})` results. If unfinished items exist, reconcile them with the current repository and resume them before making a new plan.
- Plan the work end to end before implementation: concrete edits, affected areas code paths, dependencies/callers/callees, risks/edge cases, functional-conflict/regression checks, post-edit implementation review, targeted verification, and E2E testing.
- Write the plan to `TODO.agent.md`: summary what user asks, what you will do, granular checkbox tasks, including separate items for implementation, re-reading/reviewing modified code and diff; checking whether changes conflict with or regress other functions, modules, APIs, flows, side effects, invariants, tests, or concurrent/user changes; tests/checks; E2E verification; and final completion confirmation.
- Keep `TODO.agent.md` current. Tick an item only after its work and evidence are complete. If a turn is interrupted, token-limited, or resumed later, unchecked items become the first source of truth for remaining work.
- Do not declare completion while required items remain unchecked unless a genuine external blocker exists; record the blocker in the file.

After changes, re-read full content of every modified file, review the all code, functions in that files, verify implementation, check impact/conflicts/regressions across related functions and behavior, then run planned verification/E2E tests. Continue fixing issues until complete or genuinely blocked.
```

### 4. Start chatting in the project

Open a new chat inside that ChatGPT project and give it the task you want completed. The project instructions will make ChatGPT initialize CodexBridge and continue the task using the project turn protocol.

For build instructions, configuration, tool contracts, security, deployment, troubleshooting, and other technical details, see [docs.md](docs.md).
