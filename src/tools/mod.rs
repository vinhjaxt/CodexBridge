use std::{
    borrow::Cow,
    future::Future,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use dashmap::DashMap;
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use crate::{
    audit::AuditLogger,
    config::Config,
    error::{AppError, Result as AppResult},
    project::{ProjectContext, ProjectResolver},
    request_context::{InitializationRequestContext, ProjectRequestContext, RequestIdentity},
    runtime_environment::RuntimeEnvironment,
    sandbox::SecurePathResolver,
    storage::Storage,
};

mod agent;
mod continuity;
mod contracts;
mod filesystem;
mod misc;
mod patch;
mod process;
mod registry;
mod search;

use contracts::typed_output_schema;
use process::ProcessRegistry;
use registry::NativeToolRegistry;

pub(crate) fn extension_arg<T>(
    extensions: &std::collections::BTreeMap<String, Value>,
    key: &str,
) -> AppResult<Option<T>>
where
    T: serde::de::DeserializeOwned,
{
    let Some(value) = extensions.get(key) else {
        return Ok(None);
    };
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|_| {
            AppError::new(
                "INVALID_INPUT",
                format!("extensions.{key} has an invalid value"),
            )
        })
}

fn tool_contract_hash(router: &ToolRouter<AgentHandler>) -> String {
    let mut routes = router.map.iter().collect::<Vec<_>>();
    routes.sort_by_key(|(name, _)| *name);
    let mut hasher = Sha256::new();
    hasher.update(b"codexbridge-tool-contract-v1\0");
    for (name, route) in routes {
        hasher.update(name.as_bytes());
        hasher.update([0]);
        if let Some(description) = route.attr.description.as_deref() {
            hasher.update(description.as_bytes());
        }
        hasher.update([0]);
        if let Ok(input) = serde_json::to_vec(route.attr.input_schema.as_ref()) {
            hasher.update(input);
        }
        hasher.update([0]);
        if let Some(output_schema) = route.attr.output_schema.as_ref()
            && let Ok(output) = serde_json::to_vec(output_schema.as_ref())
        {
            hasher.update(output);
        }
        hasher.update([0xff]);
    }
    format!("{:x}", hasher.finalize())
}

fn server_contract_version(router: &ToolRouter<AgentHandler>) -> String {
    let hash = tool_contract_hash(router);
    format!("{}+contract.{}", env!("CARGO_PKG_VERSION"), &hash[..12])
}

