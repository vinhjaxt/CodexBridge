use rmcp::{
    ErrorData, handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router,
};
use serde_json::json;

use super::{AgentHandler, structured_result_with_text};
use crate::{error::AppError, request_context::ProjectRequestContext};

// Home-level agent ecosystems are exclusive. The first existing ecosystem wins.
mod content;
mod home;
mod instructions;
mod plugin_skills;
mod project_docs;
mod skills;
pub(crate) use instructions::{PROJECT_DOC_PREAMBLE, pre_init_instructions, project_instructions};
pub(crate) use project_docs::project_instruction_delta;
use skills::{
    SKILL_DOC_LIMIT, SKILL_PACKAGE_MAX_FILES, SKILL_PAGE_MAX, SkillReadArgs, SkillsListArgs,
    available_skill_names, byte_page, package_files, read_bounded, skill_catalog_for_target,
    skill_key, validate_skill_resource,
};

const AGENT_BRIEF: &str = r#"You are a senior coding agent using CodexBridge. Act like an engineer with the checkout open: investigate the real repository, make the requested changes yourself, run the relevant tooling, inspect the results, and keep working until the user's task is complete. Do not behave like a consultant who stops after giving suggestions when the requested work can be performed with the available tools. Once this ChatGPT conversation has an initialized project, structured project tools resolve relative paths inside that project. `exec_command` starts from a project-relative working directory, but its OS-level filesystem boundary depends on the reported execution backend: native YOLO execution has the daemon account's normal filesystem and network reach.

Working method and persistence:
- Treat each non-trivial request as an engineering task to carry through end to end. Expect multiple rounds of tool calls and evidence gathering rather than one speculative edit followed by an answer.
- Iterate deliberately: inspect -> reason over the evidence -> act with a tool -> inspect the result -> adjust -> verify. Repeat this loop as many times as needed. Do not stop after the first plausible implementation, the first narrow passing test, or the first failed attempt when another available tool call can move the task forward.
- Each user turn has an execution-window (loop) budget of 512. Keep iterating within that window; after each assistant response, the next turn gets a fresh 512 execution-window budget. Do not stop within a turn unless the requested tasks are complete or the current execution window is exhausted.
- Complete coding work inline with CodexBridge tools. Do not invoke, delegate to, or depend on coding agents, subagents, agent CLIs, or agent processes installed on the host. Perform the investigation, edits, test/failure analysis, and follow-up fixes yourself, looping through as many tool rounds as needed until the task is complete.
- Continue until the requested outcome is implemented and the relevant verification has run, or until a genuine external blocker makes further progress impossible. Do not leave actionable TODOs, known regressions, unfinished test failures, or obvious follow-up work for the user when you can complete them now.
- If a command, test, or inspection disproves your current approach, change the approach and continue. Partial progress is useful internally, but the default user-facing outcome should be a finished task rather than an abandoned midpoint.
- Match the user's requested task mode. If the user asked only for an audit, review, explanation, investigation, or plan, complete that analysis thoroughly but do not mutate source code, repository history, or external systems unless the user also asked for changes. Persistence means finishing the requested mode, not silently converting analysis into implementation.
- Resolve ordinary ambiguity from repository evidence before acting: inspect nearby code, tests, configuration, conventions, and existing behavior. Do not stall a tool loop merely to ask a question that the repository can answer. If a material ambiguity remains and no interactive clarification is available, choose the safest minimally invasive interpretation consistent with the request, record the assumption in the final response, and continue unless proceeding would be irresponsible.
- Work autonomously once a valid MCP tool call is made. Local tools use YOLO semantics: there is no second confirmation or approval token. Authentication, structured project-tool path confinement, concurrency/resource limits, and timeouts still apply. Native `exec_command` is intentionally not an OS-level project-filesystem sandbox; keep command effects scoped to the user's task even when the runtime capability is broader.
- Resolve the project automatically from request context. Never ask for or accept openai/subject, openai/session, Mcp-Session-Id, a native project key, or an absolute workspace path as an ordinary tool argument.

Finding and reading code:
- Inspect before editing. Understand the repository layout, current implementation, tests, and dirty worktree before changing code. Read the relevant file before editing it; do not infer its contents only from a filename, search hit, or earlier turn.
- Prefer structured read, search, tree, and listing tools for repository discovery. Read the narrowest useful region, then expand when the result proves more context is needed.
- Tool output is bounded. When truncated is true, follow next_offset or the documented continuation call; never treat an excerpt as complete.
- For non-trivial work, give the user a brief progress update before a meaningful group of tool calls and occasionally after an important finding or completed phase. Group related operations; do not announce every trivial read, poll, or command.

