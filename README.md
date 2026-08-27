# CodexBridge

CodexBridge is a Streamable HTTP MCP coding-agent bridge for ChatGPT/Codex-style workflows. Its design goal is deliberately small: one compact Codex-like native tool surface, autonomous YOLO execution, isolated persistent conversation projects, durable task state, and no general application config file. Optional upstream MCP aggregation is available through one explicit operator-supplied YAML/JSON file when needed.

Inspired by: https://github.com/hypnguyen1209/codex-free


## Run

The normal invocation is:

```bash
./codex-bridge /workspace
```

The workspace argument is optional:

```bash
./codex-bridge
```

which defaults to `/workspace`.

CodexBridge listens on `0.0.0.0:3000` by default. On first start it creates a random authentication token under the selected workspace root at:

```text
<workspace>/.metadata/auth-token
```

The file is reused across restarts and is restricted to the daemon account on Unix. With the default path-auth mode, the MCP endpoint is:

```text
http://<host>:3000/<token>/mcp
```

Terminate TLS at a reverse proxy or tunnel before exposing the service publicly.

### Build from source

```bash
cargo build --release
./target/release/codex-bridge /workspace
```

Rust 1.88 or newer is required.

## Starting a ChatGPT project

After enabling CodexBridge for the chat, start a named/shared project with:

```text
Use @CodexBridge for project `<project-name>`.

Before doing any project work, initialize/join that project with `chatgpt_turn_init` and follow the brief and project instructions it returns. On later turns, follow the CodexBridge turn protocol and use the previous turn reference automatically.

Task: <what you want done>
```

The explicit `chatgpt_turn_init` bootstrap is recommended for the first prompt because MCP clients are not guaranteed to surface server `instructions` to the model consistently. The user does not need to provide OpenAI subject/session metadata, an MCP session ID, a native project key, an absolute workspace path, or a turn reference manually. After the project is initialized, later user messages can be ordinary task requests; the agent is instructed to carry the previous `[ref:...]` forward automatically.

## Philosophy

- **No general application config file.** Built-in defaults are suitable for normal controlled deployments, while environment variables remain available for resource tuning and stricter fail-closed operation. Upstream MCP aggregation is the one opt-in feature that reads an operator-supplied config file.
- **YOLO by default.** Valid tool calls execute immediately; there is no approval handshake and no shell command allowlist.
- **Small native tool surface.** Duplicate CRUD/compatibility tools are not exposed. General shell work goes through `exec_command`; file edits go through `apply_patch`. Optional upstreams default to gateway mode so a large upstream catalogue costs one dispatcher tool plus one progressively disclosed skill.
- **Codex-style coding-agent behavior.** The base brief tells the model to inspect, act, inspect results, adjust, and verify repeatedly until the requested task mode is complete. Coding work must be completed inline with CodexBridge tools rather than delegated to coding agents/subagents/agent CLIs installed on the host. It also carries dirty-worktree discipline, scope control, verification requirements, AGENTS/skills handling, planning, durable notes, bounded output, and continuation semantics.
- **Isolation where it works, portability where it does not.** Linux auto mode uses Bubblewrap only after a real usability probe succeeds. With the default `MCP_ALLOW_UNSANDBOXED_EXEC=true`, an unavailable Bubblewrap backend falls back to native execution. When Bubblewrap works but Podman cannot operate inside it, Podman alone uses native execution; setting `MCP_ALLOW_UNSANDBOXED_EXEC=false` makes either case fail closed instead.

YOLO does not disable authentication, project separation, structured filesystem-tool confinement, timeouts, concurrency limits, process limits, output caps, or auditing. Native shell fallback is intentionally different: when Bubblewrap is unavailable or bypassed, `exec_command` runs with the daemon account's normal filesystem and network reach, so the outer container/VM/account is the security boundary for shell execution.

## Public tools

CodexBridge exposes exactly 15 native tools:

| Tool | Purpose |
|---|---|
| `chatgpt_turn_init` | Bind/join the project on the first project turn, then synchronize each later project-bearing user turn with an idempotent reference and context-hash check |
| `read_file` | Bounded UTF-8 line reading with lossless same-line byte continuation for oversized/minified lines |
| `list_directory` | Sorted bounded directory listing |
| `tree` | Bounded recursive project tree |
| `glob` | File-name/path discovery |
| `grep` | Regex content search with context and continuation |
| `apply_patch` | Codex `*** Begin Patch` grammar with transactional multi-file patching, context-byte/EOL preservation, and no recursive directory deletion |
| `view_image` | Return project-local PNG/JPEG/GIF/BMP/WebP content after real image decoding succeeds |
| `exec_command` | Shell/process execution, long-running sessions, optional PTY/ConPTY |
| `write_stdin` | Continue, resize, signal, or close an `exec_command` session |
| `skills_list` | Progressive skill catalogue |
| `skills_read` | Read one selected skill/package resource |
| `remember` | Persist/delete a note in small auto-hydrated `active` memory (default) or larger on-demand `archive` history |
| `recall` | Read active or archive notes on demand; archive pages are bounded and continuations are pinned by `snapshot_hash`; optionally include the current plan |
| `update_plan` | Replace the persistent multi-step project plan; an empty `plan` clears it |

