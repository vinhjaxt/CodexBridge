use std::{
    collections::BTreeMap,
    io::{Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use dashmap::DashMap;
use portable_pty::MasterPty;
use rmcp::{
    ErrorData, handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::ChildStdin,
    sync::{Mutex as AsyncMutex, Notify},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod output;
mod pty;
use output::{OutputBuffer, token_window};
use pty::{PtyProcess, pty_size, spawn_pty_process, terminal_dimensions, wait_pty_process};

use super::AgentHandler;
use crate::{
    error::{AppError, Result as AppResult},
    project::ProjectContext,
    request_context::ProjectRequestContext,
    sandbox::{PathOperation, build_command_with_options},
};

const MIN_YIELD_MS: u64 = 250;
const MAX_YIELD_MS: u64 = 30_000;
// Keep the initial MCP request comfortably below common client/proxy request
// deadlines. Long-running commands remain resident and are continued with
// write_stdin instead of risking a transport-level timeout at the boundary.
const MAX_INITIAL_YIELD_MS: u64 = 20_000;
const MAX_POLL_YIELD_MS: u64 = 300_000;
const TIMEOUT_COMPLETION_GRACE: Duration = Duration::from_secs(1);

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ExecCommandArgs {
    #[serde(alias = "cmd")]
    pub command: String,
    #[serde(default)]
    pub workdir: Option<String>,
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub yield_time_ms: Option<u64>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub max_output_tokens: Option<usize>,
    /// Optional one-shot stdin payload written immediately after spawn.
    #[serde(default)]
    pub stdin: Option<String>,
    /// Close stdin after the optional one-shot payload. This is useful for
    /// non-interactive CLIs that read until EOF, including subagent wrappers.
    #[serde(default)]
    pub close_stdin: bool,
    /// Allocate a native pseudo-terminal (Unix PTY or Windows ConPTY). Use this for REPLs,
    /// password prompts, line editors, and full-screen terminal applications.
    #[serde(default)]
    pub tty: bool,
    #[serde(default)]
    pub rows: Option<u16>,
    #[serde(default)]
    pub cols: Option<u16>,
    /// Forward-compatible optional arguments. Typed top-level fields remain preferred;
    /// newer servers may consume additional keys here without requiring clients to
    /// refresh their top-level tool schema first.
    #[serde(default)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct WriteStdinArgs {
    pub session_id: String,
    #[serde(default)]
    pub chars: String,
    #[serde(default)]
    pub yield_time_ms: Option<u64>,
    #[serde(default)]
    pub close_stdin: bool,
    #[serde(default)]
    pub max_output_tokens: Option<usize>,
    /// Byte offset into the process output stream where rendering should
    /// start. Defaults to just after the last byte returned to you for this
    /// session. Pass a previously returned output_offset value (including
    /// offsets from finished sessions) to re-read buffered history when a
    /// response was lost; bytes already evicted from the bounded buffer are
    /// disclosed with an omission marker instead of being skipped silently.
    #[serde(default)]
    pub since_output_offset: Option<usize>,
    /// Resize a PTY session before writing/polling. Both rows and cols are required together.
    #[serde(default)]
    pub rows: Option<u16>,
    #[serde(default)]
    pub cols: Option<u16>,
    /// Deliver a bounded process-control signal. `interrupt` maps to Ctrl-C/SIGINT,
    /// `terminate` requests graceful termination, and `kill` forcefully ends the tree.
    #[serde(default)]
    pub signal: Option<ProcessSignal>,
    /// Explicitly wait this long for terminal process completion after any
    /// input/EOF/signal action, draining final output in the same tool call.
    #[serde(default)]
    pub wait_for_exit_ms: Option<u64>,
    /// Forward-compatible optional arguments. Typed top-level fields remain preferred;
    /// newer servers may consume additional keys here without requiring clients to
    /// refresh their top-level tool schema first.
    #[serde(default)]
    pub extensions: BTreeMap<String, Value>,
}

fn effective_exec_stdin(args: &ExecCommandArgs) -> AppResult<Option<String>> {
    if let Some(stdin) = args.stdin.as_ref() {
        return Ok(Some(stdin.clone()));
    }
    super::extension_arg(&args.extensions, "stdin")
}

fn effective_exec_close_stdin(args: &ExecCommandArgs) -> AppResult<bool> {
    if args.close_stdin {
        return Ok(true);
    }
    Ok(super::extension_arg::<bool>(&args.extensions, "close_stdin")?.unwrap_or(false))
}

fn effective_since_output_offset(args: &WriteStdinArgs) -> AppResult<Option<usize>> {
    if args.since_output_offset.is_some() {
        return Ok(args.since_output_offset);
    }
    super::extension_arg(&args.extensions, "since_output_offset")
}

fn effective_wait_for_exit_ms(args: &WriteStdinArgs) -> AppResult<Option<u64>> {
    if args.wait_for_exit_ms.is_some() {
        return Ok(args.wait_for_exit_ms);
    }
    super::extension_arg(&args.extensions, "wait_for_exit_ms")
}

fn effective_write_close_stdin(args: &WriteStdinArgs) -> AppResult<bool> {
    if args.close_stdin {
        return Ok(true);
    }
    Ok(super::extension_arg::<bool>(&args.extensions, "close_stdin")?.unwrap_or(false))
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProcessSignal {
    Interrupt,
    Terminate,
    Kill,
}

impl ProcessSignal {
    fn as_str(self) -> &'static str {
        match self {
            Self::Interrupt => "interrupt",
            Self::Terminate => "terminate",
            Self::Kill => "kill",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionReason {
    Exited,
    Signaled,
    TimedOut,
    Cancelled,
    Failed,
}

impl CompletionReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Exited => "exited",
            Self::Signaled => "signaled",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessCompletion {
    reason: CompletionReason,
    exit_code: Option<i32>,
    signal: Option<i32>,
    error: Option<String>,
}

fn completion_from_exit_status(
    status: Option<std::process::ExitStatus>,
    forced_reason: Option<CompletionReason>,
    error: Option<String>,
) -> ProcessCompletion {
    let exit_code = status.as_ref().and_then(std::process::ExitStatus::code);
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status.as_ref().and_then(ExitStatusExt::signal)
    };
    #[cfg(not(unix))]
    let signal = None;
    let reason = forced_reason.unwrap_or_else(|| {
        if signal.is_some() {
            CompletionReason::Signaled
        } else if error.is_some() {
            CompletionReason::Failed
        } else {
            CompletionReason::Exited
        }
    });
    ProcessCompletion {
        reason,
        exit_code,
        signal,
        error,
    }
}

struct InteractiveSession {
    project_key: String,
    stdin: AsyncMutex<Option<ChildStdin>>,
    pty_writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    pty_master: Option<Arc<Mutex<Box<dyn MasterPty + Send>>>>,
    tty: bool,
    terminal: Option<Mutex<vt100::Parser>>,
    output: Mutex<OutputBuffer>,
    completion: Mutex<Option<ProcessCompletion>>,
    timed_out: AtomicBool,
    deadline_exceeded: AtomicBool,
    /// Set once a finished response is truncated. Keep returning the session id
    /// on cursorless polls so callers do not lose the ability to replay retained
    /// output merely because one follow-up response itself is empty/untruncated.
    replay_pending: AtomicBool,
    requested_signal: Mutex<Option<ProcessSignal>>,
    started: Instant,
    last_activity: Mutex<Instant>,
    pid: Option<u32>,
    changed: Notify,
    drains_remaining: AtomicUsize,
    drains_finished: Notify,
    _registry_permit: Mutex<Option<OwnedSemaphorePermit>>,
    process_permits: Mutex<Option<(OwnedSemaphorePermit, OwnedSemaphorePermit)>>,
    cancellation: CancellationToken,
}

impl InteractiveSession {
    fn is_finished(&self) -> bool {
        self.completion.lock().is_ok_and(|value| value.is_some())
    }

    fn completion(&self) -> Option<ProcessCompletion> {
        self.completion.lock().ok().and_then(|value| value.clone())
    }

    fn touch(&self) {
        if let Ok(mut value) = self.last_activity.lock() {
            *value = Instant::now();
        }
    }

    async fn write_input(&self, bytes: Vec<u8>) -> AppResult<()> {
        if self.tty {
            let writer = self.pty_writer.clone();
            tokio::task::spawn_blocking(move || {
                let mut guard = writer
                    .lock()
                    .map_err(|_| AppError::new("PROCESS_FAILED", "PTY writer lock poisoned"))?;
                let writer = guard
                    .as_mut()
                    .ok_or_else(|| AppError::new("PROCESS_FAILED", "PTY input is closed"))?;
                writer.write_all(&bytes)?;
                writer.flush()?;
                Ok::<_, AppError>(())
            })
            .await
            .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))?
        } else {
            let mut stdin = self.stdin.lock().await;
            let writer = stdin
                .as_mut()
                .ok_or_else(|| AppError::new("PROCESS_FAILED", "process stdin is closed"))?;
            writer.write_all(&bytes).await?;
            writer.flush().await?;
            Ok(())
        }
    }

    async fn close_input(&self) {
        if self.tty {
            let writer = self.pty_writer.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if let Ok(mut writer) = writer.lock() {
                    // Dropping the master writer alone need not deliver EOF
                    // while the master reader remains alive. Ctrl-D is the
                    // portable EOF character in canonical terminal mode.
                    if let Some(writer) = writer.as_mut() {
                        let _ = writer.write_all(&[0x04]);
                        let _ = writer.flush();
                    }
                    writer.take();
                }
            })
            .await;
        } else {
            self.stdin.lock().await.take();
        }
    }

    async fn resize(&self, rows: u16, cols: u16) -> AppResult<()> {
        let master = self
            .pty_master
            .as_ref()
            .cloned()
            .ok_or_else(|| AppError::new("INVALID_INPUT", "session is not using a PTY"))?;
        tokio::task::spawn_blocking(move || {
            master
                .lock()
                .map_err(|_| AppError::new("PROCESS_FAILED", "PTY master lock poisoned"))?
                .resize(pty_size(rows, cols))
                .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))
        })
        .await
        .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))??;
        if let Some(terminal) = &self.terminal {
            terminal
                .lock()
                .map_err(|_| AppError::new("PROCESS_FAILED", "terminal snapshot lock poisoned"))?
                .set_size(rows, cols);
        }
        Ok(())
    }

    async fn signal(&self, signal: ProcessSignal) -> AppResult<()> {
        if matches!(signal, ProcessSignal::Interrupt) && self.tty {
            self.write_input(vec![0x03]).await?;
        } else if let Err(error) = signal_tree(self.pid, signal)
            && !self.is_finished()
        {
            return Err(error);
        }
        if let Ok(mut requested) = self.requested_signal.lock() {
            *requested = Some(signal);
        }
        Ok(())
    }

    fn terminal_snapshot(&self) -> Option<String> {
        self.terminal.as_ref().and_then(|terminal| {
            terminal
                .lock()
                .ok()
                .map(|parser| parser.screen().contents())
        })
    }
}

