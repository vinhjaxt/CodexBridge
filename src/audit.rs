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
const CONSOLE_EXCERPT_CHARS: usize = 500;
const CONSOLE_EXCERPT_EDGE_CHARS: usize = 250;
const CONSOLE_PRETTY_ARRAY_ITEMS: usize = 128;
const CONSOLE_PLAN_BODY_CHARS: usize = 450;
const CONSOLE_PLAN_STEP_CHARS: usize = 48;
const CONSOLE_PLAN_STEP_EDGE_CHARS: usize = 24;

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

fn console_safe_inline(value: &str) -> String {
    let mut safe = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\n' => safe.push_str("\\n"),
            '\r' => safe.push_str("\\r"),
            '\t' => safe.push_str("\\t"),
            '\0' => safe.push_str("\\0"),
            '\u{1b}' => safe.push_str("\\x1b"),
            character if character.is_control() => {
                safe.push_str(&format!("\\u{{{:x}}}", character as u32));
            }
            character => safe.push(character),
        }
    }
    safe
}

fn console_excerpt(value: &str) -> String {
    console_excerpt_edges(value, CONSOLE_EXCERPT_CHARS, CONSOLE_EXCERPT_EDGE_CHARS)
}

fn console_excerpt_edges(value: &str, maximum: usize, edge: usize) -> String {
    if value.chars().nth(maximum).is_none() {
        return value.to_owned();
    }

    let head: String = value.chars().take(edge).collect();
    let tail: String = value
        .chars()
        .rev()
        .take(edge)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}...{tail}")
}

fn console_text(value: &str) -> String {
    let bounded = console_excerpt(value);
    console_excerpt(&console_safe_inline(&bounded))
}

fn console_redacted_excerpt(value: &str, auth_token: &str) -> String {
    if value.chars().nth(CONSOLE_EXCERPT_CHARS).is_none() {
        return if auth_token.is_empty() {
            value.to_owned()
        } else {
            value.replace(auth_token, "[REDACTED]")
        };
    }

    let overlap = auth_token.chars().count();
    let edge = CONSOLE_EXCERPT_EDGE_CHARS.saturating_add(overlap);
    let head = value.chars().take(edge).collect::<String>();
    let tail = value
        .chars()
        .rev()
        .take(edge)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    let head = if auth_token.is_empty() {
        head
    } else {
        head.replace(auth_token, "[REDACTED]")
    };
    let tail = if auth_token.is_empty() {
        tail
    } else {
        tail.replace(auth_token, "[REDACTED]")
    };
    console_excerpt(&format!("{head}...{tail}"))
}

fn console_scrub_pretty(value: &Value, auth_token: &str) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    if is_sensitive_key(key) {
                        (key.clone(), Value::String("[REDACTED]".to_owned()))
                    } else {
                        (key.clone(), console_scrub_pretty(value, auth_token))
                    }
                })
                .collect(),
        ),
        Value::Array(array) => {
            let shown = array.len().min(CONSOLE_PRETTY_ARRAY_ITEMS);
            let mut values = array
                .iter()
                .take(shown)
                .map(|value| console_scrub_pretty(value, auth_token))
                .collect::<Vec<_>>();
            if array.len() > shown {
                values.push(json!({
                    "truncated": true,
                    "original_items": array.len(),
                    "shown_items": shown,
                }));
            }
            Value::Array(values)
        }
        Value::String(value) => Value::String(console_redacted_excerpt(value, auth_token)),
        other => other.clone(),
    }
}

fn console_string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(console_text)
}

fn console_optional_string_field(value: &Value, key: &str) -> Option<String> {
    match value.get(key) {
        Some(Value::String(value)) => Some(console_text(value)),
        Some(Value::Null) | None => None,
        _ => None,
    }
}

fn console_u64_field(value: &Value, key: &str) -> Option<u64> {
    value.get(key)?.as_u64()
}

fn console_bool_field(value: &Value, key: &str) -> Option<bool> {
    value.get(key)?.as_bool()
}