There are no public native `exec`/`run_command`, duplicate memory/plan CRUD tools, task CRUD tools, clock tools, Git wrappers, or download helpers. Use `exec_command` for Git, package managers, downloads, container engines, and other commands that do not need their own structured tool. Configured upstream MCP tools are additive and are described below; with no upstream config the exposed surface is exactly the 15 native tools above.

`exec_command`, `write_stdin`, and `recall` also expose an optional open `extensions` object for forward-compatible optional arguments. Typed top-level fields remain the primary contract and take precedence when present. If a supported top-level value is absent, the runtime can consume its equivalent extension key; currently this includes `exec_command.stdin` / `close_stdin`, `write_stdin.since_output_offset` / `wait_for_exit_ms` / `close_stdin`, and `recall.snapshot_hash`. Unknown extension keys are ignored so a newer server can introduce optional capabilities without requiring an immediate top-level schema refresh. A known extension key with an invalid value fails with `INVALID_INPUT`. Clients still need to discover the `extensions` envelope itself at least once before relying on that compatibility path.

The MCP `serverInfo.version` is contract-qualified as `<package-version>+contract.<12-hex>`. The fingerprint is deterministic over the exposed tool names, descriptions, input schemas, and output schemas, so a public tool-contract change changes the advertised server version even when the Cargo package version is unchanged. This is a discovery/cache-invalidation signal for clients and operators, not a guarantee that every external connector will automatically refresh cached metadata.

## Optional upstream MCP aggregation

Upstreams are disabled by default. Set `MCP_UPSTREAM_CONFIG` to a bounded YAML or JSON file when the daemon should connect to operator-approved MCP servers. The file uses the familiar `mcpServers` shape:

```yaml
mcpServers:
  docs:
    command: docs-mcp
    args: [--stdio]
    type: stdio
    mode: gateway
    tools: [search, fetch]

  remote-index:
    type: streamable_http
    url: https://mcp.example.internal/mcp
    mode: direct
```

`gateway` is the default mode. A gateway exposes one `gateway_<sanitized-server>` dispatcher whose `function` enum contains the selected upstream tools. Its generated skill uses the reserved collision-resistant name `__mcp_gateway_<sanitized-server>_<hash8>` and is available through `skills_list` / `skills_read`. The generated skill carries bounded upstream descriptions and schemas; when an inline catalogue is truncated, complete bounded metadata for one function remains available as `functions/<function>.json` through `skills_read`. Upstream metadata is reference material only and does not override system, project, or user instructions.

`direct` is an explicit opt-in for upstreams whose individual tools should be model-visible. Each selected tool is exposed as `upstream_<server>__<tool>`. Direct and gateway calls still require `chatgpt_turn_init`, share the normal global/project concurrency boundary, have a separate upstream concurrency limit and timeout, and are audited. Stdio children start with a small platform-native environment plus only the `env` entries in the upstream config: Unix uses a fixed minimal `PATH`/`LANG`, while Windows keeps a usable native `PATH`, `SystemRoot`/`WINDIR`, `ComSpec`, and temp-directory variables so configured upstreams and their child CLIs resolve normally. Streamable HTTP URLs must be `http`/`https` and may not contain embedded credentials.

The config file is capped at 1 MiB and 64 servers; each upstream catalogue is capped at 512 tools. `MAX_CONCURRENT_UPSTREAM_CALLS`, `UPSTREAM_CALL_TIMEOUT_MS`, and `MAX_GATEWAY_SKILL_BYTES` tune runtime limits. See `examples/upstreams.example.yaml` for a complete small example.

## Conversation projects

The native ChatGPT conversation identity is derived only from MCP request metadata:

```text
openai/subject + openai/session
```

`chatgpt_turn_init` is both the first project-binding boundary and the later per-user-turn synchronization primitive. The native identity above is never supplied as a normal tool argument. On the first project-bearing turn, an optional `project_key` is a validated human alias for explicit sharing/rejoin; otherwise an isolated effective project is created automatically when no continuity reference was supplied. A genuinely new named project uses that alias as its checkout directory name, so `project_key="demo"` creates `<workspace>/demo/`. Aliases are case-insensitive for identity/uniqueness and reject Windows-reserved or normalization-ambiguous names, so variants such as `Foo`/`foo`, `CON`, or a trailing-dot alias cannot become distinct logical projects that collide on a Windows filesystem. A later same-subject branch may inherit an effective project from a valid parent `turn_ref`. If a new conversation supplies an unusable `previous_turn_ref`, CodexBridge does not silently create a different private project: it returns the retryable `PROJECT_KEY_REQUIRED` error unless `project_key` already identifies the intended project. The failed attempt is non-mutating, so the caller can immediately retry `chatgpt_turn_init` with the intended `project_key` before using any other project tool.

The MCP `initialize` response carries only identity-independent coding-agent instructions: the operating brief, exact runtime shell/syntax family, effective sandbox backend, the init requirement, and any globally configured gateway guidance. It intentionally does not read project files, project memory, or project skills before the ChatGPT conversation identity has crossed the `chatgpt_turn_init` boundary.