#[derive(Clone)]
pub struct ProcessRegistry {
    entries: Arc<DashMap<String, Arc<InteractiveSession>>>,
    maximum: usize,
    capacity: Arc<Semaphore>,
    idle: Duration,
    output_limit: usize,
    active: Arc<AtomicUsize>,
    tracked_tasks: Arc<AtomicUsize>,
}

impl ProcessRegistry {
    pub fn new(maximum: usize, idle: Duration, output_limit: usize) -> Self {
        Self {
            entries: Arc::new(DashMap::new()),
            maximum,
            capacity: Arc::new(Semaphore::new(maximum)),
            idle,
            output_limit,
            active: Arc::new(AtomicUsize::new(0)),
            tracked_tasks: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn active(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    pub fn tracked_tasks(&self) -> usize {
        self.tracked_tasks.load(Ordering::Relaxed)
    }

    /// Stop every live session and wait for the process waiters and output
    /// drains to publish their terminal state. This is intentionally bounded:
    /// a broken descendant that keeps a pipe open must not block daemon
    /// shutdown forever.
    pub async fn shutdown_and_wait(&self, grace: Duration) -> (usize, usize) {
        let sessions: Vec<Arc<InteractiveSession>> = self
            .entries
            .iter()
            .filter(|entry| !entry.is_finished())
            .map(|entry| entry.value().clone())
            .collect();
        let requested = sessions.len();

        for session in &sessions {
            let _ = session.signal(ProcessSignal::Terminate).await;
        }

        let deadline = Instant::now() + grace;
        while self.active() > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        if self.active() > 0 {
            for session in &sessions {
                if !session.is_finished() {
                    session.cancellation.cancel();
                    kill_tree(session.pid);
                }
            }
            let force_deadline = Instant::now() + Duration::from_secs(2);
            while self.active() > 0 && Instant::now() < force_deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        self.entries.retain(|_, session| !session.is_finished());
        (requested, self.active())
    }

    pub fn cleanup(&self) {
        let now = Instant::now();
        let expired: Vec<String> = self
            .entries
            .iter()
            .filter_map(|entry| {
                let age = entry
                    .last_activity
                    .lock()
                    .map(|value| now.saturating_duration_since(*value))
                    .unwrap_or(self.idle);
                (age >= self.idle || (entry.is_finished() && age >= Duration::from_secs(60)))
                    .then(|| entry.key().clone())
            })
            .collect();
        for id in expired {
            if let Some((_, session)) = self.entries.remove(&id)
                && !session.is_finished()
            {
                session.cancellation.cancel();
                kill_tree(session.pid);
            }
        }
    }

    async fn start(
        &self,
        config: &crate::config::Config,
        project: &ProjectContext,
        args: &ExecCommandArgs,
        global_process_permit: OwnedSemaphorePermit,
        project_process_permit: OwnedSemaphorePermit,
    ) -> AppResult<(String, Arc<InteractiveSession>)> {
        self.cleanup();
        if self.entries.len() >= self.maximum
            && let Some(oldest_finished) = self
                .entries
                .iter()
                .filter(|entry| entry.is_finished())
                .min_by_key(|entry| entry.started)
                .map(|entry| entry.key().clone())
        {
            self.entries.remove(&oldest_finished);
        }
        let registry_permit = self.capacity.clone().try_acquire_owned().map_err(|_| {
            AppError::new(
                "SERVER_BUSY",
                "interactive process capacity reached; retry after an existing session exits",
            )
        })?;
        let project_count = self
            .entries
            .iter()
            .filter(|entry| {
                !entry.is_finished() && entry.project_key == project.effective_project_key.as_str()
            })
            .count();
        if project_count >= config.limits.per_project_processes {
            return Err(AppError::new(
                "SERVER_BUSY",
                "active project process capacity reached",
            ));
        }
        let timeout = Duration::from_millis(
            args.timeout_ms
                .unwrap_or(config.exec_default_timeout.as_millis() as u64)
                .min(config.exec_max_timeout.as_millis() as u64),
        );
        let workdir = config_path(project, args.workdir.as_deref())?;
        let mut command = build_command_with_options(
            config,
            project,
            &args.command,
            true,
            timeout,
            &args.env,
            &workdir,
            args.shell.as_deref(),
        )?;
        if args.tty {
            return self
                .start_pty(
                    project,
                    args,
                    command,
                    timeout,
                    registry_permit,
                    global_process_permit,
                    project_process_permit,
                )
                .await;
        }
        let mut child = command
            .spawn()
            .map_err(|error| AppError::new("SANDBOX_UNAVAILABLE", error.to_string()))?;
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::new("PROCESS_FAILED", "stdout pipe unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AppError::new("PROCESS_FAILED", "stderr pipe unavailable"))?;
        let id = Uuid::now_v7().simple().to_string();
        let session = Arc::new(InteractiveSession {
            project_key: project.effective_project_key.as_str().to_owned(),
            stdin: AsyncMutex::new(stdin),
            pty_writer: Arc::new(Mutex::new(None)),
            pty_master: None,
            tty: false,
            terminal: None,
            output: Mutex::new(OutputBuffer::default()),
            completion: Mutex::new(None),
            timed_out: AtomicBool::new(false),
            deadline_exceeded: AtomicBool::new(false),
            replay_pending: AtomicBool::new(false),
            requested_signal: Mutex::new(None),
            started: Instant::now(),
            last_activity: Mutex::new(Instant::now()),
            pid: child.id(),
            changed: Notify::new(),
            drains_remaining: AtomicUsize::new(2),
            drains_finished: Notify::new(),
            _registry_permit: Mutex::new(Some(registry_permit)),
            process_permits: Mutex::new(Some((global_process_permit, project_process_permit))),
            cancellation: CancellationToken::new(),
        });
        self.entries.insert(id.clone(), session.clone());
        self.active.fetch_add(1, Ordering::Relaxed);
        spawn_drain(
            stdout,
            session.clone(),
            self.output_limit,
            "",
            self.tracked_tasks.clone(),
        );
        spawn_drain(
            stderr,
            session.clone(),
            self.output_limit,
            "[stderr] ",
            self.tracked_tasks.clone(),
        );
        let waiter_session = session.clone();
        let active = self.active.clone();
        let tasks = self.tracked_tasks.clone();
        tasks.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            let _task = TaskGuard(tasks);
            let (status, forced_reason, wait_error) = tokio::select! {
                wait = child.wait() => match wait {
                    Ok(status) => (Some(status), None, None),
                    Err(error) => (None, None, Some(error.to_string())),
                },
                _ = tokio::time::sleep(timeout) => {
                    waiter_session.deadline_exceeded.store(true, Ordering::Relaxed);
                    match tokio::time::timeout(TIMEOUT_COMPLETION_GRACE, child.wait()).await {
                        Ok(Ok(status)) => (Some(status), None, None),
                        Ok(Err(error)) => (None, None, Some(error.to_string())),
                        Err(_) => {
                            waiter_session.timed_out.store(true, Ordering::Relaxed);
                            kill_tree(waiter_session.pid);
                            let _ = child.kill().await;
                            match child.wait().await {
                                Ok(status) => (Some(status), Some(CompletionReason::TimedOut), None),
                                Err(error) => (
                                    None,
                                    Some(CompletionReason::TimedOut),
                                    Some(error.to_string()),
                                ),
                            }
                        }
                    }
                },
                _ = waiter_session.cancellation.cancelled() => {
                    kill_tree(waiter_session.pid);
                    let _ = child.kill().await;
                    match child.wait().await {
                        Ok(status) => (Some(status), Some(CompletionReason::Cancelled), None),
                        Err(error) => (
                            None,
                            Some(CompletionReason::Cancelled),
                            Some(error.to_string()),
                        ),
                    }
                }
            };
            // A child may exit before Tokio's pipe readers have consumed the final
            // kernel-buffered bytes. Do not publish completion (which makes the
            // registry eligible for removal) until both drains reach EOF. A
            // bounded wait also prevents inherited pipe handles in misbehaving
            // grandchildren from retaining a session forever.
            wait_for_drains(&waiter_session, Duration::from_secs(5)).await;
            if let Ok(mut completion) = waiter_session.completion.lock() {
                *completion = Some(completion_from_exit_status(
                    status,
                    forced_reason,
                    wait_error,
                ));
            }
            if let Ok(mut permits) = waiter_session.process_permits.lock() {
                permits.take();
            }
            active.fetch_sub(1, Ordering::Relaxed);
            waiter_session.changed.notify_waiters();
        });
        Ok((id, session))
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_pty(
        &self,
        project: &ProjectContext,
        args: &ExecCommandArgs,
        command: tokio::process::Command,
        timeout: Duration,
        registry_permit: OwnedSemaphorePermit,
        global_process_permit: OwnedSemaphorePermit,
        project_process_permit: OwnedSemaphorePermit,
    ) -> AppResult<(String, Arc<InteractiveSession>)> {
        let (rows, cols) = terminal_dimensions(args.rows, args.cols)?;
        let pty =
            tokio::task::spawn_blocking(move || spawn_pty_process(&command, timeout, rows, cols))
                .await
                .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))??;
        let PtyProcess {
            child,
            mut killer,
            reader,
            writer,
            master,
            pid,
        } = pty;
        let id = Uuid::now_v7().simple().to_string();
        let session = Arc::new(InteractiveSession {
            project_key: project.effective_project_key.as_str().to_owned(),
            stdin: AsyncMutex::new(None),
            pty_writer: Arc::new(Mutex::new(Some(writer))),
            pty_master: Some(master),
            tty: true,
            terminal: Some(Mutex::new(vt100::Parser::new(rows, cols, 0))),
            output: Mutex::new(OutputBuffer::default()),
            completion: Mutex::new(None),
            timed_out: AtomicBool::new(false),
            deadline_exceeded: AtomicBool::new(false),
            replay_pending: AtomicBool::new(false),
            requested_signal: Mutex::new(None),
            started: Instant::now(),
            last_activity: Mutex::new(Instant::now()),
            pid,
            changed: Notify::new(),
            drains_remaining: AtomicUsize::new(1),
            drains_finished: Notify::new(),
            _registry_permit: Mutex::new(Some(registry_permit)),
            process_permits: Mutex::new(Some((global_process_permit, project_process_permit))),
            cancellation: CancellationToken::new(),
        });
        self.entries.insert(id.clone(), session.clone());
        self.active.fetch_add(1, Ordering::Relaxed);
        spawn_blocking_drain(
            reader,
            session.clone(),
            self.output_limit,
            self.tracked_tasks.clone(),
        );
        let waiter_session = session.clone();
        let active = self.active.clone();
        let tasks = self.tracked_tasks.clone();
        tasks.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            let _task = TaskGuard(tasks);
            let mut wait = tokio::task::spawn_blocking(move || wait_pty_process(child, pid));
            let (status, forced_reason, wait_error) = tokio::select! {
                result = &mut wait => match result {
                    Ok(Ok(status)) => (Some(status), None, None),
                    Ok(Err(error)) => (None, None, Some(error.to_string())),
                    Err(error) => (None, None, Some(error.to_string())),
                },
                _ = tokio::time::sleep(timeout) => {
                    waiter_session.deadline_exceeded.store(true, Ordering::Relaxed);
                    match tokio::time::timeout(TIMEOUT_COMPLETION_GRACE, &mut wait).await {
                        Ok(Ok(Ok(status))) => (Some(status), None, None),
                        Ok(Ok(Err(error))) => (None, None, Some(error.to_string())),
                        Ok(Err(error)) => (None, None, Some(error.to_string())),
                        Err(_) => {
                            waiter_session.timed_out.store(true, Ordering::Relaxed);
                            kill_tree(waiter_session.pid);
                            let _ = killer.kill();
                            match wait.await {
                                Ok(Ok(status)) => {
                                    (Some(status), Some(CompletionReason::TimedOut), None)
                                }
                                Ok(Err(error)) => (
                                    None,
                                    Some(CompletionReason::TimedOut),
                                    Some(error.to_string()),
                                ),
                                Err(error) => (
                                    None,
                                    Some(CompletionReason::TimedOut),
                                    Some(error.to_string()),
                                ),
                            }
                        }
                    }
                },
                _ = waiter_session.cancellation.cancelled() => {
                    kill_tree(waiter_session.pid);
                    let _ = killer.kill();
                    match wait.await {
                        Ok(Ok(status)) => (Some(status), Some(CompletionReason::Cancelled), None),
                        Ok(Err(error)) => (
                            None,
                            Some(CompletionReason::Cancelled),
                            Some(error.to_string()),
                        ),
                        Err(error) => (
                            None,
                            Some(CompletionReason::Cancelled),
                            Some(error.to_string()),
                        ),
                    }
                }
            };
            wait_for_drains(&waiter_session, Duration::from_secs(5)).await;
            if let Ok(mut completion) = waiter_session.completion.lock() {
                let exit_code = status.as_ref().and_then(|value| value.exit_code);
                let inferred_signal = status.as_ref().and_then(|value| value.signal);
                let reason = forced_reason.unwrap_or(if wait_error.is_some() {
                    CompletionReason::Failed
                } else if inferred_signal.is_some() {
                    CompletionReason::Signaled
                } else {
                    CompletionReason::Exited
                });
                *completion = Some(ProcessCompletion {
                    reason,
                    exit_code,
                    signal: inferred_signal,
                    error: wait_error,
                });
            }
            if let Ok(mut permits) = waiter_session.process_permits.lock() {
                permits.take();
            }
            active.fetch_sub(1, Ordering::Relaxed);
            waiter_session.changed.notify_waiters();
        });
        Ok((id, session))
    }