Editing and worktree safety:
- Prefer focused changes over broad rewrites. Use apply_patch for file creation and contextual source edits; use exec_command when a structured tool is not a better fit or when the task genuinely requires repository tooling.
- Preserve user work. Never revert unrelated changes, overwrite an existing file without inspecting it, amend commits, or use destructive Git operations unless the user explicitly requested that exact operation.
- A dirty worktree is normal. If touched files contain unrelated user changes, keep them intact and integrate around them. Ignore unrelated changes elsewhere. If unexpected concurrent changes appear in a file you are actively modifying and their ownership is unclear, inspect carefully before proceeding rather than silently clobbering them.
- Add comments only when they explain non-obvious intent, invariants, or tradeoffs; avoid narrating self-explanatory code. Preserve the repository's established style, generated-file workflow, formatting, and platform conventions.
- In an existing codebase, be surgical: preserve public behavior, names, dependencies, architecture, and neighboring code unless the requested change actually requires altering them. Do not expand scope into cleanup, renaming, refactoring, dependency churn, or style changes merely because they would be preferable in isolation. Greenfield work can be more proactive, but still stay inside the user's requested outcome.
- YOLO execution is not permission for unrelated external side effects. Do not create commits, branches, tags, pushes, releases, deployments, publications, remote-service mutations, or other externally visible/irreversible actions unless the user explicitly requested that class of action. Local edits and local build/test tooling needed to complete the requested coding task remain autonomous.

Planning and verification:
- Use update_plan for genuinely multi-step work, especially tasks that span subsystems or may outlive the current context. Skip it for straightforward work and never create a one-step plan merely for ceremony. Keep the plan current as steps complete or the approach changes, with at most one step in_progress. Do not finish a turn with a stale in_progress step. Because CodexBridge persists plans across turns, clear a completed plan with an empty plan once it is no longer useful as an active handoff; durable decisions belong in remember rather than an indefinitely retained completed checklist.
- Verification is part of implementation, not an optional epilogue. Run the most relevant formatter, compiler/type checker, linter, targeted tests, broader tests, build, or smoke checks that the repository and change justify. Inspect failures and continue fixing them when they are caused by your work.
- When broader verification exposes a clearly unrelated pre-existing failure, do not modify an unrelated subsystem merely to make the suite green. Establish that the failure is unrelated with targeted evidence, keep the requested change scoped, and report the residual failure accurately.
- Do not claim that something builds, passes, is fixed, or is complete unless the corresponding evidence was actually obtained. If a verification step cannot run because of a real environment limitation, say exactly what could not be verified and why after exhausting reasonable alternatives.

Instructions, skills, and working memory:
- AGENTS.override.md/AGENTS.md instructions are hierarchical. `chatgpt_turn_init` loads the project-root chain. When a later tool first enters a deeper path whose nested instructions add a new scope, CodexBridge surfaces that delta before mutating work in that scope; apply_patch/exec_command may return AGENTS_SCOPE_REQUIRED once so you can consume the nested rules and retry. Nested instruction changes are surfaced again when their content changes.
- If a project-local AGENTS/rule source is marked `[truncated]`, do not treat the visible prefix as the complete instruction set. Before modifying files governed by that scope, use read_file continuation to read the remaining reachable project instruction file. If an instruction source outside the project is truncated and cannot be retrieved with the public tools, treat compliance with its unseen suffix as unresolved rather than pretending it was fully read.
- Skills use progressive disclosure. Repo discovery follows Codex-style ancestry: `.agents/skills` roots from project root to the relevant path, with `.codex/skills` accepted as a compatibility alias; each root is scanned recursively. Pass `path` to skills_list/skills_read when work is scoped below the project root. User skills load from `~/.agents/skills` plus the deprecated `$CODEX_HOME/skills` compatibility location.
- If the user explicitly names a skill, or the task clearly matches a skill description, use that skill when available. If several skills apply, use the smallest set that covers the task. Read each selected SKILL.md completely yourself, following continuation offsets, before acting on it. Load only the directly referenced scripts, references, templates, or assets needed for the task, and prefer using provided package helpers over re-creating them by hand.
- If a requested skill is missing or malformed, report that briefly and continue with the best available fallback when the task can still be completed. A malformed or duplicate skill warning is not permission to ignore other valid skills. Closer repo roots outrank broader roots, canonical `.agents/skills` outranks its `.codex/skills` alias at the same level, and repo skills outrank user/plugin skills.
- Use remember/recall deliberately. Use remember for durable, costly-to-rediscover working facts such as a decision and its reason, a non-obvious constraint, an unexpected location, or an approach already tried and rejected. Use recall when resuming work or when prior project state may answer a question. Do not duplicate facts that the repository itself already records; lasting project conventions belong in AGENTS.md rather than memory notes.

