use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use chrono::Utc;
use dashmap::DashMap;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::{
    io::AsyncWriteExt,
    sync::{OwnedSemaphorePermit, Semaphore, mpsc},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{
    config::LogConfig,
    error::{AppError, Result},
    project::ProjectContext,
};

const PROJECT_ACTIVITY_MAX_ENTRIES: usize = 4096;

#[derive(Debug, Clone, Serialize)]
pub struct RunningTool {
    pub request_id: String,
    pub tool: String,
    #[serde(skip)]
    pub project_key: String,
    #[serde(skip)]
    pub started: Instant,
    pub summary: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ProjectActivity {
    pub last_tool: Option<String>,
    pub last_error: Option<Value>,
    pub last_successful_operation: Option<String>,
}

struct AuditInner {
    sender: mpsc::Sender<AuditEnvelope>,
    queue_bytes: Arc<Semaphore>,
    config: LogConfig,
    auth_token: String,
    dropped_total: AtomicU64,
    dropped_pending: AtomicU64,
    running: DashMap<String, RunningTool>,
    project_activity: DashMap<String, ProjectActivity>,
    project_activity_lock: Mutex<()>,
    cancellation: CancellationToken,
    writer: Mutex<Option<JoinHandle<()>>>,
}

struct AuditEnvelope {
    value: Value,
    _byte_permit: OwnedSemaphorePermit,
}

#[derive(Clone)]
pub struct AuditLogger(Arc<AuditInner>);

fn is_sensitive_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace('-', "_");
    if matches!(
        key.to_ascii_lowercase().as_str(),
        "openai/subject" | "openai/session" | "mcp-session-id"
    ) {
        return true;
    }
    if normalized.split('_').any(|part| {
        matches!(
            part,
            "secret" | "token" | "password" | "passwd" | "credential" | "credentials"
        )
    }) {
        return true;
    }
    [
        "authorization",
        "cookie",
        "set_cookie",
        "password",
        "passwd",
        "secret",
        "token",
        "access_token",
        "refresh_token",
        "api_key",
        "apikey",
        "private_key",
        "access_key",
        "secret_access_key",
        "mcp_auth_token",
    ]
    .iter()
    .any(|sensitive| normalized == *sensitive || normalized.ends_with(&format!("_{sensitive}")))
}

fn redact_tool_payload(value: &Value) -> Value {
    // Audit output is an operator debugging surface. Preserve normal tool
    // parameters/results and let `scrub` apply credential-key redaction, MCP
    // auth-token replacement, and configured byte excerpts. Blanket-redacting
    // command/content/stdout made the audit trail useless for diagnosing agent
    // behavior despite the documented excerpt limits.
    value.clone()
}

fn scrub(value: &Value, auth_token: &str, string_limit: usize) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    if is_sensitive_key(key) {
                        (key.clone(), Value::String("[REDACTED]".into()))
                    } else {
                        (key.clone(), scrub(value, auth_token, string_limit))
                    }
                })
                .collect(),
        ),
        Value::Array(array) => {
            let mut values: Vec<Value> = array
                .iter()
                .take(1024)
                .map(|value| scrub(value, auth_token, string_limit))
                .collect();
            if array.len() > values.len() {
                values.push(json!({
                    "truncated": true,
                    "original_items": array.len(),
                    "shown_items": values.len(),
                }));
            }
            Value::Array(values)
        }
        Value::String(value) => {
            let redacted = if auth_token.is_empty() {
                value.clone()
            } else {
                value.replace(auth_token, "[REDACTED]")
            };
            if redacted.len() <= string_limit {
                Value::String(redacted)
            } else {
                let mut boundary = string_limit.min(redacted.len());
                while !redacted.is_char_boundary(boundary) {
                    boundary -= 1;
                }
                json!({"excerpt":&redacted[..boundary],"original_bytes":redacted.len(),"shown_bytes":boundary,"truncated":true})
            }
        }
        other => other.clone(),
    }
}