const INSTRUCTION_SCOPE_CACHE_MAX_ENTRIES: usize = 4096;
const PROJECT_PERMIT_CACHE_MAX_ENTRIES: usize = 4096;

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct InitArgs {
    #[serde(default)]
    pub project_key: Option<String>,
    /// Continuity reference from a preceding CodexBridge assistant response.
    /// A correct reference determines the project for an unbound conversation.
    /// For an already-bound conversation, a missing/stale/invalid reference
    /// falls back to that conversation's persisted project and latest turn.
    #[serde(default)]
    pub previous_turn_ref: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct PathArgs {
    pub path: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ReadFileArgs {
    pub path: String,
    #[serde(default)]
    pub offset: usize,
    /// Byte offset within the first logical line. Normally zero; use the
    /// continuation value returned by read_file when one very long line is
    /// split by the presentation byte budget.
    #[serde(default)]
    pub line_byte_offset: usize,
    #[serde(default)]
    pub limit: Option<usize>,
    /// Optional presentation byte budget for this call. The default remains
    /// OUTPUT_FILE_BYTES; callers inspecting unusually large generated/minified
    /// lines can request a larger window up to OUTPUT_MULTI_FILE_BYTES.
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct PatchArgs {
    pub input: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PatchUpdate {
    pub path: String,
    pub old: Option<Vec<u8>>,
    pub new: Option<Vec<u8>>,
}

#[derive(Clone)]
pub(crate) struct ProjectPermitRegistry {
    entries: Arc<DashMap<String, PermitSet>>,
    mutation_lock: Arc<Mutex<()>>,
    tool_limit: usize,
    process_limit: usize,
}

type PermitSet = (Arc<Semaphore>, Arc<Semaphore>, Arc<Semaphore>);

impl ProjectPermitRegistry {
    fn new(tool_limit: usize, process_limit: usize) -> Self {
        Self {
            entries: Arc::new(DashMap::new()),
            mutation_lock: Arc::new(Mutex::new(())),
            tool_limit: tool_limit.max(1),
            process_limit: process_limit.max(1),
        }
    }

    fn get(
        &self,
        project_key: &str,
    ) -> AppResult<(Arc<Semaphore>, Arc<Semaphore>, Arc<Semaphore>)> {
        if let Some(entry) = self.entries.get(project_key) {
            return Ok(entry.clone());
        }
        let _mutation = self.mutation_lock.lock().map_err(|_| {
            AppError::new("SERVER_BUSY", "project concurrency registry lock poisoned")
        })?;
        if let Some(entry) = self.entries.get(project_key) {
            return Ok(entry.clone());
        }
        if self.entries.len() >= PROJECT_PERMIT_CACHE_MAX_ENTRIES {
            let victim = self
                .entries
                .iter()
                .find(|entry| {
                    entry.key().as_str() != project_key
                        && entry.value().0.available_permits() == self.tool_limit
                        && entry.value().1.available_permits() == self.process_limit
                        && entry.value().2.available_permits() == 1
                })
                .map(|entry| entry.key().clone());
            if let Some(victim) = victim {
                self.entries.remove(&victim);
            } else {
                return Err(AppError::new(
                    "SERVER_BUSY",
                    "project concurrency registry is full; retry after another project becomes idle",
                ));
            }
        }
        let value = (
            Arc::new(Semaphore::new(self.tool_limit)),
            Arc::new(Semaphore::new(self.process_limit)),
            Arc::new(Semaphore::new(1)),
        );
        let entry = self
            .entries
            .entry(project_key.to_owned())
            .or_insert_with(|| value.clone());
        Ok(entry.clone())
    }
}

pub(crate) struct SharedState {
    pub(crate) config: Arc<Config>,
    pub(crate) resolver: ProjectResolver,
    pub(crate) storage: Storage,
    pub(crate) audit: AuditLogger,
    pub(crate) paths: SecurePathResolver,
    pub(crate) tools: Arc<Semaphore>,
    pub(crate) cpu: Arc<Semaphore>,
    pub(crate) processes: Arc<Semaphore>,
    pub(crate) searches: Arc<Semaphore>,
    pub(crate) patches: Arc<Semaphore>,
    pub(crate) project_permits: ProjectPermitRegistry,
    instruction_scopes: Arc<DashMap<String, String>>,
    pub(crate) interactive: ProcessRegistry,
    pub(crate) active_processes: Arc<AtomicUsize>,
    pub(crate) upstream: crate::upstream::Aggregator,
}

impl SharedState {
    pub(crate) fn new(
        config: Arc<Config>,
        resolver: ProjectResolver,
        storage: Storage,
        audit: AuditLogger,
        upstream: crate::upstream::Aggregator,
    ) -> Arc<Self> {
        Arc::new(Self {
            tools: Arc::new(Semaphore::new(config.limits.max_concurrent_tools.max(1))),
            cpu: Arc::new(Semaphore::new(config.limits.max_concurrent_cpu.max(1))),
            processes: Arc::new(Semaphore::new(
                config.limits.max_concurrent_processes.max(1),
            )),
            searches: Arc::new(Semaphore::new(config.limits.max_concurrent_searches.max(1))),
            patches: Arc::new(Semaphore::new(config.limits.max_concurrent_patches.max(1))),
            project_permits: ProjectPermitRegistry::new(
                config.limits.per_project_tools,
                config.limits.per_project_processes,
            ),
            instruction_scopes: Arc::new(DashMap::new()),
            interactive: ProcessRegistry::new(
                config.max_interactive_processes,
                config.interactive_process_idle,
                config.limits.process_output_bytes,
            ),
            active_processes: Arc::new(AtomicUsize::new(0)),
            upstream,
            paths: SecurePathResolver,
            config,
            resolver,
            storage,
            audit,
        })
    }

    pub(crate) async fn permit(
        &self,
        semaphore: Arc<Semaphore>,
    ) -> AppResult<OwnedSemaphorePermit> {
        tokio::time::timeout(self.config.limits.overload_wait, semaphore.acquire_owned())
            .await
            .map_err(|_| {
                AppError::new(
                    "SERVER_BUSY",
                    "server capacity is temporarily exhausted; retry after an existing operation completes",
                )
            })?
            .map_err(|_| AppError::new("SERVER_BUSY", "server capacity semaphore is closed"))
    }

    pub(crate) async fn tool_scope(
        &self,
        identity: &RequestIdentity,
    ) -> AppResult<(ProjectContext, OwnedSemaphorePermit, OwnedSemaphorePermit)> {
        let project = self.resolver.resolve_initialized(identity)?;
        let (project_tools, _, _) = self
            .project_permits
            .get(project.effective_project_key.as_str())?;
        let global = self.permit(self.tools.clone()).await?;
        let project_permit = self.permit(project_tools).await?;
        Ok((project, global, project_permit))
    }

    pub(crate) fn active_processes(&self) -> usize {
        self.active_processes.load(Ordering::Relaxed) + self.interactive.active()
    }

    pub(crate) fn project_cache_entries(&self) -> usize {
        self.resolver.initialized_cache_entries()
    }

    pub(crate) fn scoped_instruction_notice(
        &self,
        project: &ProjectContext,
        target: &str,
    ) -> AppResult<Option<String>> {
        let docs =
            agent::project_instruction_delta(project, target, &self.config.project_doc_fallbacks)?;
        if docs.is_empty() {
            return Ok(None);
        }
        let source_key = docs
            .iter()
            .map(|(source, _)| source.as_str())
            .collect::<Vec<_>>()
            .join("\0");
        let key = format!("{}\0{}", project.native_project_key.as_str(), source_key);
        let mut rendered = String::from(
            "Nested project instructions now apply to this path. Consume them before continuing work in this scope:\n",
        );
        for (source, content) in docs {
            rendered.push_str(&format!("\n--- {source} ---\n{content}\n"));
        }
        let hash = content_hash(&rendered);
        if self
            .instruction_scopes
            .get(&key)
            .is_some_and(|existing| existing.as_str() == hash)
        {
            return Ok(None);
        }
        if self.instruction_scopes.len() >= INSTRUCTION_SCOPE_CACHE_MAX_ENTRIES {
            let victim = self
                .instruction_scopes
                .iter()
                .find(|entry| entry.key().as_str() != key)
                .map(|entry| entry.key().clone());
            if let Some(victim) = victim {
                self.instruction_scopes.remove(&victim);
            }
        }
        self.instruction_scopes.insert(key, hash);
        Ok(Some(rendered))
    }
}

#[derive(Clone)]
pub struct AgentHandler {
    pub(crate) shared: Arc<SharedState>,
    tool_router: ToolRouter<Self>,
}

impl AgentHandler {
    pub(crate) fn new(shared: Arc<SharedState>) -> Self {
        let mut tool_router = Self::native_router();
        shared.upstream.add_routes(&mut tool_router);
        if let Some(route) = tool_router.map.get_mut("exec_command") {
            let environment = RuntimeEnvironment::detect(&shared.config);
            route.attr.description = Some(Cow::Owned(format!(
                "Run a bounded command from a project-relative workdir. Native YOLO execution is not OS-filesystem-confined and has the daemon account's normal filesystem/network reach; Bubblewrap keeps network access, and Podman may fall back to native when its Bubblewrap capability probe fails. {} Long-running commands return session_id and must be continued with write_stdin rather than restarted. Use stdin+close_stdin for one-shot EOF-driven CLIs and tty=true for PTY/ConPTY programs. completion_reason is authoritative. output_offset/output_next_offset are logical byte cursors; after bounded head+tail eviction, output can include an omitted-bytes marker and evicted bytes are unrecoverable. Recover lost output with write_stdin(since_output_offset=...). max_output_tokens only caps presentation, so retry the same cursor with a larger/omitted cap if needed. Finished truncated sessions retain session_id. PTY output may contain ANSI. Shell: {} ({}). Default backend: {}. Native fallback: {}. No approval step.",
                environment.podman_invocation.agent_advice(),
                environment.shell,
                environment.shell_kind,
                environment.sandbox_backend,
                if shared.config.allow_unsandboxed_exec {
                    "enabled"
                } else {
                    "disabled"
                }
            )));
        }
        Self {
            shared,
            tool_router,
        }
    }

    pub(crate) fn native_router() -> ToolRouter<Self> {
        let mut router = NativeToolRegistry::build([
            Self::core_router(),
            Self::continuity_router(),
            Self::filesystem_router(),
            Self::process_router(),
            Self::search_router(),
            Self::agent_router(),
            Self::misc_router(),
        ]);
        for route in router.map.values_mut() {
            if let Some(schema) = typed_output_schema(route.name()) {
                route.attr.output_schema = Some(schema);
            }
        }
        router
    }

    pub(crate) fn native_tool_count() -> usize {
        Self::native_router().map.len()
    }

    pub(crate) fn validate_small(&self, value: &str) -> AppResult<()> {
        if value.len() > self.shared.config.limits.input_string_bytes {
            return Err(AppError::new(
                "INPUT_TOO_LARGE",
                "input string exceeds MAX_INPUT_STRING_BYTES",
            ));
        }
        Ok(())
    }

    pub(crate) async fn run<F, Fut>(
        &self,
        project: AppResult<ProjectContext>,
        tool: &'static str,
        params: Value,
        operation: F,
    ) -> std::result::Result<CallToolResult, ErrorData>
    where
        F: FnOnce(ProjectContext) -> Fut,
        Fut: Future<Output = AppResult<Value>>,
    {
        let project = match project {
            Ok(project) => project,
            Err(error) => return Ok(error_result(&error)),
        };
        let (project_tools, _, _) = match self
            .shared
            .project_permits
            .get(project.effective_project_key.as_str())
        {
            Ok(value) => value,
            Err(error) => return Ok(error_result(&error)),
        };
        let _global = match self.shared.permit(self.shared.tools.clone()).await {
            Ok(value) => value,
            Err(error) => return Ok(error_result(&error)),
        };
        let _project = match self.shared.permit(project_tools).await {
            Ok(value) => value,
            Err(error) => return Ok(error_result(&error)),
        };
        let (request_id, started) = self.shared.audit.tool_started(&project, tool, params);
        match operation(project.clone()).await {
            Ok(value) => {
                self.shared
                    .audit
                    .tool_finished(&project, &request_id, tool, started, &value);
                Ok(structured_result(value))
            }
            Err(error) => {
                self.shared
                    .audit
                    .tool_failed(&project, &request_id, tool, started, &error);
                Ok(error_result(&error))
            }
        }
    }

    pub(crate) async fn run_content<F, Fut>(
        &self,
        project: AppResult<ProjectContext>,
        tool: &'static str,
        params: Value,
        operation: F,
    ) -> std::result::Result<CallToolResult, ErrorData>
    where
        F: FnOnce(ProjectContext) -> Fut,
        Fut: Future<Output = AppResult<(CallToolResult, Value)>>,
    {
        let project = match project {
            Ok(project) => project,
            Err(error) => return Ok(error_result(&error)),
        };
        let (project_tools, _, _) = match self
            .shared
            .project_permits
            .get(project.effective_project_key.as_str())
        {
            Ok(value) => value,
            Err(error) => return Ok(error_result(&error)),
        };
        let _global = match self.shared.permit(self.shared.tools.clone()).await {
            Ok(value) => value,
            Err(error) => return Ok(error_result(&error)),
        };
        let _project = match self.shared.permit(project_tools).await {
            Ok(value) => value,
            Err(error) => return Ok(error_result(&error)),
        };
        let (request_id, started) = self.shared.audit.tool_started(&project, tool, params);
        match operation(project.clone()).await {
            Ok((result, audit_value)) => {
                self.shared
                    .audit
                    .tool_finished(&project, &request_id, tool, started, &audit_value);
                Ok(result)
            }
            Err(error) => {
                self.shared
                    .audit
                    .tool_failed(&project, &request_id, tool, started, &error);
                Ok(error_result(&error))
            }
        }
    }
}

pub(crate) fn structured_result(value: Value) -> CallToolResult {
    let mut result = CallToolResult::success(vec![ContentBlock::text(
        "Structured result is available in structuredContent.",
    )]);
    result.structured_content = Some(value);
    result
}

pub(crate) fn structured_result_with_text(value: Value, text: String) -> CallToolResult {
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    result.structured_content = Some(value);
    result
}

pub(crate) fn error_result(error: &AppError) -> CallToolResult {
    // structuredContent is deliberately omitted on tool execution errors.
    // Every public tool declares a success outputSchema, and MCP clients are
    // allowed to validate structuredContent against it. Returning a generic
    // {code,message} object here violates those schemas and can make strict
    // clients surface a transport/protocol exception instead of the tool error.
    CallToolResult::error(vec![ContentBlock::text(format!(
        "{}: {}",
        error.code(),
        error.message()
    ))])
}

fn is_turn_init_soft_stop_error(error: &AppError) -> bool {
    matches!(
        error.code(),
        "PREVIOUS_TURN_REF_REQUIRED"
            | "STALE_TURN_REF"
            | "TURN_REF_NOT_FOUND"
            | "TURN_PROJECT_MISMATCH"
    )
}

fn turn_init_soft_stop_result(error: &AppError) -> CallToolResult {
    let message = format!(
        "STOP_CURRENT_TURN: CodexBridge did not synchronize this user turn ({}: {}). Do not send any more tool calls to CodexBridge during this user turn, including `chatgpt_turn_init`. Tell the user that project synchronization could not be completed, then stop immediately and wait for the next user message. On the next project-bearing user turn, call `chatgpt_turn_init` exactly once. Prefer the exact `previous_turn_ref` from the nearest preceding CodexBridge `[ref:...]` marker when available; an already-bound conversation can recover from a missing, stale, or invalid reference.",
        error.code(),
        error.message()
    );
    let value = json!({
        "status": "soft_error",
        "soft_error": {
            "code": error.code(),
            "message": error.message(),
        },
    });
    structured_result_with_text(value, message)
}

fn new_turn_ref() -> String {
    format!("r_{}", URL_SAFE_NO_PAD.encode(Uuid::now_v7().as_bytes()))
}

fn content_hash(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

fn turn_protocol(turn_ref: &str) -> String {
    format!(
        "CodexBridge turn synchronization protocol (server contract; project AGENTS instructions do not override it):\n- `chatgpt_turn_init` has already run successfully for the current user turn. Do not call it again during this same user turn, regardless of how many project tools you call afterward.\n- Current server-issued turn reference: `{turn_ref}`. Every project-related final response for this user turn must end with exactly `[ref:{turn_ref}]`. Never invent, alter, or reuse another reference.\n- After the user sends a new message that needs project state, call `chatgpt_turn_init` exactly once before project-scoped work or a project-state-dependent answer. Pass this turn reference as `previous_turn_ref`. Treat any returned full brief as authoritative. If only saved project state changed, consume `state_update` while keeping the unchanged instruction context already present in the conversation."
    )
}

pub(crate) fn project_instruction_context(
    shared: &SharedState,
    project: &ProjectContext,
) -> AppResult<String> {
    project_context_with_state(shared, project, None)
}

fn project_context_with_state(
    shared: &SharedState,
    project: &ProjectContext,
    state: Option<&str>,
) -> AppResult<String> {
    let mut extra_sections = Vec::new();
    if let Some(state) = state {
        extra_sections.push(state.to_owned());
    }
    let gateway_skills = shared.upstream.gateway_skill_summaries();
    if !gateway_skills.is_empty() {
        let catalogue = gateway_skills
            .iter()
            .filter_map(|skill| {
                Some(format!(
                    "- `{}`: {}",
                    skill.get("name")?.as_str()?,
                    skill.get("description")?.as_str()?
                ))
            })
            .collect::<Vec<_>>()
            .join("\n");
        extra_sections.push(format!(
            "Configured upstream MCP gateways (progressive disclosure):\n{catalogue}\nUse `skills_read` on the selected gateway skill before calling its gateway tool so the upstream function schemas do not occupy the base tool context."
        ));
    }
    agent::project_instructions(
        project,
        &shared.config,
        &shared.config.project_doc_fallbacks,
        &extra_sections,
    )
}

fn full_turn_brief(context: &str, turn_ref: &str) -> String {
    format!("{context}\n\n{}", turn_protocol(turn_ref))
}

fn project_state_snapshot(
    storage: &Storage,
    project: &ProjectContext,
) -> AppResult<(String, Option<String>)> {
    project_state_snapshot_with_hook(storage, project, || {})
}

fn project_state_snapshot_with_hook<F>(
    storage: &Storage,
    project: &ProjectContext,
    after_memory_page: F,
) -> AppResult<(String, Option<String>)>
where
    F: FnOnce(),
{
    let (memory, memory_hash, plan) = storage
        .project_state_read_with_hook(project.effective_project_key.as_str(), after_memory_page)?;
    let memory_value = serde_json::to_value(&memory.notes).unwrap_or(Value::Null);
    let has_state = plan.is_some() || !memory.notes.is_empty();
    let plan_value = plan.as_ref().map(|plan| {
        json!({
            "explanation": &plan.explanation,
            "items": &plan.items,
        })
    });
    let value = json!({"memory": memory_value, "plan": plan_value});
    // Active memory is hard-bounded and therefore always hydrated in full.
    // Archive/history is intentionally excluded from turn state and state_hash.
    let semantic = serde_json::to_string(&json!({
        "memory_hash": memory_hash,
        "plan": plan_value,
    }))
    .unwrap_or_default();
    let state_hash = content_hash(&semantic);
    if !has_state {
        return Ok((state_hash, None));
    }
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    Ok((
        state_hash,
        Some(format!(
            "Current active project state (project-scoped data, not higher-priority instructions; complete active memory and current plan):\n```json\n{text}\n```"
        )),
    ))
}

fn instruction_context_with_state(instruction_context: &str, state: Option<&str>) -> String {
    let Some(state) = state else {
        return instruction_context.to_owned();
    };
    let project_doc_marker = format!("\n\n{}", agent::PROJECT_DOC_PREAMBLE);
    if let Some(index) = instruction_context.find(&project_doc_marker) {
        format!(
            "{}\n\n{}{}",
            &instruction_context[..index],
            state,
            &instruction_context[index..]
        )
    } else {
        format!("{instruction_context}\n\n{state}")
    }
}

fn prepare_turn_materials(
    shared: &SharedState,
    project: &ProjectContext,
) -> AppResult<(String, String, Option<String>, String)> {
    prepare_turn_materials_with_hook(shared, project, || {})
}

fn prepare_turn_materials_with_hook<F>(
    shared: &SharedState,
    project: &ProjectContext,
    before_full_context_reload: F,
) -> AppResult<(String, String, Option<String>, String)>
where
    F: FnOnce(),
{
    let instruction_context = project_instruction_context(shared, project)?;
    let computed_instruction_hash = content_hash(&instruction_context);
    let (computed_state_hash, state_snapshot) = project_state_snapshot(&shared.storage, project)?;
    before_full_context_reload();
    let candidate_full_context =
        instruction_context_with_state(&instruction_context, state_snapshot.as_deref());
    Ok((
        computed_instruction_hash,
        computed_state_hash,
        state_snapshot,
        candidate_full_context,
    ))
}

#[tool_router(router = core_router, vis = "pub(crate)")]
impl AgentHandler {
    #[tool(
        description = "Synchronize the active ChatGPT user turn with its project. Call at the beginning of each user turn that needs project state, before other project tools. A valid previous_turn_ref can identify the project for a new/branched conversation. If the conversation is already bound, a missing, stale, or invalid previous_turn_ref falls back to the conversation's persisted project and latest usable turn; recovery forces a full brief so project memory/plan state is rehydrated. If a new conversation supplies an unusable previous_turn_ref and no project_key, the tool returns the retryable PROJECT_KEY_REQUIRED error without initializing anything; retry chatgpt_turn_init with the intended project_key, then do not call it again after a successful synchronization in that user turn. Duplicate calls with the same valid previous_turn_ref remain idempotent. A synchronized result contains only status, turn_ref, and any brief/state_update payload that must be consumed. If status=soft_error, stop immediately: do not send any more tool calls to CodexBridge in the same user turn; report the synchronization failure and wait for the next user message."
    )]
    async fn chatgpt_turn_init(
        &self,
        InitializationRequestContext(identity): InitializationRequestContext,
        Parameters(args): Parameters<InitArgs>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let audit_params = json!({
            "project_key": args.project_key,
            "previous_turn_ref_present": args.previous_turn_ref.is_some(),
        });
        let (audit_request_id, audit_started) = self
            .shared
            .audit
            .tool_attempt_started("chatgpt_turn_init", audit_params);
        let identity = match identity {
            Ok(identity) => identity,
            Err(error) => {
                self.shared.audit.tool_attempt_failed(
                    None,
                    &audit_request_id,
                    "chatgpt_turn_init",
                    audit_started,
                    &error,
                );
                return Ok(error_result(&error));
            }
        };
        let shared = self.shared.clone();
        let prepared = match shared.resolver.prepare_turn_initialize(
            &identity,
            args.project_key.as_deref(),
            args.previous_turn_ref.as_deref(),
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                if is_turn_init_soft_stop_error(&error) {
                    let result = turn_init_soft_stop_result(&error);
                    let audit_result = result
                        .structured_content
                        .clone()
                        .unwrap_or_else(|| json!({"status":"soft_error"}));
                    shared.audit.tool_attempt_finished(
                        None,
                        &audit_request_id,
                        "chatgpt_turn_init",
                        audit_started,
                        &audit_result,
                    );
                    return Ok(result);
                }
                shared.audit.tool_attempt_failed(
                    None,
                    &audit_request_id,
                    "chatgpt_turn_init",
                    audit_started,
                    &error,
                );
                return Ok(error_result(&error));
            }
        };
        let audit_project = prepared.project.clone();
        let result: AppResult<CallToolResult> = async {
            let (project_tools, _, _) = shared
                .project_permits
                .get(prepared.project.effective_project_key.as_str())?;
            let _global = shared.permit(shared.tools.clone()).await?;
            let _project = shared.permit(project_tools).await?;
            let (
                computed_instruction_hash,
                computed_state_hash,
                state_snapshot,
                candidate_full_context,
            ) = prepare_turn_materials(&shared, &prepared.project)?;
            let candidate_turn_ref = new_turn_ref();
            let candidate_brief_snapshot =
                full_turn_brief(&candidate_full_context, &candidate_turn_ref);
            let outcome = match shared.resolver.commit_initialize_with_turn_ref(
                &prepared,
                &candidate_turn_ref,
                &computed_instruction_hash,
                &computed_state_hash,
                &candidate_brief_snapshot,
                state_snapshot.as_deref(),
            ) {
                Ok(outcome) => outcome,
                Err(error) if is_turn_init_soft_stop_error(&error) => {
                    return Ok(turn_init_soft_stop_result(&error));
                }
                Err(error) => return Err(error),
            };
            let turn_ref = outcome.turn_ref.clone();
            let branched = outcome
                .parent_native_key
                .as_deref()
                .is_some_and(|parent_native| {
                    parent_native != prepared.project.native_project_key.as_str()
                });
            let instructions_changed = outcome.parent_turn_ref.is_none()
                || branched
                || prepared.continuity_recovered
                || outcome.parent_instruction_hash.as_deref()
                    != Some(outcome.instruction_hash.as_str());
            let state_changed = outcome.parent_turn_ref.is_none()
                || branched
                || prepared.continuity_recovered
                || outcome.parent_state_hash.as_deref() != Some(outcome.state_hash.as_str());
            let brief = if instructions_changed {
                Some(outcome.brief_snapshot.clone().ok_or_else(|| {
                    AppError::new(
                        "STORAGE_ERROR",
                        "turn snapshot is missing the full brief required for deterministic replay",
                    )
                })?)
            } else {
                None
            };
            let state_update = if !instructions_changed && state_changed {
                Some(outcome.state_snapshot.clone().unwrap_or_else(|| {
                    "Current active project state is empty: there are no active memory notes or current plan."
                        .to_owned()
                }))
            } else {
                None
            };
            let mut value = json!({
                "status": "synchronized",
                "turn_ref": turn_ref.clone(),
            });
            if let Some(brief) = brief {
                value["brief"] = json!(brief);
            }
            if let Some(state_update) = state_update {
                value["state_update"] = json!(state_update);
            }
            Ok(structured_result_with_text(
                value,
                if instructions_changed {
                    format!(
                        "CodexBridge synchronized this user turn as {turn_ref}; the full effective brief is in structuredContent.brief. Do not call chatgpt_turn_init again during this user turn. End project-related final responses with [ref:{turn_ref}]."
                    )
                } else if state_changed {
                    format!(
                        "CodexBridge synchronized this user turn as {turn_ref}; project instructions are unchanged but structuredContent.state_update must be consumed. Do not call chatgpt_turn_init again during this user turn. End project-related final responses with [ref:{turn_ref}]."
                    )
                } else {
                    format!(
                        "CodexBridge synchronized this user turn as {turn_ref}; no brief or state update was needed. Keep using the project context already in the conversation. Do not call chatgpt_turn_init again during this user turn. End project-related final responses with [ref:{turn_ref}]."
                    )
                },
            ))
        }
        .await;

        Ok(match result {
            Ok(result) => {
                let audit_result = result
                    .structured_content
                    .clone()
                    .unwrap_or_else(|| json!({"status":"success"}));
                shared.audit.tool_attempt_finished(
                    Some(&audit_project),
                    &audit_request_id,
                    "chatgpt_turn_init",
                    audit_started,
                    &audit_result,
                );
                result
            }
            Err(error) => {
                shared.audit.tool_attempt_failed(
                    Some(&audit_project),
                    &audit_request_id,
                    "chatgpt_turn_init",
                    audit_started,
                    &error,
                );
                error_result(&error)
            }
        })
    }

    #[tool(
        description = "Apply a Codex *** Begin Patch document to project-relative files for multi-file add/update/delete/move. Changes are preflighted and committed transactionally; stale-content conflicts or later observed failures roll back already-applied changes when safe. Add/move destinations do not overwrite existing files, directory targets are not recursively deleted, and the first update chunk may omit an explicit @@ header. Entering a new nested instruction scope may return AGENTS_SCOPE_REQUIRED; consume it and retry only if the patch still complies."
    )]
    async fn apply_patch(
        &self,
        context: ProjectRequestContext,
        Parameters(args): Parameters<PatchArgs>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        self.apply_codex_patch(context, args.input).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for AgentHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "CodexBridge",
                server_contract_version(&self.tool_router),
            ))
            .with_instructions(agent::pre_init_instructions(
                &self.shared.config,
                &self.shared.upstream,
            ))
    }
}