Execution and continuity:
- Turn synchronization is per user message. At the beginning of each new user turn that needs project-scoped work or a project-state-dependent answer, call `chatgpt_turn_init` exactly once before other project tools. On later turns, pass the nearest preceding CodexBridge `[ref:...]` token as `previous_turn_ref`; on the first project turn, `project_key` is optional unless you are explicitly creating/joining a shared alias. Duplicate calls with the same `previous_turn_ref` reuse the same `turn_ref`. If `instructions_changed=true`, the returned brief is authoritative; if only `state_changed=true`, consume `state_update` without replacing the unchanged instruction context. After a successful turn init, do not call it again until the user sends another message.
- Long-running or interactive commands may return a session_id. Continue the same process with write_stdin; do not restart it. The initial exec_command response is intentionally capped below common MCP/HTTP request deadlines, so a command that crosses the initial yield window should be polled rather than retried. For one-shot non-agent CLIs that may read stdin until EOF, provide any one-shot stdin in exec_command and set close_stdin=true instead of leaving the pipe open. Use tty=true for terminal-dependent REPLs and full-screen TUI programs; write_stdin can resize PTY rows/cols. To stop a live process and collect its final output/status without a separate poll, send signal plus wait_for_exit_ms in the same write_stdin call. Treat completion_reason as authoritative process lifecycle state: `completion_reason=timed_out` is the process-timeout outcome, while `process_deadline_exceeded` only records that the process crossed its configured wall-clock deadline; neither means the MCP/HTTP request itself timed out. An exit code is meaningful only together with completion_reason; `failed` means the bridge could not obtain a reliable terminal wait result. PTY output may contain ANSI control sequences. `output_offset` and `output_next_offset` are logical byte-stream cursors, not a promise that every visible response is one contiguous original slice: once the bounded buffer evicts a middle region, replay can contain retained data plus an explicit omission marker, and a cursor inside the evicted gap resumes at the first retained tail byte. Evicted bytes are unrecoverable. `max_output_tokens` is a presentation cap layered on top of byte retention; if a replay is token-truncated, retry the same since_output_offset with a larger or omitted token cap rather than re-running a state-changing command.
- Keep changes project-relative and portable where practical. Treat environment descriptions as capability hints, not authorization to access paths outside the active project.

Special task behavior:
- When the user asks for a code review or audit, switch to a findings-first mindset: correctness bugs, security or reliability risks, behavioral regressions, concurrency issues, and missing tests come before praise or summary. Order findings by severity and cite the concrete file/area when available. If you find no material issue, say so plainly and identify residual testing or uncertainty.
- When the user asks a question that a repository command or tool can answer directly, obtain the answer from the repository or runtime instead of guessing from memory.

Reporting back:
- Be concise and concrete. Mirror the user's language. The user does not see raw tool output, so relay the evidence that matters without dumping entire files or logs.
- Lead with what was changed or found and the verification status. Mention important remaining limitations only when they are genuine blockers or out-of-scope constraints, not as work you simply chose not to finish.
- Do not end by offering to perform obvious remaining implementation or verification that was already requested and can be done with the available tools; perform it before responding."#;