/// Apply an aggregate serialized-size ceiling after recursive redaction. Per-string limits alone
/// do not constrain an event containing a very large array of individually small values.
fn bound_event(value: Value, maximum: usize) -> Value {
    let maximum = maximum.max(1024);
    let Ok(serialized) = serde_json::to_vec(&value) else {
        return json!({"event":"serialization_error"});
    };
    if serialized.len() <= maximum {
        return value;
    }
    let shown = maximum
        .saturating_sub(768usize.min(maximum / 2))
        .min(serialized.len());
    let mut envelope = serde_json::Map::new();
    for key in [
        "timestamp",
        "event",
        "request_id",
        "tool",
        "project",
        "status",
    ] {
        if let Some(field) = value.get(key) {
            envelope.insert(key.to_owned(), field.clone());
        }
    }
    envelope.insert("truncated".into(), Value::Bool(true));
    envelope.insert("original_bytes".into(), serialized.len().into());
    envelope.insert("shown_bytes".into(), shown.into());
    envelope.insert(
        "event_excerpt".into(),
        Value::String(String::from_utf8_lossy(&serialized[..shown]).into_owned()),
    );
    Value::Object(envelope)
}

fn event_file(root: &Path, event: &str) -> PathBuf {
    let name = match event {
        "tool_call" | "tool_result" | "tool_error" | "tool_timeout" | "process_started"
        | "process_exited" | "process_killed" => "tool-calls.log",
        event if event.starts_with("task_") || event == "tasks_updated" => "tasks.log",
        event if event.starts_with("plan_") => "plans.log",
        _ => "rust-agent.log",
    };
    root.join(name)
}

async fn rotate(path: &Path, max_bytes: u64, max_files: usize) -> std::io::Result<()> {
    if tokio::fs::metadata(path)
        .await
        .map(|metadata| metadata.len())
        .unwrap_or(0)
        < max_bytes
    {
        return Ok(());
    }
    if max_files == 0 {
        tokio::fs::remove_file(path).await.ok();
        return Ok(());
    }
    let oldest = path.with_extension(format!("log.{max_files}"));
    tokio::fs::remove_file(oldest).await.ok();
    for index in (1..max_files).rev() {
        let from = path.with_extension(format!("log.{index}"));
        let to = path.with_extension(format!("log.{}", index + 1));
        if tokio::fs::metadata(&from).await.is_ok() {
            tokio::fs::rename(from, to).await?;
        }
    }
    tokio::fs::rename(path, path.with_extension("log.1")).await
}

async fn writer_loop(inner: Arc<AuditInner>, mut receiver: mpsc::Receiver<AuditEnvelope>) {
    let mut files: HashMap<PathBuf, tokio::fs::File> = HashMap::new();
    loop {
        let event = tokio::select! {
            event = receiver.recv() => event,
            _ = inner.cancellation.cancelled() => {
                while let Ok(event) = receiver.try_recv() {
                    write_event(&inner, &mut files, event.value).await;
                }
                break;
            }
        };
        match event {
            Some(event) => {
                render_console(&inner, &event.value);
                write_event(&inner, &mut files, event.value).await;
            }
            None => break,
        }
    }
    for file in files.values_mut() {
        let _ = file.flush().await;
    }
}

fn render_console(inner: &AuditInner, event: &Value) {
    if let Some(line) = console_line(inner, event) {
        println!("{line}");
    }
}