For project work, the agent calls `chatgpt_turn_init` at the beginning of each new user turn, before other project tools or a project-state-dependent answer. On later turns it should pass the `[ref:...]` token from the nearest preceding CodexBridge assistant final response as `previous_turn_ref`, but that value is now a recoverable continuity hint rather than the sole project locator. If the native conversation is already bound, a missing, stale, cross-branch, or nonexistent ref falls back to the conversation's persisted project and latest usable turn. `PROJECT_KEY_REQUIRED` is the one expected pre-initialization retry path: retry with the intended `project_key`. After a successful synchronization it must not call `chatgpt_turn_init` again until the user sends another message.

Each successful new turn issues a compact server-generated UUIDv7 `turn_ref`. A full returned brief contains the exact final-answer marker for that turn, for example `[ref:r_...]`; compact receipts carry the same marker, and project-related final responses must end with it. For one native conversation, `(native_key, previous_turn_ref)` is unique internally: retrying the same valid parent is idempotent and returns the already-created child `turn_ref` instead of extending the chain twice.

A new ChatGPT conversation under the same `openai/subject` may branch from a valid prior `turn_ref` and inherit that reference's effective project without repeating `project_key`. Turn references are subject-scoped in SQLite, so a reference from another OpenAI subject cannot be used as a project-join capability. If the supplied ref cannot identify a project for an otherwise-unbound conversation, `PROJECT_KEY_REQUIRED` is returned; retrying with the intended `project_key` is sufficient, and the bad ref is ignored for project selection.

The successful structured result is intentionally minimal: `status="synchronized"`, the server-issued `turn_ref`, and only a `brief` and/or `state_update` when that payload must be consumed. Project-resolution metadata, internal hashes, change flags, transport/identity metadata, parent/reuse metadata, and null placeholders are not exposed because they do not change the caller's next action. Soft-stop results contain only `status="soft_error"` plus `soft_error.code` and `soft_error.message`.

Every call still rebuilds two independently hashed layers internally. The **instruction context** contains the runtime environment, skill/gateway catalogues, and project instructions. The **active project state** contains the complete active-memory set plus the complete current plan. Active memory is intentionally small and hard-bounded, so `chatgpt_turn_init` never paginates or truncates it. Archive/history is excluded from this state and its hash because archive changes must not consume turn context automatically. The turn-specific protocol and `turn_ref` are excluded from both hashes. SQLite stores the instruction/state hashes for each turn and compares them with the hashes stored on `previous_turn_ref`; these hashes are server implementation state and are not returned by `chatgpt_turn_init`.

On the first project turn, on a branched conversation, after continuity recovery from a missing/stale/invalid ref, or when the instruction layer changed, `brief` contains the refreshed instruction context, **full active memory**, the **full current plan**, and the turn protocol. Recovery deliberately forces a full brief even when hashes match. If only active memory/current plan changed during a normal valid continuation, `state_update` carries that complete active project state without repeating the full brief. Archive-only writes do not produce `state_update`. If neither instruction nor active state changed, both optional payload fields are omitted.

The transport still does not provide a trustworthy user-message/turn identifier. Therefore CodexBridge can hard-enforce idempotency for duplicate calls that carry the same valid `previous_turn_ref`, but a recovery call that omitted or supplied an unusable ref is anchored to the latest persisted turn for the already-bound native conversation and forces a full brief. CodexBridge still cannot detect a later user message for which the model never calls `chatgpt_turn_init` at all. The “call once at the start of every project-bearing user turn” requirement remains an agent protocol contract; no timing heuristic is used.

Project files live under the configured workspace root. Named projects use the validated alias directly; unnamed/private projects use their opaque effective key:

```text
<workspace>/<project-name>/          # explicit new project_key
<workspace>/<effective-project-key>/ # unnamed/private project
```

while service metadata lives under:

```text
<workspace>/.metadata/
```

The metadata area is not exposed through normal project filesystem tools.

## Persistent state

Conversation bindings, aliases, active/archive notes, plans, and turn references remain in one SQLite database under the metadata area. SQLite runs in WAL mode. Fresh databases use schema v5. Schema-v4 databases are migrated in place: existing notes that fit the new active-memory quota remain active, preferring the most recently updated notes deterministically; overflow is moved to archive without data loss. Older development schemas remain unsupported. CodexBridge uses one dedicated serialized writer connection plus a small pool of four query-only reader connections, so independent reads can progress concurrently and WAL readers can continue while the single writer commits. Writes remain intentionally ordered through one writer because SQLite itself permits only one writer at a time and turn-initialization transactions depend on deterministic compare-and-commit semantics.

Active working memory is bounded per effective project to **64 notes**, **256 bytes per key**, **64 KiB per value**, and **64 KiB aggregate key+value bytes**. `remember` defaults to `scope="active"`; active writes that exceed quota fail explicitly and suggest archiving history rather than silently evicting state. Because this entire set is bounded, `chatgpt_turn_init` always hydrates every active note together with the full current plan. Plans remain capped at 100 items and 256 KiB total, and an empty `update_plan` clears the plan.