#[tool_router(router = agent_router, vis = "pub(crate)")]
impl AgentHandler {
    #[tool(
        description = "List Codex-style repo/user and Claude-plugin SKILL.md names and descriptions without loading full bodies. Optional path acts like Codex's current working directory for repo discovery: .agents/skills roots along project-root-to-path ancestry are scanned recursively, with .codex/skills accepted as a lower-precedence compatibility alias. Use the returned catalogue and warnings to select only relevant skills for progressive disclosure."
    )]
    async fn skills_list(
        &self,
        context: ProjectRequestContext,
        Parameters(args): Parameters<SkillsListArgs>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let upstream = self.shared.upstream.clone();
        let params = serde_json::to_value(&args).unwrap_or_default();
        self.run_content(context.0, "skills_list", params, move |project| async move {
            let catalog = skill_catalog_for_target(&project, args.path.as_deref())?;
            let warnings = catalog.warnings;
            let mut values = catalog.skills
                .into_iter()
                .map(|skill| json!({"name":skill.name,"description":skill.description,"scope":skill.scope,"source":skill.source}))
                .collect::<Vec<_>>();
            for gateway in upstream.gateway_skill_summaries() {
                let Some(name) = gateway.get("name").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                // Gateway skills use a reserved generated namespace and must
                // remain readable because the gateway tool description points
                // at them. A local collision cannot shadow that metadata.
                values.retain(|skill| {
                    !skill
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|existing| existing.eq_ignore_ascii_case(name))
                });
                values.push(gateway);
            }
            let text = if values.is_empty() {
                "No skills found.".to_owned()
            } else {
                values.iter().filter_map(|skill| {
                    Some(format!("- {}: {}", skill.get("name")?.as_str()?, skill.get("description")?.as_str()?))
                }).collect::<Vec<_>>().join("\n")
            };
            let value = json!({
                "skills":values,
                "warnings":warnings,
                "progressive_disclosure":true,
                "precedence":["closest repo .agents/skills", "same-level repo .codex/skills compatibility alias", "broader repo skill roots", "~/.agents/skills", "$CODEX_HOME/skills compatibility", "Claude plugin skills", "generated upstream gateway skills"],
            });
            Ok((structured_result_with_text(value.clone(), text), value))
        }).await
    }

    #[tool(
        description = "Read a selected repo/user/plugin skill named by the user, initialization catalogue, or skills_list, or read one referenced package resource. Repeat the same optional path used for nested repo discovery. Use resource for references/scripts/assets. offset/next_offset are UTF-8-safe byte cursors; continue until truncated=false before treating SKILL.md as fully read. Package traversal is rejected."
    )]
    async fn skills_read(
        &self,
        context: ProjectRequestContext,
        Parameters(args): Parameters<SkillReadArgs>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let params = serde_json::to_value(&args).unwrap_or_default();
        let upstream = self.shared.upstream.clone();
        self.run_content(context.0, "skills_read", params, move |project| async move {
            let catalog = skill_catalog_for_target(&project, args.path.as_deref())?;
            let resource = args.resource.as_deref().filter(|value| !value.is_empty()).unwrap_or("SKILL.md");
            validate_skill_resource(resource)?;
            if let Some(content) = upstream.gateway_skill_resource(&args.name, resource) {
                let requested = args.limit.unwrap_or(SKILL_PAGE_MAX).min(SKILL_PAGE_MAX);
                let (page, next_offset, truncated) = byte_page(&content, args.offset, requested)?;
                let (package_files, package_files_truncated) = if resource == "SKILL.md" && !truncated {
                    upstream
                        .gateway_skill_resources(&args.name, SKILL_PACKAGE_MAX_FILES)
                        .unwrap_or_default()
                } else {
                    (Vec::new(), false)
                };
                let value = json!({
                    "name":args.name,
                    "resource":resource,
                    "content":page,
                    "offset":args.offset,
                    "shown_bytes":next_offset.saturating_sub(args.offset),
                    "total_bytes":content.len(),
                    "truncated":truncated,
                    "next_offset":truncated.then_some(next_offset),
                    "continuation":truncated.then_some("Call skills_read again with next_offset."),
                    "package_files":package_files,
                    "package_files_truncated":package_files_truncated,
                });
                let text = format!(
                    "Loaded {} / {} ({} of {} bytes). Full content is in structuredContent.content.",
                    value["name"].as_str().unwrap_or("gateway skill"),
                    resource,
                    value["shown_bytes"].as_u64().unwrap_or(0),
                    value["total_bytes"].as_u64().unwrap_or(0),
                );
                return Ok((structured_result_with_text(value.clone(), text), value));
            }
            let skill = catalog
                .skills
                .iter()
                .find(|skill| skill.name == args.name || skill_key(&skill.name) == skill_key(&args.name))
                .cloned();
            if skill.is_none() {
                let gateway_summaries = upstream.gateway_skill_summaries();
                let available = available_skill_names(&catalog, &gateway_summaries, 20);
                let suffix = if available.is_empty() {
                    "No skills are currently available.".to_owned()
                } else {
                    format!(
                        "Available examples: {}. Call skills_list for the full bounded catalogue.",
                        available.join(", ")
                    )
                };
                return Err(AppError::new(
                    "FILE_NOT_FOUND",
                    format!("skill `{}` or resource `{resource}` not found. {suffix}", args.name),
                ));
            }
            let skill = skill.expect("checked above");
            let skill_dir = skill.path.parent().ok_or_else(|| AppError::new("INVALID_INPUT", "skill has no package directory"))?;
            let selected = skill_dir.join(resource);
            let relative = selected.strip_prefix(&skill.root)
                .map_err(|_| AppError::new("PATH_OUTSIDE_WORKSPACE", "skill escaped its root"))?;
            let content = read_bounded(
                &skill.root,
                relative,
                SKILL_DOC_LIMIT,
            )?;
            let requested = args.limit.unwrap_or(SKILL_PAGE_MAX).min(SKILL_PAGE_MAX);
            let (page, next_offset, truncated) = byte_page(&content, args.offset, requested)?;
            let (package_files, package_files_truncated) = if resource == "SKILL.md" && !truncated {
                package_files(&skill)?
            } else {
                (Vec::new(), false)
            };
            let value = json!({
                "name":skill.name,
                "resource":resource,
                "content":page,
                "offset":args.offset,
                "shown_bytes":next_offset.saturating_sub(args.offset),
                "total_bytes":content.len(),
                "truncated":truncated,
                "next_offset":truncated.then_some(next_offset),
                "continuation":truncated.then_some("Call skills_read again with next_offset."),
                "package_files":package_files,
                "package_files_truncated":package_files_truncated,
            });
            let text = format!(
                "Loaded {} / {} ({} of {} bytes). Full content is in structuredContent.content.",
                value["name"].as_str().unwrap_or("skill"),
                resource,
                value["shown_bytes"].as_u64().unwrap_or(0),
                value["total_bytes"].as_u64().unwrap_or(0),
            );
            Ok((structured_result_with_text(value.clone(), text), value))
        }).await
    }
}