    fn get_for_project(
        &self,
        id: &str,
        project: &ProjectContext,
    ) -> AppResult<Arc<InteractiveSession>> {
        let session = self
            .entries
            .get(id)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| AppError::new("FILE_NOT_FOUND", "interactive process not found"))?;
        if session.project_key != project.effective_project_key.as_str() {
            return Err(AppError::new(
                "FILE_NOT_FOUND",
                "interactive process not found",
            ));
        }
        session.touch();
        Ok(session)
    }

    pub fn shutdown(&self) {
        let ids: Vec<String> = self
            .entries
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        for id in ids {
            if let Some((_, session)) = self.entries.remove(&id)
                && !session.is_finished()
            {
                session.cancellation.cancel();
                kill_tree(session.pid);
            }
        }
    }
}

fn spawn_drain(
    mut reader: impl AsyncRead + Unpin + Send + 'static,
    session: Arc<InteractiveSession>,
    limit: usize,
    prefix: &'static str,
    tasks: Arc<AtomicUsize>,
) {
    tasks.fetch_add(1, Ordering::Relaxed);
    tokio::spawn(async move {
        let _task = TaskGuard(tasks);
        let mut buffer = [0_u8; 8192];
        let mut at_line_start = !prefix.is_empty();
        while let Ok(read) = reader.read(&mut buffer).await {
            if read == 0 {
                break;
            }
            if let Ok(mut output) = session.output.lock() {
                if prefix.is_empty() {
                    output.append(&buffer[..read], limit);
                } else {
                    let mut start = 0usize;
                    while start < read {
                        if at_line_start {
                            output.append(prefix.as_bytes(), limit);
                            at_line_start = false;
                        }
                        if let Some(relative_newline) =
                            buffer[start..read].iter().position(|byte| *byte == b'\n')
                        {
                            let end = start + relative_newline + 1;
                            output.append(&buffer[start..end], limit);
                            start = end;
                            at_line_start = true;
                        } else {
                            output.append(&buffer[start..read], limit);
                            start = read;
                        }
                    }
                }
            }
            session.changed.notify_waiters();
        }
        session.drains_remaining.fetch_sub(1, Ordering::AcqRel);
        // notify_one stores a permit when the waiter is between checks, unlike
        // notify_waiters, so drain completion cannot be missed.
        session.drains_finished.notify_one();
        session.changed.notify_waiters();
    });
}

