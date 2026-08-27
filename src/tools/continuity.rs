use std::collections::BTreeMap;

use rmcp::{
    ErrorData, handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::AgentHandler;
use crate::{
    audit::project_json,
    error::{AppError, Result as AppResult},
    request_context::ProjectRequestContext,
    storage::{PlanItemRecord, Storage},
};

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct RememberArgs {
    pub key: String,
    pub value: String,
    #[serde(default)]
    pub scope: MemoryScope,
}

#[derive(Debug, Default, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MemoryScope {
    #[default]
    Active,
    Archive,
}

#[derive(Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct RecallArgs {
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub max_results: Option<usize>,
    /// Snapshot hash returned by the first page. Pass it on continuation calls
    /// so concurrent memory changes fail explicitly instead of shifting OFFSET.
    #[serde(default)]
    pub snapshot_hash: Option<String>,
    #[serde(default)]
    pub include_plan: bool,
    #[serde(default)]
    pub scope: MemoryScope,
    /// Forward-compatible optional arguments. Typed top-level fields remain preferred;
    /// newer servers may consume additional keys here without requiring clients to
    /// refresh their top-level tool schema first.
    #[serde(default)]
    pub extensions: BTreeMap<String, Value>,
}

fn effective_snapshot_hash(args: &RecallArgs) -> AppResult<Option<String>> {
    if let Some(snapshot_hash) = args.snapshot_hash.as_ref() {
        return Ok(Some(snapshot_hash.clone()));
    }
    super::extension_arg(&args.extensions, "snapshot_hash")
}

fn validate_recall_continuation(
    args: &RecallArgs,
    snapshot_hash: &Option<String>,
) -> AppResult<()> {
    if args.key.is_none() && args.offset > 0 && snapshot_hash.is_none() {
        return Err(AppError::new(
            "INVALID_INPUT",
            "snapshot_hash is required when continuing recall with offset > 0",
        ));
    }
    Ok(())
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct PlanItem {
    pub step: String,
    pub status: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct UpdatePlanArgs {
    pub plan: Vec<PlanItem>,
    #[serde(default)]
    pub explanation: Option<String>,
}

fn remember_value(
    storage: &Storage,
    project_key: &str,
    key: &str,
    value: &str,
    scope: MemoryScope,
) -> AppResult<(bool, bool)> {
    if value.is_empty() {
        let deleted = match scope {
            MemoryScope::Active => storage.memory_delete(project_key, key)?,
            MemoryScope::Archive => storage.memory_archive_delete(project_key, key)?,
        };
        return Ok((false, deleted));
    }
    match scope {
        MemoryScope::Active => storage.memory_set(project_key, key, value)?,
        MemoryScope::Archive => storage.memory_archive_set(project_key, key, value)?,
    }
    Ok((true, false))
}

fn normalized_memory_key(raw: &str) -> AppResult<String> {
    let key = raw.trim();
    if key.is_empty() {
        return Err(AppError::new(
            "INVALID_INPUT",
            "memory key must contain non-whitespace characters",
        ));
    }
    Ok(key.to_owned())
}

fn normalized_explanation(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    })
}