const ANSI_RESET: &str = "\x1b[0m";
const ANSI_GRAY: &str = "\x1b[90m";
const ANSI_RED: &str = "\x1b[31m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_YELLOW: &str = "\x1b[33m";
const ANSI_BLUE: &str = "\x1b[34m";

fn console_color(value: &str, color: &str) -> String {
    format!("{color}{value}{ANSI_RESET}")
}

fn console_project(event: &Value) -> &str {
    event
        .get("project")
        .and_then(Value::as_object)
        .and_then(|project| {
            project
                .get("alias")
                .and_then(Value::as_str)
                .or_else(|| project.get("effective_key").and_then(Value::as_str))
        })
        .unwrap_or("global")
}

fn console_line(inner: &AuditInner, event: &Value) -> Option<String> {
    let event_name = event.get("event").and_then(Value::as_str)?;
    let project = console_project(event);
    let tool = event
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    match event_name {
        "tool_call" => {
            let params = event.get("params").cloned().unwrap_or(Value::Null);
            let params = scrub(&params, &inner.auth_token, inner.config.console_param_bytes);
            let params = bound_event(params, inner.config.console_param_bytes.saturating_mul(4));
            let rendered = serde_json::to_string(&params)
                .unwrap_or_else(|_| "{\"serialization_error\":true}".to_owned());
            Some(format!(
                "{} [{}] {} {rendered}",
                console_color("=>>", ANSI_GRAY),
                console_color(project, ANSI_GREEN),
                console_color(tool, ANSI_RED)
            ))
        }
        "tool_result" => {
            let result = event.get("result").cloned().unwrap_or(Value::Null);
            let result = scrub(
                &result,
                &inner.auth_token,
                inner.config.console_result_bytes,
            );
            let result = bound_event(result, inner.config.console_result_bytes.saturating_mul(4));
            let rendered = serde_json::to_string(&result)
                .unwrap_or_else(|_| "{\"serialization_error\":true}".to_owned());
            Some(format!(
                "{} [{}] {} {rendered}",
                console_color("<<=", ANSI_GRAY),
                console_color(project, ANSI_BLUE),
                console_color(tool, ANSI_YELLOW)
            ))
        }
        "tool_error" | "tool_timeout" => {
            let error = event.get("error").cloned().unwrap_or(Value::Null);
            let error = scrub(&error, &inner.auth_token, inner.config.console_result_bytes);
            let rendered = serde_json::to_string(&error)
                .unwrap_or_else(|_| "{\"serialization_error\":true}".to_owned());
            Some(format!(
                "{} [{}] {} {event_name}: {rendered}",
                console_color("<<=", ANSI_GRAY),
                console_color(project, ANSI_BLUE),
                console_color(tool, ANSI_YELLOW)
            ))
        }
        _ => None,
    }
}

async fn write_event(
    inner: &AuditInner,
    files: &mut HashMap<PathBuf, tokio::fs::File>,
    event: Value,
) {
    let safe = bound_event(
        scrub(&event, &inner.auth_token, inner.config.file_event_bytes),
        inner.config.file_event_bytes,
    );
    let event_name = safe
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or("daemon_event");
    let path = event_file(&inner.config.root, event_name);
    let needs_rotate = tokio::fs::metadata(&path)
        .await
        .map(|metadata| metadata.len())
        .unwrap_or(0)
        >= inner.config.max_file_bytes;
    if needs_rotate {
        files.remove(&path);
        let _ = rotate(&path, inner.config.max_file_bytes, inner.config.max_files).await;
    }
    if !files.contains_key(&path)
        && let Ok(file) = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
    {
        files.insert(path.clone(), file);
    }
    if let Some(file) = files.get_mut(&path)
        && let Ok(mut line) = serde_json::to_vec(&safe)
    {
        if line.len() > inner.config.file_event_bytes {
            let original_bytes = line.len();
            let shown_bytes = inner
                .config
                .file_event_bytes
                .saturating_sub(512)
                .min(line.len());
            let excerpt = String::from_utf8_lossy(&line[..shown_bytes]);
            line = serde_json::to_vec(&json!({
                "timestamp": Utc::now().to_rfc3339(),
                "event": event_name,
                "log_truncated": true,
                "original_bytes": original_bytes,
                "stored_bytes": shown_bytes,
                "event_excerpt": excerpt
            }))
            .unwrap_or_default();
        }
        line.push(b'\n');
        let _ = file.write_all(&line).await;
    }
}

impl AuditLogger {
    fn update_project_activity(&self, key: String, activity: ProjectActivity) {
        let Ok(_guard) = self.0.project_activity_lock.lock() else {
            return;
        };
        if !self.0.project_activity.contains_key(&key)
            && self.0.project_activity.len() >= PROJECT_ACTIVITY_MAX_ENTRIES
        {
            let victim = self
                .0
                .project_activity
                .iter()
                .find(|entry| entry.key().as_str() != key)
                .map(|entry| entry.key().clone());
            if let Some(victim) = victim {
                self.0.project_activity.remove(&victim);
            }
        }
        self.0.project_activity.insert(key, activity);
    }

    pub async fn new(config: LogConfig, auth_token: String) -> Result<Self> {
        tokio::fs::create_dir_all(&config.root).await?;
        let (sender, receiver) = mpsc::channel(config.queue_capacity);
        let inner = Arc::new(AuditInner {
            sender,
            queue_bytes: Arc::new(Semaphore::new(config.queue_max_bytes)),
            config,
            auth_token,
            dropped_total: AtomicU64::new(0),
            dropped_pending: AtomicU64::new(0),
            running: DashMap::new(),
            project_activity: DashMap::new(),
            project_activity_lock: Mutex::new(()),
            cancellation: CancellationToken::new(),
            writer: Mutex::new(None),
        });
        let writer = tokio::spawn(writer_loop(inner.clone(), receiver));
        *inner
            .writer
            .lock()
            .map_err(|_| AppError::new("PROCESS_FAILED", "audit writer lock poisoned"))? =
            Some(writer);
        Ok(Self(inner))
    }

    pub fn emit(&self, event: Value) {
        let mut event = event;
        if let Value::Object(object) = &mut event {
            object
                .entry("timestamp")
                .or_insert_with(|| Value::String(Utc::now().to_rfc3339()));
        }
        let event = bound_event(
            scrub(&event, &self.0.auth_token, self.0.config.file_event_bytes),
            self.0.config.file_event_bytes,
        );
        let bytes = serde_json::to_vec(&event)
            .map(|value| value.len())
            .unwrap_or(self.0.config.file_event_bytes)
            .clamp(1, u32::MAX as usize) as u32;
        let permit = self.0.queue_bytes.clone().try_acquire_many_owned(bytes);
        let sent = permit.ok().and_then(|permit| {
            self.0
                .sender
                .try_send(AuditEnvelope {
                    value: event,
                    _byte_permit: permit,
                })
                .ok()
        });
        if sent.is_none() {
            let dropped = self.0.dropped_pending.fetch_add(1, Ordering::Relaxed) + 1;
            self.0.dropped_total.fetch_add(1, Ordering::Relaxed);
            if dropped.is_power_of_two() {
                eprintln!("audit log queue full; dropped events: {dropped}");
            }
        } else {
            let dropped = self.0.dropped_pending.swap(0, Ordering::Relaxed);
            if dropped > 0 {
                self.emit(json!({
                    "event": "log_events_dropped",
                    "count": dropped
                }));
            }
        }
    }

    pub fn tool_started(
        &self,
        project: &ProjectContext,
        tool: &str,
        params: Value,
    ) -> (String, Instant) {
        let request_id = uuid::Uuid::now_v7().to_string();
        let started = Instant::now();
        let audit_params = redact_tool_payload(&params);
        self.0.running.insert(
            request_id.clone(),
            RunningTool {
                request_id: request_id.clone(),
                tool: tool.to_owned(),
                project_key: project.effective_project_key.as_str().to_owned(),
                started,
                summary: summarize(&audit_params),
            },
        );
        self.emit(json!({"event":"tool_call","request_id":request_id,"tool":tool,"project":project_json(project),"params":audit_params}));
        (request_id, started)
    }

    pub fn tool_attempt_started(&self, tool: &str, params: Value) -> (String, Instant) {
        let request_id = uuid::Uuid::now_v7().to_string();
        let started = Instant::now();
        let audit_params = redact_tool_payload(&params);
        self.emit(json!({
            "event":"tool_call",
            "request_id":request_id,
            "tool":tool,
            "project":Value::Null,
            "params":audit_params
        }));
        (request_id, started)
    }

    pub fn tool_attempt_finished(
        &self,
        project: Option<&ProjectContext>,
        request_id: &str,
        tool: &str,
        started: Instant,
        result: &Value,
    ) {
        let audit_result = redact_tool_payload(result);
        self.emit(json!({
            "event":"tool_result",
            "request_id":request_id,
            "tool":tool,
            "project":project.map(project_json).unwrap_or(Value::Null),
            "duration_ms":started.elapsed().as_millis(),
            "status":"success",
            "result":audit_result
        }));
    }

    pub fn tool_attempt_failed(
        &self,
        project: Option<&ProjectContext>,
        request_id: &str,
        tool: &str,
        started: Instant,
        error: &AppError,
    ) {
        self.emit(json!({
            "event": if error.code() == "PROCESS_TIMEOUT" { "tool_timeout" } else { "tool_error" },
            "request_id":request_id,
            "tool":tool,
            "project":project.map(project_json).unwrap_or(Value::Null),
            "duration_ms":started.elapsed().as_millis(),
            "status":"error",
            "error":{"code":error.code(),"message":error.message()}
        }));
    }

    pub fn tool_finished(
        &self,
        project: &ProjectContext,
        request_id: &str,
        tool: &str,
        started: Instant,
        result: &Value,
    ) {
        self.0.running.remove(request_id);
        let key = project.effective_project_key.as_str().to_owned();
        let mut activity = self
            .0
            .project_activity
            .get(&key)
            .map(|value| value.clone())
            .unwrap_or_default();
        activity.last_tool = Some(tool.to_owned());
        activity.last_successful_operation = Some(tool.to_owned());
        self.update_project_activity(key, activity);
        let audit_result = redact_tool_payload(result);
        self.emit(json!({"event":"tool_result","request_id":request_id,"tool":tool,"project":project_json(project),"duration_ms":started.elapsed().as_millis(),"status":"success","result":audit_result}));
    }

    pub fn tool_failed(
        &self,
        project: &ProjectContext,
        request_id: &str,
        tool: &str,
        started: Instant,
        error: &AppError,
    ) {
        self.0.running.remove(request_id);
        let error_json = json!({"code":error.code(),"message":error.message()});
        let key = project.effective_project_key.as_str().to_owned();
        let mut activity = self
            .0
            .project_activity
            .get(&key)
            .map(|value| value.clone())
            .unwrap_or_default();
        activity.last_tool = Some(tool.to_owned());
        activity.last_error = Some(error_json.clone());
        self.update_project_activity(key, activity);
        let event = if error.code() == "PROCESS_TIMEOUT" {
            "tool_timeout"
        } else {
            "tool_error"
        };
        self.emit(json!({"event":event,"request_id":request_id,"tool":tool,"project":project_json(project),"duration_ms":started.elapsed().as_millis(),"status":"error","error":error_json}));
    }

    pub fn running_for_project(&self, project: &ProjectContext) -> Vec<Value> {
        self.0.running.iter().filter(|entry| entry.project_key == project.effective_project_key.as_str()).take(256).map(|entry| json!({"request_id":entry.request_id,"tool":entry.tool,"duration_ms":entry.started.elapsed().as_millis(),"summary":entry.summary})).collect()
    }
    pub fn activity(&self, project: &ProjectContext) -> ProjectActivity {
        self.0
            .project_activity
            .get(project.effective_project_key.as_str())
            .map(|value| value.clone())
            .unwrap_or_default()
    }
    pub fn running_count(&self) -> usize {
        self.0.running.len()
    }
    pub fn dropped_count(&self) -> u64 {
        self.0.dropped_total.load(Ordering::Relaxed)
    }
    pub fn queue_capacity(&self) -> usize {
        self.0.config.queue_capacity
    }
    pub fn queue_bytes_available(&self) -> usize {
        self.0.queue_bytes.available_permits()
    }

    pub async fn shutdown(&self) {
        self.0.cancellation.cancel();
        let handle = self
            .0
            .writer
            .lock()
            .ok()
            .and_then(|mut writer| writer.take());
        if let Some(handle) = handle {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        }
    }
}

pub fn project_json(project: &ProjectContext) -> Value {
    json!({"native_key":project.native_project_key.as_str(),"effective_key":project.effective_project_key.as_str(),"alias":project.project_alias,"transport_mode":project.transport_mode})
}

fn summarize(value: &Value) -> String {
    if let Some(command) = value.get("command").and_then(Value::as_str) {
        return command.chars().take(160).collect();
    }
    if let Some(path) = value.get("path").and_then(Value::as_str) {
        return path.chars().take(160).collect();
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_recursive_and_auth_token_is_removed_from_strings() {
        let value = json!({"nested":{"api_key":"x","AWS_SECRET_ACCESS_KEY":"aws","safe":"url/token-value/mcp"},"array":[{"password":"p"}]});
        let safe = scrub(&value, "token-value", 4096);
        let text = safe.to_string();
        assert!(!text.contains("token-value"));
        assert!(!text.contains("\"x\""));
        assert!(!text.contains("\"p\""));
        assert!(!text.contains("\"aws\""));
        assert!(text.contains("[REDACTED]"));
    }

    #[test]
    fn tool_payloads_preserve_debug_content_before_credential_scrub() {
        let value = json!({
            "command":"curl -H 'Authorization: Bearer top-secret' https://example.invalid",
            "content":"source-secret",
            "matches":[{"path":"a.rs","text":"inline-secret","line":1}],
            "path":"a.rs"
        });
        let audit = redact_tool_payload(&value);
        assert_eq!(audit["content"], "source-secret");
        assert_eq!(audit["matches"][0]["text"], "inline-secret");
        assert_eq!(audit["path"], "a.rs");

        let safe = scrub(&audit, "top-secret", 4096);
        let text = safe.to_string();
        assert!(!text.contains("top-secret"));
        assert!(text.contains("source-secret"));
        assert!(text.contains("inline-secret"));
    }

    #[test]
    fn large_strings_are_explicitly_excerpted() {
        let safe = scrub(&json!({"content":"abcdefghij"}), "", 4);
        assert_eq!(safe["content"]["truncated"], true);
        assert_eq!(safe["content"]["original_bytes"], 10);
        assert_eq!(safe["content"]["shown_bytes"], 4);
    }

    #[test]
    fn aggregate_event_size_is_bounded_and_marked() {
        let items: Vec<_> = (0..10_000).map(|index| format!("item-{index}")).collect();
        let safe = scrub(&json!({"event":"tool_result","items":items}), "", 128);
        let bounded = bound_event(safe, 4096);
        assert_eq!(bounded["truncated"], true);
        assert_eq!(bounded["event"], "tool_result");
        assert!(serde_json::to_vec(&bounded).unwrap().len() < 4608);
    }

    #[test]
    fn project_activity_cache_is_bounded() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let config = LogConfig {
                root: directory.path().to_path_buf(),
                queue_capacity: 8,
                queue_max_bytes: 64 * 1024,
                console_param_bytes: 64,
                console_result_bytes: 64,
                file_event_bytes: 4096,
                max_file_bytes: 1024 * 1024,
                max_files: 1,
            };
            let logger = AuditLogger::new(config, "secret-token".into())
                .await
                .unwrap();
            for index in 0..=PROJECT_ACTIVITY_MAX_ENTRIES {
                logger.update_project_activity(
                    format!("project-{index}"),
                    ProjectActivity::default(),
                );
            }
            assert_eq!(
                logger.0.project_activity.len(),
                PROJECT_ACTIVITY_MAX_ENTRIES
            );
            logger.shutdown().await;
        });
    }

    #[test]
    fn concurrent_project_activity_updates_respect_the_hard_cap() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let config = LogConfig {
                root: directory.path().to_path_buf(),
                queue_capacity: 8,
                queue_max_bytes: 64 * 1024,
                console_param_bytes: 64,
                console_result_bytes: 64,
                file_event_bytes: 4096,
                max_file_bytes: 1024 * 1024,
                max_files: 1,
            };
            let logger = AuditLogger::new(config, "secret-token".into())
                .await
                .unwrap();
            std::thread::scope(|scope| {
                for thread_index in 0..16 {
                    let logger = logger.clone();
                    scope.spawn(move || {
                        for index in 0..400 {
                            logger.update_project_activity(
                                format!("project-{thread_index}-{index}"),
                                ProjectActivity::default(),
                            );
                        }
                    });
                }
            });
            assert!(logger.0.project_activity.len() <= PROJECT_ACTIVITY_MAX_ENTRIES);
            logger.shutdown().await;
        });
    }

    #[test]
    fn console_output_is_compact_and_tool_focused() {
        let config = LogConfig {
            root: PathBuf::from("unused"),
            queue_capacity: 8,
            queue_max_bytes: 1024,
            console_param_bytes: 256,
            console_result_bytes: 256,
            file_event_bytes: 4096,
            max_file_bytes: 1024,
            max_files: 1,
        };
        let (sender, _receiver) = mpsc::channel(1);
        let inner = AuditInner {
            sender,
            queue_bytes: Arc::new(Semaphore::new(1024)),
            config,
            auth_token: "bridge-secret".to_owned(),
            dropped_total: AtomicU64::new(0),
            dropped_pending: AtomicU64::new(0),
            running: DashMap::new(),
            project_activity: DashMap::new(),
            project_activity_lock: Mutex::new(()),
            cancellation: CancellationToken::new(),
            writer: Mutex::new(None),
        };
        let line = console_line(
            &inner,
            &json!({
                "event":"tool_call",
                "tool":"grep",
                "project":{"alias":"demo","effective_key":"opaque"},
                "params":{"query":"needle","token":"must-hide"}
            }),
        )
        .unwrap();
        assert_eq!(
            line,
            "\u{1b}[90m=>>\u{1b}[0m [\u{1b}[32mdemo\u{1b}[0m] \u{1b}[31mgrep\u{1b}[0m {\"query\":\"needle\",\"token\":\"[REDACTED]\"}"
        );

        let result = console_line(
            &inner,
            &json!({
                "event":"tool_result",
                "tool":"grep",
                "project":{"alias":"demo","effective_key":"opaque"},
                "result":{"matches":3,"token":"bridge-secret"}
            }),
        )
        .unwrap();
        assert_eq!(
            result,
            "\u{1b}[90m<<=\u{1b}[0m [\u{1b}[34mdemo\u{1b}[0m] \u{1b}[33mgrep\u{1b}[0m {\"matches\":3,\"token\":\"[REDACTED]\"}"
        );

        let fallback = console_line(
            &inner,
            &json!({
                "event":"tool_call",
                "tool":"read_file",
                "project":{"alias":null,"effective_key":"effective-project"},
                "params":{"path":"src/lib.rs"}
            }),
        )
        .unwrap();
        assert_eq!(
            fallback,
            "\u{1b}[90m=>>\u{1b}[0m [\u{1b}[32meffective-project\u{1b}[0m] \u{1b}[31mread_file\u{1b}[0m {\"path\":\"src/lib.rs\"}"
        );

        let error = console_line(
            &inner,
            &json!({
                "event":"tool_error",
                "tool":"exec_command",
                "project":{"alias":"demo"},
                "error":{"code":"PROCESS_FAILED","message":"bridge-secret failed"}
            }),
        )
        .unwrap();
        assert_eq!(
            error,
            "\u{1b}[90m<<=\u{1b}[0m [\u{1b}[34mdemo\u{1b}[0m] \u{1b}[33mexec_command\u{1b}[0m tool_error: {\"code\":\"PROCESS_FAILED\",\"message\":\"[REDACTED] failed\"}"
        );
    }

    #[test]
    fn file_events_preserve_content_excerpts_but_redact_credentials() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .build()
            .unwrap();
        runtime.block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let config = LogConfig {
            root: directory.path().to_path_buf(),
            queue_capacity: 32,
            queue_max_bytes: 64 * 1024,
            console_param_bytes: 4,
            console_result_bytes: 8,
            file_event_bytes: 4096,
            max_file_bytes: 1024 * 1024,
            max_files: 2,
        };
        let logger = AuditLogger::new(config, "auth-token-value".into())
            .await
            .unwrap();
        logger.emit(json!({
            "event":"tool_call",
            "request_id":"request-1",
            "tool":"write_file",
            "params":{
                "content":"abcdefghijklmnopqrstuvwxyz-auth-token-value",
                "openai/subject":"usr_raw",
                "nested":{"MCP_AUTH_TOKEN":"auth-token-value"}
            }
        }));
        logger.emit(json!({"event":"tool_result","request_id":"request-1","tool":"write_file","result":{"content":"abcdefghijklmnopqrstuvwxyz"}}));
        logger.shutdown().await;
        let content = std::fs::read_to_string(directory.path().join("tool-calls.log")).unwrap();
        assert_eq!(content.matches("request-1").count(), 2);
        assert!(content.contains("abcdefghijklmnopqrstuvwxyz"));
        assert!(!content.contains("auth-token-value"));
        assert!(!content.contains("usr_raw"));
            assert!(content.contains("[REDACTED]"));
        });
    }

    #[test]
    fn byte_bounded_queue_rejects_an_event_larger_than_available_permits() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let logger = AuditLogger::new(
                LogConfig {
                    root: directory.path().to_path_buf(),
                    queue_capacity: 8,
                    queue_max_bytes: 64,
                    console_param_bytes: 64,
                    console_result_bytes: 64,
                    file_event_bytes: 4096,
                    max_file_bytes: 1024 * 1024,
                    max_files: 1,
                },
                "secret-token".into(),
            )
            .await
            .unwrap();
            logger.emit(json!({"event":"tool_result","result":"x".repeat(1024)}));
            assert_eq!(logger.dropped_count(), 1);
            assert_eq!(logger.queue_bytes_available(), 64);
            logger.shutdown().await;
        });
    }
}