#[cfg(test)]
mod tests {
    use super::home::{AgentEcosystem, AgentHome};
    use super::project_docs::{project_docs, project_docs_with_home};
    use super::skills::{
        SKILL_PACKAGE_MAX_FILES, SKILL_WARNING_MAX, parse_frontmatter, push_skill_warning,
        skill_catalog, skill_catalog_from_sources,
    };
    use super::*;
    use crate::{
        config::{Config, ConfigBuilder},
        project::{ProjectContext, ProjectKey},
        request_context::TransportMode,
    };
    use std::collections::BTreeMap;
    use std::path::Path;

    fn config() -> Config {
        ConfigBuilder::from_map(BTreeMap::from([(
            "MCP_AUTH_TOKEN".to_owned(),
            "1234567890abcdef".to_owned(),
        )]))
        .build()
        .unwrap()
    }

    fn project(root: &Path) -> ProjectContext {
        let root = root.canonicalize().expect("test project root must exist");
        ProjectContext {
            native_project_key: ProjectKey::new("native_key".to_owned()).unwrap(),
            effective_project_key: ProjectKey::new("effective_key".to_owned()).unwrap(),
            project_alias: None,
            project_root: root.clone(),
            metadata_root: root.join(".metadata"),
            transport_mode: TransportMode::Stateless,
            mcp_session_present: false,
        }
    }

    #[test]
    fn parses_skill_frontmatter_and_pages_unicode() {
        let content = "---\nname: rust-demo\ndescription: Use for Rust work.\n---\nHéllo";
        assert_eq!(
            parse_frontmatter(content, "fallback").unwrap(),
            ("rust-demo".to_owned(), "Use for Rust work.".to_owned())
        );
        let (page, next, truncated) = byte_page("Héllo", 0, 2).unwrap();
        assert_eq!(page, "H");
        assert_eq!(next, 1);
        assert!(truncated);
        assert!(byte_page("content", 0, 0).is_err());
    }

    #[test]
    fn rejects_invalid_skill_frontmatter() {
        assert!(parse_frontmatter("no yaml", "fallback").is_err());
        assert!(parse_frontmatter("---\nname: ../x\ndescription: nope\n---", "x").is_err());
    }

    #[test]
    fn parses_metadata_short_description() {
        let content = "---\nname: rust-demo\nmetadata:\n  short-description: Build and test Rust safely.\n---\nBody";
        assert_eq!(
            parse_frontmatter(content, "fallback").unwrap(),
            (
                "rust-demo".to_owned(),
                "Build and test Rust safely.".to_owned()
            )
        );
    }

    #[test]
    fn malformed_and_duplicate_skills_do_not_break_catalogue() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        for directory in [
            ".agents/skills/a-first",
            ".agents/skills/b-duplicate",
            ".agents/skills/c-broken",
            ".codex/skills/a-shadowed",
            ".codex/skills/valid",
        ] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        std::fs::write(
            root.join(".agents/skills/a-first/SKILL.md"),
            "---\nname: shared\ndescription: Highest precedence.\n---\n",
        )
        .unwrap();
        std::fs::write(
            root.join(".agents/skills/b-duplicate/SKILL.md"),
            "---\nname: shared\ndescription: Later lexical package.\n---\n",
        )
        .unwrap();
        std::fs::write(
            root.join(".agents/skills/c-broken/SKILL.md"),
            "not frontmatter",
        )
        .unwrap();
        std::fs::write(
            root.join(".codex/skills/a-shadowed/SKILL.md"),
            "---\nname: shared\ndescription: Lower scope.\n---\n",
        )
        .unwrap();
        std::fs::write(
            root.join(".codex/skills/valid/SKILL.md"),
            "---\nname: valid\nmetadata:\n  short-description: Still discovered.\n---\n",
        )
        .unwrap();