#[tool_router(router = continuity_router, vis = "pub(crate)")]
impl AgentHandler {
    #[tool(
        description = "Persist or delete one project-scoped note. scope=active (default) is small working memory that chatgpt_turn_init always hydrates in full with the current plan; use it only for concise durable facts that are costly to rediscover. scope=archive stores larger history that is never injected automatically and is retrieved on demand with recall scope=archive. An empty value deletes the key from the selected scope."
    )]
    async fn remember(
        &self,
        context: ProjectRequestContext,
        Parameters(args): Parameters<RememberArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if args.value.len() > self.shared.config.limits.write_bytes {
            return Ok(super::error_result(&AppError::new(
                "INPUT_TOO_LARGE",
                "memory value too large",
            )));
        }
        let key = match normalized_memory_key(&args.key) {
            Ok(key) => key,
            Err(error) => return Ok(super::error_result(&error)),
        };
        let value = args.value;
        let storage = self.shared.storage.clone();
        let scope = args.scope;
        let params = json!({"key":key,"value":value,"scope":scope});
        self.run(context.0, "remember", params, move |project| async move {
            let project_key = project.effective_project_key.as_str();
            let (saved, deleted) = remember_value(&storage, project_key, &key, &value, scope)?;
            Ok(json!({"key":key,"saved":saved,"deleted":deleted}))
        })
        .await
    }

    #[tool(
        description = "Read project-scoped memory on demand. scope=active (default) reads the small working-memory set that chatgpt_turn_init hydrates fully; scope=archive reads durable history that is not injected automatically. With key, return one note. Without key, return a bounded lexicographically sorted page using offset/max_results. The first page returns snapshot_hash; continuations with offset>0 require that exact hash so concurrent changes fail with PAGINATION_STALE. Set include_plan=true to include the complete current plan."
    )]
    async fn recall(
        &self,
        context: ProjectRequestContext,
        Parameters(args): Parameters<RecallArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let storage = self.shared.storage.clone();
        let snapshot_hash = match effective_snapshot_hash(&args) {
            Ok(snapshot_hash) => snapshot_hash,
            Err(error) => return Ok(super::error_result(&error)),
        };
        let params = serde_json::to_value(&args).unwrap_or_default();
        self.run(context.0, "recall", params, move |project| async move {
            let project_key = project.effective_project_key.as_str();
            validate_recall_continuation(&args, &snapshot_hash)?;
            if let Some(key) = args.key {
                let key = normalized_memory_key(&key)?;
                let value = match args.scope {
                    MemoryScope::Active => storage.memory_get(project_key, &key)?,
                    MemoryScope::Archive => storage.memory_archive_get(project_key, &key)?,
                };
                return Ok(json!({
                    "key":key,
                    "value":value,
                    "plan": if args.include_plan { storage.plan_get(project_key)? } else { None },
                }));
            }
            let requested = args
                .max_results
                .unwrap_or(crate::storage::MEMORY_RECALL_MAX_ENTRIES);
            let (page, snapshot_hash) = match args.scope {
                MemoryScope::Active => storage.memory_recall_page_from_snapshot(
                    project_key,
                    args.offset,
                    requested,
                    snapshot_hash.as_deref(),
                )?,
                MemoryScope::Archive => storage.memory_archive_recall_page_from_snapshot(
                    project_key,
                    args.offset,
                    requested,
                    snapshot_hash.as_deref(),
                )?,
            };
            let mut value = serde_json::to_value(page).unwrap_or_default();
            if let Some(object) = value.as_object_mut() {
                object.insert(
                    "snapshot_hash".to_owned(),
                    serde_json::Value::String(snapshot_hash.clone()),
                );
                object.insert(
                    "plan".to_owned(),
                    if args.include_plan {
                        serde_json::to_value(storage.plan_get(project_key)?).unwrap_or_default()
                    } else {
                        serde_json::Value::Null
                    },
                );
                if let Some(next_offset) = object.get("next_offset").and_then(serde_json::Value::as_u64) {
                    object.insert(
                        "continuation".to_owned(),
                        serde_json::Value::String(format!(
                            "Call recall again with scope={}, offset={next_offset} and snapshot_hash={snapshot_hash} to continue memory enumeration.",
                            match args.scope { MemoryScope::Active => "active", MemoryScope::Archive => "archive" }
                        )),
                    );
                } else {
                    object.insert("continuation".to_owned(), serde_json::Value::Null);
                }
            }
            Ok(value)
        })
        .await
    }

    #[tool(
        description = "Replace the active project's persistent task plan. Each item has step plus status pending, in_progress, or completed; at most one item may be in_progress. Pass an empty plan to clear the persisted plan when the task checklist is no longer useful. Use explanation when the plan changes materially."
    )]
    async fn update_plan(
        &self,
        context: ProjectRequestContext,
        Parameters(args): Parameters<UpdatePlanArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if args.plan.len() > 100
            || args.plan.iter().any(|item| {
                !matches!(
                    item.status.as_str(),
                    "pending" | "in_progress" | "completed"
                )
            })
            || args
                .plan
                .iter()
                .filter(|item| item.status == "in_progress")
                .count()
                > 1
        {
            return Ok(super::error_result(&AppError::new(
                "INVALID_INPUT",
                "plan must contain at most 100 steps, valid statuses, and at most one in_progress step",
            )));
        }
        let storage = self.shared.storage.clone();
        let audit = self.shared.audit.clone();
        let params = serde_json::to_value(&args).unwrap_or_default();
        self.run(
            context.0,
            "update_plan",
            params,
            move |project| async move {
                if args.plan.is_empty() {
                    storage.plan_clear(project.effective_project_key.as_str())?;
                    audit.emit(json!({
                        "event":"plan_updated",
                        "project":project_json(&project),
                        "plan":serde_json::Value::Null
                    }));
                    return Ok(json!({"updated":true,"plan":serde_json::Value::Null}));
                }
                let items = args
                    .plan
                    .into_iter()
                    .map(|item| PlanItemRecord {
                        step: item.step,
                        status: item.status,
                    })
                    .collect();
                let plan = storage.plan_set(
                    project.effective_project_key.as_str(),
                    normalized_explanation(args.explanation),
                    items,
                )?;
                audit.emit(
                    json!({"event":"plan_updated","project":project_json(&project),"plan":plan}),
                );
                Ok(json!({"updated":true,"plan":plan}))
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_snapshot_hash_extensions_fallback_and_typed_value_precedence() {
        let extension_only: RecallArgs = serde_json::from_value(json!({
            "extensions":{"snapshot_hash":"extension-hash"}
        }))
        .unwrap();
        assert_eq!(
            effective_snapshot_hash(&extension_only).unwrap().as_deref(),
            Some("extension-hash")
        );

        let typed: RecallArgs = serde_json::from_value(json!({
            "snapshot_hash":"typed-hash",
            "extensions":{"snapshot_hash":123,"future_cursor":{"opaque":true}}
        }))
        .unwrap();
        assert_eq!(
            effective_snapshot_hash(&typed).unwrap().as_deref(),
            Some("typed-hash")
        );

        let unknown_only: RecallArgs = serde_json::from_value(json!({
            "extensions":{"future_cursor":{"opaque":true}}
        }))
        .unwrap();
        assert_eq!(effective_snapshot_hash(&unknown_only).unwrap(), None);
    }

    #[test]
    fn recall_snapshot_hash_extension_rejects_invalid_type() {
        let args: RecallArgs = serde_json::from_value(json!({
            "extensions":{"snapshot_hash":123}
        }))
        .unwrap();
        let error = effective_snapshot_hash(&args).unwrap_err();
        assert_eq!(error.code(), "INVALID_INPUT");
        assert!(error.message().contains("extensions.snapshot_hash"));
    }

    #[test]
    fn recall_continuation_without_snapshot_hash_is_rejected() {
        let args: RecallArgs = serde_json::from_value(json!({"offset":1})).unwrap();
        let snapshot_hash = effective_snapshot_hash(&args).unwrap();
        let error = validate_recall_continuation(&args, &snapshot_hash).unwrap_err();
        assert_eq!(error.code(), "INVALID_INPUT");
        assert!(error.message().contains("snapshot_hash is required"));

        let direct: RecallArgs = serde_json::from_value(json!({"key":"alpha","offset":1})).unwrap();
        validate_recall_continuation(&direct, &None).unwrap();

        let extension: RecallArgs = serde_json::from_value(json!({
            "offset":1,
            "extensions":{"snapshot_hash":"snapshot"}
        }))
        .unwrap();
        let extension_hash = effective_snapshot_hash(&extension).unwrap();
        validate_recall_continuation(&extension, &extension_hash).unwrap();
    }

    #[tokio::test]
    async fn regression_public_update_plan_empty_plan_clears_persisted_plan() {
        use std::{collections::BTreeMap, sync::Arc};

        use crate::{
            audit::AuditLogger,
            config::ConfigBuilder,
            project::{ProjectContext, ProjectKey, ProjectResolver},
            request_context::TransportMode,
            tools::SharedState,
            upstream::Aggregator,
        };

        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let config = Arc::new(
            ConfigBuilder::from_map(BTreeMap::from([
                ("MCP_AUTH_TOKEN".to_owned(), "1234567890abcdef".to_owned()),
                ("WORKSPACE_ROOT".to_owned(), workspace.display().to_string()),
            ]))
            .build()
            .unwrap(),
        );
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        storage
            .plan_set(
                "effective",
                None,
                vec![PlanItemRecord {
                    step: "stale task".to_owned(),
                    status: "completed".to_owned(),
                }],
            )
            .unwrap();
        let resolver = ProjectResolver::new(workspace, storage.clone()).unwrap();
        let audit = AuditLogger::new(config.logs.clone(), config.auth_token.clone())
            .await
            .unwrap();
        let shared = SharedState::new(
            config,
            resolver,
            storage.clone(),
            audit,
            Aggregator::default(),
        );
        let handler = AgentHandler::new(shared);
        let project = ProjectContext {
            native_project_key: ProjectKey::new("native".to_owned()).unwrap(),
            effective_project_key: ProjectKey::new("effective".to_owned()).unwrap(),
            project_alias: None,
            project_root: directory.path().join("project"),
            metadata_root: directory.path().join("metadata"),
            transport_mode: TransportMode::Stateless,
            mcp_session_present: false,
        };

        let response = handler
            .update_plan(
                ProjectRequestContext(Ok(project)),
                Parameters(UpdatePlanArgs {
                    plan: Vec::new(),
                    explanation: None,
                }),
            )
            .await
            .unwrap();

        assert!(
            !response.is_error.unwrap_or(false),
            "an empty Codex-compatible plan should be a public clear operation"
        );
        assert!(
            storage.plan_get("effective").unwrap().is_none(),
            "empty public update_plan left stale persisted plan state behind"
        );
    }

    #[test]
    fn remember_empty_value_deletes_existing_note() {
        let directory = tempfile::tempdir().expect("tempdir");
        let storage = Storage::open(&directory.path().join("state.sqlite3")).expect("storage");
        assert_eq!(
            remember_value(&storage, "p", "key", "value", MemoryScope::Active).expect("save"),
            (true, false)
        );
        assert_eq!(
            remember_value(&storage, "p", "key", "", MemoryScope::Active).expect("delete"),
            (false, true)
        );
        assert_eq!(storage.memory_get("p", "key").expect("get"), None);
    }

    #[test]
    fn archive_memory_is_separate_from_active_memory() {
        let directory = tempfile::tempdir().expect("tempdir");
        let storage = Storage::open(&directory.path().join("state.sqlite3")).expect("storage");
        remember_value(&storage, "p", "history", "archived", MemoryScope::Archive).unwrap();
        assert_eq!(storage.memory_get("p", "history").unwrap(), None);
        assert_eq!(
            storage
                .memory_archive_get("p", "history")
                .unwrap()
                .as_deref(),
            Some("archived")
        );
        remember_value(&storage, "p", "history", "", MemoryScope::Archive).unwrap();
        assert_eq!(storage.memory_archive_get("p", "history").unwrap(), None);
    }

    #[test]
    fn memory_keys_are_trimmed_and_whitespace_only_is_rejected() {
        assert_eq!(
            normalized_memory_key("  build-system  ").unwrap(),
            "build-system"
        );
        assert!(normalized_memory_key(" \t\n ").is_err());
    }

    #[test]
    fn blank_plan_explanation_normalizes_to_none() {
        assert_eq!(normalized_explanation(Some("   ".to_owned())), None);
        assert_eq!(
            normalized_explanation(Some("  why this order  ".to_owned())),
            Some("why this order".to_owned())
        );
    }
    #[test]
    fn normalized_memory_key_collapses_only_outer_whitespace() {
        // Inner spacing is preserved so `memory key` and `memorykey` stay
        // distinct; only outer trim is applied for normalization.
        assert_eq!(
            normalized_memory_key("  spaced key  ").unwrap(),
            "spaced key"
        );
        assert_ne!(
            normalized_memory_key("a b").unwrap(),
            normalized_memory_key("ab").unwrap()
        );
        assert!(normalized_memory_key("\t\n").is_err());
    }

    #[test]
    fn remember_value_delete_reports_missing_as_false() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        // Deleting a note that never existed is (saved=false, deleted=false).
        let (saved, deleted) =
            remember_value(&storage, "p", "ghost", "", MemoryScope::Active).unwrap();
        assert!(!saved);
        assert!(!deleted);
        storage.memory_set("p", "real", "value").unwrap();
        let (saved, deleted) =
            remember_value(&storage, "p", "real", "", MemoryScope::Active).unwrap();
        assert!(!saved);
        assert!(deleted);
    }
}