fn console_human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes_f = bytes as f64;
    if bytes_f >= GIB {
        format!("{:.1} GiB", bytes_f / GIB)
    } else if bytes_f >= MIB {
        format!("{:.1} MiB", bytes_f / MIB)
    } else if bytes_f >= KIB {
        format!("{:.1} KiB", bytes_f / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn console_join_fields(fields: Vec<Option<String>>) -> String {
    fields.into_iter().flatten().collect::<Vec<_>>().join("  ")
}

fn console_list_lines<'a>(
    values: impl Iterator<Item = &'a Value>,
    limit: usize,
    mut render: impl FnMut(&Value) -> Option<String>,
) -> String {
    let values = values.collect::<Vec<_>>();
    let total = values.len();
    let mut lines = values
        .into_iter()
        .take(limit)
        .filter_map(&mut render)
        .map(|line| format!("\n    {line}"))
        .collect::<String>();
    if total > limit {
        lines.push_str(&format!("\n    ... +{} more", total - limit));
    }
    lines
}

fn console_patch_targets(input: &str) -> Vec<String> {
    input
        .lines()
        .filter_map(|line| {
            [
                ("*** Add File: ", "+"),
                ("*** Update File: ", "~"),
                ("*** Delete File: ", "-"),
                ("*** Move to: ", "→"),
            ]
            .into_iter()
            .find_map(|(prefix, marker)| {
                line.strip_prefix(prefix)
                    .map(|path| format!("{marker} {}", console_text(path)))
            })
        })
        .collect()
}

fn console_render_tool_call(tool: &str, params: &Value) -> Option<String> {
    match tool {
        "chatgpt_turn_init" => Some(console_join_fields(vec![
            console_optional_string_field(params, "project_key")
                .map(|value| format!("project={value}")),
            Some(format!(
                "previous_turn_ref={}",
                console_bool_field(params, "previous_turn_ref_present")?
            )),
        ])),
        "apply_patch" => {
            let input = params.get("input")?.as_str()?;
            let targets = console_patch_targets(input);
            let mut rendered = "patch".to_owned();
            if !targets.is_empty() {
                for target in targets.iter().take(6) {
                    rendered.push_str(&format!("\n    {target}"));
                }
                if targets.len() > 6 {
                    rendered.push_str(&format!("\n    ... +{} more", targets.len() - 6));
                }
            }
            Some(rendered)
        }
        "read_file" => {
            let path = console_string_field(params, "path")?;
            Some(console_join_fields(vec![
                Some(path),
                console_u64_field(params, "offset").map(|value| format!("line={}", value + 1)),
                console_u64_field(params, "line_byte_offset")
                    .filter(|value| *value != 0)
                    .map(|value| format!("byte={value}")),
                console_u64_field(params, "limit").map(|value| format!("limit={value}")),
                console_u64_field(params, "max_bytes")
                    .map(|value| format!("max={}", console_human_bytes(value))),
            ]))
        }
        "list_directory" => Some(console_join_fields(vec![
            Some(console_optional_string_field(params, "path").unwrap_or_else(|| ".".to_owned())),
            console_u64_field(params, "offset")
                .filter(|value| *value != 0)
                .map(|value| format!("offset={value}")),
            console_u64_field(params, "max_results").map(|value| format!("max={value}")),
        ])),
        "tree" => Some(console_join_fields(vec![
            Some(console_optional_string_field(params, "path").unwrap_or_else(|| ".".to_owned())),
            console_u64_field(params, "max_depth").map(|value| format!("depth={value}")),
            console_u64_field(params, "offset")
                .filter(|value| *value != 0)
                .map(|value| format!("offset={value}")),
            console_u64_field(params, "max_entries").map(|value| format!("max={value}")),
        ])),
        "glob" => {
            let pattern = console_string_field(params, "pattern")?;
            Some(console_join_fields(vec![
                Some(format!("pattern={pattern}")),
                Some(format!(
                    "path={}",
                    console_optional_string_field(params, "path").unwrap_or_else(|| ".".to_owned())
                )),
                console_u64_field(params, "offset")
                    .filter(|value| *value != 0)
                    .map(|value| format!("offset={value}")),
            ]))
        }
        "grep" => {
            let query = console_string_field(params, "query")?;
            Some(console_join_fields(vec![
                Some(format!("query={query}")),
                Some(format!(
                    "path={}",
                    console_optional_string_field(params, "path").unwrap_or_else(|| ".".to_owned())
                )),
                console_optional_string_field(params, "include")
                    .map(|value| format!("include={value}")),
                console_u64_field(params, "context")
                    .filter(|value| *value != 0)
                    .map(|value| format!("context={value}")),
                console_bool_field(params, "files_only")
                    .filter(|value| *value)
                    .map(|_| "files_only".to_owned()),
            ]))
        }
        "view_image" => console_string_field(params, "path"),
        "exec_command" => {
            let command = console_string_field(params, "command")?;
            Some(console_join_fields(vec![
                Some(format!("$ {command}")),
                console_optional_string_field(params, "workdir")
                    .map(|value| format!("cwd={value}")),
                console_u64_field(params, "timeout_ms").map(|value| format!("timeout={value}ms")),
                console_bool_field(params, "tty")
                    .filter(|value| *value)
                    .map(|_| "tty".to_owned()),
                params
                    .get("stdin")
                    .filter(|value| !value.is_null())
                    .map(|_| "stdin=<provided>".to_owned()),
            ]))
        }
        "write_stdin" => {
            let session = console_string_field(params, "session_id")?;
            let chars = params
                .get("chars")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(|_| "input=<provided>".to_owned());
            Some(console_join_fields(vec![
                Some(format!("session={session}")),
                chars,
                console_optional_string_field(params, "signal")
                    .map(|value| format!("signal={value}")),
                console_bool_field(params, "close_stdin")
                    .filter(|value| *value)
                    .map(|_| "close_stdin".to_owned()),
                console_u64_field(params, "since_output_offset")
                    .map(|value| format!("since={value}")),
                console_u64_field(params, "wait_for_exit_ms")
                    .map(|value| format!("wait={value}ms")),
            ]))
        }
        "skills_list" => Some(format!(
            "path={}",
            console_optional_string_field(params, "path").unwrap_or_else(|| ".".to_owned())
        )),
        "skills_read" => {
            let name = console_string_field(params, "name")?;
            Some(console_join_fields(vec![
                Some(format!("skill={name}")),
                Some(format!(
                    "resource={}",
                    console_optional_string_field(params, "resource")
                        .unwrap_or_else(|| "SKILL.md".to_owned())
                )),
                console_u64_field(params, "offset")
                    .filter(|value| *value != 0)
                    .map(|value| format!("offset={value}")),
                console_u64_field(params, "limit").map(|value| format!("limit={value}")),
            ]))
        }
        "remember" => {
            let key = console_string_field(params, "key")?;
            let operation = match params.get("value") {
                Some(Value::String(value)) if value.is_empty() => "delete",
                Some(_) => "set",
                None => return None,
            };
            Some(format!("{operation} {key}"))
        }
        "recall" => Some(console_join_fields(vec![
            console_optional_string_field(params, "key")
                .map(|value| format!("key={value}"))
                .or_else(|| Some("memory page".to_owned())),
            console_u64_field(params, "offset")
                .filter(|value| *value != 0)
                .map(|value| format!("offset={value}")),
            console_u64_field(params, "max_results").map(|value| format!("max={value}")),
            console_bool_field(params, "include_plan")
                .filter(|value| *value)
                .map(|_| "include_plan".to_owned()),
        ])),
        "update_plan" => Some(String::new()),
        _ => None,
    }
}

fn console_render_process_result(result: &Value) -> Option<String> {
    let reason = console_string_field(result, "completion_reason")?;
    let mut fields = vec![Some(reason)];
    if let Some(exit_code) = result.get("exit_code").and_then(Value::as_i64) {
        fields.push(Some(format!("exit={exit_code}")));
    }
    if let Some(seconds) = result.get("wall_time_seconds").and_then(Value::as_f64) {
        fields.push(Some(format!("{seconds:.2}s")));
    }
    if let Some(session) = console_optional_string_field(result, "session_id") {
        fields.push(Some(format!("session={session}")));
    }
    if console_bool_field(result, "truncated") == Some(true) {
        fields.push(Some("truncated".to_owned()));
    }
    if let Some(output) = console_optional_string_field(result, "output")
        && !output.is_empty()
    {
        fields.push(Some(format!("output={output}")));
    }
    Some(console_join_fields(fields))
}

fn console_render_tool_result(tool: &str, result: &Value) -> Option<String> {
    match tool {
        "chatgpt_turn_init" => {
            let status = console_string_field(result, "status")?;
            if status == "soft_error" {
                let code = result
                    .get("soft_error")
                    .and_then(|value| console_string_field(value, "code"))
                    .unwrap_or_else(|| "soft_error".to_owned());
                return Some(format!("{status} {code}"));
            }
            Some(console_join_fields(vec![
                Some(status),
                console_optional_string_field(result, "project_key")
                    .map(|value| format!("project={value}")),
                console_optional_string_field(result, "workspace_state")
                    .map(|value| format!("workspace={value}")),
                console_bool_field(result, "instructions_changed")
                    .map(|value| format!("instructions_changed={value}")),
                console_bool_field(result, "state_changed")
                    .map(|value| format!("state_changed={value}")),
                console_optional_string_field(result, "turn_ref")
                    .map(|value| format!("turn={value}")),
            ]))
        }
        "apply_patch" => {
            let applied = console_bool_field(result, "applied")?;
            let count = console_u64_field(result, "count")?;
            let mut rendered = format!(
                "{} {count} file(s)",
                if applied { "applied" } else { "not applied" }
            );
            if let Some(files) = result.get("files").and_then(Value::as_array) {
                rendered.push_str(&console_list_lines(files.iter(), 8, |file| {
                    let path = console_string_field(file, "path")?;
                    let operation = console_string_field(file, "operation")?;
                    let marker = match operation.as_str() {
                        "create" => "+",
                        "delete" => "-",
                        "move" => "→",
                        _ => "~",
                    };
                    let destination = console_optional_string_field(file, "destination")
                        .map(|destination| format!(" -> {destination}"))
                        .unwrap_or_default();
                    Some(format!("{marker} {path}{destination}"))
                }));
            }
            Some(rendered)
        }
        "read_file" => {
            let path = console_string_field(result, "path")?;
            let bytes = console_u64_field(result, "bytes")?;
            let total_lines = console_u64_field(result, "total_lines")?;
            let shown_lines = console_u64_field(result, "shown_lines")?;
            let offset = console_u64_field(result, "offset")?;
            let mut rendered = format!(
                "{path}  {}  lines={}  window={}..{}",
                console_human_bytes(bytes),
                total_lines,
                offset + 1,
                offset.saturating_add(shown_lines)
            );
            if console_bool_field(result, "truncated") == Some(true) {
                rendered.push_str("  truncated");
            }
            if let Some(content) = console_optional_string_field(result, "content")
                && !content.is_empty()
            {
                rendered.push_str(&format!("  content={content}"));
            }
            Some(rendered)
        }
        "list_directory" => {
            let path = console_string_field(result, "path")?;
            let count = console_u64_field(result, "count")?;
            let mut rendered = format!(
                "{path}  {count} entr{}",
                if count == 1 { "y" } else { "ies" }
            );
            if let Some(entries) = result.get("entries").and_then(Value::as_array) {
                rendered.push_str(&console_list_lines(entries.iter(), 8, |entry| {
                    let name = console_string_field(entry, "name")?;
                    let kind = console_string_field(entry, "type")?;
                    let bytes = console_u64_field(entry, "bytes")
                        .map(console_human_bytes)
                        .unwrap_or_else(|| "-".to_owned());
                    Some(format!("{kind:<9} {bytes:>9}  {name}"))
                }));
            }
            Some(rendered)
        }
        "tree" => {
            let path = console_string_field(result, "path")?;
            let entries = result.get("entries")?.as_array()?;
            let mut rendered = format!("{path}  {} entries", entries.len());
            rendered.push_str(&console_list_lines(entries.iter(), 10, |entry| {
                let item_path = console_string_field(entry, "path")?;
                let kind = console_string_field(entry, "type")?;
                let depth = console_u64_field(entry, "depth").unwrap_or(0).min(12) as usize;
                let suffix = match kind.as_str() {
                    "directory" => "/",
                    "symlink" => "@",
                    _ => "",
                };
                Some(format!(
                    "{}{}{}",
                    "  ".repeat(depth.saturating_sub(1)),
                    item_path,
                    suffix
                ))
            }));
            Some(rendered)
        }
        "glob" => {
            let paths = result.get("paths")?.as_array()?;
            let count = console_u64_field(result, "count").unwrap_or(paths.len() as u64);
            let mut rendered = format!("{count} match(es)");
            rendered.push_str(&console_list_lines(paths.iter(), 10, |path| {
                path.as_str().map(console_text)
            }));
            Some(rendered)
        }
        "grep" => {
            let matches = result.get("matches")?.as_array()?;
            let count = console_u64_field(result, "count").unwrap_or(matches.len() as u64);
            let mut rendered = format!("{count} match(es)");
            if console_bool_field(result, "incomplete") == Some(true) {
                rendered.push_str("  incomplete");
            }
            rendered.push_str(&console_list_lines(matches.iter(), 8, |item| {
                let path = console_string_field(item, "path")?;
                let line = console_u64_field(item, "line")
                    .map(|line| format!(":{line}"))
                    .unwrap_or_default();
                let text = console_optional_string_field(item, "text")
                    .map(|text| format!("  {text}"))
                    .unwrap_or_default();
                Some(format!("{path}{line}{text}"))
            }));
            Some(rendered)
        }
        "view_image" => {
            let path = console_string_field(result, "path")?;
            let mime = console_string_field(result, "mime_type")?;
            let bytes = console_u64_field(result, "bytes")?;
            Some(console_join_fields(vec![
                Some(path),
                Some(mime),
                Some(console_human_bytes(bytes)),
            ]))
        }
        "exec_command" | "write_stdin" => console_render_process_result(result),
        "skills_list" => {
            let skills = result.get("skills")?.as_array()?;
            let mut rendered = format!("{} skill(s)", skills.len());
            rendered.push_str(&console_list_lines(skills.iter(), 8, |skill| {
                let name = console_string_field(skill, "name")?;
                let description = console_optional_string_field(skill, "description")
                    .map(|value| format!(" — {value}"))
                    .unwrap_or_default();
                Some(format!("{name}{description}"))
            }));
            Some(rendered)
        }
        "skills_read" => {
            let name = console_string_field(result, "name")?;
            let resource = console_string_field(result, "resource")?;
            let shown = console_u64_field(result, "shown_bytes")?;
            let total = console_u64_field(result, "total_bytes")?;
            let truncated = console_bool_field(result, "truncated")?;
            Some(console_join_fields(vec![
                Some(format!("skill={name}")),
                Some(format!("resource={resource}")),
                Some(format!("shown={}", console_human_bytes(shown))),
                Some(format!("total={}", console_human_bytes(total))),
                truncated.then(|| "continued".to_owned()),
            ]))
        }
        "remember" => {
            let key = console_string_field(result, "key")?;
            let saved = console_bool_field(result, "saved")?;
            let deleted = console_bool_field(result, "deleted")?;
            let state = if saved {
                "saved"
            } else if deleted {
                "deleted"
            } else {
                "unchanged"
            };
            Some(format!("{state} {key}"))
        }
        "recall" => {
            if let Some(key) = console_optional_string_field(result, "key") {
                let value = result.get("value")?;
                let found = !value.is_null();
                return Some(format!(
                    "key={key}  {}",
                    if found { "found" } else { "missing" }
                ));
            }
            let notes = result.get("notes")?.as_array()?;
            let total = console_u64_field(result, "total").unwrap_or(notes.len() as u64);
            let offset = console_u64_field(result, "offset").unwrap_or(0);
            let mut rendered = format!("{} memory note(s)  offset={offset}/{total}", notes.len());
            rendered.push_str(&console_list_lines(notes.iter(), 10, |note| {
                console_string_field(note, "key")
            }));
            if let Some(plan) = result.get("plan").filter(|value| !value.is_null()) {
                let tasks = plan
                    .get("items")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0);
                rendered.push_str(&format!("  plan={tasks} task(s)"));
            }
            Some(rendered)
        }
        "update_plan" => Some(String::new()),
        _ => None,
    }
}