Archive/history is separate durable memory and is never included in `brief`, `state_update`, or the active-state hash. Use `remember(..., scope="archive")` to store archive notes and `recall(..., scope="archive")` to retrieve them. Archive memory allows up to **4096 notes**, **256 KiB per value**, and **16 MiB aggregate bytes**. Archive enumeration is lexicographically sorted and paginated at most 128 entries / 1 MiB per call. The first page includes a semantic `snapshot_hash`; every continuation must send that same hash with `next_offset`. If the selected memory scope changes between pages, the continuation returns `PAGINATION_STALE` instead of silently repeating or skipping rows.

Turn-reference creation is part of the same transaction as the project-binding commit, together with subject scope, parent reference, `instruction_hash`, and `state_hash`, so a failed turn init cannot persist only one half of that boundary. Complete active memory, its semantic hash, and the current plan used by `chatgpt_turn_init` are read from one SQLite read transaction, so the committed `state_hash` cannot describe a different database revision than the visible active state. Archive/history is deliberately outside that hash. The instruction hash and returned brief are likewise derived from one loaded instruction context rather than independently re-reading project instructions. Deterministic duplicate continuation retries replay the brief/state snapshot committed for that turn even if durable state changes afterward. Rewind/checkpoint restoration is intentionally not implemented yet; `turn_ref` is already persisted as the stable anchor that a later snapshot layer can attach to.

The read pool removes the previous application-wide `Arc<Mutex<Connection>>` bottleneck without changing the storage schema or project identity model. Each reader is opened with `query_only=ON`; mutations cannot accidentally escape onto the read pool.

## Instructions and skills

Global **instruction** discovery still selects one daemon-account home agent ecosystem. The first existing directory wins:

```text
~/.agents
~/.codex
~/.claude
```

That selected home layer supplies global instructions/rules. Skill discovery is independent and follows OpenAI Codex-style roots instead of the exclusive home-agent selection above.

Project `AGENTS.override.md` / `AGENTS.md` instructions are hierarchical. The root chain is included by `chatgpt_turn_init`; when work first enters a deeper path with additional nested instructions, CodexBridge surfaces that scope delta before mutation. `apply_patch` / `exec_command(workdir=...)` may return `AGENTS_SCOPE_REQUIRED` once so the agent can consume the nested rules and retry. If a reachable project instruction source is byte-truncated, the coding-agent brief tells the model to recover the remainder with `read_file` before modifying files governed by that scope.

Repo skills are discovered from `.agents/skills` along the project-root-to-relevant-path ancestry and each root is scanned recursively (bounded to the same six-level traversal shape used by Codex). `skills_list` and `skills_read` accept an optional `path`; pass it when work is scoped below the project root so nested repo skill roots become visible. CodexBridge also accepts `.codex/skills` at the same ancestry levels as a lower-precedence compatibility alias:

```text
project/.agents/skills/**/SKILL.md
project/.codex/skills/**/SKILL.md        # compatibility alias
project/subdir/.agents/skills/**/SKILL.md
project/subdir/.codex/skills/**/SKILL.md # compatibility alias
```

User skills load from `~/.agents/skills` plus the deprecated `$CODEX_HOME/skills` location for Codex compatibility. Repo `.claude/skills` is not treated as a Codex skill root; Claude plugin-cache skills remain separately discovered and namespaced as `<plugin>:<skill>`. Closer repo roots win duplicate names; `.agents/skills` wins over `.codex/skills` at the same level; repo skills win over user/plugin skills. Skill lookup is case-insensitive while preserving the declared display name.

Agent instruction and skill content intentionally follows symlinks, including shared targets outside the project root. That is a separate trust policy from ordinary filesystem tools. Instruction loading is globally bounded by bytes and file count; oversized instruction content is truncated rather than preventing conversation initialization.

Runtime environment data is not exposed as another MCP tool. One shared internal `RuntimeEnvironment` representation feeds the MCP initialize instructions, initialized project brief, `exec_command` description, and diagnostics so shell/backend wording cannot drift into contradictory copies.

## Filesystem security

Ordinary filesystem operations reject absolute paths, traversal, and symlink components. On Unix, critical reads and mutations use descriptor-relative `openat`/`mkdirat`/`renameat`/`unlinkat` operations with no-follow flags. Regular-file reads open with `O_NONBLOCK`, verify the opened descriptor is a regular file, and therefore reject FIFOs/devices instead of potentially waiting on them. `read_file` keeps one opened file descriptor for layout scanning and rendering, so an atomic file replacement during a call cannot mix metadata from one inode with content from another. `grep` routes candidate content reads back through `SecurePathResolver`; on Unix this uses the same descriptor-relative no-follow reader instead of validating a path and later following that pathname again.

`apply_patch` is serialized per effective project, preflights expected old content to detect stale concurrent edits, and rolls back already observed writes when a later operation fails without overwriting a newly detected concurrent change. The parser tracks explicit context-line indices, matching OpenAI Codex's apply-patch model, so lenient whitespace/Unicode matching never rewrites unchanged context bytes after deletions. Patch matching is line-ending agnostic (LF-authored hunks apply to CRLF files), and unchanged context keeps its exact source bytes and line ending; replacement/inserted lines inherit the appropriate source-region ending, so mixed-EOL files are not globally renormalized. For Codex grammar parity, the first update chunk may omit an explicit `@@` header. `*** Delete File` is file-only and rejects directory targets rather than recursively deleting a directory tree.