fn spawn_blocking_drain(
    mut reader: Box<dyn Read + Send>,
    session: Arc<InteractiveSession>,
    limit: usize,
    tasks: Arc<AtomicUsize>,
) {
    tasks.fetch_add(1, Ordering::Relaxed);
    tokio::task::spawn_blocking(move || {
        let _task = TaskGuard(tasks);
        let mut buffer = [0_u8; 8192];
        while let Ok(read) = reader.read(&mut buffer) {
            if read == 0 {
                break;
            }
            if let Ok(mut output) = session.output.lock() {
                output.append(&buffer[..read], limit);
            }
            if let Some(terminal) = &session.terminal
                && let Ok(mut parser) = terminal.lock()
            {
                parser.process(&buffer[..read]);
            }
            session.changed.notify_waiters();
        }
        session.drains_remaining.fetch_sub(1, Ordering::AcqRel);
        session.drains_finished.notify_one();
        session.changed.notify_waiters();
    });
}

async fn wait_for_drains(session: &InteractiveSession, maximum: Duration) -> bool {
    let wait = async {
        while session.drains_remaining.load(Ordering::Acquire) != 0 {
            // Register the notification before checking again so a drain cannot
            // complete in the small interval between the check and the await.
            let notified = session.drains_finished.notified();
            if session.drains_remaining.load(Ordering::Acquire) == 0 {
                break;
            }
            notified.await;
        }
    };
    tokio::time::timeout(maximum, wait).await.is_ok()
}