fn console_plan(event: &Value) -> Option<String> {
    let plan = event.get("plan")?;
    if plan.is_null() {
        return Some(format!("    {}", console_color("plan cleared", ANSI_GRAY)));
    }
    let items = plan.get("items")?.as_array()?;
    if items.is_empty() {
        return Some(format!("    {}", console_color("plan empty", ANSI_GRAY)));
    }

    let rendered = items
        .iter()
        .filter_map(|item| {
            let step = console_excerpt_edges(
                &console_string_field(item, "step")?,
                CONSOLE_PLAN_STEP_CHARS,
                CONSOLE_PLAN_STEP_EDGE_CHARS,
            );
            let status = console_string_field(item, "status")?;
            let color = match status.as_str() {
                "completed" => ANSI_GREEN,
                "in_progress" => ANSI_YELLOW,
                "pending" => ANSI_GRAY,
                _ => ANSI_RED,
            };
            Some((step, status, color))
        })
        .collect::<Vec<_>>();
    if rendered.is_empty() {
        return None;
    }
    let step_width = rendered
        .iter()
        .map(|(step, _, _)| step.chars().count())
        .max()
        .unwrap_or_default();
    let total = rendered.len();
    let mut lines = Vec::new();
    let mut visible_chars = 0usize;
    for (step, status, color) in rendered {
        let line_chars = 4usize
            .saturating_add(step_width)
            .saturating_add(4)
            .saturating_add(status.chars().count())
            .saturating_add(usize::from(!lines.is_empty()));
        if visible_chars.saturating_add(line_chars) > CONSOLE_PLAN_BODY_CHARS {
            break;
        }
        visible_chars = visible_chars.saturating_add(line_chars);
        lines.push(format!(
            "    {step:<step_width$}    {}",
            console_color(&status, color)
        ));
    }
    if lines.len() < total {
        lines.push(format!("    ... +{} more", total - lines.len()));
    }
    Some(lines.join("\n"))
}

