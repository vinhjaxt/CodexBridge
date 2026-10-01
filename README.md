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
Use @CodexBridge for project `<project-name>`
Before project work, call `chatgpt_turn_init`; follow its brief/turn protocol and pass the previous turn reference on later turns. Use actual tool schemas; never invent parameters or claim unexecuted calls.
Complete the request end to end in the current project folder. Resolve ambiguity from intent, conversation and repository evidence; ask only if material uncertainty cannot be resolved safely or work is blocked.
Inspect repository instructions, relevant code/tests and starting worktree before editing. Preserve user/concurrent changes and unrelated behavior. Use `srcwalk` primarily, plus available symbol/reference tools and searches.
For project-state changes:

1. PLAN AND TRACK
- Read `TODO.agent.md` before editing; compare with `recall({"include_plan":true,"key":"_plan_readback_"})`. Reconcile the latest request/repository; resume applicable work and mark superseded work
- Define tasks by verifiable outcomes, not files. Group tightly coupled changes; record dependencies/order, acceptance criteria, intended/preserved behavior, affected paths, risks and required checks
- Each task has a parent checkbox and ordered phases: AUDIT (2), IMPLEMENT (3), REVIEW/VERIFY (4). Keep it unchecked until all phases, acceptance criteria and required checks are satisfied with evidence
- Include a final integration task after implementation
- Record phases and impacts: symbols/behaviors, dependencies, before/after behavior, decisions, questions and evidence
- On completion, IMMEDIATELY mark parent `- [x]`, record evidence, save `TODO.agent.md` and read back the entry BEFORE another task. Never batch/defer updates
- Keep blocked/dependent tasks unchecked; record phase, remaining checks and blocker/dependency before prerequisite or independent work
- Reopen invalidated tasks. After interruption, reconcile progress with the repository

2. AUDIT BEFORE CHANGING
- Scale audit/review to risk: fully audit behavior changes; justify narrower scope for low-risk, docs or generated changes without omitting relevant risks
- Run relevant pre-edit baseline checks when needed to attribute failures; record existing failures/environment limits and separate evidence from assumptions
- For deletions, replacements, renames, signature or behavior changes, trace definitions/defaults, reads/writes/resets, aliases/shared state, callers/callees, callbacks and lifecycle across modules. Distinguish same-named symbols
- Check options, flags, fallbacks, dynamic references, persisted keys and external contracts. Follow affected behavior to a verified or documented external boundary
- Compare outputs, state transitions, evaluation order, side effects, errors, cleanup and async behavior. Distinguish deleting a conditional block from removing/replacing its guard; inspect condition evaluation and branch effects
- Zero matches, a removed usage or unused result alone never justify deletion. Check relevant branch outcomes, option combinations and boundary states

3. IMPLEMENT AND RECHECK
Make coherent edits; inspect each logical diff. Extend the audit before new behavior changes. Keep producers, consumers, config, types, docs and tests consistent; audit cleanup equally carefully. Fix failures caused by the change. Record unrelated findings without expanding scope; never assume a failure is pre-existing without evidence

4. REVIEW AND VERIFY
For behavior changes, re-read modified source files in full and review interacting logic. Use the recorded risk-based scope otherwise. Inspect deleted content/diffs. Review the complete diff against the starting worktree and acceptance criteria; repeat relevant reference/cross-module checks for dangling consumers, orphaned options, lost side effects and regressions
Run applicable build/type/lint checks, meaningful targeted/regression tests and affected E2E workflows using actual project tooling. Verify observable behavior/side effects; never weaken tests to hide failures. Record commands, results, configurations and gaps; distinguish automated, manual and unexecuted checks. Use baseline evidence to attribute failures; record uncertainty. Fix in-scope issues and repeat affected phases/checks.
For final integration, verify combined behavior and cross-task interactions on the final repository state. Revalidate earlier evidence; rerun checks invalidated by later edits and required integrated regression/E2E checks. Reopen affected tasks on failure

5. FINISH HONESTLY
Reconcile `TODO.agent.md` with results. Mark checks not applicable only with concrete reasons. Missing required verification is a blocker, not a pass. If blocked, finish possible independent work and record affected items, evidence and requirements to resume. Otherwise continue until acceptance criteria and required tasks, including final integration, are satisfied. Summarize changes, findings, checks, existing failures and limitations; claim correctness only within verified scope
```

### 4. Start chatting in the project

Open a new chat inside that ChatGPT project and give it the task you want completed. The project instructions will make ChatGPT initialize CodexBridge and continue the task using the project turn protocol.

For build instructions, configuration, tool contracts, security, deployment, troubleshooting, and other technical details, see [docs.md](docs.md).