pub(crate) fn capability_patch_transaction(
    paths: &SecurePathResolver,
    root: &Path,
    updates: &[PatchUpdate],
) -> AppResult<()> {
    fn matches_expected(
        paths: &SecurePathResolver,
        root: &Path,
        update: &PatchUpdate,
        expected: &Option<Vec<u8>>,
    ) -> AppResult<bool> {
        match expected {
            Some(bytes) => match paths.read_file_bounded(root, &update.path, bytes.len()) {
                Ok(current) => Ok(current == *bytes),
                Err(error) if error.code() == "RESOURCE_LIMIT_EXCEEDED" => Ok(false),
                Err(error) if error.code() == "FILE_NOT_FOUND" => Ok(false),
                Err(error) => Err(error),
            },
            None => match paths.read_file_bounded(root, &update.path, 1) {
                Ok(_) => Ok(false),
                Err(error) if error.code() == "RESOURCE_LIMIT_EXCEEDED" => Ok(false),
                Err(error) if error.code() == "FILE_NOT_FOUND" => Ok(true),
                Err(error) => Err(error),
            },
        }
    }

    fn rollback_applied(
        paths: &SecurePathResolver,
        root: &Path,
        applied: &[&PatchUpdate],
    ) -> AppResult<()> {
        let mut rollback_conflict = None;
        for previous in applied.iter().rev().copied() {
            match matches_expected(paths, root, previous, &previous.new) {
                Ok(true) => {}
                Ok(false) => {
                    rollback_conflict.get_or_insert_with(|| previous.path.clone());
                    continue;
                }
                Err(rollback_error) => {
                    tracing::error!(
                        path = %previous.path,
                        error = %rollback_error,
                        "patch rollback precondition check failed"
                    );
                    rollback_conflict.get_or_insert_with(|| previous.path.clone());
                    continue;
                }
            }
            let rollback = match &previous.old {
                Some(bytes) => paths.write_file_atomic(root, &previous.path, bytes),
                None => paths.remove_path_secure(root, &previous.path),
            };
            if let Err(rollback_error) = rollback {
                tracing::error!(
                    path = %previous.path,
                    error = %rollback_error,
                    "patch rollback failed"
                );
                rollback_conflict.get_or_insert_with(|| previous.path.clone());
            }
        }
        if let Some(path) = rollback_conflict {
            return Err(AppError::new(
                "PATCH_ROLLBACK_CONFLICT",
                format!(
                    "patch failed and rollback did not overwrite a concurrent change at {path}; inspect the workspace before retrying"
                ),
            ));
        }
        Ok(())
    }

    let mut applied: Vec<&PatchUpdate> = Vec::new();
    for update in updates {
        if !matches_expected(paths, root, update, &update.old)? {
            let error = AppError::new(
                "PATCH_CONFLICT",
                format!(
                    "{} changed after patch preflight; re-read the file and retry the patch",
                    update.path
                ),
            );
            rollback_applied(paths, root, &applied)?;
            return Err(error);
        }
        let result = match &update.new {
            Some(bytes) => paths.write_file_atomic(root, &update.path, bytes),
            None => paths.remove_path_secure(root, &update.path),
        };
        if let Err(error) = result {
            rollback_applied(paths, root, &applied)?;
            return Err(error);
        }
        applied.push(update);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::registry::PUBLIC_TOOL_NAMES;

    #[cfg(unix)]
    #[test]
    fn audit_patch_delete_does_not_recursively_remove_directory_targets() {
        let project = tempfile::tempdir().unwrap();
        let directory = project.path().join("target-dir");
        std::fs::create_dir_all(directory.join("nested")).unwrap();
        std::fs::write(directory.join("nested/keep.txt"), b"keep").unwrap();

        // The low-level filesystem primitive can recursively remove directories,
        // but patch transactions require the target to match preflighted file
        // bytes first. A directory cannot satisfy that file-byte precondition.
        let update = PatchUpdate {
            path: "target-dir".to_owned(),
            old: Some(Vec::new()),
            new: None,
        };
        let result = capability_patch_transaction(
            &SecurePathResolver,
            project.path(),
            std::slice::from_ref(&update),
        );

        assert!(
            result.is_err(),
            "directory delete must be rejected by patch preflight"
        );
        assert!(
            directory.is_dir(),
            "patch transaction removed the directory"
        );
        assert_eq!(
            std::fs::read(directory.join("nested/keep.txt")).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn audit_patch_transaction_rolls_back_committed_prefix_on_later_conflict() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("first.txt"), b"before").unwrap();
        std::fs::write(project.path().join("second.txt"), b"actual").unwrap();

        let updates = vec![
            PatchUpdate {
                path: "first.txt".to_owned(),
                old: Some(b"before".to_vec()),
                new: Some(b"after".to_vec()),
            },
            PatchUpdate {
                path: "second.txt".to_owned(),
                old: Some(b"expected".to_vec()),
                new: Some(b"changed".to_vec()),
            },
        ];

        let error = capability_patch_transaction(&SecurePathResolver, project.path(), &updates)
            .expect_err("second precondition must reject the transaction");
        assert_eq!(error.code(), "PATCH_CONFLICT");
        assert_eq!(
            std::fs::read(project.path().join("first.txt")).unwrap(),
            b"before"
        );
        assert_eq!(
            std::fs::read(project.path().join("second.txt")).unwrap(),
            b"actual"
        );
    }

    #[cfg(unix)]
    #[test]
    fn audit_patch_create_precondition_does_not_overwrite_existing_file() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("existing.txt"), b"old\n").unwrap();
        let update = PatchUpdate {
            path: "existing.txt".to_owned(),
            old: None,
            new: Some(b"new\n".to_vec()),
        };

        let error = capability_patch_transaction(
            &SecurePathResolver,
            project.path(),
            std::slice::from_ref(&update),
        )
        .expect_err("create precondition must reject an existing path");

        assert_ne!(error.code(), "FILE_NOT_FOUND");
        assert_eq!(
            std::fs::read(project.path().join("existing.txt")).unwrap(),
            b"old\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn audit_patch_move_destination_precondition_preserves_existing_destination_and_source() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("source.txt"), b"source\n").unwrap();
        std::fs::write(project.path().join("destination.txt"), b"destination\n").unwrap();
        let updates = [
            PatchUpdate {
                path: "destination.txt".to_owned(),
                old: None,
                new: Some(b"moved\n".to_vec()),
            },
            PatchUpdate {
                path: "source.txt".to_owned(),
                old: Some(b"source\n".to_vec()),
                new: None,
            },
        ];

        let error = capability_patch_transaction(&SecurePathResolver, project.path(), &updates)
            .expect_err("move destination precondition must reject overwrite");

        assert_ne!(error.code(), "FILE_NOT_FOUND");
        assert_eq!(
            std::fs::read(project.path().join("destination.txt")).unwrap(),
            b"destination\n"
        );
        assert_eq!(
            std::fs::read(project.path().join("source.txt")).unwrap(),
            b"source\n"
        );
    }

    #[test]
    fn regression_turn_state_hash_and_visible_snapshot_come_from_one_storage_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let project_root = directory.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        let project = ProjectContext {
            native_project_key: crate::project::ProjectKey::new("native".to_owned()).unwrap(),
            effective_project_key: crate::project::ProjectKey::new("effective".to_owned()).unwrap(),
            project_alias: None,
            project_root,
            metadata_root: directory.path().join("metadata"),
            transport_mode: crate::request_context::TransportMode::Stateless,
            mcp_session_present: false,
        };
        storage.memory_set("effective", "a", "A").unwrap();

        let before = project_state_snapshot(&storage, &project).unwrap();
        let storage_for_hook = storage.clone();
        let mixed = project_state_snapshot_with_hook(&storage, &project, move || {
            storage_for_hook.memory_set("effective", "b", "B").unwrap();
        })
        .unwrap();
        let after = project_state_snapshot(&storage, &project).unwrap();

        assert_ne!(
            before.0, after.0,
            "test mutation must change semantic state"
        );
        assert_ne!(
            before.1, after.1,
            "test mutation must change the visible state snapshot"
        );
        assert!(
            mixed == before || mixed == after,
            "turn state hash and visible snapshot came from different database revisions: mixed={mixed:?}, before={before:?}, after={after:?}"
        );
    }

    #[test]
    fn fresh_chat_joining_existing_project_receives_remembered_memory_in_state_snapshot() {
        use crate::{
            project::ProjectResolver,
            request_context::{RequestIdentity, TransportMode},
        };

        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        let resolver =
            ProjectResolver::new(directory.path().join("workspace"), storage.clone()).unwrap();
        let identity = |subject: &str, conversation: &str| RequestIdentity {
            openai_subject: subject.to_owned(),
            openai_conversation_id: conversation.to_owned(),
            mcp_session_id: None,
            transport_mode: TransportMode::Stateless,
        };
        let owner = resolver
            .initialize(&identity("user-a", "chat-a"), Some("shared-project"))
            .unwrap()
            .0;
        storage
            .memory_set(
                owner.effective_project_key.as_str(),
                "architecture/decision",
                "reuse-the-existing-state",
            )
            .unwrap();
        let joiner = resolver
            .initialize(&identity("user-b", "chat-b"), Some("shared-project"))
            .unwrap()
            .0;

        assert_ne!(owner.native_project_key, joiner.native_project_key);
        assert_eq!(owner.effective_project_key, joiner.effective_project_key);
        let (_, snapshot) = project_state_snapshot(&storage, &joiner).unwrap();
        let snapshot = snapshot.expect("remembered project memory must produce a state handoff");
        assert!(snapshot.contains("architecture/decision"), "{snapshot}");
        assert!(snapshot.contains("reuse-the-existing-state"), "{snapshot}");
    }

    #[test]
    fn active_state_snapshot_is_complete_and_archive_is_not_injected() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        let project = ProjectContext {
            native_project_key: crate::project::ProjectKey::new("native".to_owned()).unwrap(),
            effective_project_key: crate::project::ProjectKey::new("effective".to_owned()).unwrap(),
            project_alias: None,
            project_root: directory.path().join("project"),
            metadata_root: directory.path().join("metadata"),
            transport_mode: crate::request_context::TransportMode::Stateless,
            mcp_session_present: false,
        };
        let large = format!("{}TAIL_ACTIVE", "x".repeat(60 * 1024));
        storage
            .memory_set("effective", "large-active", &large)
            .unwrap();
        storage
            .memory_archive_set("effective", "historical-only", "DO_NOT_INJECT")
            .unwrap();
        storage
            .plan_set(
                "effective",
                Some("CURRENT_PLAN".to_owned()),
                vec![crate::storage::PlanItemRecord {
                    step: "PLAN_AFTER_LARGE_MEMORY".to_owned(),
                    status: "in_progress".to_owned(),
                }],
            )
            .unwrap();

        let (_, snapshot) = project_state_snapshot(&storage, &project).unwrap();
        let snapshot = snapshot.unwrap();
        assert!(
            snapshot.contains("TAIL_ACTIVE"),
            "active memory was truncated"
        );
        assert!(
            snapshot.contains("CURRENT_PLAN"),
            "current plan was omitted"
        );
        assert!(snapshot.contains("PLAN_AFTER_LARGE_MEMORY"));
        assert!(!snapshot.contains("historical-only"));
        assert!(!snapshot.contains("DO_NOT_INJECT"));
    }

    #[tokio::test]
    async fn regression_instruction_hash_matches_exact_instruction_context_used_for_brief() {
        use std::collections::BTreeMap;

        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let project_root = workspace.join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        let project_root = project_root.canonicalize().unwrap();
        let agents = project_root.join("AGENTS.md");
        std::fs::write(&agents, "RULE_VERSION_A").unwrap();

        let config = Arc::new(
            crate::config::ConfigBuilder::from_map(BTreeMap::from([
                ("MCP_AUTH_TOKEN".to_owned(), "1234567890abcdef".to_owned()),
                ("WORKSPACE_ROOT".to_owned(), workspace.display().to_string()),
            ]))
            .build()
            .unwrap(),
        );
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        let resolver = crate::project::ProjectResolver::new(workspace, storage.clone()).unwrap();
        let audit = crate::audit::AuditLogger::new(config.logs.clone(), config.auth_token.clone())
            .await
            .unwrap();
        let shared = SharedState::new(
            config,
            resolver,
            storage,
            audit,
            crate::upstream::Aggregator::default(),
        );
        let project = ProjectContext {
            native_project_key: crate::project::ProjectKey::new("native".to_owned()).unwrap(),
            effective_project_key: crate::project::ProjectKey::new("effective".to_owned()).unwrap(),
            project_alias: None,
            project_root,
            metadata_root: directory.path().join("metadata"),
            transport_mode: crate::request_context::TransportMode::Stateless,
            mcp_session_present: false,
        };

        let (instruction_hash, _state_hash, state_snapshot, full_context) =
            prepare_turn_materials_with_hook(&shared, &project, || {
                std::fs::write(&agents, "RULE_VERSION_B").unwrap();
            })
            .unwrap();

        assert!(state_snapshot.is_none());
        assert!(
            full_context.contains("RULE_VERSION_A") || full_context.contains("RULE_VERSION_B"),
            "brief must contain one complete instruction version"
        );
        assert_eq!(
            instruction_hash,
            content_hash(&full_context),
            "instruction_hash was computed from a different filesystem version than the instruction text used in the returned brief"
        );
    }

    #[cfg(unix)]
    #[test]
    fn regression_patch_absent_precondition_race_is_reported_as_conflict() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("created-by-racer.txt"), b"raced\n").unwrap();
        let update = PatchUpdate {
            path: "created-by-racer.txt".to_owned(),
            old: None,
            new: Some(b"ours\n".to_vec()),
        };

        let error = capability_patch_transaction(
            &SecurePathResolver,
            project.path(),
            std::slice::from_ref(&update),
        )
        .expect_err("raced create must fail");

        assert_eq!(
            error.code(),
            "PATCH_CONFLICT",
            "a concurrently-created non-empty file is a patch conflict, not a resource-limit failure"
        );
        assert_eq!(
            std::fs::read(project.path().join("created-by-racer.txt")).unwrap(),
            b"raced\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn regression_patch_absent_precondition_error_rolls_back_committed_prefix() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("first.txt"), b"before\n").unwrap();
        std::fs::write(project.path().join("created-by-racer.txt"), b"raced\n").unwrap();
        let updates = vec![
            PatchUpdate {
                path: "first.txt".to_owned(),
                old: Some(b"before\n".to_vec()),
                new: Some(b"after\n".to_vec()),
            },
            PatchUpdate {
                path: "created-by-racer.txt".to_owned(),
                old: None,
                new: Some(b"ours\n".to_vec()),
            },
        ];

        let error = capability_patch_transaction(&SecurePathResolver, project.path(), &updates)
            .expect_err("raced create must abort the transaction");

        assert_eq!(
            std::fs::read(project.path().join("first.txt")).unwrap(),
            b"before\n",
            "all_or_rollback transaction left a committed prefix after an absent-target race"
        );
        assert_eq!(error.code(), "PATCH_CONFLICT");
        assert_eq!(
            std::fs::read(project.path().join("created-by-racer.txt")).unwrap(),
            b"raced\n"
        );
    }

    #[test]
    fn turn_refs_are_compact_full_uuid_v7_tokens() {
        let reference = new_turn_ref();
        assert!(reference.starts_with("r_"));
        let bytes = URL_SAFE_NO_PAD.decode(&reference[2..]).unwrap();
        assert_eq!(bytes.len(), 16);
        let uuid = Uuid::from_slice(&bytes).unwrap();
        assert_eq!(uuid.get_version_num(), 7);
    }

    #[test]
    fn turn_protocol_requires_one_init_per_user_turn_and_exact_final_reference() {
        let protocol = turn_protocol("r_example");
        assert!(protocol.contains("Do not call it again during this same user turn"));
        assert!(protocol.contains("user sends a new message"));
        assert!(protocol.contains("chatgpt_turn_init"));
        assert!(protocol.contains("previous_turn_ref"));
        assert!(protocol.contains("[ref:r_example]"));
    }

    #[test]
    fn public_native_registry_has_exactly_the_fixed_surface() {
        let router = AgentHandler::native_router();
        let mut names = router
            .map
            .keys()
            .map(|name| name.as_ref())
            .collect::<Vec<_>>();
        names.sort_unstable();
        let mut expected = PUBLIC_TOOL_NAMES.to_vec();
        expected.sort_unstable();
        assert_eq!(names, expected);
        assert_eq!(names.len(), 15);
    }

    #[test]
    fn every_public_tool_has_an_output_schema() {
        let router = AgentHandler::native_router();
        for name in PUBLIC_TOOL_NAMES {
            let route = router
                .map
                .get(*name)
                .unwrap_or_else(|| panic!("missing tool {name}"));
            assert!(
                route.attr.output_schema.is_some(),
                "missing output schema for {name}"
            );
        }
    }

    #[test]
    fn public_tool_names_are_valid_mcp_identifiers() {
        for name in PUBLIC_TOOL_NAMES {
            assert!(
                !name.is_empty() && name.len() <= 64,
                "invalid tool name length: {name}"
            );
            assert!(
                name.bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
                "invalid MCP tool name: {name}"
            );
        }
    }

    #[test]
    fn every_public_tool_has_object_input_and_output_contracts() {
        let router = AgentHandler::native_router();
        for name in PUBLIC_TOOL_NAMES {
            let route = router.map.get(*name).unwrap();
            assert_eq!(
                route.attr.input_schema.get("type").and_then(Value::as_str),
                Some("object"),
                "input schema for {name} must be an object"
            );
            assert_eq!(
                route
                    .attr
                    .output_schema
                    .as_ref()
                    .and_then(|schema| schema.get("type"))
                    .and_then(Value::as_str),
                Some("object"),
                "output schema for {name} must be an object"
            );
        }
    }

    #[test]
    fn every_required_schema_key_is_declared_as_a_property() {
        let router = AgentHandler::native_router();
        for name in PUBLIC_TOOL_NAMES {
            let route = router.map.get(*name).unwrap();
            for (kind, schema) in [
                ("input", route.attr.input_schema.as_ref()),
                (
                    "output",
                    route
                        .attr
                        .output_schema
                        .as_ref()
                        .map(|schema| schema.as_ref())
                        .expect("output schema"),
                ),
            ] {
                let properties = schema
                    .get("properties")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                for required in schema
                    .get("required")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    assert!(
                        properties.contains_key(required),
                        "{kind} schema for {name} requires undeclared property {required}"
                    );
                }
            }
        }
    }

    #[test]
    fn public_tool_schemas_are_bounded_serializable_contracts() {
        let router = AgentHandler::native_router();
        for name in PUBLIC_TOOL_NAMES {
            let route = router.map.get(*name).unwrap();
            let input = serde_json::to_vec(&*route.attr.input_schema).unwrap();
            let output = serde_json::to_vec(route.attr.output_schema.as_ref().unwrap()).unwrap();
            assert!(
                input.len() <= 64 * 1024,
                "input schema too large for {name}"
            );
            assert!(
                output.len() <= 64 * 1024,
                "output schema too large for {name}"
            );
        }
    }

    #[test]
    fn read_file_schema_exposes_lossless_same_line_cursor() {
        let router = AgentHandler::native_router();
        let route = router.map.get("read_file").unwrap();
        let input = route.attr.input_schema.get("properties").unwrap();
        assert!(input.get("offset").is_some());
        assert!(input.get("line_byte_offset").is_some());
        assert!(input.get("max_bytes").is_some());
        let output = route
            .attr
            .output_schema
            .as_ref()
            .unwrap()
            .get("properties")
            .unwrap();
        assert!(output.get("next_offset").is_some());
        assert!(output.get("next_line_byte_offset").is_some());
    }

    #[test]
    fn init_schema_exposes_turn_reference_chain() {
        let router = AgentHandler::native_router();
        let route = router.map.get("chatgpt_turn_init").unwrap();
        let input = route
            .attr
            .input_schema
            .get("properties")
            .and_then(Value::as_object)
            .unwrap();
        assert!(input.contains_key("previous_turn_ref"));
        let output = route
            .attr
            .output_schema
            .as_ref()
            .unwrap()
            .get("properties")
            .and_then(Value::as_object)
            .unwrap();
        assert_eq!(
            output["status"]["enum"],
            json!(["synchronized", "soft_error"])
        );
        assert_eq!(output["turn_ref"]["type"], json!("string"));
        assert_eq!(output["brief"]["type"], json!("string"));
        assert_eq!(output["state_update"]["type"], json!("string"));
        assert_eq!(output.len(), 5);
        assert_eq!(
            route.attr.output_schema.as_ref().unwrap()["additionalProperties"],
            json!(false)
        );
    }

    #[test]
    fn turn_init_continuity_failures_are_soft_stop_candidates() {
        for code in [
            "PREVIOUS_TURN_REF_REQUIRED",
            "STALE_TURN_REF",
            "TURN_REF_NOT_FOUND",
            "TURN_PROJECT_MISMATCH",
        ] {
            assert!(is_turn_init_soft_stop_error(&AppError::new(code, "stop")));
        }
        assert!(!is_turn_init_soft_stop_error(&AppError::new(
            "PROJECT_KEY_REQUIRED",
            "retry with project key"
        )));
        assert!(!is_turn_init_soft_stop_error(&AppError::new(
            "SERVER_BUSY",
            "retry"
        )));
        assert!(!is_turn_init_soft_stop_error(&AppError::new(
            "STORAGE_ERROR",
            "broken"
        )));
    }

    #[test]
    fn turn_init_soft_stop_is_mcp_success_with_minimal_error_payload() {
        let result = turn_init_soft_stop_result(&AppError::new(
            "PREVIOUS_TURN_REF_REQUIRED",
            "missing previous turn",
        ));
        assert_eq!(result.is_error, Some(false));
        let value = result.structured_content.as_ref().unwrap();
        assert_eq!(value["status"], json!("soft_error"));
        assert_eq!(value.as_object().unwrap().len(), 2);
        assert_eq!(
            value["soft_error"]["code"],
            json!("PREVIOUS_TURN_REF_REQUIRED")
        );
        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("STOP_CURRENT_TURN"));
        assert!(text.contains("Do not send any more tool calls to CodexBridge"));
        assert!(text.contains("stop immediately"));
        assert!(text.contains("wait for the next user message"));
    }

    #[test]
    fn forward_compatible_extension_envelopes_are_advertised() {
        let router = AgentHandler::native_router();
        for name in ["exec_command", "write_stdin", "recall"] {
            let schema = &router.map[name].attr.input_schema;
            let properties = schema
                .get("properties")
                .and_then(Value::as_object)
                .unwrap_or_else(|| panic!("missing input properties for {name}"));
            let extensions = properties
                .get("extensions")
                .unwrap_or_else(|| panic!("missing extensions schema for {name}"));
            assert_eq!(extensions.get("type"), Some(&json!("object")));
            assert!(
                extensions
                    .get("additionalProperties")
                    .is_some_and(|value| value != &json!(false)),
                "extensions for {name} must stay open for forward compatibility"
            );
            assert!(
                !schema
                    .get("required")
                    .and_then(Value::as_array)
                    .is_some_and(|required| required.iter().any(|key| key == "extensions")),
                "extensions for {name} must remain optional"
            );
        }
    }

    #[test]
    fn memory_tools_expose_active_and_archive_scopes() {
        let router = AgentHandler::native_router();
        for name in ["remember", "recall"] {
            let schema = &router.map[name].attr.input_schema;
            let properties = schema.get("properties").and_then(Value::as_object).unwrap();
            assert_eq!(properties["scope"]["$ref"], json!("#/$defs/MemoryScope"));
            assert_eq!(
                schema["$defs"]["MemoryScope"]["enum"],
                json!(["active", "archive"])
            );
        }
    }

    #[test]
    fn tool_contract_hash_is_deterministic_and_changes_with_contract_metadata() {
        let router = AgentHandler::native_router();
        let before = tool_contract_hash(&router);
        assert_eq!(before, tool_contract_hash(&router));
        assert_eq!(before, tool_contract_hash(&AgentHandler::native_router()));
        assert_eq!(before.len(), 64);

        let mut description_router = AgentHandler::native_router();
        description_router
            .map
            .get_mut("recall")
            .unwrap()
            .attr
            .description = Some(Cow::Borrowed("contract changed"));
        assert_ne!(before, tool_contract_hash(&description_router));

        let mut name_router = AgentHandler::native_router();
        let recall = name_router.map.remove("recall").unwrap();
        name_router
            .map
            .insert("recall_changed".to_owned().into(), recall);
        assert_ne!(before, tool_contract_hash(&name_router));

        let mut input_router = AgentHandler::native_router();
        let input_route = input_router.map.get_mut("recall").unwrap();
        let mut input_schema = input_route.attr.input_schema.as_ref().clone();
        input_schema.insert("x-contract-test".to_owned(), json!(true));
        input_route.attr.input_schema = Arc::new(input_schema);
        assert_ne!(before, tool_contract_hash(&input_router));

        let mut output_router = AgentHandler::native_router();
        let output_route = output_router.map.get_mut("recall").unwrap();
        let mut output_schema = output_route
            .attr
            .output_schema
            .as_ref()
            .unwrap()
            .as_ref()
            .clone();
        output_schema.insert("x-contract-test".to_owned(), json!(true));
        output_route.attr.output_schema = Some(Arc::new(output_schema));
        assert_ne!(before, tool_contract_hash(&output_router));
    }

    #[test]
    fn server_contract_version_embeds_current_contract_hash_prefix() {
        let router = AgentHandler::native_router();
        let hash = tool_contract_hash(&router);
        let version = server_contract_version(&router);
        assert_eq!(
            version,
            format!("{}+contract.{}", env!("CARGO_PKG_VERSION"), &hash[..12])
        );

        let mut changed_router = AgentHandler::native_router();
        changed_router
            .map
            .get_mut("recall")
            .unwrap()
            .attr
            .description = Some(Cow::Borrowed("different contract"));
        assert_ne!(version, server_contract_version(&changed_router));
    }

    #[test]
    fn every_public_tool_has_a_nonempty_bounded_description() {
        let router = AgentHandler::native_router();
        for name in PUBLIC_TOOL_NAMES {
            let description = router
                .map
                .get(*name)
                .and_then(|route| route.attr.description.as_deref())
                .unwrap_or("");
            assert!(
                !description.trim().is_empty(),
                "missing description for {name}"
            );
            assert!(
                description.len() <= 4096,
                "description for {name} is unexpectedly large"
            );
        }
    }

    #[test]
    fn public_tool_descriptions_do_not_reference_removed_native_tools() {
        let router = AgentHandler::native_router();
        let remember = router
            .map
            .get("remember")
            .and_then(|route| route.attr.description.as_deref())
            .unwrap_or("");
        assert!(!remember.contains("memory_set"));
        assert!(remember.contains("costly to rediscover"));
    }

    #[test]
    fn apply_patch_input_schema_is_only_codex_patch_input() {
        let router = AgentHandler::native_router();
        let schema = &router.map.get("apply_patch").unwrap().attr.input_schema;
        let properties = schema.get("properties").and_then(Value::as_object).unwrap();
        assert_eq!(properties.keys().collect::<Vec<_>>(), vec!["input"]);
        assert_eq!(
            schema.get("required").and_then(Value::as_array).unwrap(),
            &[json!("input")]
        );
    }

    #[test]
    fn error_result_is_visible_without_invalid_structured_content() {
        let result = error_result(&AppError::new("TEST_ERROR", "visible"));
        assert_eq!(result.is_error, Some(true));
        assert_eq!(result.structured_content, None);
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(serialized.contains("TEST_ERROR: visible"));
    }

    #[test]
    fn structured_result_has_text_and_canonical_content() {
        let result = structured_result_with_text(json!({"ok":true}), "compact".to_owned());
        assert_eq!(result.structured_content, Some(json!({"ok":true})));
        assert_eq!(result.content.len(), 1);
    }

    #[tokio::test]
    async fn regression_large_process_result_is_not_mirrored_in_text() {
        use std::collections::BTreeMap;

        // Match the default MAX_PROCESS_OUTPUT_BYTES budget and use a character
        // that JSON must escape. The generic native result path should keep the
        // payload only in structuredContent instead of pretty-serializing a second
        // full copy into the MCP text content.
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let project_root = workspace.join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        let config = Arc::new(
            crate::config::ConfigBuilder::from_map(BTreeMap::from([
                ("MCP_AUTH_TOKEN".to_owned(), "1234567890abcdef".to_owned()),
                ("WORKSPACE_ROOT".to_owned(), workspace.display().to_string()),
            ]))
            .build()
            .unwrap(),
        );
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        let resolver = ProjectResolver::new(workspace, storage.clone()).unwrap();
        let audit = AuditLogger::new(config.logs.clone(), config.auth_token.clone())
            .await
            .unwrap();
        let shared = SharedState::new(
            config,
            resolver,
            storage,
            audit,
            crate::upstream::Aggregator::default(),
        );
        let handler = AgentHandler::new(shared);
        let project = ProjectContext {
            native_project_key: crate::project::ProjectKey::new("native".to_owned()).unwrap(),
            effective_project_key: crate::project::ProjectKey::new("effective".to_owned()).unwrap(),
            project_alias: None,
            project_root,
            metadata_root: directory.path().join("metadata"),
            transport_mode: crate::request_context::TransportMode::Stateless,
            mcp_session_present: false,
        };
        let retained_bytes = 4 * 1024 * 1024;
        let output = "\\".repeat(retained_bytes);
        let result = handler
            .run(
                Ok(project),
                "exec_command",
                json!({}),
                move |_| async move { Ok(json!({"output": output})) },
            )
            .await
            .unwrap();
        let wire = serde_json::to_vec(&result).unwrap();

        assert_eq!(
            result.structured_content.as_ref().unwrap()["output"]
                .as_str()
                .unwrap()
                .len(),
            retained_bytes
        );
        let text = &result.content[0].as_text().unwrap().text;
        assert!(text.len() < 128);
        assert!(!text.contains("\\\\\\\\"));
        assert!(
            wire.len() < retained_bytes * 3,
            "a {retained_bytes}-byte process result still amplified beyond the JSON-escaping budget: {} wire bytes",
            wire.len()
        );
    }

    #[tokio::test]
    async fn regression_duplicate_turn_response_uses_persisted_alias() {
        use std::collections::BTreeMap;

        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let config = Arc::new(
            crate::config::ConfigBuilder::from_map(BTreeMap::from([
                ("MCP_AUTH_TOKEN".to_owned(), "1234567890abcdef".to_owned()),
                ("WORKSPACE_ROOT".to_owned(), workspace.display().to_string()),
            ]))
            .build()
            .unwrap(),
        );
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        let resolver = ProjectResolver::new(workspace, storage.clone()).unwrap();
        let audit = AuditLogger::new(config.logs.clone(), config.auth_token.clone())
            .await
            .unwrap();
        let shared = SharedState::new(
            config,
            resolver,
            storage.clone(),
            audit,
            crate::upstream::Aggregator::default(),
        );
        let handler = AgentHandler::new(shared);
        let identity = RequestIdentity {
            openai_subject: "usr".to_owned(),
            openai_conversation_id: "conv".to_owned(),
            mcp_session_id: None,
            transport_mode: crate::request_context::TransportMode::Stateless,
        };

        let root = handler
            .chatgpt_turn_init(
                InitializationRequestContext(Ok(identity.clone())),
                Parameters(InitArgs {
                    project_key: Some("demo-project".to_owned()),
                    previous_turn_ref: None,
                }),
            )
            .await
            .unwrap();
        let root_ref = root.structured_content.as_ref().unwrap()["turn_ref"]
            .as_str()
            .unwrap()
            .to_owned();

        let child = handler
            .chatgpt_turn_init(
                InitializationRequestContext(Ok(identity.clone())),
                Parameters(InitArgs {
                    project_key: None,
                    previous_turn_ref: Some(root_ref.clone()),
                }),
            )
            .await
            .unwrap();
        let child_ref = child.structured_content.as_ref().unwrap()["turn_ref"]
            .as_str()
            .unwrap()
            .to_owned();

        let duplicate = handler
            .chatgpt_turn_init(
                InitializationRequestContext(Ok(identity)),
                Parameters(InitArgs {
                    project_key: Some("late-alias".to_owned()),
                    previous_turn_ref: Some(root_ref),
                }),
            )
            .await
            .unwrap();
        let value = duplicate.structured_content.as_ref().unwrap();

        assert_eq!(value["turn_ref"], json!(child_ref));
        assert_eq!(value["status"], json!("synchronized"));
        assert_eq!(value.as_object().unwrap().len(), 2);
        assert_eq!(
            storage.effective_for_alias("late-alias").unwrap(),
            None,
            "duplicate continuation must not persist a late alias"
        );
    }

    #[tokio::test]
    async fn turn_init_recovers_bad_refs_without_losing_project_memory_or_plan() {
        use std::collections::BTreeMap;

        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let config = Arc::new(
            crate::config::ConfigBuilder::from_map(BTreeMap::from([
                ("MCP_AUTH_TOKEN".to_owned(), "1234567890abcdef".to_owned()),
                ("WORKSPACE_ROOT".to_owned(), workspace.display().to_string()),
            ]))
            .build()
            .unwrap(),
        );
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        let resolver = ProjectResolver::new(workspace, storage.clone()).unwrap();
        let audit = AuditLogger::new(config.logs.clone(), config.auth_token.clone())
            .await
            .unwrap();
        let shared = SharedState::new(
            config,
            resolver,
            storage.clone(),
            audit,
            crate::upstream::Aggregator::default(),
        );
        let handler = AgentHandler::new(shared);
        let identity = |conversation: &str| RequestIdentity {
            openai_subject: "usr".to_owned(),
            openai_conversation_id: conversation.to_owned(),
            mcp_session_id: None,
            transport_mode: crate::request_context::TransportMode::Stateless,
        };

        let root = handler
            .chatgpt_turn_init(
                InitializationRequestContext(Ok(identity("owner"))),
                Parameters(InitArgs {
                    project_key: Some("demo-project".to_owned()),
                    previous_turn_ref: None,
                }),
            )
            .await
            .unwrap();
        let root_ref = root.structured_content.as_ref().unwrap()["turn_ref"]
            .as_str()
            .unwrap()
            .to_owned();

        storage
            .memory_set("demo-project", "continuity/memory", "same-project-memory")
            .unwrap();
        storage
            .plan_set(
                "demo-project",
                Some("same-project-plan".to_owned()),
                vec![crate::storage::PlanItemRecord {
                    step: "keep-the-same-plan".to_owned(),
                    status: "in_progress".to_owned(),
                }],
            )
            .unwrap();

        let valid_branch = handler
            .chatgpt_turn_init(
                InitializationRequestContext(Ok(identity("valid-branch"))),
                Parameters(InitArgs {
                    project_key: None,
                    previous_turn_ref: Some(root_ref.clone()),
                }),
            )
            .await
            .unwrap();
        let valid_value = valid_branch.structured_content.as_ref().unwrap();
        assert_eq!(valid_value["status"], json!("synchronized"));
        let valid_brief = valid_value["brief"].as_str().unwrap();
        assert!(valid_brief.contains("continuity/memory"), "{valid_brief}");
        assert!(valid_brief.contains("same-project-memory"), "{valid_brief}");
        assert!(valid_brief.contains("same-project-plan"), "{valid_brief}");
        assert!(valid_brief.contains("keep-the-same-plan"), "{valid_brief}");

        let recovered = handler
            .chatgpt_turn_init(
                InitializationRequestContext(Ok(identity("owner"))),
                Parameters(InitArgs {
                    project_key: None,
                    previous_turn_ref: Some("r_invalid".to_owned()),
                }),
            )
            .await
            .unwrap();
        let recovered_value = recovered.structured_content.as_ref().unwrap();
        assert_eq!(recovered_value["status"], json!("synchronized"));
        let recovered_brief = recovered_value["brief"].as_str().unwrap();
        assert!(
            recovered_brief.contains("continuity/memory"),
            "{recovered_brief}"
        );
        assert!(
            recovered_brief.contains("same-project-memory"),
            "{recovered_brief}"
        );
        assert!(
            recovered_brief.contains("same-project-plan"),
            "{recovered_brief}"
        );
        assert!(
            recovered_brief.contains("keep-the-same-plan"),
            "{recovered_brief}"
        );

        let unresolved = handler
            .chatgpt_turn_init(
                InitializationRequestContext(Ok(identity("needs-key"))),
                Parameters(InitArgs {
                    project_key: None,
                    previous_turn_ref: Some("r_invalid".to_owned()),
                }),
            )
            .await
            .unwrap();
        assert_eq!(unresolved.is_error, Some(true));
        assert!(unresolved.structured_content.is_none());
        let unresolved_text = unresolved.content[0].as_text().unwrap().text.as_str();
        assert!(unresolved_text.contains("PROJECT_KEY_REQUIRED"));
        assert!(unresolved_text.contains("project_key"));

        let joined_with_key = handler
            .chatgpt_turn_init(
                InitializationRequestContext(Ok(identity("needs-key"))),
                Parameters(InitArgs {
                    project_key: Some("demo-project".to_owned()),
                    previous_turn_ref: Some("r_invalid".to_owned()),
                }),
            )
            .await
            .unwrap();
        let joined_value = joined_with_key.structured_content.as_ref().unwrap();
        assert_eq!(joined_value["status"], json!("synchronized"));
        let joined_brief = joined_value["brief"].as_str().unwrap();
        assert!(joined_brief.contains("continuity/memory"), "{joined_brief}");
        assert!(
            joined_brief.contains("same-project-memory"),
            "{joined_brief}"
        );
        assert!(joined_brief.contains("same-project-plan"), "{joined_brief}");
        assert!(
            joined_brief.contains("keep-the-same-plan"),
            "{joined_brief}"
        );
    }

    #[test]
    fn structured_success_and_unstructured_error_keep_distinct_contracts() {
        let success = structured_result_with_text(json!({"kind":"success"}), "visible".to_owned());
        assert_eq!(success.is_error, Some(false));
        assert_eq!(success.structured_content, Some(json!({"kind":"success"})));

        let error = error_result(&AppError::new("BOOM", "failed"));
        assert_eq!(error.is_error, Some(true));
        assert_eq!(error.structured_content, None);
        let serialized = serde_json::to_string(&error).unwrap();
        assert!(serialized.contains("BOOM: failed"));
    }

    #[test]
    fn common_codex_argument_aliases_deserialize_without_extra_tools() {
        let search: search::SearchArgs = serde_json::from_value(json!({
            "pattern":"needle",
            "ignoreCase":true,
            "filesOnly":true,
            "maxResults":7
        }))
        .unwrap();
        assert_eq!(search.query, "needle");
        assert!(search.ignore_case);
        assert!(search.files_only);
        assert_eq!(search.max_results, Some(7));

        let read: ReadFileArgs = serde_json::from_value(json!({"path":"a.txt"})).unwrap();
        assert_eq!(read.offset, 0);
        assert_eq!(read.line_byte_offset, 0);
        assert_eq!(read.limit, None);
        assert_eq!(read.max_bytes, None);
    }

    #[test]
    fn project_permits_are_reused_for_the_same_project() {
        let registry = ProjectPermitRegistry::new(2, 3);
        let first = registry.get("project").unwrap();
        let second = registry.get("project").unwrap();
        assert!(Arc::ptr_eq(&first.0, &second.0));
        assert!(Arc::ptr_eq(&first.1, &second.1));
        assert!(Arc::ptr_eq(&first.2, &second.2));
    }

    #[test]
    fn project_permit_registry_evicts_only_idle_entries_at_capacity() {
        let registry = ProjectPermitRegistry::new(2, 3);
        for index in 0..PROJECT_PERMIT_CACHE_MAX_ENTRIES {
            registry.get(&format!("project-{index}")).unwrap();
        }
        assert_eq!(registry.entries.len(), PROJECT_PERMIT_CACHE_MAX_ENTRIES);
        registry.get("replacement").unwrap();
        assert_eq!(registry.entries.len(), PROJECT_PERMIT_CACHE_MAX_ENTRIES);
        assert!(registry.entries.contains_key("replacement"));
    }

    #[test]
    fn project_permit_registry_fails_closed_when_every_entry_is_active() {
        let registry = ProjectPermitRegistry::new(1, 1);
        let mut held = Vec::with_capacity(PROJECT_PERMIT_CACHE_MAX_ENTRIES);
        for index in 0..PROJECT_PERMIT_CACHE_MAX_ENTRIES {
            let permits = registry.get(&format!("project-{index}")).unwrap();
            held.push(permits.0.clone().try_acquire_owned().unwrap());
        }
        let error = registry.get("overflow").unwrap_err();
        assert_eq!(error.code(), "SERVER_BUSY");
        assert_eq!(registry.entries.len(), PROJECT_PERMIT_CACHE_MAX_ENTRIES);
        drop(held);
    }

    #[test]
    fn project_permit_registry_never_evicts_an_active_project() {
        let registry = ProjectPermitRegistry::new(1, 1);
        let active = registry.get("active").unwrap();
        let held = active.0.clone().try_acquire_owned().unwrap();
        for index in 1..PROJECT_PERMIT_CACHE_MAX_ENTRIES {
            registry.get(&format!("idle-{index}")).unwrap();
        }

        registry.get("replacement").unwrap();

        assert!(registry.entries.contains_key("active"));
        assert!(registry.entries.contains_key("replacement"));
        assert_eq!(registry.entries.len(), PROJECT_PERMIT_CACHE_MAX_ENTRIES);
        drop(held);
    }

    #[test]
    fn concurrent_project_permit_inserts_respect_the_hard_cap() {
        let registry = ProjectPermitRegistry::new(1, 1);
        let threads = 16;
        let inserts_per_thread = PROJECT_PERMIT_CACHE_MAX_ENTRIES / threads + 64;
        std::thread::scope(|scope| {
            for thread_index in 0..threads {
                let registry = registry.clone();
                scope.spawn(move || {
                    for index in 0..inserts_per_thread {
                        let _ = registry.get(&format!("p-{thread_index}-{index}"));
                    }
                });
            }
        });

        assert!(registry.entries.len() <= PROJECT_PERMIT_CACHE_MAX_ENTRIES);
    }

    #[test]
    fn patch_transaction_rolls_back_applied_updates_after_a_later_failure() {
        let directory = tempfile::tempdir().unwrap();
        let paths = SecurePathResolver;
        let root = directory.path();
        paths.write_file_atomic(root, "a.txt", b"old").unwrap();
        let updates = vec![
            PatchUpdate {
                path: "a.txt".to_owned(),
                old: Some(b"old".to_vec()),
                new: Some(b"new".to_vec()),
            },
            PatchUpdate {
                path: "missing.txt".to_owned(),
                old: Some(b"never-applied".to_vec()),
                new: None,
            },
        ];
        assert!(capability_patch_transaction(&paths, root, &updates).is_err());
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"old");
    }

    #[test]
    fn patch_transaction_rejects_stale_preflight_without_overwriting_current_file() {
        let directory = tempfile::tempdir().unwrap();
        let paths = SecurePathResolver;
        let root = directory.path();
        paths.write_file_atomic(root, "a.txt", b"current").unwrap();
        let error = capability_patch_transaction(
            &paths,
            root,
            &[PatchUpdate {
                path: "a.txt".to_owned(),
                old: Some(b"stale".to_vec()),
                new: Some(b"new".to_vec()),
            }],
        )
        .unwrap_err();
        assert_eq!(error.code(), "PATCH_CONFLICT");
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"current");
    }
}