`read_file` remains line-oriented for normal source-code work: `offset` is the 0-based logical line and `limit` is a line count. It scans ranged content without requiring the whole file to fit `MAX_WRITE_BYTES`, so files larger than 8 MiB remain readable. If one logical line alone exceeds the presentation byte budget, the result keeps `next_offset` on that same line and returns `next_line_byte_offset`; pass it back as `line_byte_offset` to continue from the exact UTF-8 byte boundary. This prevents the remainder of minified/generated single-line files from being skipped. The default window is `OUTPUT_FILE_BYTES` (256 KiB); a caller can set `max_bytes` for one `read_file` call up to `OUTPUT_MULTI_FILE_BYTES` (1 MiB by default), reducing round trips for large generated/minified lines without globally increasing every file response. Truly multi-megabyte single lines still require continuation because MCP/model context is intentionally bounded.

`list_directory`, `tree`, `glob`, and `grep` share default ignore rules plus Git ignore semantics, including nested `.gitignore` files and `.git/info/exclude` with root `.gitignore` taking the higher Git precedence. Search output reports incomplete/traversal-limit/skipped-file state explicitly; callers must not treat a partial zero-result page as exhaustive.

Agent content (`AGENTS.md`, rules, skills, skill resources) is intentionally exempt from that no-follow policy as described above.

## Process execution

`exec_command` is the single general command tool. It supports:

- bounded output and token-window truncation;
- wall-clock deadlines with explicit lifecycle state;
- global and per-project process concurrency limits;
- process-tree termination;
- Unix CPU and file-descriptor resource limits;
- long-running sessions continued with `write_stdin`;
- optional one-shot `stdin` plus `close_stdin=true` for non-agent CLIs that read until EOF;
- integrated signal-and-wait via `write_stdin(signal=..., wait_for_exit_ms=...)`, which drains final output and returns terminal status in the same call;
- the initial `exec_command` wait is capped at 20 seconds so a long build does not sit on a common ~30-second MCP/HTTP request boundary; the process remains live and should be continued with `write_stdin`;
- native Unix PTY / Windows ConPTY via `tty=true`;
- terminal resize and interrupt/terminate/kill signals; PTY interrupt writes Ctrl-C, Unix non-TTY interrupt/terminate map to SIGINT/SIGTERM, Windows non-TTY interrupt sends Ctrl-Break to a dedicated process group, Windows terminate uses non-forced `taskkill.exe /T`, and kill forcefully terminates the tree;
- byte-cursored output replay over a bounded head+tail buffer. While retained output is contiguous, `output_offset` / `output_next_offset` identify the logical stream range directly. After the middle of a long stream is evicted, CodexBridge keeps the logical head and tail ranges separate and inserts an explicit `buffered bytes omitted` marker rather than pretending the retained bytes form one contiguous range. A lost response can be replayed with `write_stdin(since_output_offset=...)`; a cursor inside an evicted gap resumes at the first retained tail byte and reports the omission explicitly rather than replaying stale head bytes.

Process results expose `completion_reason` (`running`, `exited`, `signaled`, `timed_out`, `cancelled`, or `failed`), nullable `exit_code`, `deadline_exceeded`, `timed_out`, and an optional wait `error`. A requested signal does not by itself make a successful graceful exit `signaled`: if the process handles SIGTERM/SIGINT and exits with code 0, the authoritative result is `completion_reason=exited` plus `requested_signal=...`. On Unix, both pipe-backed commands and PTY sessions preserve the native wait status, so actual signal termination is reported as `completion_reason=signaled` with the signal number instead of being inferred from a nonzero exit code. CodexBridge does not use a synthetic `exit_code=-1` for signal/timeout cases. When the requested wall-clock deadline is reached, the waiter allows a bounded one-second completion grace: if the process is already finishing, its real exit status is preserved with `deadline_exceeded=true` and `timed_out=false`; if it remains alive, CodexBridge terminates the process tree and returns `completion_reason=timed_out` with `timed_out=true`.

Sessions that already have a `session_id` remain in the registry briefly after completion so explicit replay remains possible. In addition, if the initial `exec_command` call finishes before yielding but its byte buffer or token window was truncated, that finished response deliberately retains a `session_id` and a recovery continuation. A clean, fully delivered finished command may omit the session id because no replay is required.

For programs that may consume stdin until EOF—especially CLI wrappers around coding subagents—set `close_stdin=true` in the initial `exec_command` call and optionally provide the initial `stdin` payload there. Keep stdin open only for intentionally interactive sessions that will be continued with `write_stdin`.

### Bubblewrap and Podman-in-Podman