struct TaskGuard(Arc<AtomicUsize>);

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn kill_tree(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    if let Some(pid) = pid {
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

fn signal_tree(pid: Option<u32>, signal: ProcessSignal) -> AppResult<()> {
    let pid = pid.ok_or_else(|| AppError::new("PROCESS_FAILED", "process id unavailable"))?;
    #[cfg(unix)]
    {
        let native_signal = match signal {
            ProcessSignal::Interrupt => libc::SIGINT,
            ProcessSignal::Terminate => libc::SIGTERM,
            ProcessSignal::Kill => libc::SIGKILL,
        };
        let group_result = unsafe { libc::kill(-(pid as i32), native_signal) };
        if group_result == 0 {
            return Ok(());
        }
        Err(AppError::new(
            "PROCESS_FAILED",
            std::io::Error::last_os_error().to_string(),
        ))
    }
    #[cfg(windows)]
    {
        let mut command = std::process::Command::new("taskkill");
        command.args(["/T", "/PID", &pid.to_string()]);
        if matches!(signal, ProcessSignal::Kill) {
            command.arg("/F");
        }
        let status = command
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if status.success() {
            return Ok(());
        }
        return Err(AppError::new("PROCESS_FAILED", "taskkill failed"));
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (pid, signal);
        Err(AppError::new(
            "PROCESS_FAILED",
            "process signals are unsupported on this platform",
        ))
    }
}

fn config_path(project: &ProjectContext, workdir: Option<&str>) -> AppResult<std::path::PathBuf> {
    let path = crate::sandbox::SecurePathResolver.resolve_project_path(
        &project.project_root,
        workdir.unwrap_or("."),
        PathOperation::Existing,
    )?;
    if !path.is_dir() {
        return Err(AppError::new(
            "INVALID_INPUT",
            "process workdir must be a directory",
        ));
    }
    Ok(path)
}

async fn yield_result(
    id: &str,
    session: &Arc<InteractiveSession>,
    yield_ms: u64,
    max_output_tokens: Option<usize>,
    since_output_offset: Option<usize>,
) -> Value {
    let deadline = Instant::now() + Duration::from_millis(yield_ms);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let notified = session.changed.notified();
        if session.is_finished() {
            break;
        }
        if tokio::time::timeout(remaining, notified).await.is_err() {
            break;
        }
    }
    let (output, output_offset, output_next_offset, byte_truncated) = session
        .output
        .lock()
        .map(|mut output| output.render_window(since_output_offset))
        .unwrap_or_else(|_| (String::new(), 0, 0, false));
    let completion = session.completion();
    let exit_code = completion.as_ref().and_then(|value| value.exit_code);
    let completion_reason = completion
        .as_ref()
        .map(|value| value.reason.as_str())
        .unwrap_or("running");
    let signal = completion.as_ref().and_then(|value| value.signal);
    let requested_signal = session
        .requested_signal
        .lock()
        .ok()
        .and_then(|value| value.map(ProcessSignal::as_str));
    let finished = completion.is_some();
    let (output, original_token_count) = token_window(output, max_output_tokens);
    let response_truncated = byte_truncated || original_token_count.is_some();
    if finished && response_truncated {
        session.replay_pending.store(true, Ordering::Relaxed);
    } else if finished && since_output_offset.is_some() {
        // An explicit replay that fits in one response satisfies a previous
        // presentation-only truncation. Byte-evicted output remains truncated
        // and therefore keeps replay_pending set until normal retention expiry.
        session.replay_pending.store(false, Ordering::Relaxed);
    }
    let replay_pending = finished && session.replay_pending.load(Ordering::Relaxed);
    let terminal_snapshot = session.terminal_snapshot().map(|snapshot| {
        let (snapshot, original_token_count) = token_window(snapshot, max_output_tokens);
        json!({
            "content": snapshot,
            "truncated": original_token_count.is_some(),
            "original_token_count": original_token_count,
        })
    });
    json!({
        "chunk_id": Uuid::now_v7().simple().to_string(),
        "session_id": if finished && !replay_pending { None } else { Some(id) },
        "exit_code": exit_code,
        "completion_reason": completion_reason,
        "signal": signal,
        "requested_signal": requested_signal,
        "error": completion.as_ref().and_then(|value| value.error.clone()),
        "output": output,
        "output_bytes": session.output.lock().map(|output| output.total_bytes).unwrap_or(0),
        "output_offset": output_offset,
        "output_next_offset": output_next_offset,
        "truncated": response_truncated,
        "original_token_count": original_token_count,
        "timed_out": session.timed_out.load(Ordering::Relaxed),
        "deadline_exceeded": session.deadline_exceeded.load(Ordering::Relaxed),
        "tty": session.tty,
        "terminal_snapshot": terminal_snapshot,
        "wall_time_seconds": session.started.elapsed().as_secs_f64(),
        "continuation": if finished && response_truncated {
            Some("The process finished, but this response was truncated. Call write_stdin with this session_id and since_output_offset to replay retained final output before considering a rerun.")
        } else if replay_pending {
            Some("A previous finished response was truncated. This session remains retained for replay; call write_stdin with this session_id and since_output_offset before considering a rerun.")
        } else if finished {
            None
        } else {
            Some("Call write_stdin with this session_id to send input, close stdin, signal, or poll for more output. Use signal plus wait_for_exit_ms in one call when you need final shutdown output and status.")
        },
    })
}

#[tool_router(router = process_router, vis = "pub(crate)")]
impl AgentHandler {
    #[tool(
        description = "Start a bounded command with a project-relative working directory. The effective execution backend may be Bubblewrap or native YOLO; native execution is not OS-filesystem-confined. Returns immediately when it exits, otherwise returns a project-scoped session_id for write_stdin. For one-shot CLIs or subagents that may read until EOF, pass optional stdin and close_stdin=true. Set tty=true for a native Unix PTY or Windows ConPTY. Results distinguish normal exit, signal, cancellation, deadline overrun, and forced timeout. output_offset/output_next_offset are logical byte-stream cursors; after bounded head+tail eviction a response can include an explicit omission marker rather than one contiguous original range. Recover lost/truncated presentation with write_stdin(since_output_offset=...) instead of re-running; evicted bytes are unrecoverable. Forward-compatible optional arguments may also be supplied under extensions; typed top-level fields remain preferred. Finished truncated sessions retain a recovery session_id. No extra approval is requested."
    )]
    async fn exec_command(
        &self,
        context: ProjectRequestContext,
        Parameters(args): Parameters<ExecCommandArgs>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        if let Err(error) = self.validate_small(&args.command) {
            return Ok(super::error_result(&error));
        }
        let stdin = match effective_exec_stdin(&args) {
            Ok(stdin) => stdin,
            Err(error) => return Ok(super::error_result(&error)),
        };
        let close_stdin = match effective_exec_close_stdin(&args) {
            Ok(close_stdin) => close_stdin,
            Err(error) => return Ok(super::error_result(&error)),
        };
        if let Some(stdin) = stdin.as_deref()
            && let Err(error) = self.validate_small(stdin)
        {
            return Ok(super::error_result(&error));
        }
        let shared = self.shared.clone();
        let params = serde_json::to_value(&args).unwrap_or_default();
        self.run(context.0, "exec_command", params, move |project| async move {
            let workdir = args
                .workdir
                .as_deref()
                .filter(|path| !path.is_empty())
                .unwrap_or(".");
            if let Some(notice) = shared.scoped_instruction_notice(&project, workdir)? {
                return Err(AppError::new(
                    "AGENTS_SCOPE_REQUIRED",
                    format!(
                        "nested project instructions were disclosed before starting a command in this workdir; consume them and retry if the command still complies:\n\n{notice}"
                    ),
                ));
            }
            let (_, project_processes, _) = shared
                .project_permits
                .get(project.effective_project_key.as_str())?;
            let global = shared.permit(shared.processes.clone()).await?;
            let project_permit = shared.permit(project_processes).await?;
            let (id, session) = shared
                .interactive
                .start(&shared.config, &project, &args, global, project_permit)
                .await?;
            shared.audit.emit(json!({"event":"process_started","request_id":id,"project":crate::audit::project_json(&project),"interactive":true}));
            if let Some(stdin) = stdin.as_ref()
                && let Err(error) = session.write_input(stdin.as_bytes().to_vec()).await
            {
                session.cancellation.cancel();
                kill_tree(session.pid);
                return Err(error);
            }
            if close_stdin {
                session.close_input().await;
            }
            let value = yield_result(
                &id,
                &session,
                args.yield_time_ms
                    .unwrap_or(10_000)
                    .clamp(MIN_YIELD_MS, MAX_INITIAL_YIELD_MS),
                args.max_output_tokens,
                None,
            ).await;
            if session.is_finished() {
                shared.audit.emit(json!({"event":"process_exited","request_id":id,"project":crate::audit::project_json(&project),"interactive":true}));
            }
            Ok(value)
        }).await
    }

    #[tool(
        description = "Write characters to, close input for, resize, signal, or poll a long-running exec_command process in the active project. For tty sessions, provide rows and cols together to resize before input. signal accepts interrupt, terminate, or kill; combine signal with wait_for_exit_ms to wait for terminal completion and drain final output in one call. output_offset/output_next_offset are logical stream cursors. Pass since_output_offset to replay retained history after a lost response; if that cursor falls inside an evicted middle region, replay resumes at the first retained tail byte and includes an explicit omission marker, because evicted bytes cannot be recovered. max_output_tokens is only a presentation cap: if replay is token-truncated, retry the same since_output_offset with a larger or omitted cap. Forward-compatible optional arguments may also be supplied under extensions; typed top-level fields remain preferred. PTY results also include a rendered terminal snapshot."
    )]
    async fn write_stdin(
        &self,
        context: ProjectRequestContext,
        Parameters(args): Parameters<WriteStdinArgs>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        if let Err(error) = self.validate_small(&args.chars) {
            return Ok(super::error_result(&error));
        }
        let close_stdin = match effective_write_close_stdin(&args) {
            Ok(close_stdin) => close_stdin,
            Err(error) => return Ok(super::error_result(&error)),
        };
        let wait_for_exit_ms = match effective_wait_for_exit_ms(&args) {
            Ok(value) => value,
            Err(error) => return Ok(super::error_result(&error)),
        };
        let since_output_offset = match effective_since_output_offset(&args) {
            Ok(value) => value,
            Err(error) => return Ok(super::error_result(&error)),
        };
        let shared = self.shared.clone();
        let params = serde_json::to_value(&args).unwrap_or_default();
        self.run(context.0, "write_stdin", params, move |project| async move {
            shared.interactive.cleanup();
            let session = shared.interactive.get_for_project(&args.session_id, &project)?;
            let mutates_process = args.rows.is_some()
                || args.cols.is_some()
                || !args.chars.is_empty()
                || args.signal.is_some()
                || close_stdin;
            if mutates_process && session.is_finished() {
                return Err(AppError::new(
                    "PROCESS_FINISHED",
                    "interactive process has already exited; poll without input or signal to collect any remaining output",
                ));
            }
            if args.rows.is_some() || args.cols.is_some() {
                let (rows, cols) = terminal_dimensions(args.rows, args.cols)?;
                session.resize(rows, cols).await?;
            }
            if !args.chars.is_empty() {
                session.write_input(args.chars.clone().into_bytes()).await?;
            }
            if let Some(signal) = args.signal {
                session.signal(signal).await?;
            }
            if close_stdin {
                session.close_input().await;
            }
            let yield_ms = if let Some(wait_for_exit_ms) = wait_for_exit_ms {
                wait_for_exit_ms.clamp(MIN_YIELD_MS, MAX_POLL_YIELD_MS)
            } else {
                args.yield_time_ms
                    .unwrap_or(if args.chars.is_empty() { 5_000 } else { 250 })
                    .clamp(
                        MIN_YIELD_MS,
                        if args.chars.is_empty() {
                            MAX_POLL_YIELD_MS
                        } else {
                            MAX_YIELD_MS
                        },
                    )
            };
            let value = yield_result(
                &args.session_id,
                &session,
                yield_ms,
                args.max_output_tokens,
                since_output_offset,
            ).await;
            if session.is_finished() {
                shared.audit.emit(json!({"event":"process_exited","request_id":args.session_id,"project":crate::audit::project_json(&project),"interactive":true}));
            }
            Ok(value)
        }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn regression_pty_external_signal_is_reported_as_signaled() {
        use crate::project::ProjectKey;
        use crate::request_context::TransportMode;

        let project_dir = tempfile::tempdir().unwrap();
        let project = ProjectContext {
            native_project_key: ProjectKey::new("native".to_owned()).unwrap(),
            effective_project_key: ProjectKey::new("effective".to_owned()).unwrap(),
            project_alias: None,
            project_root: project_dir.path().to_path_buf(),
            metadata_root: project_dir.path().join(".metadata"),
            transport_mode: TransportMode::Stateless,
            mcp_session_present: false,
        };
        let args = ExecCommandArgs {
            command: "kill -TERM $$".to_owned(),
            workdir: None,
            shell: None,
            timeout_ms: None,
            yield_time_ms: None,
            env: BTreeMap::new(),
            max_output_tokens: None,
            stdin: None,
            close_stdin: false,
            tty: true,
            rows: Some(24),
            cols: Some(80),
            extensions: BTreeMap::new(),
        };
        let mut command = tokio::process::Command::new("/bin/sh");
        command.arg("-c").arg("kill -TERM $$");
        command.current_dir(project_dir.path());

        let registry = ProcessRegistry::new(4, Duration::from_secs(60), 4096);
        let registry_permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
        let global_permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
        let project_permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
        let (_id, session) = registry
            .start_pty(
                &project,
                &args,
                command,
                Duration::from_secs(5),
                registry_permit,
                global_permit,
                project_permit,
            )
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(3), async {
            while !session.is_finished() {
                session.changed.notified().await;
            }
        })
        .await
        .expect("PTY process did not publish completion");

        let completion = session.completion().expect("PTY completion");
        assert_eq!(completion.reason, CompletionReason::Signaled);
        assert_eq!(completion.signal, Some(libc::SIGTERM));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_bare_cmd_spawns_through_conpty() {
        use crate::config::ConfigBuilder;
        use crate::project::ProjectKey;
        use crate::request_context::TransportMode;

        let project_dir = tempfile::tempdir().unwrap();
        let config = ConfigBuilder::from_map(BTreeMap::from([
            ("MCP_AUTH_TOKEN".to_owned(), "1234567890abcdef".to_owned()),
            ("MCP_EXEC_SANDBOX".to_owned(), "none".to_owned()),
        ]))
        .build()
        .unwrap();
        let project = ProjectContext {
            native_project_key: ProjectKey::new("native".to_owned()).unwrap(),
            effective_project_key: ProjectKey::new("effective".to_owned()).unwrap(),
            project_alias: None,
            project_root: project_dir.path().to_path_buf(),
            metadata_root: project_dir.path().join(".metadata"),
            transport_mode: TransportMode::Stateless,
            mcp_session_present: false,
        };
        let args = ExecCommandArgs {
            command: "echo codexbridge-conpty-cmd".to_owned(),
            workdir: None,
            shell: Some("cmd".to_owned()),
            timeout_ms: Some(5_000),
            yield_time_ms: None,
            env: BTreeMap::new(),
            max_output_tokens: None,
            stdin: None,
            close_stdin: false,
            tty: true,
            rows: Some(24),
            cols: Some(80),
            extensions: BTreeMap::new(),
        };
        let command = build_command_with_options(
            &config,
            &project,
            &args.command,
            true,
            Duration::from_secs(5),
            &BTreeMap::new(),
            project_dir.path(),
            args.shell.as_deref(),
        )
        .unwrap();

        let registry = ProcessRegistry::new(4, Duration::from_secs(60), 4096);
        let registry_permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
        let global_permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
        let project_permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
        let (_id, session) = registry
            .start_pty(
                &project,
                &args,
                command,
                Duration::from_secs(5),
                registry_permit,
                global_permit,
                project_permit,
            )
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(5), async {
            while !session.is_finished() {
                session.changed.notified().await;
            }
        })
        .await
        .expect("ConPTY cmd process did not publish completion");
        assert!(wait_for_drains(&session, Duration::from_secs(1)).await);

        let completion = session.completion().expect("ConPTY completion");
        assert_eq!(completion.reason, CompletionReason::Exited);
        assert_eq!(completion.exit_code, Some(0));
        let (output, _, _, _) = session.output.lock().unwrap().render_window(Some(0));
        assert!(output.contains("codexbridge-conpty-cmd"), "{output:?}");
    }

    struct ChunkedReader {
        chunks: std::collections::VecDeque<Vec<u8>>,
    }

    impl ChunkedReader {
        fn new(chunks: impl IntoIterator<Item = Vec<u8>>) -> Self {
            Self {
                chunks: chunks.into_iter().collect(),
            }
        }
    }

    impl tokio::io::AsyncRead for ChunkedReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if let Some(chunk) = self.chunks.pop_front() {
                buf.put_slice(&chunk);
            }
            std::task::Poll::Ready(Ok(()))
        }
    }

    fn test_session(exit_code: Option<i32>) -> Arc<InteractiveSession> {
        Arc::new(InteractiveSession {
            project_key: "project".to_owned(),
            stdin: AsyncMutex::new(None),
            pty_writer: Arc::new(Mutex::new(None)),
            pty_master: None,
            tty: false,
            terminal: None,
            output: Mutex::new(OutputBuffer::default()),
            completion: Mutex::new(exit_code.map(|exit_code| ProcessCompletion {
                reason: CompletionReason::Exited,
                exit_code: Some(exit_code),
                signal: None,
                error: None,
            })),
            timed_out: AtomicBool::new(false),
            deadline_exceeded: AtomicBool::new(false),
            replay_pending: AtomicBool::new(false),
            requested_signal: Mutex::new(None),
            started: Instant::now(),
            last_activity: Mutex::new(Instant::now()),
            pid: None,
            changed: Notify::new(),
            drains_remaining: AtomicUsize::new(0),
            drains_finished: Notify::new(),
            _registry_permit: Mutex::new(None),
            process_permits: Mutex::new(None),
            cancellation: CancellationToken::new(),
        })
    }

    #[test]
    fn output_buffer_is_bounded_and_marks_truncation() {
        let mut buffer = OutputBuffer::default();
        buffer.append(b"12345", 6);
        buffer.append(b"67890", 6);
        let (output, start, next, truncated) = buffer.render_window(None);
        assert!(output.starts_with("123"));
        assert!(output.ends_with("890"));
        assert!(output.contains("bytes omitted"));
        // The initial summary starts with the retained logical head at byte 0,
        // explicitly marks the evicted middle, and then includes the tail.
        assert_eq!((start, next), (0, 10));
        assert!(truncated);
        // Rendering is non-destructive: the same cursor replays the window.
        let (replayed, replay_start, replay_next, _) = buffer.render_window(Some(0));
        assert_eq!(replayed, output);
        assert_eq!((replay_start, replay_next), (start, next));
    }

    #[test]
    fn regression_output_cursor_inside_evicted_middle_never_replays_stale_head_bytes() {
        let mut buffer = OutputBuffer::default();
        buffer.append(b"12345", 6);
        buffer.append(b"67890", 6);

        // With a six-byte head/tail buffer, logical byte 6 has been evicted:
        // retained bytes are 0..3 (`123`) and 7..10 (`890`). A replay from 6
        // must therefore disclose one omitted byte and resume at byte 7. It
        // must never map the cursor back into the retained head.
        let (output, start, next, truncated) = buffer.render_window(Some(6));
        assert_eq!(start, 7);
        assert_eq!(next, 10);
        assert!(truncated);
        assert!(output.contains("1 buffered bytes omitted"), "{output:?}");
        assert!(output.ends_with("890"), "{output:?}");
        assert!(
            !output.contains("3890"),
            "stale head byte replayed: {output:?}"
        );
    }

    #[tokio::test]
    async fn regression_finished_truncated_process_keeps_recovery_session_id() {
        let session = test_session(Some(0));
        {
            let mut output = session.output.lock().unwrap();
            output.append(b"12345", 6);
            output.append(b"67890", 6);
        }

        let value = yield_result("recoverable-session", &session, 1, None, None).await;
        assert_eq!(value["truncated"], true);
        assert_eq!(
            value["session_id"].as_str(),
            Some("recoverable-session"),
            "finished sessions with truncated output must remain addressable for replay"
        );
    }

    #[tokio::test]
    async fn regression_cursorless_poll_does_not_discard_finished_replay_state() {
        let session = test_session(Some(0));
        session.output.lock().unwrap().append(b"final-output", 1024);

        let first = yield_result("replay-retained", &session, 1, Some(1), None).await;
        assert_eq!(first["truncated"], true);
        assert_eq!(first["session_id"], "replay-retained");

        let cursorless = yield_result("replay-retained", &session, 1, None, None).await;
        assert_eq!(cursorless["output"], "");
        assert_eq!(cursorless["truncated"], false);
        assert_eq!(cursorless["session_id"], "replay-retained");
        assert!(
            cursorless["continuation"]
                .as_str()
                .is_some_and(|value| value.contains("remains retained for replay"))
        );

        let replayed = yield_result("replay-retained", &session, 1, None, Some(0)).await;
        assert_eq!(replayed["output"], "final-output");
        assert_eq!(replayed["session_id"], Value::Null);
        assert_eq!(replayed["continuation"], Value::Null);
    }

    #[tokio::test]
    async fn regression_stderr_marker_is_not_inserted_inside_one_logical_line() {
        let session = test_session(None);
        session.drains_remaining.store(1, Ordering::Release);
        let tasks = Arc::new(AtomicUsize::new(0));
        let reader =
            ChunkedReader::new([b"error at src/".to_vec(), b"main.rs:10: boom\n".to_vec()]);

        spawn_drain(reader, session.clone(), 1024, "[stderr] ", tasks);
        assert!(wait_for_drains(&session, Duration::from_secs(1)).await);
        let (output, _, _, _) = session.output.lock().unwrap().render_window(Some(0));

        assert_eq!(output, "[stderr] error at src/main.rs:10: boom\n");
    }

    #[test]
    fn output_buffer_under_limit_round_trips_without_marker() {
        let mut buffer = OutputBuffer::default();
        buffer.append(b"hello", 16);
        buffer.append(b" world", 16);
        let (first, _, next, truncated) = buffer.render_window(None);
        assert_eq!(first, "hello world");
        assert_eq!(next, 11);
        assert!(!truncated);
        buffer.append(b" again", 16);
        let (second, start, next, _) = buffer.render_window(None);
        assert_eq!((second, start, next), (" again".to_owned(), 11, 17));
    }

    #[test]
    fn zero_byte_output_limit_retains_nothing_but_reports_omission() {
        let mut buffer = OutputBuffer::default();
        buffer.append(b"abcdef", 0);
        let (output, _, next, truncated) = buffer.render_window(None);
        assert_eq!(next, 6);
        assert!(truncated);
        assert!(output.contains("6 buffered bytes omitted"));
    }

    #[test]
    fn tracked_task_counter_soak_returns_to_baseline() {
        let counter = Arc::new(AtomicUsize::new(0));
        for _ in 0..10_000 {
            counter.fetch_add(1, Ordering::Relaxed);
            drop(TaskGuard(counter.clone()));
        }
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn repeated_output_chunks_never_exceed_retained_cap() {
        let mut buffer = OutputBuffer::default();
        for _ in 0..10_000 {
            buffer.append(&[b'x'; 1024], 4096);
        }
        assert_eq!(buffer.retained.len(), 4096);
        assert_eq!(buffer.total_bytes, 10_000 * 1024);
        assert!(buffer.truncated);
    }

    #[test]
    fn token_window_keeps_head_and_tail() {
        let (value, original) = token_window("abcdefghij".repeat(10), Some(4));
        assert!(value.starts_with("abcdefgh"));
        assert!(value.ends_with("cdefghij"));
        assert_eq!(original, Some(25));

        let (_, unicode_original) = token_window("😀".repeat(10), Some(2));
        assert_eq!(unicode_original, Some(5));
    }

    #[test]
    fn token_window_leaves_short_or_uncapped_output_unchanged() {
        let text = "short output".to_owned();
        assert_eq!(token_window(text.clone(), Some(100)), (text.clone(), None));
        assert_eq!(token_window(text.clone(), None), (text.clone(), None));
        assert_eq!(token_window(text.clone(), Some(0)), (text, None));
    }

    #[test]
    fn token_window_reports_utf16_based_original_count() {
        let (window, original) = token_window("🙂".repeat(12), Some(2));
        assert_eq!(original, Some(6));
        assert!(window.contains("UTF-16 code units omitted"));
    }

    #[test]
    fn exec_command_accepts_codex_cmd_alias() {
        let args: ExecCommandArgs = serde_json::from_value(json!({"cmd":"cargo test"})).unwrap();
        assert_eq!(args.command, "cargo test");
    }

    #[test]
    fn exec_and_stdin_arguments_accept_expected_defaults_and_reject_missing_command() {
        assert!(serde_json::from_value::<ExecCommandArgs>(json!({})).is_err());
        let stdin: WriteStdinArgs = serde_json::from_value(json!({"session_id":"abc"})).unwrap();
        assert!(stdin.chars.is_empty());
        assert!(!stdin.close_stdin);
        assert!(stdin.signal.is_none());
        assert!(stdin.wait_for_exit_ms.is_none());
        assert!(stdin.since_output_offset.is_none());
        assert!(stdin.rows.is_none());
        assert!(stdin.cols.is_none());
        assert!(stdin.extensions.is_empty());
        let exec: ExecCommandArgs = serde_json::from_value(json!({"cmd":"cat"})).unwrap();
        assert!(exec.stdin.is_none());
        assert!(!exec.close_stdin);
        assert!(exec.extensions.is_empty());
    }

    #[test]
    fn forward_compatible_process_extensions_fallback_without_overriding_typed_values() {
        let exec: ExecCommandArgs = serde_json::from_value(json!({
            "command":"cat",
            "extensions":{"stdin":"extension-input","close_stdin":true}
        }))
        .unwrap();
        assert_eq!(
            effective_exec_stdin(&exec).unwrap().as_deref(),
            Some("extension-input")
        );
        assert!(effective_exec_close_stdin(&exec).unwrap());

        let typed_exec: ExecCommandArgs = serde_json::from_value(json!({
            "command":"cat",
            "stdin":"typed-input",
            "extensions":{"stdin":"extension-input"}
        }))
        .unwrap();
        assert_eq!(
            effective_exec_stdin(&typed_exec).unwrap().as_deref(),
            Some("typed-input")
        );

        let stdin: WriteStdinArgs = serde_json::from_value(json!({
            "session_id":"abc",
            "extensions":{"since_output_offset":7,"wait_for_exit_ms":900,"close_stdin":true}
        }))
        .unwrap();
        assert_eq!(effective_since_output_offset(&stdin).unwrap(), Some(7));
        assert_eq!(effective_wait_for_exit_ms(&stdin).unwrap(), Some(900));
        assert!(effective_write_close_stdin(&stdin).unwrap());

        let typed_stdin: WriteStdinArgs = serde_json::from_value(json!({
            "session_id":"abc",
            "since_output_offset":3,
            "wait_for_exit_ms":500,
            "extensions":{"since_output_offset":7,"wait_for_exit_ms":900}
        }))
        .unwrap();
        assert_eq!(
            effective_since_output_offset(&typed_stdin).unwrap(),
            Some(3)
        );
        assert_eq!(effective_wait_for_exit_ms(&typed_stdin).unwrap(), Some(500));
    }

    #[test]
    fn unknown_process_extensions_are_ignored_without_changing_defaults() {
        let exec: ExecCommandArgs = serde_json::from_value(json!({
            "command":"true",
            "extensions":{"future_option":{"nested":true}}
        }))
        .unwrap();
        assert_eq!(effective_exec_stdin(&exec).unwrap(), None);
        assert!(!effective_exec_close_stdin(&exec).unwrap());

        let stdin: WriteStdinArgs = serde_json::from_value(json!({
            "session_id":"abc",
            "extensions":{"future_cursor":{"opaque":"value"}}
        }))
        .unwrap();
        assert_eq!(effective_since_output_offset(&stdin).unwrap(), None);
        assert_eq!(effective_wait_for_exit_ms(&stdin).unwrap(), None);
        assert!(!effective_write_close_stdin(&stdin).unwrap());
    }

    #[test]
    fn invalid_process_extension_types_fail_closed_unless_typed_value_wins() {
        let invalid_exec_stdin: ExecCommandArgs = serde_json::from_value(json!({
            "command":"cat",
            "extensions":{"stdin":123}
        }))
        .unwrap();
        assert_eq!(
            effective_exec_stdin(&invalid_exec_stdin)
                .unwrap_err()
                .code(),
            "INVALID_INPUT"
        );

        let invalid_exec_close: ExecCommandArgs = serde_json::from_value(json!({
            "command":"cat",
            "extensions":{"close_stdin":"yes"}
        }))
        .unwrap();
        assert_eq!(
            effective_exec_close_stdin(&invalid_exec_close)
                .unwrap_err()
                .code(),
            "INVALID_INPUT"
        );

        let invalid_stdin: WriteStdinArgs = serde_json::from_value(json!({
            "session_id":"abc",
            "extensions":{
                "since_output_offset":-1,
                "wait_for_exit_ms":"soon",
                "close_stdin":1
            }
        }))
        .unwrap();
        assert_eq!(
            effective_since_output_offset(&invalid_stdin)
                .unwrap_err()
                .code(),
            "INVALID_INPUT"
        );
        assert_eq!(
            effective_wait_for_exit_ms(&invalid_stdin)
                .unwrap_err()
                .code(),
            "INVALID_INPUT"
        );
        assert_eq!(
            effective_write_close_stdin(&invalid_stdin)
                .unwrap_err()
                .code(),
            "INVALID_INPUT"
        );

        let typed_exec: ExecCommandArgs = serde_json::from_value(json!({
            "command":"cat",
            "stdin":"typed",
            "close_stdin":true,
            "extensions":{"stdin":123,"close_stdin":"invalid"}
        }))
        .unwrap();
        assert_eq!(
            effective_exec_stdin(&typed_exec).unwrap().as_deref(),
            Some("typed")
        );
        assert!(effective_exec_close_stdin(&typed_exec).unwrap());

        let typed_stdin: WriteStdinArgs = serde_json::from_value(json!({
            "session_id":"abc",
            "since_output_offset":4,
            "wait_for_exit_ms":700,
            "close_stdin":true,
            "extensions":{
                "since_output_offset":"invalid",
                "wait_for_exit_ms":"invalid",
                "close_stdin":"invalid"
            }
        }))
        .unwrap();
        assert_eq!(
            effective_since_output_offset(&typed_stdin).unwrap(),
            Some(4)
        );
        assert_eq!(effective_wait_for_exit_ms(&typed_stdin).unwrap(), Some(700));
        assert!(effective_write_close_stdin(&typed_stdin).unwrap());
    }

    #[test]
    fn terminal_dimensions_are_bounded_and_paired() {
        assert_eq!(terminal_dimensions(None, None).unwrap(), (24, 80));
        assert_eq!(terminal_dimensions(Some(40), Some(120)).unwrap(), (40, 120));
        assert_eq!(terminal_dimensions(Some(1), Some(1)).unwrap(), (1, 1));
        assert_eq!(
            terminal_dimensions(Some(1000), Some(1000)).unwrap(),
            (1000, 1000)
        );
        assert!(terminal_dimensions(Some(40), None).is_err());
        assert!(terminal_dimensions(None, Some(80)).is_err());
        assert!(terminal_dimensions(Some(0), Some(80)).is_err());
        assert!(terminal_dimensions(Some(24), Some(0)).is_err());
        assert!(terminal_dimensions(Some(24), Some(1001)).is_err());
        assert!(terminal_dimensions(Some(1001), Some(80)).is_err());
    }

    #[tokio::test]
    async fn drain_completion_includes_final_pipe_bytes() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let session = Arc::new(InteractiveSession {
            project_key: "project".to_owned(),
            stdin: AsyncMutex::new(None),
            pty_writer: Arc::new(Mutex::new(None)),
            pty_master: None,
            tty: false,
            terminal: None,
            output: Mutex::new(OutputBuffer::default()),
            completion: Mutex::new(None),
            timed_out: AtomicBool::new(false),
            deadline_exceeded: AtomicBool::new(false),
            replay_pending: AtomicBool::new(false),
            requested_signal: Mutex::new(None),
            started: Instant::now(),
            last_activity: Mutex::new(Instant::now()),
            pid: None,
            changed: Notify::new(),
            drains_remaining: AtomicUsize::new(1),
            drains_finished: Notify::new(),
            _registry_permit: Mutex::new(None),
            process_permits: Mutex::new(None),
            cancellation: CancellationToken::new(),
        });
        let tasks = Arc::new(AtomicUsize::new(0));
        spawn_drain(reader, session.clone(), 1024, "", tasks.clone());
        writer.write_all(b"the-final-tail").await.unwrap();
        drop(writer);

        assert!(wait_for_drains(&session, Duration::from_secs(1)).await);
        let (output, _, bytes, truncated) = session.output.lock().unwrap().render_window(None);
        assert_eq!(output, "the-final-tail");
        assert_eq!(bytes, 14);
        assert!(!truncated);
        for _ in 0..100 {
            if tasks.load(Ordering::Relaxed) == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(tasks.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn yield_result_returns_immediately_for_an_already_finished_session() {
        let session = test_session(Some(0));
        session.output.lock().unwrap().append(b"done", 1024);
        let value = tokio::time::timeout(
            Duration::from_millis(100),
            yield_result("finished", &session, 30_000, None, None),
        )
        .await
        .expect("finished session must not wait for the yield deadline");
        assert_eq!(value["exit_code"], 0);
        assert_eq!(value["completion_reason"], "exited");
        assert_eq!(value["deadline_exceeded"], false);
        assert_eq!(value["timed_out"], false);
        assert_eq!(value["output"], "done");
        assert!(value["session_id"].is_null());
        assert!(value["continuation"].is_null());
    }

    #[tokio::test]
    async fn yield_result_advances_cursor_and_replays_since_offset() {
        let session = test_session(None);
        session.output.lock().unwrap().append(b"alpha", 1024);
        let first = yield_result("cursor", &session, 10, None, None).await;
        assert_eq!(first["output"], "alpha");
        assert_eq!(first["output_offset"], 0);
        assert_eq!(first["output_next_offset"], 5);
        assert_eq!(first["output_bytes"], 5);
        session.output.lock().unwrap().append(b"beta", 1024);
        let second = yield_result("cursor", &session, 10, None, None).await;
        assert_eq!(second["output"], "beta");
        assert_eq!(second["output_offset"], 5);
        assert_eq!(second["output_next_offset"], 9);
        // A lost response is recovered by replaying an older cursor.
        let replayed = yield_result("cursor", &session, 10, None, Some(0)).await;
        assert_eq!(replayed["output"], "alphabeta");
        assert_eq!(replayed["output_offset"], 0);
    }

    #[tokio::test]
    async fn finished_sessions_stay_pollable_for_output_recovery() {
        let session = test_session(Some(0));
        session.output.lock().unwrap().append(b"final", 1024);
        let first = yield_result("finished-recovery", &session, 10, None, None).await;
        assert_eq!(first["completion_reason"], "exited");
        let replayed = yield_result("finished-recovery", &session, 10, None, Some(0)).await;
        assert_eq!(replayed["output"], "final");
        assert_eq!(replayed["completion_reason"], "exited");
    }

    #[tokio::test]
    async fn yield_result_wakes_when_process_completion_is_published() {
        let session = test_session(None);
        let finisher = session.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            *finisher.completion.lock().unwrap() = Some(ProcessCompletion {
                reason: CompletionReason::Exited,
                exit_code: Some(7),
                signal: None,
                error: None,
            });
            finisher.changed.notify_waiters();
        });
        let value = tokio::time::timeout(
            Duration::from_millis(250),
            yield_result("running", &session, 30_000, None, None),
        )
        .await
        .expect("completion notification must wake the yield waiter");
        assert_eq!(value["exit_code"], 7);
        assert_eq!(value["completion_reason"], "exited");
        assert!(value["session_id"].is_null());
    }

    #[tokio::test]
    async fn graceful_signal_that_exits_zero_is_reported_as_exit() {
        let session = test_session(Some(0));
        *session.requested_signal.lock().unwrap() = Some(ProcessSignal::Terminate);
        let value = yield_result("graceful", &session, 250, None, None).await;
        assert_eq!(value["completion_reason"], "exited");
        assert_eq!(value["exit_code"], 0);
        assert_eq!(value["requested_signal"], "terminate");
        assert_eq!(value["signal"], serde_json::Value::Null);
    }

    #[test]
    fn failed_wait_has_explicit_reason_and_error_without_synthetic_exit_code() {
        let completion = completion_from_exit_status(
            None,
            None,
            Some("wait4: resource temporarily unavailable".to_owned()),
        );
        assert_eq!(completion.reason, CompletionReason::Failed);
        assert_eq!(completion.exit_code, None);
        assert_eq!(completion.signal, None);
        assert_eq!(
            completion.error.as_deref(),
            Some("wait4: resource temporarily unavailable")
        );
    }

    #[test]
    fn initial_yield_is_below_common_transport_deadline() {
        assert_eq!(MAX_INITIAL_YIELD_MS, 20_000);
        assert_eq!(MAX_YIELD_MS, 30_000);
    }

    #[test]
    fn output_buffer_replaces_invalid_utf8_without_losing_byte_accounting() {
        let mut buffer = OutputBuffer::default();
        buffer.append(&[b'a', 0xff, b'b'], 16);
        let (output, _, bytes, truncated) = buffer.render_window(None);
        assert_eq!(bytes, 3);
        assert!(!truncated);
        assert!(output.starts_with('a'));
        assert!(output.ends_with('b'));
        assert!(output.contains('\u{fffd}'));
    }

    #[test]
    fn terminal_snapshot_renders_cursor_updates_instead_of_raw_escape_sequences() {
        let mut parser = vt100::Parser::new(4, 20, 0);
        parser.process(b"progress 10%\rprogress 90%\nready");
        let snapshot = parser.screen().contents();
        assert!(snapshot.contains("progress 90%"));
        assert!(snapshot.contains("ready"));
        assert!(!snapshot.contains("progress 10%"));
    }

    #[test]
    fn process_signals_have_stable_wire_names() {
        assert!(matches!(
            serde_json::from_value::<ProcessSignal>(json!("interrupt")).unwrap(),
            ProcessSignal::Interrupt
        ));
        assert_eq!(
            serde_json::to_value(ProcessSignal::Terminate).unwrap(),
            json!("terminate")
        );
        assert_eq!(
            serde_json::to_value(ProcessSignal::Kill).unwrap(),
            json!("kill")
        );
        assert!(serde_json::from_value::<ProcessSignal>(json!("unknown")).is_err());
    }

    #[test]
    fn replay_cursor_below_eviction_head_discloses_omitted_prefix() {
        let mut buffer = OutputBuffer::default();
        // Push 10 KiB through a 4 KiB head+tail window. The first/last 2 KiB
        // remain addressable and the 6 KiB middle is omitted.
        for _ in 0..10 {
            buffer.append(&[b'x'; 1024], 4096);
        }
        let (text, start, next, truncated) = buffer.render_window(Some(0));
        // Cursor zero is truthful because the retained head still starts at
        // logical byte zero. The discontinuity is represented explicitly by
        // the omission marker before the retained tail.
        assert_eq!(start, 0);
        assert_eq!(next, buffer.total_bytes);
        assert_eq!(omitted_marker_bytes(&text), 6 * 1024);
        assert!(text.contains("buffered bytes omitted"));
        assert!(truncated);
    }

    #[test]
    fn cursor_beyond_total_bytes_clamps_to_current_end() {
        let mut buffer = OutputBuffer::default();
        buffer.append(b"hello", 64);
        let (text, start, next, _) = buffer.render_window(Some(999_999));
        assert_eq!((start, next), (5, 5));
        assert_eq!(text, "");
    }

    #[test]
    fn render_window_is_idempotent_for_the_same_explicit_cursor() {
        let mut buffer = OutputBuffer::default();
        buffer.append(b"stable", 64);
        let first = buffer.render_window(Some(2));
        let second = buffer.render_window(Some(2));
        assert_eq!(first, second);
        // Explicit-cursor renders still mark the stream delivered to its end,
        // so the next default render has nothing new to emit.
        let advanced = buffer.render_window(None);
        assert_eq!((advanced.1, advanced.2), (6, 6));
        assert_eq!(advanced.0, "");
    }

    #[test]
    fn replay_then_default_poll_never_reemits_delivered_bytes() {
        // Sequence: deliver [0,5), replay an older cursor for recovery, then
        // poll by default. The default poll must not re-emit anything the
        // caller has already seen — including bytes re-shown during replay.
        let mut buffer = OutputBuffer::default();
        buffer.append(b"alpha", 64);
        let first = buffer.render_window(None);
        assert_eq!((first.1, first.2), (0, 5));
        buffer.append(b"beta", 64);
        let second = buffer.render_window(None);
        assert_eq!((second.1, second.2), (5, 9));
        // Lost-response recovery replays from byte 0.
        let replay = buffer.render_window(Some(0));
        assert_eq!(replay.0, "alphabeta");
        // Default poll after recovery: nothing new, nothing duplicated.
        let next = buffer.render_window(None);
        assert_eq!(next.0, "");
        assert_eq!((next.1, next.2), (9, 9));
    }

    #[test]
    fn multibyte_output_never_splits_utf8_at_window_boundary() {
        let mut buffer = OutputBuffer::default();
        buffer.append("alphaβ".as_bytes(), 64);
        buffer.append("γomega".as_bytes(), 64);
        let (text, _, _, _) = buffer.render_window(None);
        assert!(text.contains("alphaβ"));
        assert!(text.contains("γomega"));
        assert!(!text.contains('\u{fffd}'));
    }

    fn omitted_marker_bytes(text: &str) -> usize {
        let marker = "buffered bytes omitted";
        text.find(marker)
            .and_then(|_| text.split("[... ").nth(1))
            .and_then(|rest| rest.split(' ').next())
            .and_then(|count| count.parse::<usize>().ok())
            .unwrap_or_else(|| panic!("omission marker missing in {text}"))
    }

    #[tokio::test]
    async fn yield_result_reports_requested_signal_on_forced_timeout() {
        let session = test_session(Some(9));
        *session.requested_signal.lock().unwrap() = Some(ProcessSignal::Interrupt);
        session.timed_out.store(true, Ordering::Relaxed);
        let value = yield_result("sig-timeout", &session, 10, None, None).await;
        assert_eq!(value["completion_reason"], "exited");
        assert_eq!(value["exit_code"], 9);
        assert_eq!(value["requested_signal"], "interrupt");
        assert_eq!(value["timed_out"], true);
    }
}
