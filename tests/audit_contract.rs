use std::time::Duration;

use codex_bridge::{
    audit::AuditLogger,
    config::LogConfig,
    project::{ProjectContext, ProjectKey},
    request_context::TransportMode,
};
use serde_json::json;

fn log_config(root: std::path::PathBuf) -> LogConfig {
    LogConfig {
        root,
        queue_capacity: 64,
        queue_max_bytes: 1024 * 1024,
        console_param_bytes: 256,
        console_result_bytes: 256,
        file_event_bytes: 4096,
        max_file_bytes: 1024 * 1024,
        max_files: 4,
    }
}

fn project(root: &std::path::Path) -> ProjectContext {
    ProjectContext {
        native_project_key: ProjectKey::new("native_project".to_owned()).unwrap(),
        effective_project_key: ProjectKey::new("effective_project".to_owned()).unwrap(),
        project_alias: Some("team".to_owned()),
        project_root: root.join("project"),
        metadata_root: root.join("metadata"),
        transport_mode: TransportMode::Stateless,
        mcp_session_present: false,
    }
}

async fn wait_for_log(root: &std::path::Path, needle: &str) -> String {
    for _ in 0..100 {
        let mut combined = String::new();
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                if entry.path().is_file()
                    && let Ok(content) = std::fs::read_to_string(entry.path())
                {
                    combined.push_str(&content);
                }
            }
        }
        if combined.contains(needle) {
            return combined;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("audit output never contained {needle}");
}

#[tokio::test]
async fn emitted_events_are_persisted_and_auth_token_is_redacted() {
    let temp = tempfile::tempdir().unwrap();
    let logs = temp.path().join("logs");
    let token = "audit-secret-token";
    let logger = AuditLogger::new(log_config(logs.clone()), token.to_owned())
        .await
        .unwrap();
    logger.emit(json!({"event":"integration_event","text":format!("prefix-{token}-suffix")}));
    let content = wait_for_log(&logs, "integration_event").await;
    assert!(!content.contains(token));
    logger.shutdown().await;
}

#[tokio::test]
async fn tool_lifecycle_updates_running_and_activity_state() {
    let temp = tempfile::tempdir().unwrap();
    let logs = temp.path().join("logs");
    let logger = AuditLogger::new(log_config(logs), "token".to_owned())
        .await
        .unwrap();
    let project = project(temp.path());
    let (request_id, started) = logger.tool_started(&project, "read_file", json!({"path":"a.txt"}));
    assert_eq!(logger.running_count(), 1);
    assert_eq!(logger.running_for_project(&project).len(), 1);
    logger.tool_finished(
        &project,
        &request_id,
        "read_file",
        started,
        &json!({"ok":true}),
    );
    assert_eq!(logger.running_count(), 0);
    let activity = logger.activity(&project);
    assert_eq!(activity.last_tool.as_deref(), Some("read_file"));
    assert_eq!(
        activity.last_successful_operation.as_deref(),
        Some("read_file")
    );
    logger.shutdown().await;
}

#[tokio::test]
async fn unscoped_turn_init_attempt_is_persisted_as_tool_lifecycle() {
    let temp = tempfile::tempdir().unwrap();
    let logs = temp.path().join("logs");
    let logger = AuditLogger::new(log_config(logs.clone()), "token".to_owned())
        .await
        .unwrap();
    let project = project(temp.path());
    let (request_id, started) = logger.tool_attempt_started(
        "chatgpt_turn_init",
        json!({"project_key":"demo","previous_turn_ref_present":false}),
    );
    logger.tool_attempt_finished(
        Some(&project),
        &request_id,
        "chatgpt_turn_init",
        started,
        &json!({
            "initialized":true,
            "brief":"must-not-persist",
            "state_update":"must-not-persist-either"
        }),
    );
    logger.shutdown().await;

    let content = std::fs::read_to_string(logs.join("tool-calls.log")).unwrap();
    assert_eq!(content.matches(&request_id).count(), 2);
    assert!(content.contains("chatgpt_turn_init"));
    assert!(content.contains("must-not-persist"));
    assert!(content.contains("must-not-persist-either"));
}

#[tokio::test]
async fn failed_tool_clears_running_entry_and_records_error() {
    let temp = tempfile::tempdir().unwrap();
    let logger = AuditLogger::new(log_config(temp.path().join("logs")), "token".to_owned())
        .await
        .unwrap();
    let project = project(temp.path());
    let (request_id, started) = logger.tool_started(&project, "grep", json!({"pattern":"x"}));
    logger.tool_failed(
        &project,
        &request_id,
        "grep",
        started,
        &codex_bridge::error::AppError::new("TEST_FAILURE", "expected"),
    );
    assert_eq!(logger.running_count(), 0);
    let activity = logger.activity(&project);
    assert_eq!(activity.last_tool.as_deref(), Some("grep"));
    assert_eq!(activity.last_error.unwrap()["code"], "TEST_FAILURE");
    logger.shutdown().await;
}

#[tokio::test]
async fn running_queries_are_isolated_by_effective_project() {
    let temp = tempfile::tempdir().unwrap();
    let logger = AuditLogger::new(log_config(temp.path().join("logs")), "token".to_owned())
        .await
        .unwrap();
    let first = project(temp.path());
    let mut second = project(temp.path());
    second.effective_project_key = ProjectKey::new("other_effective".to_owned()).unwrap();
    let (_request_id, _started) = logger.tool_started(&first, "tree", json!({}));
    assert_eq!(logger.running_for_project(&first).len(), 1);
    assert!(logger.running_for_project(&second).is_empty());
    logger.shutdown().await;
}

#[tokio::test]
async fn shutdown_is_idempotent_and_does_not_leak_queue_permits() {
    let temp = tempfile::tempdir().unwrap();
    let logger = AuditLogger::new(log_config(temp.path().join("logs")), "token".to_owned())
        .await
        .unwrap();
    let initial = logger.queue_bytes_available();
    logger.emit(json!({"event":"one"}));
    logger.emit(json!({"event":"two"}));
    logger.shutdown().await;
    logger.shutdown().await;
    assert_eq!(
        logger.queue_bytes_available(),
        initial,
        "shutdown must release every byte permit acquired by queued events"
    );
}