`MCP_EXEC_SANDBOX=auto` is the default. On Linux, CodexBridge performs an actual Bubblewrap probe once using the same `/usr/bin/bwrap` executable used for real sandbox launches; it does not probe one binary and later execute a PATH-shadowed replacement. If Bubblewrap is absent or namespaces/mounts are unavailable—as is common inside rootless/restricted Podman containers—the effective backend becomes native automatically when YOLO fallback is enabled. Bubblewrap intentionally keeps host networking available with `--share-net`; CodexBridge does not impose network isolation on shell commands.

Podman gets additional capability probes **only on Linux**; Windows and macOS do not run Podman probes or advertise Podman as a local execution option. When Bubblewrap itself works on Linux, CodexBridge runs a bounded, non-mutating `podman info --format json` probe inside the same sandbox shape. If that succeeds, direct Podman commands may remain inside Bubblewrap. If it fails or times out, Podman uses the native backend only when unsandboxed execution is allowed; otherwise the command fails with `SANDBOX_UNAVAILABLE`. Native fallback only selects where the command runs; it does not grant extra container-engine privileges. Separately, CodexBridge probes the daemon runtime once to determine how the agent should invoke Podman. A non-root direct probe goes beyond `podman info`: after confirming a rootless client it enters Podman's user namespace and attempts an image-free nested mount/PID namespace with a private `/proc`, catching restricted Podman-in-Podman environments where `podman info` succeeds but `podman run` cannot start. Passwordless `sudo -n podman info --format json` is also probed. If direct Podman is unusable but sudo works, the agent is told to use `sudo -n podman ...` from the start. If both direct and sudo are usable, the agent starts with `podman ...` but is explicitly required to retry the same Podman operation once as `sudo -n podman ...` when the direct attempt fails with a rootless-runtime symptom such as `crun`, `/proc`/mount, `Operation not permitted`, permission, or user-namespace errors. Explicit sudo Podman is forced onto the native exec backend rather than being placed inside Bubblewrap, so that retry is not trapped in the sandbox that may have caused the failure. The resulting instruction is included in the agent environment summary and the `exec_command` description. CodexBridge does not rewrite arbitrary command text or silently elevate commands; the retry is an explicit agent action using the already verified passwordless sudo capability. For interactive `podman run -it`, the agent is also instructed to request `tty=true`. CodexBridge does not rely on shell aliases and does not prescribe what Podman is used for; build, run, dependency, and runtime-build workflows belong in the project's `AGENTS.md`.

Podman's own lifecycle semantics still apply. In particular, `podman run --rm` deletes the container object when it exits, so a later `podman inspect` cannot recover post-exit state regardless of the MCP process API. When shutdown evidence requires inspecting container state after exit, use a retained named container; when only command-side evidence is required, `write_stdin(signal=..., wait_for_exit_ms=...)` can signal the foreground process and return its drained final output plus terminal process status in one call.

On daemon shutdown, CodexBridge first sends terminate to live interactive sessions, waits for their process waiters/output drains for a bounded grace period, then force-cleans anything still alive and records the remaining-process count in `server_stopped`. This improves graceful shutdown evidence when the outer container itself remains alive. If the outer container is destroyed with `--rm`, no application-level API can recover logs or process state after the container is gone.

The effective backend and default shell are included in the `exec_command` tool description and startup diagnostics.

CodexBridge intentionally does **not** set Linux `RLIMIT_NPROC`. That limit is accounted per real UID across the whole host/user namespace rather than per spawned command, so a value such as 128 can prevent Podman, Cargo, or a compiler from creating threads when unrelated platform processes share the daemon UID. Command concurrency is bounded by CodexBridge's global/per-project process semaphores; production deployments should bound total process/thread fan-out with the outer container/VM cgroup PID limit (for example Podman's `--pids-limit`) instead of a child `RLIMIT_NPROC`.

## Authentication

Authentication is always enabled. If `MCP_AUTH_TOKEN` is not supplied, CodexBridge creates and persists one under `<workspace>/.metadata/auth-token`. Explicit tokens must be 16-512 bytes; path/either mode also rejects path separators in the token.

`MCP_AUTH_MODE` supports:

- `path` (default): `/<token>/mcp`
- `bearer`: `/mcp` with `Authorization: Bearer <token>`
- `either`: both forms

Token comparisons are constant-time. `/health` is unauthenticated for deployment health checks. Credentials are redacted from audit output.

## Environment overrides

No general application config file is read. For normal use, none of these are required; `MCP_UPSTREAM_CONFIG` is the one optional operator-supplied YAML/JSON file used for upstream aggregation.