fn console_project(inner: &AuditInner, event: &Value) -> String {
    let project = event
        .get("project")
        .and_then(Value::as_object)
        .and_then(|project| {
            project
                .get("alias")
                .and_then(Value::as_str)
                .or_else(|| project.get("effective_key").and_then(Value::as_str))
        })
        .unwrap_or("global");
    let project = if inner.auth_token.is_empty() {
        project.to_owned()
    } else {
        project.replace(&inner.auth_token, "[REDACTED]")
    };
    console_text(&project)
}

fn console_generic_payload(
    inner: &AuditInner,
    event: &Value,
    key: &str,
    string_limit: usize,
) -> String {
    let value = event.get(key).cloned().unwrap_or(Value::Null);
    let value = scrub(&value, &inner.auth_token, string_limit);
    let value = bound_event(value, string_limit.saturating_mul(4));
    let rendered = serde_json::to_string(&value)
        .unwrap_or_else(|_| "{\"serialization_error\":true}".to_owned());
    console_excerpt(&rendered)
}

fn console_pretty_payload(inner: &AuditInner, event: &Value, key: &str) -> Value {
    event
        .get(key)
        .map(|value| console_scrub_pretty(value, &inner.auth_token))
        .unwrap_or(Value::Null)
}

fn console_line(inner: &AuditInner, event: &Value) -> Option<String> {
    let event_name = event.get("event").and_then(Value::as_str)?;
    let project = console_project(inner, event);
    let tool = event
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    match event_name {
        "tool_call" => {
            let params = console_pretty_payload(inner, event, "params");
            let rendered = console_render_tool_call(tool, &params)
                .map(|rendered| console_excerpt(&rendered))
                .unwrap_or_else(|| {
                    console_generic_payload(
                        inner,
                        event,
                        "params",
                        inner.config.console_param_bytes,
                    )
                });
            Some(format!(
                "[{}] {} {}{}",
                console_color(&project, ANSI_GREEN),
                console_color("->", ANSI_GRAY),
                console_color(tool, ANSI_RED),
                if rendered.is_empty() {
                    String::new()
                } else {
                    format!(" {rendered}")
                }
            ))
        }
        "tool_result" => {
            let result = console_pretty_payload(inner, event, "result");
            let rendered = console_render_tool_result(tool, &result)
                .map(|rendered| console_excerpt(&rendered))
                .unwrap_or_else(|| {
                    console_generic_payload(
                        inner,
                        event,
                        "result",
                        inner.config.console_result_bytes,
                    )
                });
            Some(format!(
                "[{}] {} {}{}",
                console_color(&project, ANSI_BLUE),
                console_color("<-", ANSI_GRAY),
                console_color(tool, ANSI_YELLOW),
                if rendered.is_empty() {
                    String::new()
                } else {
                    format!(" {rendered}")
                }
            ))
        }
        "tool_error" | "tool_timeout" => {
            let error = console_pretty_payload(inner, event, "error");
            let rendered = console_string_field(&error, "message")
                .map(|message| {
                    let code = console_optional_string_field(&error, "code")
                        .map(|code| format!("{code}: "))
                        .unwrap_or_default();
                    format!("{code}{message}")
                })
                .unwrap_or_else(|| {
                    console_generic_payload(
                        inner,
                        event,
                        "error",
                        inner.config.console_result_bytes,
                    )
                });
            let rendered = console_excerpt(&rendered);
            Some(format!(
                "[{}] {} {} {event_name}: {rendered}",
                console_color(&project, ANSI_BLUE),
                console_color("<-", ANSI_GRAY),
                console_color(tool, ANSI_YELLOW)
            ))
        }
        "plan_updated" => {
            let safe = console_scrub_pretty(event, &inner.auth_token);
            console_plan(&safe)
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

    fn console_test_inner() -> AuditInner {
        let config = LogConfig {
            root: PathBuf::from("unused"),
            queue_capacity: 8,
            queue_max_bytes: 64 * 1024,
            console_param_bytes: 4096,
            console_result_bytes: 8192,
            file_event_bytes: 4096,
            max_file_bytes: 1024 * 1024,
            max_files: 1,
        };
        let (sender, _receiver) = mpsc::channel(1);
        AuditInner {
            sender,
            queue_bytes: Arc::new(Semaphore::new(64 * 1024)),
            config,
            auth_token: "bridge-secret".to_owned(),
            dropped_total: AtomicU64::new(0),
            dropped_pending: AtomicU64::new(0),
            running: DashMap::new(),
            project_activity: DashMap::new(),
            project_activity_lock: Mutex::new(()),
            cancellation: CancellationToken::new(),
            writer: Mutex::new(None),
        }
    }

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
            "[\u{1b}[32mdemo\u{1b}[0m] \u{1b}[90m->\u{1b}[0m \u{1b}[31mgrep\u{1b}[0m query=needle  path=."
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
            "[\u{1b}[34mdemo\u{1b}[0m] \u{1b}[90m<-\u{1b}[0m \u{1b}[33mgrep\u{1b}[0m {\"matches\":3,\"token\":\"[REDACTED]\"}"
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
            "[\u{1b}[32meffective-project\u{1b}[0m] \u{1b}[90m->\u{1b}[0m \u{1b}[31mread_file\u{1b}[0m src/lib.rs"
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
            "[\u{1b}[34mdemo\u{1b}[0m] \u{1b}[90m<-\u{1b}[0m \u{1b}[33mexec_command\u{1b}[0m tool_error: PROCESS_FAILED: [REDACTED] failed"
        );
    }

    #[test]
    fn console_excerpt_keeps_250_leading_and_trailing_characters() {
        let value = format!(
            "{}{}{}",
            "a".repeat(250),
            "middle".repeat(32),
            "z".repeat(250)
        );
        let excerpt = console_excerpt(&value);

        assert_eq!(
            excerpt,
            format!("{}...{}", "a".repeat(250), "z".repeat(250))
        );
        assert_eq!(excerpt.chars().count(), 503);

        let unicode = format!("{}x{}", "🙂".repeat(250), "界".repeat(250));
        let unicode_excerpt = console_excerpt(&unicode);
        assert_eq!(
            unicode_excerpt,
            format!("{}...{}", "🙂".repeat(250), "界".repeat(250))
        );
    }

    #[test]
    fn console_plan_renders_one_colored_status_per_task() {
        let rendered = console_plan(&json!({
            "event":"plan_updated",
            "project":{"alias":"demo"},
            "plan":{
                "items":[
                    {"step":"compile","status":"in_progress"},
                    {"step":"test","status":"pending"},
                    {"step":"ship","status":"completed"}
                ]
            }
        }))
        .unwrap();

        assert_eq!(
            rendered,
            "    compile    \u{1b}[33min_progress\u{1b}[0m\n    test       \u{1b}[90mpending\u{1b}[0m\n    ship       \u{1b}[32mcompleted\u{1b}[0m"
        );
    }

    #[test]
    fn console_plan_has_one_bounded_visible_budget_for_large_valid_plans() {
        let items = (0..100)
            .map(|index| {
                json!({
                    "step": format!("task-{index:03}-{}", "x".repeat(180)),
                    "status": if index == 0 { "in_progress" } else { "pending" },
                })
            })
            .collect::<Vec<_>>();
        let rendered = console_plan(&json!({"plan":{"items":items}})).unwrap();
        let plain = [
            ANSI_RESET,
            ANSI_GRAY,
            ANSI_RED,
            ANSI_GREEN,
            ANSI_YELLOW,
            ANSI_BLUE,
        ]
        .into_iter()
        .fold(rendered.clone(), |value, code| value.replace(code, ""));

        assert!(
            plain.chars().count() <= CONSOLE_EXCERPT_CHARS,
            "large plan escaped console budget: {} chars\n{plain}",
            plain.chars().count()
        );
        assert!(
            plain.contains("... +"),
            "omitted task count missing: {plain}"
        );
        assert!(plain.lines().count() < 100, "large plan was dumped in full");
    }

    #[test]
    fn console_pretty_scrub_bounds_large_strings_before_custom_rendering() {
        let inner = console_test_inner();
        let payload = format!(
            "{}bridge-secret{}{}",
            "a".repeat(240),
            "middle".repeat(200_000),
            "z".repeat(300)
        );
        let event = json!({"result":{"output":payload}});
        let safe = console_pretty_payload(&inner, &event, "result");
        let output = safe["output"].as_str().unwrap();

        assert!(output.chars().count() <= 503, "{output}");
        assert!(output.contains("[REDACTED]"), "{output}");
        assert!(!output.contains("bridge-secret"), "{output}");
        assert!(output.ends_with(&"z".repeat(250)), "{output}");
        assert!(!output.contains("middlemiddlemiddle"), "{output}");
    }

    #[test]
    fn recall_plan_summary_does_not_embed_nested_ansi_or_dump_plan_lines() {
        let rendered = console_render_tool_result(
            "recall",
            &json!({
                "notes":[],"offset":0,"total":0,
                "plan":{"items":[{"step":"compile","status":"in_progress"}]}
            }),
        )
        .unwrap();
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        assert!(rendered.contains("plan=1 task(s)"), "{rendered}");
        assert!(!rendered.contains("compile"), "{rendered}");
    }

    #[test]
    fn update_plan_console_lifecycle_avoids_dumping_plan_json() {
        let config = LogConfig {
            root: PathBuf::from("unused"),
            queue_capacity: 8,
            queue_max_bytes: 1024,
            console_param_bytes: 4096,
            console_result_bytes: 8192,
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

        let call = console_line(
            &inner,
            &json!({
                "event":"tool_call",
                "tool":"update_plan",
                "project":{"alias":"demo"},
                "params":{"plan":[{"step":"compile","status":"in_progress"}]}
            }),
        )
        .unwrap();
        assert_eq!(
            call,
            "[\u{1b}[32mdemo\u{1b}[0m] \u{1b}[90m->\u{1b}[0m \u{1b}[31mupdate_plan\u{1b}[0m"
        );

        let result = console_line(
            &inner,
            &json!({
                "event":"tool_result",
                "tool":"update_plan",
                "project":{"alias":"demo"},
                "result":{"updated":true,"plan":{"items":[{"step":"compile","status":"completed"}]}}
            }),
        )
        .unwrap();
        assert_eq!(
            result,
            "[\u{1b}[34mdemo\u{1b}[0m] \u{1b}[90m<-\u{1b}[0m \u{1b}[33mupdate_plan\u{1b}[0m"
        );
    }

    #[test]
    fn console_line_truncates_large_serialized_payload_in_the_middle() {
        let config = LogConfig {
            root: PathBuf::from("unused"),
            queue_capacity: 8,
            queue_max_bytes: 1024,
            console_param_bytes: 4096,
            console_result_bytes: 4096,
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
        let payload = format!(
            "{}{}{}",
            "a".repeat(300),
            "middle".repeat(50),
            "z".repeat(300)
        );
        let line = console_line(
            &inner,
            &json!({
                "event":"tool_call",
                "tool":"exec_command",
                "project":{"alias":"demo"},
                "params":{"payload":payload}
            }),
        )
        .unwrap();

        assert!(line.contains("..."), "{line}");
        assert!(!line.contains("middlemiddlemiddle"), "{line}");
        assert!(line.contains(&"a".repeat(200)), "{line}");
        assert!(line.contains(&"z".repeat(200)), "{line}");
    }

    #[test]
    fn console_pretty_renderers_cover_every_public_tool_shape() {
        let calls = vec![
            (
                "chatgpt_turn_init",
                json!({"project_key":"demo","previous_turn_ref_present":true}),
            ),
            (
                "apply_patch",
                json!({"input":"*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-old\n+new\n*** End Patch"}),
            ),
            (
                "read_file",
                json!({"path":"src/lib.rs","offset":2,"limit":20}),
            ),
            (
                "list_directory",
                json!({"path":"src","offset":0,"max_results":20}),
            ),
            (
                "tree",
                json!({"path":"src","max_depth":3,"offset":0,"max_entries":30}),
            ),
            ("glob", json!({"pattern":"**/*.rs","path":"src","offset":0})),
            (
                "grep",
                json!({"query":"needle","path":"src","include":"*.rs","context":1,"files_only":false}),
            ),
            ("view_image", json!({"path":"image.png"})),
            (
                "exec_command",
                json!({"command":"cargo test","workdir":".","timeout_ms":30000,"tty":false}),
            ),
            (
                "write_stdin",
                json!({"session_id":"session-1","chars":"x","close_stdin":false,"wait_for_exit_ms":500}),
            ),
            ("skills_list", json!({"path":"src"})),
            (
                "skills_read",
                json!({"name":"pdfs","resource":"SKILL.md","offset":0,"limit":4096}),
            ),
            ("remember", json!({"key":"decision","value":"keep sqlite"})),
            (
                "recall",
                json!({"offset":0,"max_results":10,"include_plan":true}),
            ),
            (
                "update_plan",
                json!({"plan":[{"step":"test","status":"in_progress"}]}),
            ),
        ];
        for (tool, params) in calls {
            let rendered = console_render_tool_call(tool, &params)
                .unwrap_or_else(|| panic!("missing call renderer for {tool}: {params}"));
            assert!(!rendered.starts_with('{'), "{tool}: {rendered}");
        }

        let results = vec![
            (
                "chatgpt_turn_init",
                json!({
                    "status":"synchronized","project_key":"demo","workspace_state":"existing",
                    "instructions_changed":false,"state_changed":true,"turn_ref":"r_demo"
                }),
            ),
            (
                "apply_patch",
                json!({
                    "applied":true,"count":2,
                    "files":[
                        {"path":"src/lib.rs","operation":"update"},
                        {"path":"src/new.rs","operation":"create"}
                    ]
                }),
            ),
            (
                "read_file",
                json!({
                    "path":"src/lib.rs","bytes":1200,"total_lines":40,"shown_lines":10,
                    "offset":5,"truncated":false,"content":"line 6\nline 7"
                }),
            ),
            (
                "list_directory",
                json!({
                    "path":"src","count":2,
                    "entries":[
                        {"name":"lib.rs","type":"file","bytes":1200},
                        {"name":"tools","type":"directory","bytes":null}
                    ]
                }),
            ),
            (
                "tree",
                json!({
                    "path":"src","entries":[
                        {"path":"src/tools","type":"directory","depth":1},
                        {"path":"src/tools/mod.rs","type":"file","depth":2}
                    ]
                }),
            ),
            (
                "glob",
                json!({"count":2,"paths":["src/lib.rs","src/main.rs"]}),
            ),
            (
                "grep",
                json!({
                    "count":1,"incomplete":false,
                    "matches":[{"path":"src/lib.rs","line":7,"text":"needle"}]
                }),
            ),
            (
                "view_image",
                json!({"path":"image.png","mime_type":"image/png","bytes":2048}),
            ),
            (
                "exec_command",
                json!({
                    "completion_reason":"exited","exit_code":0,"wall_time_seconds":1.25,
                    "session_id":null,"truncated":false,"output":"ok"
                }),
            ),
            (
                "write_stdin",
                json!({
                    "completion_reason":"running","exit_code":null,"wall_time_seconds":2.0,
                    "session_id":"session-1","truncated":false,"output":"more"
                }),
            ),
            (
                "skills_list",
                json!({"skills":[{"name":"pdfs","description":"Work with PDFs"}]}),
            ),
            (
                "skills_read",
                json!({
                    "name":"pdfs","resource":"SKILL.md","shown_bytes":4096,
                    "total_bytes":8000,"truncated":true
                }),
            ),
            (
                "remember",
                json!({"key":"decision","saved":true,"deleted":false}),
            ),
            (
                "recall",
                json!({
                    "notes":[{"key":"decision","value":"keep sqlite"}],
                    "offset":0,"total":1,"plan":null
                }),
            ),
            ("update_plan", json!({"updated":true,"plan":null})),
        ];
        for (tool, result) in results {
            let rendered = console_render_tool_result(tool, &result)
                .unwrap_or_else(|| panic!("missing result renderer for {tool}: {result}"));
            assert!(!rendered.starts_with('{'), "{tool}: {rendered}");
        }
    }

    #[test]
    fn console_rendering_is_read_only_and_file_audit_shape_is_unchanged() {
        let inner = console_test_inner();
        let event = json!({
            "event":"tool_result",
            "tool":"exec_command",
            "project":{"alias":"demo","effective_key":"demo"},
            "status":"success",
            "result":{
                "completion_reason":"exited","exit_code":0,"output":"hello",
                "output_bytes":5,"output_offset":0,"output_next_offset":5,"truncated":false
            }
        });
        let original = event.clone();
        let file_before = bound_event(
            scrub(&event, &inner.auth_token, inner.config.file_event_bytes),
            inner.config.file_event_bytes,
        );

        let rendered = console_line(&inner, &event).unwrap();
        assert!(rendered.contains("exited"), "{rendered}");

        let file_after = bound_event(
            scrub(&event, &inner.auth_token, inner.config.file_event_bytes),
            inner.config.file_event_bytes,
        );
        assert_eq!(
            event, original,
            "console rendering mutated the audit/tool value"
        );
        assert_eq!(
            file_before, file_after,
            "console rendering changed the JSON value that the file-audit path would persist"
        );
    }

    #[test]
    fn console_custom_renderers_redact_before_rendering_and_hide_unneeded_values() {
        let inner = console_test_inner();
        let exec = console_line(
            &inner,
            &json!({
                "event":"tool_call",
                "tool":"exec_command",
                "project":{"alias":"demo"},
                "params":{
                    "command":"echo bridge-secret",
                    "env":{"API_TOKEN":"also-secret"},
                    "stdin":"bridge-secret"
                }
            }),
        )
        .unwrap();
        assert!(!exec.contains("bridge-secret"), "{exec}");
        assert!(!exec.contains("also-secret"), "{exec}");
        assert!(exec.contains("[REDACTED]"), "{exec}");
        assert!(exec.contains("stdin=<provided>"), "{exec}");

        let remember = console_line(
            &inner,
            &json!({
                "event":"tool_call",
                "tool":"remember",
                "project":{"alias":"demo"},
                "params":{"key":"decision","value":"bridge-secret private memory"}
            }),
        )
        .unwrap();
        assert!(remember.contains("set decision"), "{remember}");
        assert!(!remember.contains("private memory"), "{remember}");
        assert!(!remember.contains("bridge-secret"), "{remember}");
    }

    #[test]
    fn console_custom_renderers_escape_terminal_control_sequences() {
        let inner = console_test_inner();
        let line = console_line(
            &inner,
            &json!({
                "event":"tool_call",
                "tool":"exec_command",
                "project":{"alias":"demo\u{1b}[2J"},
                "params":{"command":"echo \u{1b}[31mred\nnext\rline\tend"}
            }),
        )
        .unwrap();
        let plain = [
            ANSI_RESET,
            ANSI_GRAY,
            ANSI_RED,
            ANSI_GREEN,
            ANSI_YELLOW,
            ANSI_BLUE,
        ]
        .into_iter()
        .fold(line.clone(), |value, code| value.replace(code, ""));
        assert!(!plain.contains('\u{1b}'), "{line:?}");
        assert!(!plain.contains('\r'), "{line:?}");
        assert_eq!(plain.lines().count(), 1, "{line:?}");
        assert!(plain.contains("demo\\x1b[2J"), "{plain}");
        assert!(
            plain.contains("\\x1b[31mred\\nnext\\rline\\tend"),
            "{plain}"
        );

        let plan = console_line(
            &inner,
            &json!({
                "event":"plan_updated",
                "project":{"alias":"demo"},
                "plan":{"items":[{"step":"task\u{1b}[2J\nspoof","status":"completed"}]}
            }),
        )
        .unwrap();
        let plan_plain = [
            ANSI_RESET,
            ANSI_GRAY,
            ANSI_RED,
            ANSI_GREEN,
            ANSI_YELLOW,
            ANSI_BLUE,
        ]
        .into_iter()
        .fold(plan.clone(), |value, code| value.replace(code, ""));
        assert!(!plan_plain.contains('\u{1b}'), "{plan:?}");
        assert_eq!(plan_plain.lines().count(), 1, "{plan:?}");
        assert!(plan_plain.contains("task\\x1b[2J\\nspoof"), "{plan_plain}");
    }

    #[test]
    fn console_malformed_custom_shapes_fall_back_without_panicking() {
        let inner = console_test_inner();
        for event in [
            json!({
                "event":"tool_result","tool":"exec_command","project":{"alias":"demo"},
                "result":{"completion_reason":123,"output":"x"}
            }),
            json!({
                "event":"tool_result","tool":"grep","project":{"alias":"demo"},
                "result":{"matches":3,"count":3}
            }),
            json!({
                "event":"tool_call","tool":"read_file","project":{"alias":"demo"},
                "params":{"path":123}
            }),
        ] {
            let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                console_line(&inner, &event)
            }))
            .expect("console rendering must never panic")
            .expect("tool lifecycle event should remain visible");
            assert!(rendered.contains('{'), "expected JSON fallback: {rendered}");
        }
    }

    #[test]
    fn console_process_output_keeps_head_and_tail_after_pretty_rendering() {
        let inner = console_test_inner();
        let output = format!(
            "{}{}{}",
            "a".repeat(300),
            "middle".repeat(60),
            "z".repeat(300)
        );
        let line = console_line(
            &inner,
            &json!({
                "event":"tool_result",
                "tool":"exec_command",
                "project":{"alias":"demo"},
                "result":{
                    "completion_reason":"exited","exit_code":0,"wall_time_seconds":0.1,
                    "session_id":null,"truncated":false,"output":output
                }
            }),
        )
        .unwrap();
        assert!(line.contains(&"a".repeat(200)), "{line}");
        assert!(line.contains(&"z".repeat(200)), "{line}");
        assert!(line.contains("..."), "{line}");
        assert!(!line.contains("middlemiddlemiddle"), "{line}");
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