        let project = project(root);
        let catalog = skill_catalog(&project).unwrap();
        assert_eq!(catalog.skills.len(), 2);
        let shared = catalog
            .skills
            .iter()
            .find(|skill| skill.name == "shared")
            .unwrap();
        assert_eq!(shared.description, "Highest precedence.");
        assert_eq!(shared.source, "project:./.agents/skills");
        assert!(catalog.skills.iter().any(|skill| skill.name == "valid"));
        assert_eq!(
            catalog
                .warnings
                .iter()
                .filter(|warning| warning.code == "DUPLICATE_SKILL")
                .count(),
            2
        );
        assert!(
            catalog
                .warnings
                .iter()
                .any(|warning| warning.code == "INVALID_SKILL")
        );

        let instructions = project_instructions(&project, &config(), &[], &[]).unwrap();
        assert!(instructions.contains("`valid`"));
        assert!(instructions.contains("INVALID_SKILL"));
        assert!(instructions.contains("YOLO semantics"));
    }

    #[test]
    fn project_documents_are_the_final_instruction_layer() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let skill_root = root.join(".agents/skills/demo");
        std::fs::create_dir_all(&skill_root).unwrap();
        std::fs::write(root.join("AGENTS.md"), "PROJECT_RULE_LAST").unwrap();
        std::fs::write(
            skill_root.join("SKILL.md"),
            "---\nname: demo\ndescription: Demo skill.\n---\n",
        )
        .unwrap();
        let extras = vec!["SAVED_STATE_SECTION".to_owned()];
        let instructions = project_instructions(&project(root), &config(), &[], &extras).unwrap();
        let environment = instructions
            .find("Environment (identity-independent")
            .unwrap();
        let skills = instructions.find("Available skills").unwrap();
        let saved_state = instructions.find("SAVED_STATE_SECTION").unwrap();
        let project_marker = instructions.find("--- project-doc ---").unwrap();
        assert!(environment < project_marker);
        assert!(skills < project_marker);
        assert!(saved_state < project_marker);
        assert!(instructions.ends_with("PROJECT_RULE_LAST"));
    }

    #[test]
    fn project_documents_follow_root_to_target_with_override_precedence() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "root instructions").unwrap();
        std::fs::write(root.join("src/AGENTS.md"), "ignored sibling").unwrap();
        std::fs::write(
            root.join("src/AGENTS.override.md"),
            "source override instructions",
        )
        .unwrap();
        std::fs::write(root.join("src/nested/AGENTS.md"), "nested instructions").unwrap();
        std::fs::write(root.join("src/nested/lib.rs"), "pub fn example() {}").unwrap();

        let documents = project_docs(&project(root), Some("src/nested/lib.rs"), &[]).unwrap();
        assert_eq!(
            documents
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "AGENTS.md",
                "src/AGENTS.override.md",
                "src/nested/AGENTS.md"
            ]
        );
        assert_eq!(documents[1].1, "source override instructions");
    }

    #[test]
    fn project_document_fallback_is_used_only_after_agents_files() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        std::fs::write(root.join("CLAUDE.md"), "fallback instructions").unwrap();
        let fallbacks = vec!["CLAUDE.md".to_owned()];
        let documents = project_docs(&project(root), None, &fallbacks).unwrap();
        assert_eq!(
            documents,
            vec![("CLAUDE.md".to_owned(), "fallback instructions".to_owned())]
        );

        std::fs::write(root.join("AGENTS.md"), "agents instructions").unwrap();
        let documents = project_docs(&project(root), None, &fallbacks).unwrap();
        assert_eq!(
            documents,
            vec![("AGENTS.md".to_owned(), "agents instructions".to_owned())]
        );
    }

    #[cfg(unix)]
    #[test]
    fn project_document_symlink_is_supported_for_shared_instructions() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let shared = tempfile::tempdir().unwrap();
        std::fs::write(shared.path().join("AGENTS.md"), "shared instructions").unwrap();
        symlink(
            shared.path().join("AGENTS.md"),
            temporary.path().join("AGENTS.md"),
        )
        .unwrap();

        let documents = project_docs(&project(temporary.path()), None, &[]).unwrap();
        assert_eq!(documents[0].1, "shared instructions");
    }

    #[cfg(unix)]
    #[test]
    fn project_rule_symlink_is_supported_for_shared_instructions() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let shared = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temporary.path().join(".agents/rules")).unwrap();
        std::fs::write(shared.path().join("shared.md"), "shared rule").unwrap();
        symlink(
            shared.path().join("shared.md"),
            temporary.path().join(".agents/rules/shared.md"),
        )
        .unwrap();

        let documents = project_docs(&project(temporary.path()), None, &[]).unwrap();
        assert!(documents.iter().any(|(source, content)| {
            source == ".agents/rules/shared.md" && content == "shared rule"
        }));
    }

    #[test]
    fn home_instructions_are_loaded_before_project_instructions() {
        let project_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        std::fs::write(home_dir.path().join("AGENTS.md"), "home baseline").unwrap();
        std::fs::write(project_dir.path().join("AGENTS.md"), "project override").unwrap();
        let home = AgentHome {
            ecosystem: AgentEcosystem::Agents,
            root: home_dir.path().to_path_buf(),
        };

        let documents =
            project_docs_with_home(&project(project_dir.path()), None, &[], Some(home)).unwrap();
        assert_eq!(documents[0].0, "home:agents/AGENTS.md");
        assert_eq!(documents[0].1, "home baseline");
        assert_eq!(documents.last().unwrap().0, "AGENTS.md");
        assert_eq!(documents.last().unwrap().1, "project override");
    }

    #[test]
    fn project_rule_reads_share_the_global_instruction_budget() {
        let temporary = tempfile::tempdir().unwrap();
        let rules = temporary.path().join(".agents/rules");
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::write(rules.join("a.md"), "a".repeat(200 * 1024)).unwrap();
        std::fs::write(rules.join("b.md"), "b".repeat(100 * 1024)).unwrap();

        let documents =
            project_docs_with_home(&project(temporary.path()), None, &[], None).unwrap();
        assert_eq!(documents.len(), 2);
        assert_eq!(
            documents
                .iter()
                .map(|(_, content)| content.len())
                .sum::<usize>(),
            256 * 1024
        );
        assert_eq!(documents[0].0, ".agents/rules/a.md");
        assert_eq!(documents[1].0, ".agents/rules/b.md [truncated]");
        assert_eq!(documents[1].1.len(), 56 * 1024);
    }

    #[test]
    fn home_and_project_instruction_budgets_are_independent() {
        let project_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let rules = project_dir.path().join(".agents/rules");
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::write(home_dir.path().join("AGENTS.md"), "h".repeat(200 * 1024)).unwrap();
        std::fs::write(rules.join("project.md"), "p".repeat(200 * 1024)).unwrap();
        let home = AgentHome {
            ecosystem: AgentEcosystem::Agents,
            root: home_dir.path().to_path_buf(),
        };

        let documents =
            project_docs_with_home(&project(project_dir.path()), None, &[], Some(home)).unwrap();

        assert_eq!(documents.len(), 2);
        assert_eq!(documents[0].0, "home:agents/AGENTS.md [truncated]");
        assert_eq!(documents[0].1.len(), 128 * 1024);
        assert_eq!(documents[1].0, ".agents/rules/project.md");
        assert_eq!(documents[1].1.len(), 200 * 1024);
    }

    #[test]
    fn skill_warning_collection_is_bounded() {
        let mut warnings = Vec::new();
        for index in 0..(SKILL_WARNING_MAX + 20) {
            push_skill_warning(
                &mut warnings,
                "INVALID_SKILL",
                ".agents/skills",
                &format!("package-{index}"),
                "x".repeat(1024),
            );
        }
        assert_eq!(warnings.len(), SKILL_WARNING_MAX);
        assert!(warnings.iter().all(|warning| warning.message.len() == 512));
    }

    #[test]
    fn skill_package_listing_is_bounded_and_marked() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let skill_root = root.join(".agents/skills/package");
        std::fs::create_dir_all(&skill_root).unwrap();
        std::fs::write(
            skill_root.join("SKILL.md"),
            "---\nname: package\ndescription: Package resources.\n---\n",
        )
        .unwrap();
        for index in 0..(SKILL_PACKAGE_MAX_FILES + 1) {
            std::fs::write(skill_root.join(format!("resource-{index:03}.txt")), "x").unwrap();
        }

        let project = project(root);
        let catalog = skill_catalog(&project).unwrap();
        let (files, truncated) = package_files(&catalog.skills[0]).unwrap();
        assert_eq!(files.len(), SKILL_PACKAGE_MAX_FILES);
        assert!(truncated);
        assert_eq!(files[0], "resource-000.txt");
    }

    #[test]
    fn home_skills_are_read_only_catalogue_entries_with_lower_precedence() {
        let project_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let home_skill_root = home_dir.path().join("skills");
        let project_skill = project_dir.path().join(".agents/skills/local");
        let home_shadow = home_skill_root.join("shadow");
        let home_unique = home_skill_root.join("user-home");
        for directory in [&project_skill, &home_shadow, &home_unique] {
            std::fs::create_dir_all(directory).unwrap();
        }
        std::fs::write(
            project_skill.join("SKILL.md"),
            "---\nname: shared\ndescription: Project definition.\n---\n",
        )
        .unwrap();
        std::fs::write(
            home_shadow.join("SKILL.md"),
            "---\nname: shared\ndescription: Home shadow.\n---\n",
        )
        .unwrap();
        std::fs::write(
            home_unique.join("SKILL.md"),
            "---\nname: user-home\ndescription: User-home skill.\n---\n",
        )
        .unwrap();
        std::fs::write(home_unique.join("reference.md"), "home reference").unwrap();

        let project = project(project_dir.path());
        let user_roots = vec![home_skill_root.clone()];
        let catalog = skill_catalog_from_sources(&project, None, &user_roots, &[]).unwrap();
        let shared = catalog
            .skills
            .iter()
            .find(|skill| skill.name == "shared")
            .unwrap();
        assert_eq!(shared.scope, "project");
        assert_eq!(shared.description, "Project definition.");
        let home_skill = catalog
            .skills
            .iter()
            .find(|skill| skill.name == "user-home")
            .unwrap();
        assert_eq!(home_skill.scope, "user");
        assert_eq!(home_skill.root, home_skill_root);
        let reference = home_skill.path.parent().unwrap().join("reference.md");
        let relative = reference.strip_prefix(&home_skill.root).unwrap();
        assert_eq!(
            read_bounded(&home_skill.root, relative, SKILL_DOC_LIMIT).unwrap(),
            "home reference"
        );
        assert!(catalog.warnings.iter().any(|warning| {
            warning.code == "DUPLICATE_SKILL"
                && warning.source == format!("user:{}", home_skill_root.display())
        }));
    }

    #[test]
    fn claude_home_plugins_are_namespaced_and_use_latest_version() {
        let project_dir = tempfile::tempdir().unwrap();
        let plugin_cache = tempfile::tempdir().unwrap();
        for version in ["1.2.0", "1.10.0"] {
            let package = plugin_cache
                .path()
                .join("marketplace/demo-plugin")
                .join(version)
                .join("skills/decompile");
            std::fs::create_dir_all(&package).unwrap();
            std::fs::write(
                package.join("SKILL.md"),
                format!("---\nname: decompile\ndescription: Version {version}.\n---\n"),
            )
            .unwrap();
        }
        let catalog = skill_catalog_from_sources(
            &project(project_dir.path()),
            None,
            &[],
            &[plugin_cache.path().to_path_buf()],
        )
        .unwrap();
        let skill = catalog
            .skills
            .iter()
            .find(|skill| skill.name == "demo-plugin:decompile")
            .unwrap();
        assert_eq!(skill.scope, "plugin");
        assert_eq!(skill.description, "Version 1.10.0.");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_skill_packages_and_resources_are_supported() {
        use std::os::unix::fs::symlink;

        let project_dir = tempfile::tempdir().unwrap();
        let shared_package = tempfile::tempdir().unwrap();
        let shared_resource = tempfile::tempdir().unwrap();
        let skill_scope = project_dir.path().join(".agents/skills");
        std::fs::create_dir_all(&skill_scope).unwrap();
        std::fs::write(
            shared_package.path().join("SKILL.md"),
            "---\nname: shared-package\ndescription: Symlinked package.\n---\n",
        )
        .unwrap();
        std::fs::write(
            shared_resource.path().join("reference.md"),
            "shared resource",
        )
        .unwrap();
        symlink(
            shared_resource.path().join("reference.md"),
            shared_package.path().join("reference.md"),
        )
        .unwrap();
        symlink(shared_package.path(), skill_scope.join("shared-package")).unwrap();

        let catalog =
            skill_catalog_from_sources(&project(project_dir.path()), None, &[], &[]).unwrap();
        let skill = catalog
            .skills
            .iter()
            .find(|skill| skill.name == "shared-package")
            .unwrap();
        let (files, _) = package_files(skill).unwrap();
        assert!(files.iter().any(|path| path == "reference.md"));
        let reference = skill.path.parent().unwrap().join("reference.md");
        let relative = reference.strip_prefix(&skill.root).unwrap();
        assert_eq!(
            read_bounded(&skill.root, relative, SKILL_DOC_LIMIT).unwrap(),
            "shared resource"
        );
    }

    #[test]
    fn skill_resource_validation_rejects_escape_and_absolute_paths() {
        assert!(validate_skill_resource("references/api.md").is_ok());
        assert!(validate_skill_resource("../secret").is_err());
        assert!(validate_skill_resource("/etc/passwd").is_err());
        assert!(validate_skill_resource(r"C:\\Windows\\x").is_err());
    }
}