| Variable | Default | Purpose |
|---|---:|---|
| `WORKSPACE_ROOT` | `/workspace` | Workspace root when no positional argument is given |
| `MCP_BIND` | `0.0.0.0:3000` | Listener address |
| `MCP_AUTH_TOKEN` | generated/persisted | Override the generated token |
| `MCP_AUTH_MODE` | `path` | `path`, `bearer`, or `either` |
| `MCP_EXEC_SANDBOX` | `auto` | `auto`, `bwrap`, or `none` |
| `MCP_ALLOW_UNSANDBOXED_EXEC` | `true` | Permit native fallback; set false only when fail-closed Bubblewrap is required |
| `MCP_ALLOWED_HOSTS` | empty | Optional RMCP Host allowlist |
| `LOG_ROOT` | `<workspace>/.metadata/logs` | Audit log directory |
| `EXEC_DEFAULT_TIMEOUT_MS` | `120000` | Default command timeout |
| `EXEC_MAX_TIMEOUT_MS` | `3600000` | Maximum requested command timeout (1 hour); operators can lower it for stricter workloads |
| `MAX_CONCURRENT_TOOL_CALLS` | `64` | Global tool-call concurrency |
| `MAX_CONCURRENT_CPU_TASKS` | max(CPU, 2) | Global blocking/CPU-heavy task concurrency |
| `MAX_CONCURRENT_PROCESSES` | min(CPU, 8) | Global process concurrency |
| `MAX_PROJECT_TOOL_CALLS` | `8` | Per-project tool fairness |
| `MAX_PROJECT_PROCESSES` | max(global processes - 1, 1) | Per-project process concurrency |
| `MAX_CONCURRENT_SEARCHES` | max(CPU, 2) | Concurrent search scans |
| `MAX_CONCURRENT_PATCHES` | `4` | Global patch transaction concurrency |
| `MAX_INTERACTIVE_PROCESSES` | `32` | Tracked long-running/interactive sessions |
| `INTERACTIVE_PROCESS_IDLE_SECS` | `900` | Idle retention before process-session cleanup |
| `MAX_LEGACY_MCP_SESSIONS` | `1024` | Cap for pre-stateless MCP transport sessions |
| `MCP_SESSION_IDLE_SECS` | `3600` | Idle expiry for legacy MCP sessions |
| `STATUS_INTERVAL_SECS` | `0` | Periodic daemon-status audit interval; `0` disables it |
| `MAX_REQUEST_BODY_BYTES` | `16 MiB` | Streamable HTTP request-body ceiling |
| `MAX_INPUT_STRING_BYTES` | `1 MiB` | Ceiling for large string/tool command inputs |
| `MAX_PROCESS_OUTPUT_BYTES` | `4 MiB` | Hard retained process-output budget |
| `MAX_WRITE_BYTES` | `8 MiB` | Maximum resulting file size for mutation paths such as `apply_patch`; not a whole-file `read_file` limit |
| `MAX_PATCH_BYTES` | `4 MiB` | Patch-input ceiling |
| `MAX_MULTI_PATHS` | `64` | Maximum paths/actions accepted by one bounded multi-path operation such as `apply_patch` |
| `MAX_RESULTS` | `1000` | Hard listing/search result ceiling |
| `MAX_TRAVERSED_ENTRIES` | `100000` | Hard traversal budget for recursive discovery/search |
| `OUTPUT_FILE_BYTES` | `256 KiB` | Default file presentation budget |
| `OUTPUT_MULTI_FILE_BYTES` | `1 MiB` | Default aggregate multi-file presentation budget |
| `OUTPUT_MAX_RESULTS` | `500` | Default listing/search/tree page size |
| `OUTPUT_SEARCH_BYTES` | `512 KiB` | Default search presentation budget |
| `OVERLOAD_WAIT_MS` | `500` | Bounded wait before concurrency pressure returns busy |
| `MCP_UPSTREAM_CONFIG` | unset | Optional YAML/JSON `mcpServers` file; unset means no upstream tools |
| `MAX_CONCURRENT_UPSTREAM_CALLS` | `8` | Per-upstream call concurrency ceiling |
| `UPSTREAM_CALL_TIMEOUT_MS` | `120000` | Timeout for one upstream MCP call |
| `MAX_GATEWAY_SKILL_BYTES` | `1 MiB` | Maximum generated gateway skill catalogue size |
| `MCP_PROJECT_DOC_FALLBACKS` | empty | Up to 16 extra simple instruction filenames after AGENTS names |
| `MCP_CONTAINER_SOCKET` | unset | Optional trusted container-engine socket exposed to process execution |
| `MCP_CONTAINER_CONFIG_ROOT` | unset | Optional container configuration root mounted read-only into Bubblewrap |
| `LOG_QUEUE_CAPACITY` | `4096` | Maximum queued audit events before bounded drop accounting applies |
| `LOG_QUEUE_MAX_BYTES` | `64 MiB` | Aggregate byte budget for queued audit events |
| `CONSOLE_PARAM_EXCERPT_BYTES` | `4096` | Console/audit parameter excerpt budget |
| `CONSOLE_RESULT_EXCERPT_BYTES` | `8192` | Console/audit result excerpt budget |
| `LOG_MAX_EVENT_BYTES` | `1 MiB` | Maximum serialized JSONL audit event size before bounded excerpting |
| `LOG_MAX_FILE_SIZE_MB` | `100` | Rotate an audit file after this many MiB |
| `LOG_MAX_FILES` | `10` | Maximum retained rotated audit files |

Additional resource/log limit environment variables remain supported for operators, but they are not required to start the service.

## Container deployment

Build and run with Podman:

```bash
podman build -t codex-bridge -f Containerfile .
podman run -d --name codex-bridge \
  -p 3000:3000 \
  --pids-limit 4096 \
  -v codex-bridge-workspace:/workspace \
  codex-bridge
```

Or:

```bash
podman compose -f podman-compose.yml up -d --build
```

The named workspace volume persists projects, SQLite state, audit logs, and the generated authentication token. The runtime image includes Bubblewrap, Git, curl, jq, Podman, and podman-compose. Bubblewrap may automatically fall back to native execution when the outer container does not permit nested namespaces. How a project chooses to use Podman is intentionally outside CodexBridge policy and should be described by that project's `AGENTS.md`.

An operator may still expose a Podman service socket explicitly with `MCP_CONTAINER_SOCKET`; that is optional infrastructure rather than CodexBridge's preferred workflow. Treat any mounted container-engine socket as a high-authority capability.

## Observability and shutdown

`/health` is intentionally unauthenticated and reports `status`, active HTTP requests, and active legacy MCP sessions. Detailed JSONL audit logs are written under `<workspace>/.metadata/logs` by default using bounded event/byte queues, rotation, and parameter/result excerpts. Normal tool data such as paths, patch/content text, commands, stdout/stderr, brief/state excerpts, and search/read results is retained up to the configured console/file byte budgets so the audit trail remains useful for debugging. MCP authentication tokens are replaced wherever they occur, and credential-shaped fields such as authorization/password/token/API-key values are redacted recursively. Set `STATUS_INTERVAL_SECS` above zero to emit periodic daemon-status audit events with request/process/session/tool/cache/log-queue counters.

SIGTERM/SIGINT triggers graceful HTTP shutdown, process cleanup, legacy-session cleanup, audit flush, and interactive subprocess termination.

## Verification

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --bins --examples
bash -n scripts/mcp_smoke.sh
git diff --check
scripts/mcp_smoke.sh
```

CI runs the native Rust test suite, Clippy, and bin/example builds on Ubuntu, Windows, and macOS; Ubuntu additionally runs formatting, smoke-script syntax/whitespace checks, and the real MCP HTTP smoke suite. The smoke suite launches deterministic stdio upstreams and verifies both direct forwarding and gateway dispatch/progressive skill disclosure. The manual release workflow has a native Ubuntu/Windows/macOS test gate before building raw `codex-bridge` binaries for Linux x86-64, Linux ARM64, macOS Apple Silicon, and Windows x86-64.

## Known boundaries

- Normal project filesystem confinement is strongest on Unix where descriptor-relative no-follow APIs are available; non-Unix implementations rely more heavily on canonical validation.
- Multi-file patch rollback covers failures observed during the call but is not crash-atomic across sudden power/process failure.
- Legacy RMCP transport sessions are in-memory and do not survive restart; project bindings/files/memory/plans do.
- External MCP clients/connectors may cache model-facing tool metadata independently of the running CodexBridge server. The deployed server's `tools/list` response is the canonical contract. The contract-qualified `serverInfo.version` provides a change signal and the open `extensions` envelope reduces dependence on future top-level schema additions, but CodexBridge cannot force an external connector to invalidate an already cached schema. A connector that has not yet discovered `extensions`, or that needs a newly added top-level field, may require re-discovery or reconnection.
- The transport does not supply a trustworthy ChatGPT user-message ID. A lost response to the very first `chatgpt_turn_init` cannot be replayed from a client idempotency key that does not exist; later duplicate continuations are deterministic through `previous_turn_ref`.
- `recall` pagination detects concurrent mutations within the selected active/archive scope with `snapshot_hash`; it does not retain point-in-time snapshots. A `PAGINATION_STALE` continuation must restart enumeration from offset 0 against the new scope snapshot.
- Process output retention is bounded. Once the middle of a long stdout/stderr stream is evicted from the head+tail buffer, those omitted bytes cannot be reconstructed; replay reports the gap explicitly instead of fabricating a contiguous byte range. Commands that require a complete durable log should write that log to a project file or another durable sink.
- `turn_refs` are durable history and currently have no retention/compaction policy, so long-lived installations should monitor metadata database growth.
- Per-project permit entries and audit project-activity entries are currently retained in memory for the daemon lifetime. Deployments that create an unbounded number of one-off effective projects should restart/monitor the daemon or add an operator-side lifecycle policy until eviction is implemented.
- Native fallback intentionally has no OS-level Bubblewrap isolation. In container deployments, the outer container/VM/account becomes the process isolation boundary.
- CodexBridge does not use Linux `RLIMIT_NPROC` because it is UID-wide and can collide with unrelated processes. Use an outer cgroup/container PID limit for a hard process/thread ceiling.
- Network access is intentionally available to shell commands. Treat the authenticated MCP endpoint as a powerful coding-agent capability.
- A configured `MCP_CONTAINER_SOCKET` is a high-authority capability and can effectively delegate the outer container engine's privileges to project commands. Only mount a deliberately trusted rootless/container service socket.
- Upstream MCP servers are operator-trusted capabilities. A configured stdio command runs as the daemon account with a minimal inherited environment plus explicitly configured variables, while Streamable HTTP upstreams can return model-visible metadata/results; only configure servers whose behavior and data boundary are acceptable for the deployment.
