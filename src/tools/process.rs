use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use dashmap::DashMap;
use rmcp::{
    ErrorData, handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{ChildStdin, Command as TokioCommand},
    sync::{Mutex as AsyncMutex, Notify},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod output;
mod pty;
use output::{OutputBuffer, token_window};
use pty::{
    PtyProcess, SharedPtyMaster, pty_size, spawn_pty_process, terminal_dimensions, wait_pty_process,
};

use super::AgentHandler;
use crate::{
    error::{AppError, Result as AppResult},
    project::ProjectContext,
    request_context::ProjectRequestContext,
    sandbox::{
        PathOperation, build_command_with_options_and_runtime_bind, invokes_direct_podman,
        invokes_podman, invokes_sudo_podman,
    },
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const MIN_YIELD_MS: u64 = 250;
const MAX_YIELD_MS: u64 = 30_000;
// Keep the initial MCP request comfortably below common client/proxy request
// deadlines. Long-running commands remain resident and are continued with
// write_stdin instead of risking a transport-level timeout at the boundary.
const MAX_INITIAL_YIELD_MS: u64 = 20_000;
// Follow-up polls are MCP requests too. Keep them under the same conservative
// transport budget as the initial request; callers can repeat write_stdin for
// longer-running processes instead of holding one connector request open.
const MAX_POLL_YIELD_MS: u64 = MAX_INITIAL_YIELD_MS;
const TIMEOUT_COMPLETION_GRACE: Duration = Duration::from_secs(1);
const PODMAN_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const PODMAN_CIDFILE_PREFIX: &str = "cid.";
const PODMAN_EXECUTION_LABEL_KEY: &str = "io.codexbridge.execution";

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ExecCommandArgs {
    #[serde(alias = "cmd")]
    pub command: String,
    #[serde(default)]
    pub workdir: Option<String>,
    #[serde(default)]
    pub shell: Option<String>,
    /// Maximum lifetime of the spawned process. This is not the MCP response
    /// wait; use `yield_time_ms` to control how long the initial call waits
    /// before returning a session_id for continued polling.
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
    /// Deliver a bounded process-control signal. `interrupt` sends Ctrl-C to a PTY or
    /// SIGINT on Unix. Hidden non-TTY Windows processes have no console and reject
    /// `interrupt`; use `terminate` or `kill` for those sessions. `terminate` requests
    /// SIGTERM on Unix or taskkill tree termination on Windows, with a forced fallback
    /// when Windows cannot terminate the hidden process gracefully. `kill` forcefully
    /// ends the process tree.
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

#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowsSignalAction {
    UnsupportedInterrupt,
    Taskkill { force: bool },
}

#[cfg(any(windows, test))]
fn windows_signal_action(signal: ProcessSignal) -> WindowsSignalAction {
    match signal {
        ProcessSignal::Interrupt => WindowsSignalAction::UnsupportedInterrupt,
        ProcessSignal::Terminate => WindowsSignalAction::Taskkill { force: false },
        ProcessSignal::Kill => WindowsSignalAction::Taskkill { force: true },
    }
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

#[derive(Debug, Clone, Copy)]
enum PodmanCleanupMode {
    Direct,
    Sudo,
}

struct PodmanExecutionTracker {
    host_dir: PathBuf,
    sandbox_dir: PathBuf,
    cleanup_modes: Vec<PodmanCleanupMode>,
    execution_label: String,
    configured_host: Option<String>,
    container_host: Option<String>,
    docker_host: Option<String>,
}

impl PodmanExecutionTracker {
    fn prepare(
        config: &crate::config::Config,
        project: &ProjectContext,
        command: &str,
    ) -> AppResult<Option<Self>> {
        if !cfg!(target_os = "linux") {
            return Ok(None);
        }
        validate_podman_tracking_command(command)?;
        if !invokes_podman(command) {
            return Ok(None);
        }

        let id = Uuid::now_v7().simple().to_string();
        let execution_label = format!("{PODMAN_EXECUTION_LABEL_KEY}={id}");
        let host_dir = project
            .metadata_root
            .join("tmp")
            .join(format!("podman-exec-{id}"));
        fs::create_dir(&host_dir)?;
        let setup = (|| -> AppResult<(PathBuf, PathBuf)> {
            #[cfg(unix)]
            fs::set_permissions(&host_dir, fs::Permissions::from_mode(0o700))?;

            let podman_wrapper = host_dir.join("podman");
            fs::write(
                &podman_wrapper,
                r#"#!/bin/sh
track=$CODEXBRIDGE_PODMAN_TRACK_DIR
label=$CODEXBRIDGE_PODMAN_EXEC_LABEL
PATH=$CODEXBRIDGE_PODMAN_REAL_PATH
export PATH
case "${1-}" in
  run|create)
    subcommand=$1
    shift
    has_cidfile=0
    for arg in "$@"; do
      case "$arg" in
        --cidfile|--cidfile=*) has_cidfile=1 ;;
      esac
    done
    if [ "$has_cidfile" -eq 1 ]; then
      exec podman "$subcommand" --label "$label" "$@"
    fi
    exec podman "$subcommand" --label "$label" --cidfile "$track/cid.$$.txt" "$@"
    ;;
  *) exec podman "$@" ;;
esac
"#,
            )?;
            let sudo_wrapper = host_dir.join("sudo");
            fs::write(
                &sudo_wrapper,
                r#"#!/bin/sh
track=$CODEXBRIDGE_PODMAN_TRACK_DIR
label=$CODEXBRIDGE_PODMAN_EXEC_LABEL
container_host=${CONTAINER_HOST-}
docker_host=${DOCKER_HOST-}
PATH=$CODEXBRIDGE_PODMAN_REAL_PATH
export PATH
run_sudo() {
  sudo_opt=$1
  shift
  if [ -n "$sudo_opt" ]; then
    if [ -n "$container_host" ] && [ -n "$docker_host" ]; then
      exec sudo "$sudo_opt" env "CONTAINER_HOST=$container_host" "DOCKER_HOST=$docker_host" "$@"
    elif [ -n "$container_host" ]; then
      exec sudo "$sudo_opt" env "CONTAINER_HOST=$container_host" "$@"
    elif [ -n "$docker_host" ]; then
      exec sudo "$sudo_opt" env "DOCKER_HOST=$docker_host" "$@"
    else
      exec sudo "$sudo_opt" "$@"
    fi
  else
    if [ -n "$container_host" ] && [ -n "$docker_host" ]; then
      exec sudo env "CONTAINER_HOST=$container_host" "DOCKER_HOST=$docker_host" "$@"
    elif [ -n "$container_host" ]; then
      exec sudo env "CONTAINER_HOST=$container_host" "$@"
    elif [ -n "$docker_host" ]; then
      exec sudo env "DOCKER_HOST=$docker_host" "$@"
    else
      exec sudo "$@"
    fi
  fi
}
run_podman() {
  sudo_opt=$1
  podman=$2
  shift 2
  case "${1-}" in
    run|create)
      subcommand=$1
      shift
        has_cidfile=0
      for arg in "$@"; do
        case "$arg" in
            --cidfile|--cidfile=*) has_cidfile=1 ;;
        esac
      done
      cidfile="$track/cid.$$.txt"
        if [ "$has_cidfile" -eq 1 ]; then
      run_sudo "$sudo_opt" "$podman" "$subcommand" --label "$label" "$@"
        fi
    run_sudo "$sudo_opt" "$podman" "$subcommand" --label "$label" --cidfile "$cidfile" "$@"
      ;;
  *) run_sudo "$sudo_opt" "$podman" "$@" ;;
  esac
}
case "${1-}" in
  -n|--non-interactive)
    opt=$1
    shift
    case "${1-}" in
      podman|*/podman)
        podman=$1
        shift
        run_podman "$opt" "$podman" "$@"
        ;;
    esac
    exec sudo "$opt" "$@"
    ;;
  podman|*/podman)
    podman=$1
    shift
    run_podman "" "$podman" "$@"
    ;;
  *) exec sudo "$@" ;;
esac
"#,
            )?;
            #[cfg(unix)]
            for wrapper in [&podman_wrapper, &sudo_wrapper] {
                fs::set_permissions(wrapper, fs::Permissions::from_mode(0o700))?;
            }
            Ok((podman_wrapper, sudo_wrapper))
        })();
        if let Err(error) = setup {
            let _ = fs::remove_dir_all(&host_dir);
            return Err(error);
        }

        let mut cleanup_modes = Vec::with_capacity(2);
        if invokes_direct_podman(command) {
            cleanup_modes.push(PodmanCleanupMode::Direct);
        }
        if invokes_sudo_podman(command) {
            cleanup_modes.push(PodmanCleanupMode::Sudo);
        }
        let sandbox_dir = PathBuf::from(format!("/run/codexbridge-podman-{id}"));
        let configured_host = config
            .container_socket
            .as_ref()
            .map(|socket| format!("unix://{}", socket.to_string_lossy()));
        Ok(Some(Self {
            host_dir,
            sandbox_dir,
            cleanup_modes,
            execution_label,
            configured_host: configured_host.clone(),
            container_host: configured_host.clone(),
            docker_host: configured_host,
        }))
    }

    fn runtime_bind(&self) -> (&Path, &Path) {
        (&self.host_dir, &self.sandbox_dir)
    }

    fn configure_command(
        &mut self,
        command: &mut tokio::process::Command,
        use_bwrap: bool,
        environment: &BTreeMap<String, String>,
    ) {
        let runtime_dir = if use_bwrap {
            &self.sandbox_dir
        } else {
            &self.host_dir
        };
        let base_path = environment.get("PATH").cloned().unwrap_or_else(|| {
            if use_bwrap {
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_owned()
            } else {
                std::env::var("PATH").unwrap_or_else(|_| {
                    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_owned()
                })
            }
        });
        command.env("CODEXBRIDGE_PODMAN_TRACK_DIR", runtime_dir);
        command.env("CODEXBRIDGE_PODMAN_EXEC_LABEL", &self.execution_label);
        command.env("CODEXBRIDGE_PODMAN_REAL_PATH", &base_path);
        command.env("PATH", format!("{}:{base_path}", runtime_dir.display()));
        self.container_host = cleanup_podman_host(
            environment.get("CONTAINER_HOST"),
            self.configured_host.as_ref(),
            use_bwrap,
        );
        self.docker_host = cleanup_podman_host(
            environment.get("DOCKER_HOST"),
            self.configured_host.as_ref(),
            use_bwrap,
        );
    }

    async fn cleanup(&self) -> AppResult<()> {
        let mut failures = Vec::new();
        for mode in &self.cleanup_modes {
            if let Err(error) = self.cleanup_mode(*mode).await {
                failures.push(error.message().to_owned());
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(AppError::new(
                "PROCESS_FAILED",
                format!("Podman forced-exit cleanup failed: {}", failures.join("; ")),
            ))
        }
    }

    async fn cleanup_mode(&self, mode: PodmanCleanupMode) -> AppResult<()> {
        let mut owned = podman_container_ids_from_cidfiles(&self.host_dir)?
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let filter = format!("label={}", self.execution_label);
        let listed = self
            .podman_output(mode, &["ps", "-aq", "--filter", &filter])
            .await?;
        if !listed.status.success() {
            return Err(AppError::new(
                "PROCESS_FAILED",
                format!(
                    "Podman ownership lookup failed: {}",
                    String::from_utf8_lossy(&listed.stderr).trim()
                ),
            ));
        }
        owned.extend(podman_container_ids_from_bytes(&listed.stdout));
        if owned.is_empty() {
            return Ok(());
        }
        let mut remove_args = vec![
            "rm".to_owned(),
            "-f".to_owned(),
            "--time".to_owned(),
            "0".to_owned(),
            "--ignore".to_owned(),
        ];
        remove_args.extend(owned);
        let remove_refs = remove_args.iter().map(String::as_str).collect::<Vec<_>>();
        let removed = self.podman_output(mode, &remove_refs).await?;
        if removed.status.success() {
            Ok(())
        } else {
            Err(AppError::new(
                "PROCESS_FAILED",
                format!(
                    "Podman container removal failed: {}",
                    String::from_utf8_lossy(&removed.stderr).trim()
                ),
            ))
        }
    }

    async fn podman_output(
        &self,
        mode: PodmanCleanupMode,
        args: &[&str],
    ) -> AppResult<std::process::Output> {
        let mut command = match mode {
            PodmanCleanupMode::Direct => TokioCommand::new("podman"),
            PodmanCleanupMode::Sudo => {
                let mut command = TokioCommand::new("sudo");
                command.args(["-n", "env"]);
                if let Some(host) = &self.container_host {
                    command.arg(format!("CONTAINER_HOST={host}"));
                }
                if let Some(host) = &self.docker_host {
                    command.arg(format!("DOCKER_HOST={host}"));
                }
                command.arg("podman");
                command
            }
        };
        command.args(args);
        if matches!(mode, PodmanCleanupMode::Direct) {
            command.env_clear();
            command.env(
                "PATH",
                std::env::var("PATH").unwrap_or_else(|_| {
                    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_owned()
                }),
            );
            if let Some(host) = &self.container_host {
                command.env("CONTAINER_HOST", host);
            }
            if let Some(host) = &self.docker_host {
                command.env("DOCKER_HOST", host);
            }
        }
        tokio::time::timeout(PODMAN_CLEANUP_TIMEOUT, command.output())
            .await
            .map_err(|_| AppError::new("PROCESS_TIMEOUT", "Podman cleanup command timed out"))?
            .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))
    }
}

fn cleanup_podman_host(
    override_value: Option<&String>,
    configured_host: Option<&String>,
    use_bwrap: bool,
) -> Option<String> {
    match override_value {
        Some(value) if use_bwrap && value == "unix:///run/podman.sock" => configured_host.cloned(),
        Some(value) => Some(value.clone()),
        None => configured_host.cloned(),
    }
}

fn validate_podman_tracking_command(command: &str) -> AppResult<()> {
    for segment in command.split(|character: char| "|&;()<>".contains(character)) {
        let tokens = segment
            .split_whitespace()
            .map(|token| token.trim_matches(['\'', '"']))
            .filter(|token| !token.is_empty())
            .collect::<Vec<_>>();
        if let Some(reason) = unsafe_podman_create_invocation(&tokens) {
            return Err(AppError::new(
                "INVALID_INPUT",
                format!(
                    "Podman run/create invocation cannot be safely tracked for timeout/cancel cleanup: {reason}"
                ),
            ));
        }
    }
    Ok(())
}

fn unsafe_podman_create_invocation(tokens: &[&str]) -> Option<&'static str> {
    let mut index = 0usize;
    let mut path_overridden = false;
    let mut sudo = false;
    let mut sudo_absolute = false;
    let mut unsupported_sudo_option = false;

    while index < tokens.len() {
        let token = tokens[index];
        if let Some((name, _)) = token.split_once('=')
            && !name.is_empty()
            && !name.starts_with('-')
        {
            path_overridden |= name == "PATH";
            index += 1;
            continue;
        }
        let base = token.rsplit(['/', '\\']).next().unwrap_or(token);
        match base {
            "env" => {
                index += 1;
                while index < tokens.len() && tokens[index].starts_with('-') {
                    match tokens[index] {
                        "-i" | "--ignore-environment" => {
                            path_overridden = true;
                            index += 1;
                        }
                        "-u" | "--unset" => {
                            path_overridden |= tokens.get(index + 1).copied() == Some("PATH");
                            index = index.saturating_add(2);
                        }
                        "--unset=PATH" => {
                            path_overridden = true;
                            index += 1;
                        }
                        _ => {
                            // Unknown env options can alter command lookup. Treat
                            // them as an unsafe PATH context if this segment later
                            // resolves to a Podman run/create invocation.
                            path_overridden = true;
                            index += 1;
                        }
                    }
                }
            }
            "command" => {
                index += 1;
                while index < tokens.len() && tokens[index].starts_with('-') {
                    path_overridden = true;
                    index += 1;
                }
            }
            "exec" | "nohup" | "rtk" => index += 1,
            "sudo" => {
                sudo = true;
                sudo_absolute = token.contains(['/', '\\']);
                index += 1;
                while index < tokens.len() && tokens[index].starts_with('-') {
                    if !matches!(tokens[index], "-n" | "--non-interactive") {
                        unsupported_sudo_option = true;
                    }
                    if matches!(tokens[index], "-u" | "--user" | "-g" | "--group") {
                        index = index.saturating_add(2);
                    } else {
                        index += 1;
                    }
                }
            }
            _ => break,
        }
    }

    if (index >= tokens.len()
        || tokens[index]
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(tokens[index])
            != "podman")
        && (path_overridden || unsupported_sudo_option)
        && let Some(relative) = tokens[index.min(tokens.len())..]
            .iter()
            .position(|token| token.rsplit(['/', '\\']).next().unwrap_or(token) == "podman")
    {
        index = index.min(tokens.len()).saturating_add(relative);
    }
    if index >= tokens.len() {
        return None;
    }
    let podman_token = tokens[index];
    let podman_base = podman_token
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(podman_token);
    if podman_base != "podman" {
        return None;
    }
    let args = &tokens[index + 1..];
    let direct_subcommand = args.first().copied();
    let directly_creates = matches!(direct_subcommand, Some("run" | "create"));
    let nested_creates = matches!(
        (args.first().copied(), args.get(1).copied()),
        (Some("container"), Some("run" | "create"))
    );
    let later_creates = args.iter().any(|token| matches!(*token, "run" | "create"));
    if !directly_creates && !nested_creates && !later_creates {
        return None;
    }
    if !directly_creates {
        return Some(
            "run/create must be the direct Podman subcommand; global/container aliases bypass the cidfile wrapper",
        );
    }
    if args
        .iter()
        .any(|token| token.contains(PODMAN_EXECUTION_LABEL_KEY))
    {
        return Some("the Bridge-owned Podman execution label may not be overridden");
    }
    if path_overridden {
        return Some("PATH is overridden before Podman, which can bypass the Bridge wrapper");
    }
    if unsupported_sudo_option {
        return Some("unsupported sudo options can bypass the Bridge sudo wrapper");
    }
    if sudo_absolute {
        return Some("an absolute sudo path bypasses the Bridge sudo wrapper");
    }
    if !sudo && podman_token.contains(['/', '\\']) {
        return Some("an absolute Podman path bypasses the Bridge podman wrapper");
    }
    None
}

fn podman_container_ids_from_cidfiles(directory: &Path) -> AppResult<Vec<String>> {
    let mut ids = std::collections::BTreeSet::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(PODMAN_CIDFILE_PREFIX) || !name.ends_with(".txt") {
            continue;
        }
        let value = fs::read_to_string(entry.path())?;
        let id = value.trim();
        if (12..=128).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            ids.insert(id.to_owned());
        }
    }
    Ok(ids.into_iter().collect())
}

fn podman_container_ids_from_bytes(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|byte| *byte == b'\n' || *byte == b'\r')
        .filter_map(|line| std::str::from_utf8(line).ok())
        .map(str::trim)
        .filter(|id| {
            (12..=128).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        .map(str::to_owned)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

impl Drop for PodmanExecutionTracker {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.host_dir);
    }
}

struct InteractiveSession {
    project_key: String,
    stdin: AsyncMutex<Option<ChildStdin>>,
    pty_writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    pty_master: Option<SharedPtyMaster>,
    tty: bool,
    terminal: Option<Mutex<vt100::Parser>>,
    output: Mutex<OutputBuffer>,
    completion: Mutex<Option<ProcessCompletion>>,
    process_deadline_exceeded: AtomicBool,
    /// Set once a finished response is truncated. Keep returning the session id
    /// on cursorless polls so callers do not lose the ability to replay retained
    /// output merely because one follow-up response itself is empty/untruncated.
    replay_pending: AtomicBool,
    requested_signal: Mutex<Option<ProcessSignal>>,
    started: Instant,
    last_activity: Mutex<Instant>,
    /// Completion-relative retention anchor. `last_activity` can be much older
    /// than process exit for commands that run unattended between polls.
    finished_at: Mutex<Option<Instant>>,
    pid: Option<u32>,
    changed: Notify,
    drains_remaining: AtomicUsize,
    drains_finished: Notify,
    /// Limits concurrently executing processes only. Finished sessions may remain
    /// in `ProcessRegistry::entries` for output replay after this permit is released.
    execution_capacity_permit: Mutex<Option<OwnedSemaphorePermit>>,
    process_permits: Mutex<Option<(OwnedSemaphorePermit, OwnedSemaphorePermit)>>,
    podman_tracker: Mutex<Option<PodmanExecutionTracker>>,
    podman_cleanup_requested: AtomicBool,
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

    fn release_execution_permits(&self) {
        if let Ok(mut permit) = self.execution_capacity_permit.lock() {
            permit.take();
        }
        if let Ok(mut permits) = self.process_permits.lock() {
            permits.take();
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
            let mut master = master
                .lock()
                .map_err(|_| AppError::new("PROCESS_FAILED", "PTY master lock poisoned"))?;
            master
                .as_mut()
                .ok_or_else(|| AppError::new("PROCESS_FAILED", "PTY is closed"))?
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

    async fn close_pty_handles_after_exit(&self) {
        if !self.tty {
            return;
        }
        let writer = self.pty_writer.clone();
        let master = self.pty_master.as_ref().cloned();
        let _ = tokio::task::spawn_blocking(move || {
            if let Ok(mut writer) = writer.lock() {
                writer.take();
            }
            if let Some(master) = master
                && let Ok(mut master) = master.lock()
            {
                // On Windows portable-pty keeps the ConPTY handle inside
                // MasterPty. The output pipe is not guaranteed to reach EOF
                // until this value is dropped and ClosePseudoConsole runs.
                // Dropping it on a blocking worker lets the concurrent drain
                // thread consume any final console bytes while Windows closes
                // the pseudo console.
                master.take();
            }
        })
        .await;
    }

    async fn signal(&self, signal: ProcessSignal) -> AppResult<()> {
        if matches!(signal, ProcessSignal::Terminate | ProcessSignal::Kill) {
            self.podman_cleanup_requested.store(true, Ordering::Release);
        }
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

    fn make_room_for_session(&self) {
        while self.entries.len() >= self.maximum {
            let victim = self
                .entries
                .iter()
                .filter(|entry| entry.is_finished())
                .min_by_key(|entry| (entry.replay_pending.load(Ordering::Relaxed), entry.started))
                .map(|entry| entry.key().clone());
            let Some(victim) = victim else {
                break;
            };
            self.entries.remove(&victim);
        }
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
                let expired = if entry.is_finished() {
                    let finished_age = entry
                        .finished_at
                        .lock()
                        .ok()
                        .and_then(|value| *value)
                        .map(|value| now.saturating_duration_since(value))
                        .unwrap_or_default();
                    let retention = if entry.replay_pending.load(Ordering::Relaxed) {
                        self.idle.max(Duration::from_secs(60))
                    } else {
                        Duration::from_secs(60)
                    };
                    finished_age >= retention
                } else {
                    entry
                        .last_activity
                        .lock()
                        .map(|value| now.saturating_duration_since(*value) >= self.idle)
                        .unwrap_or(true)
                };
                expired.then(|| entry.key().clone())
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
        self.make_room_for_session();
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
        let mut podman_tracker = PodmanExecutionTracker::prepare(config, project, &args.command)?;
        let runtime_bind = podman_tracker
            .as_ref()
            .map(PodmanExecutionTracker::runtime_bind);
        let (mut command, use_bwrap) = build_command_with_options_and_runtime_bind(
            config,
            project,
            &args.command,
            true,
            timeout,
            &args.env,
            &workdir,
            args.shell.as_deref(),
            runtime_bind,
        )?;
        if let Some(tracker) = podman_tracker.as_mut() {
            tracker.configure_command(&mut command, use_bwrap, &args.env);
        }
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
                    podman_tracker,
                )
                .await;
        }
        #[cfg(windows)]
        crate::platform::configure_windows_non_tty_process(&mut command);
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
            process_deadline_exceeded: AtomicBool::new(false),
            replay_pending: AtomicBool::new(false),
            requested_signal: Mutex::new(None),
            started: Instant::now(),
            last_activity: Mutex::new(Instant::now()),
            finished_at: Mutex::new(None),
            pid: child.id(),
            changed: Notify::new(),
            drains_remaining: AtomicUsize::new(2),
            drains_finished: Notify::new(),
            execution_capacity_permit: Mutex::new(Some(registry_permit)),
            process_permits: Mutex::new(Some((global_process_permit, project_process_permit))),
            podman_tracker: Mutex::new(podman_tracker),
            podman_cleanup_requested: AtomicBool::new(false),
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
            let (status, forced_reason, mut wait_error) = tokio::select! {
                wait = child.wait() => match wait {
                    Ok(status) => (Some(status), None, None),
                    Err(error) => (None, None, Some(error.to_string())),
                },
                _ = tokio::time::sleep(timeout) => {
                    waiter_session.process_deadline_exceeded.store(true, Ordering::Relaxed);
                    match tokio::time::timeout(TIMEOUT_COMPLETION_GRACE, child.wait()).await {
                        Ok(Ok(status)) => (Some(status), None, None),
                        Ok(Err(error)) => (None, None, Some(error.to_string())),
                        Err(_) => {
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
            cleanup_podman_after_forced_exit(&waiter_session, forced_reason, &mut wait_error).await;
            // A child may exit before Tokio's pipe readers have consumed the final
            // kernel-buffered bytes. Do not publish completion (which makes the
            // registry eligible for removal) until both drains reach EOF. A
            // bounded wait also prevents inherited pipe handles in misbehaving
            // grandchildren from retaining a session forever.
            wait_for_drains(&waiter_session, Duration::from_secs(5)).await;
            if let Ok(mut finished_at) = waiter_session.finished_at.lock() {
                *finished_at = Some(Instant::now());
            }
            if let Ok(mut completion) = waiter_session.completion.lock() {
                *completion = Some(completion_from_exit_status(
                    status,
                    forced_reason,
                    wait_error,
                ));
            }
            waiter_session.release_execution_permits();
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
        podman_tracker: Option<PodmanExecutionTracker>,
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
            process_deadline_exceeded: AtomicBool::new(false),
            replay_pending: AtomicBool::new(false),
            requested_signal: Mutex::new(None),
            started: Instant::now(),
            last_activity: Mutex::new(Instant::now()),
            finished_at: Mutex::new(None),
            pid,
            changed: Notify::new(),
            drains_remaining: AtomicUsize::new(1),
            drains_finished: Notify::new(),
            execution_capacity_permit: Mutex::new(Some(registry_permit)),
            process_permits: Mutex::new(Some((global_process_permit, project_process_permit))),
            podman_tracker: Mutex::new(podman_tracker),
            podman_cleanup_requested: AtomicBool::new(false),
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
            let (status, forced_reason, mut wait_error) = tokio::select! {
                result = &mut wait => match result {
                    Ok(Ok(status)) => (Some(status), None, None),
                    Ok(Err(error)) => (None, None, Some(error.to_string())),
                    Err(error) => (None, None, Some(error.to_string())),
                },
                _ = tokio::time::sleep(timeout) => {
                    waiter_session.process_deadline_exceeded.store(true, Ordering::Relaxed);
                    match tokio::time::timeout(TIMEOUT_COMPLETION_GRACE, &mut wait).await {
                        Ok(Ok(Ok(status))) => (Some(status), None, None),
                        Ok(Ok(Err(error))) => (None, None, Some(error.to_string())),
                        Ok(Err(error)) => (None, None, Some(error.to_string())),
                        Err(_) => {
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
            cleanup_podman_after_forced_exit(&waiter_session, forced_reason, &mut wait_error).await;
            waiter_session.close_pty_handles_after_exit().await;
            wait_for_drains(&waiter_session, Duration::from_secs(5)).await;
            if let Ok(mut finished_at) = waiter_session.finished_at.lock() {
                *finished_at = Some(Instant::now());
            }
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
            waiter_session.release_execution_permits();
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

async fn cleanup_podman_after_forced_exit(
    session: &InteractiveSession,
    forced_reason: Option<CompletionReason>,
    wait_error: &mut Option<String>,
) {
    let explicit_cleanup = session
        .podman_cleanup_requested
        .swap(false, Ordering::AcqRel);
    let tracker = session
        .podman_tracker
        .lock()
        .ok()
        .and_then(|mut tracker| tracker.take());
    let Some(tracker) = tracker else {
        return;
    };
    if !explicit_cleanup
        && !matches!(
            forced_reason,
            Some(CompletionReason::TimedOut | CompletionReason::Cancelled)
        )
    {
        return;
    }
    if let Err(error) = tracker.cleanup().await {
        let cleanup_error = error.message().to_owned();
        match wait_error {
            Some(existing) => {
                existing.push_str("; ");
                existing.push_str(&cleanup_error);
            }
            None => *wait_error = Some(cleanup_error),
        }
    }
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
        let _ = crate::platform::windows_taskkill(pid, true);
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
        match windows_signal_action(signal) {
            WindowsSignalAction::UnsupportedInterrupt => Err(AppError::new(
                "PROCESS_FAILED",
                "interrupt is unavailable for hidden non-TTY Windows processes; use terminate or kill",
            )),
            WindowsSignalAction::Taskkill { force } => {
                let status = crate::platform::windows_taskkill(pid, force)?;
                if status.success() {
                    return Ok(());
                }
                if !force {
                    let forced_status = crate::platform::windows_taskkill(pid, true)?;
                    if forced_status.success() {
                        return Ok(());
                    }
                    return Err(AppError::new(
                        "PROCESS_FAILED",
                        format!(
                            "taskkill failed for process tree {pid} with status {status}; forced fallback failed with status {forced_status}"
                        ),
                    ));
                }
                Err(AppError::new(
                    "PROCESS_FAILED",
                    format!("taskkill failed for process tree {pid} with status {status}"),
                ))
            }
        }
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
    let completion = session.completion();
    let finished = completion.is_some();
    let (output, output_offset, output_next_offset, byte_truncated) = session
        .output
        .lock()
        .map(|mut output| output.render_window(since_output_offset, finished))
        .unwrap_or_else(|_| (String::new(), 0, 0, false));
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
        "process_deadline_exceeded": session.process_deadline_exceeded.load(Ordering::Relaxed),
        "tty": session.tty,
        "terminal_snapshot": terminal_snapshot,
        "wall_time_seconds": session.started.elapsed().as_secs_f64(),
        "continuation": if finished && response_truncated {
              Some("The process finished, but this response was truncated. Call write_stdin with this session_id and since_output_offset to replay retained final output promptly; retained finished sessions can be evicted under bounded registry pressure.")
        } else if replay_pending {
              Some("A previous finished response was truncated. This session remains retained for replay for now; call write_stdin with this session_id and since_output_offset promptly because retained finished sessions can be evicted under bounded registry pressure.")
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
        description = "Start a bounded command with a project-relative working directory. The effective execution backend may be Bubblewrap or native YOLO; native execution is not OS-filesystem-confined. timeout_ms is the maximum lifetime of the spawned process, not the MCP request wait; use yield_time_ms to control how long the initial call waits before returning a session_id. Returns immediately when it exits, otherwise returns a project-scoped session_id for write_stdin. For one-shot CLIs or subagents that may read until EOF, pass optional stdin and close_stdin=true. Set tty=true for a native Unix PTY or Windows ConPTY. Results distinguish normal exit, signal, cancellation, deadline overrun, and forced timeout. output_offset/output_next_offset are logical byte-stream cursors; after bounded head+tail eviction a response can include an explicit omission marker rather than one contiguous original range. Recover lost/truncated presentation with write_stdin(since_output_offset=...) instead of re-running; evicted bytes are unrecoverable. Forward-compatible optional arguments may also be supplied under extensions; typed top-level fields remain preferred. Finished truncated sessions retain a recovery session_id. No extra approval is requested."
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
        description = "Write characters to, close input for, resize, signal, or poll a long-running exec_command process in the active project. For tty sessions, provide rows and cols together to resize before input. signal accepts interrupt, terminate, or kill. interrupt sends Ctrl-C to a PTY or SIGINT on Unix; hidden non-TTY Windows processes have no console and reject interrupt, so use terminate or kill for those sessions. terminate uses SIGTERM on Unix or taskkill tree termination on Windows with a forced fallback when graceful termination is unavailable; kill forcefully ends the tree. wait_for_exit_ms/yield_time_ms are bounded to a transport-safe poll window; repeat write_stdin for longer waits instead of holding one MCP request open. Combine signal with wait_for_exit_ms to wait for terminal completion and drain final output/status when it fits in that poll window. output_offset/output_next_offset are logical stream cursors. Pass since_output_offset to replay retained history after a lost response; if that cursor falls inside an evicted middle region, replay resumes at the first retained tail byte and includes an explicit omission marker, because evicted bytes cannot be recovered. max_output_tokens is only a presentation cap: if replay is token-truncated, retry the same since_output_offset with a larger or omitted cap. Forward-compatible optional arguments may also be supplied under extensions; typed top-level fields remain preferred. PTY results also include a rendered terminal snapshot."
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

    #[cfg(windows)]
    fn windows_test_project(project_dir: &tempfile::TempDir) -> ProjectContext {
        use crate::project::ProjectKey;
        use crate::request_context::TransportMode;

        ProjectContext {
            native_project_key: ProjectKey::new("native".to_owned()).unwrap(),
            effective_project_key: ProjectKey::new("effective".to_owned()).unwrap(),
            project_alias: None,
            project_root: project_dir.path().to_path_buf(),
            metadata_root: project_dir.path().join(".metadata"),
            transport_mode: TransportMode::Stateless,
            mcp_session_present: false,
        }
    }

    #[cfg(windows)]
    fn windows_test_config() -> crate::config::Config {
        use crate::config::ConfigBuilder;

        ConfigBuilder::from_map(BTreeMap::from([
            ("MCP_AUTH_TOKEN".to_owned(), "1234567890abcdef".to_owned()),
            ("MCP_EXEC_SANDBOX".to_owned(), "none".to_owned()),
        ]))
        .build()
        .unwrap()
    }

    #[cfg(windows)]
    fn windows_exec_args(command: impl Into<String>, shell: Option<&str>) -> ExecCommandArgs {
        ExecCommandArgs {
            command: command.into(),
            workdir: None,
            shell: shell.map(str::to_owned),
            timeout_ms: Some(10_000),
            yield_time_ms: None,
            env: BTreeMap::new(),
            max_output_tokens: None,
            stdin: None,
            close_stdin: false,
            tty: false,
            rows: None,
            cols: None,
            extensions: BTreeMap::new(),
        }
    }

    #[cfg(windows)]
    async fn windows_start_exec(
        config: &crate::config::Config,
        project: &ProjectContext,
        args: &ExecCommandArgs,
    ) -> (ProcessRegistry, Arc<InteractiveSession>) {
        let registry = ProcessRegistry::new(4, Duration::from_secs(60), 16 * 1024);
        let global_permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
        let project_permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
        let (_id, session) = registry
            .start(config, project, args, global_permit, project_permit)
            .await
            .unwrap();
        (registry, session)
    }

    #[cfg(windows)]
    async fn windows_wait_for_session(session: &Arc<InteractiveSession>) {
        tokio::time::timeout(Duration::from_secs(20), async {
            while !session.is_finished() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Windows process did not publish completion");
        assert!(wait_for_drains(session, Duration::from_secs(2)).await);
    }

    #[cfg(windows)]
    async fn windows_wait_for_output(session: &Arc<InteractiveSession>, needle: &str) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let (output, _, _, _) =
                    session.output.lock().unwrap().render_window(Some(0), false);
                if output.contains(needle) {
                    break;
                }
                assert!(
                    !session.is_finished(),
                    "Windows process exited before producing {needle:?}: {output:?}"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("Windows process did not produce {needle:?}"));
    }

    #[cfg(windows)]
    fn write_windows_probe_cmd(project_dir: &tempfile::TempDir) {
        std::fs::write(
            project_dir.path().join("hidden-probe.cmd"),
            "@echo off\r\n\"%CODEXBRIDGE_HIDDEN_PROBE_EXE%\" --exact tools::process::tests::windows_hidden_powershell_cmd_shim_console_probe_child --ignored --nocapture\r\nexit /b %ERRORLEVEL%\r\n",
        )
        .unwrap();
    }

    #[cfg(windows)]
    fn windows_hidden_cmd_shim_probe_args() -> ExecCommandArgs {
        let mut args = windows_exec_args("& .\\hidden-probe.cmd", None);
        args.env.insert(
            "CODEXBRIDGE_HIDDEN_PROBE_EXE".to_owned(),
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        );
        args.env.insert(
            "CODEXBRIDGE_HIDDEN_CMD_SHIM_PROBE".to_owned(),
            "1".to_owned(),
        );
        args
    }

    #[cfg(windows)]
    fn assert_windows_exit_success(session: &Arc<InteractiveSession>) {
        let completion = session.completion().expect("hidden process completion");
        assert_eq!(completion.reason, CompletionReason::Exited);
        assert_eq!(completion.exit_code, Some(0));
    }

    #[cfg(windows)]
    fn assert_windows_command_success(session: &Arc<InteractiveSession>) {
        assert_windows_exit_success(session);
        let (output, _, _, _) = session.output.lock().unwrap().render_window(Some(0), true);
        assert!(output.contains("codexbridge-hidden-stdout"), "{output:?}");
        assert!(output.contains("codexbridge-hidden-stderr"), "{output:?}");
    }

    #[cfg(windows)]
    fn windows_long_running_args() -> ExecCommandArgs {
        windows_exec_args(
            "echo codexbridge-hidden-long-ready & ping.exe -n 60 127.0.0.1 >nul",
            Some("cmd"),
        )
    }

    #[cfg(windows)]
    fn windows_tree_probe_args(
        project_dir: &tempfile::TempDir,
    ) -> (ExecCommandArgs, std::path::PathBuf) {
        let pid_file = project_dir.path().join("tree-probe.pid");
        let mut args = windows_exec_args(
            "echo codexbridge-hidden-long-ready & \"%CODEXBRIDGE_TREE_PROBE_EXE%\" --exact tools::process::tests::windows_process_tree_probe_child --ignored --nocapture",
            Some("cmd"),
        );
        args.env.insert(
            "CODEXBRIDGE_TREE_PROBE_EXE".to_owned(),
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        );
        args.env.insert(
            "CODEXBRIDGE_TREE_PROBE_PID_FILE".to_owned(),
            pid_file.to_string_lossy().into_owned(),
        );
        args.env
            .insert("CODEXBRIDGE_TREE_PROBE_CHILD".to_owned(), "1".to_owned());
        (args, pid_file)
    }

    #[cfg(windows)]
    async fn windows_wait_for_probe_pid(pid_file: &std::path::Path) -> u32 {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(value) = std::fs::read_to_string(pid_file)
                    && let Ok(pid) = value.trim().parse::<u32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Windows tree probe child never published its PID")
    }

    #[cfg(windows)]
    fn windows_process_is_running(pid: u32) -> bool {
        use std::os::windows::process::CommandExt as _;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

        let output = std::process::Command::new(crate::platform::windows_system32_executable(
            "tasklist.exe",
        ))
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .expect("tasklist probe must launch");
        assert!(output.status.success(), "tasklist probe failed: {output:?}");
        String::from_utf8_lossy(&output.stdout).contains(&format!(",\"{pid}\","))
    }

    #[cfg(windows)]
    async fn windows_assert_process_exited(pid: u32) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while windows_process_is_running(pid) {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("descendant process {pid} survived process-tree termination"));
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "child harness: invoked by Windows process-tree parent tests"]
    fn windows_process_tree_probe_child() {
        assert_eq!(
            std::env::var_os("CODEXBRIDGE_TREE_PROBE_CHILD").as_deref(),
            Some(std::ffi::OsStr::new("1")),
            "child harness must only run under Windows process-tree parent tests"
        );
        let pid_file =
            std::env::var_os("CODEXBRIDGE_TREE_PROBE_PID_FILE").expect("tree probe child PID file");
        std::fs::write(pid_file, std::process::id().to_string()).unwrap();
        std::thread::sleep(Duration::from_secs(60));
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "child harness: invoked by the Windows native-exit parent test"]
    fn windows_native_exit_probe_child() {
        assert_eq!(
            std::env::var_os("CODEXBRIDGE_NATIVE_EXIT_PROBE_CHILD").as_deref(),
            Some(std::ffi::OsStr::new("1")),
            "child harness must only run under the Windows native-exit parent test"
        );
        std::process::exit(37);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_default_powershell_propagates_native_exit_code_end_to_end() {
        let project_dir = tempfile::tempdir().unwrap();
        let config = windows_test_config();
        let project = windows_test_project(&project_dir);
        let mut args = windows_exec_args(
            "& $env:CODEXBRIDGE_NATIVE_EXIT_PROBE_EXE --exact tools::process::tests::windows_native_exit_probe_child --ignored --nocapture",
            None,
        );
        args.env.insert(
            "CODEXBRIDGE_NATIVE_EXIT_PROBE_EXE".to_owned(),
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        );
        args.env.insert(
            "CODEXBRIDGE_NATIVE_EXIT_PROBE_CHILD".to_owned(),
            "1".to_owned(),
        );

        let (_registry, session) = windows_start_exec(&config, &project, &args).await;
        windows_wait_for_session(&session).await;
        let completion = session.completion().expect("native exit probe completion");
        assert_eq!(completion.reason, CompletionReason::Exited);
        assert_eq!(completion.exit_code, Some(37));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_non_tty_exec_explicit_cmd_keeps_stdout_and_stderr_pipes() {
        let project_dir = tempfile::tempdir().unwrap();
        let config = windows_test_config();
        let project = windows_test_project(&project_dir);
        let args = windows_exec_args(
            "echo codexbridge-hidden-stdout & echo codexbridge-hidden-stderr 1>&2",
            Some("cmd"),
        );

        let (_registry, session) = windows_start_exec(&config, &project, &args).await;
        windows_wait_for_session(&session).await;
        assert_windows_command_success(&session);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_non_tty_exec_default_powershell_keeps_stdout_and_stderr_pipes() {
        let project_dir = tempfile::tempdir().unwrap();
        let config = windows_test_config();
        let project = windows_test_project(&project_dir);
        let args = windows_exec_args(
            "Write-Output 'codexbridge-hidden-stdout'; [Console]::Error.WriteLine('codexbridge-hidden-stderr')",
            None,
        );

        let (_registry, session) = windows_start_exec(&config, &project, &args).await;
        windows_wait_for_session(&session).await;
        assert_windows_command_success(&session);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_hidden_powershell_cmd_shim_does_not_create_nested_console() {
        let project_dir = tempfile::tempdir().unwrap();
        write_windows_probe_cmd(&project_dir);
        let config = windows_test_config();
        let project = windows_test_project(&project_dir);
        let args = windows_hidden_cmd_shim_probe_args();

        let (_registry, session) = windows_start_exec(&config, &project, &args).await;
        windows_wait_for_session(&session).await;
        assert_windows_exit_success(&session);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "child harness: invoked by the Windows hidden PowerShell .cmd parent test"]
    fn windows_hidden_powershell_cmd_shim_console_probe_child() {
        assert_eq!(
            std::env::var_os("CODEXBRIDGE_HIDDEN_CMD_SHIM_PROBE").as_deref(),
            Some(std::ffi::OsStr::new("1")),
            "child harness must only run under the hidden PowerShell .cmd parent test"
        );

        use windows_sys::Win32::System::Console::GetConsoleWindow;

        let window = unsafe { GetConsoleWindow() };
        let console_window_detected =
            std::env::var_os("CODEXBRIDGE_HIDDEN_CMD_SHIM_FORCE_CONSOLE_WINDOW").is_some()
                || !window.is_null();
        assert!(
            !console_window_detected,
            "PowerShell .cmd shim inherited or created a console window"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_hidden_powershell_cmd_shim_parent_rejects_console_probe_failure() {
        let project_dir = tempfile::tempdir().unwrap();
        write_windows_probe_cmd(&project_dir);
        let config = windows_test_config();
        let project = windows_test_project(&project_dir);
        let mut args = windows_hidden_cmd_shim_probe_args();
        args.env.insert(
            "CODEXBRIDGE_HIDDEN_CMD_SHIM_FORCE_CONSOLE_WINDOW".to_owned(),
            "1".to_owned(),
        );

        let (_registry, session) = windows_start_exec(&config, &project, &args).await;
        windows_wait_for_session(&session).await;
        let parent_accepts_failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_windows_exit_success(&session);
        }));
        assert!(
            parent_accepts_failure.is_err(),
            "parent must reject a console-probe child failure"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_hidden_non_tty_interrupt_fails_explicitly_and_kill_still_works() {
        let project_dir = tempfile::tempdir().unwrap();
        let config = windows_test_config();
        let project = windows_test_project(&project_dir);
        let args = windows_long_running_args();

        let (_registry, session) = windows_start_exec(&config, &project, &args).await;
        windows_wait_for_output(&session, "codexbridge-hidden-long-ready").await;

        let error = session.signal(ProcessSignal::Interrupt).await.unwrap_err();
        assert!(
            error
                .message()
                .contains("interrupt is unavailable for hidden non-TTY Windows processes"),
            "{error}"
        );
        assert!(
            !session.is_finished(),
            "unsupported interrupt killed the process"
        );

        session.signal(ProcessSignal::Kill).await.unwrap();
        windows_wait_for_session(&session).await;
        assert!(session.is_finished());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_hidden_non_tty_terminate_still_ends_the_process_tree() {
        let project_dir = tempfile::tempdir().unwrap();
        let config = windows_test_config();
        let project = windows_test_project(&project_dir);
        let (args, pid_file) = windows_tree_probe_args(&project_dir);

        let (_registry, session) = windows_start_exec(&config, &project, &args).await;
        let descendant_pid = windows_wait_for_probe_pid(&pid_file).await;
        assert!(windows_process_is_running(descendant_pid));

        session.signal(ProcessSignal::Terminate).await.unwrap();
        windows_wait_for_session(&session).await;
        assert!(session.is_finished());
        windows_assert_process_exited(descendant_pid).await;
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_hidden_non_tty_timeout_still_kills_and_reaps_the_process() {
        let project_dir = tempfile::tempdir().unwrap();
        let config = windows_test_config();
        let project = windows_test_project(&project_dir);
        let (mut args, pid_file) = windows_tree_probe_args(&project_dir);
        args.timeout_ms = Some(5_000);

        let (_registry, session) = windows_start_exec(&config, &project, &args).await;
        let descendant_pid = windows_wait_for_probe_pid(&pid_file).await;
        assert!(windows_process_is_running(descendant_pid));
        windows_wait_for_session(&session).await;
        let completion = session.completion().expect("timed out hidden completion");
        assert_eq!(completion.reason, CompletionReason::TimedOut);
        assert!(session.process_deadline_exceeded.load(Ordering::Relaxed));
        windows_assert_process_exited(descendant_pid).await;
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_hidden_non_tty_registry_shutdown_still_kills_the_process_tree() {
        let project_dir = tempfile::tempdir().unwrap();
        let config = windows_test_config();
        let project = windows_test_project(&project_dir);
        let (args, pid_file) = windows_tree_probe_args(&project_dir);

        let (registry, session) = windows_start_exec(&config, &project, &args).await;
        let descendant_pid = windows_wait_for_probe_pid(&pid_file).await;
        assert!(windows_process_is_running(descendant_pid));

        registry.shutdown();
        windows_wait_for_session(&session).await;
        let completion = session.completion().expect("shutdown hidden completion");
        assert_ne!(
            completion.exit_code,
            Some(0),
            "registry shutdown allowed the hidden process to exit successfully"
        );
        windows_assert_process_exited(descendant_pid).await;
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_tty_exec_through_start_remains_conpty_backed() {
        let project_dir = tempfile::tempdir().unwrap();
        let config = windows_test_config();
        let project = windows_test_project(&project_dir);
        let mut args = windows_exec_args("echo codexbridge-conpty-through-start", Some("cmd"));
        args.tty = true;
        args.rows = Some(24);
        args.cols = Some(80);

        let (_registry, session) = windows_start_exec(&config, &project, &args).await;
        assert!(
            session.tty,
            "tty=true did not select the ConPTY session path"
        );
        windows_wait_for_session(&session).await;
        let completion = session.completion().expect("ConPTY completion");
        assert_eq!(completion.reason, CompletionReason::Exited);
        assert_eq!(completion.exit_code, Some(0));
        let (output, _, _, _) = session.output.lock().unwrap().render_window(Some(0), true);
        assert!(
            output.contains("codexbridge-conpty-through-start"),
            "{output:?}"
        );
    }

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
                None,
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
    #[test]
    fn windows_bare_cmd_spawns_through_conpty() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::process::tests::windows_bare_cmd_spawns_through_conpty_child",
                "--ignored",
                "--nocapture",
            ])
            .env("CODEXBRIDGE_CONPTY_CMD_PROBE", "1")
            .spawn()
            .unwrap();
        let started = Instant::now();
        loop {
            match child.try_wait().unwrap() {
                Some(status) => {
                    assert!(status.success(), "ConPTY child probe failed: {status}");
                    break;
                }
                None if started.elapsed() < Duration::from_secs(15) => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                None => {
                    let _ = crate::platform::windows_taskkill(child.id(), true);
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("ConPTY cmd probe hung during startup or completion");
                }
            }
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "child harness: invoked by windows_bare_cmd_spawns_through_conpty"]
    async fn windows_bare_cmd_spawns_through_conpty_child() {
        assert_eq!(
            std::env::var_os("CODEXBRIDGE_CONPTY_CMD_PROBE").as_deref(),
            Some(std::ffi::OsStr::new("1")),
            "child harness must only run under windows_bare_cmd_spawns_through_conpty"
        );
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
        let command = crate::sandbox::build_command_with_options(
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
                None,
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
        assert!(
            session.pty_writer.lock().unwrap().is_none(),
            "finished ConPTY session retained its input writer"
        );
        assert!(
            session
                .pty_master
                .as_ref()
                .expect("ConPTY master handle")
                .lock()
                .unwrap()
                .is_none(),
            "finished ConPTY session retained its master handle"
        );

        let completion = session.completion().expect("ConPTY completion");
        assert_eq!(completion.reason, CompletionReason::Exited);
        assert_eq!(completion.exit_code, Some(0));
        let (output, _, _, _) = session.output.lock().unwrap().render_window(Some(0), true);
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
            process_deadline_exceeded: AtomicBool::new(false),
            replay_pending: AtomicBool::new(false),
            requested_signal: Mutex::new(None),
            started: Instant::now(),
            last_activity: Mutex::new(Instant::now()),
            finished_at: Mutex::new(exit_code.map(|_| Instant::now())),
            pid: None,
            changed: Notify::new(),
            drains_remaining: AtomicUsize::new(0),
            drains_finished: Notify::new(),
            execution_capacity_permit: Mutex::new(None),
            process_permits: Mutex::new(None),
            podman_tracker: Mutex::new(None),
            podman_cleanup_requested: AtomicBool::new(false),
            cancellation: CancellationToken::new(),
        })
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn podman_tracker_prepares_cidfile_wrappers() {
        use crate::{config::ConfigBuilder, project::ProjectKey, request_context::TransportMode};

        let directory = tempfile::tempdir().unwrap();
        let project_root = directory.path().join("project");
        let metadata_root = directory.path().join("metadata");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::create_dir_all(metadata_root.join("tmp")).unwrap();
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
            project_root,
            metadata_root,
            transport_mode: TransportMode::Stateless,
            mcp_session_present: false,
        };

        let tracker = PodmanExecutionTracker::prepare(
            &config,
            &project,
            "rtk sudo -n podman run --rm alpine true",
        )
        .unwrap()
        .expect("Podman command should allocate an execution tracker");
        let host_dir = tracker.host_dir.clone();
        assert!(
            std::fs::read_to_string(host_dir.join("podman"))
                .unwrap()
                .contains("--cidfile \"$track/cid.$$.txt\""),
        );
        assert!(
            std::fs::read_to_string(host_dir.join("podman"))
                .unwrap()
                .contains("--label \"$label\""),
        );
        assert!(
            std::fs::read_to_string(host_dir.join("sudo"))
                .unwrap()
                .contains("--cidfile \"$cidfile\""),
        );
        let sudo_wrapper = std::fs::read_to_string(host_dir.join("sudo")).unwrap();
        assert!(
            sudo_wrapper.contains("CONTAINER_HOST=$container_host"),
            "sudo wrapper must preserve the effective Podman daemon context"
        );
        assert!(
            sudo_wrapper.contains("DOCKER_HOST=$docker_host"),
            "sudo wrapper must preserve the effective Docker-compatible daemon context"
        );
        assert_eq!(tracker.cleanup_modes.len(), 1);
        assert!(matches!(tracker.cleanup_modes[0], PodmanCleanupMode::Sudo));
        drop(tracker);
        assert!(
            !host_dir.exists(),
            "tracker temp directory leaked after drop"
        );
    }

    #[test]
    fn podman_tracking_rejects_wrapper_bypass_forms_but_accepts_managed_forms() {
        for command in [
            "podman run --rm alpine true",
            "podman create alpine true",
            "sudo -n podman run --rm alpine true",
            "sudo --non-interactive /usr/bin/podman create alpine true",
            "env FOO=1 podman run --rm alpine true",
            "rtk sudo -n podman run --rm alpine true",
            "podman run --cidfile /tmp/user.cid alpine true",
            "podman run --cidfile=/tmp/user.cid alpine true",
        ] {
            assert!(
                validate_podman_tracking_command(command).is_ok(),
                "managed form rejected: {command}"
            );
        }
        for command in [
            "/usr/bin/podman run --rm alpine true",
            "PATH=/usr/bin podman run --rm alpine true",
            "env PATH=/usr/bin podman run --rm alpine true",
            "env -i podman run --rm alpine true",
            "env -u PATH podman run --rm alpine true",
            "command -p podman run --rm alpine true",
            "/usr/bin/sudo -n podman run --rm alpine true",
            "sudo -E podman run --rm alpine true",
            "sudo -u root podman run --rm alpine true",
            "podman --remote run --rm alpine true",
            "podman container run --rm alpine true",
            "podman run --label io.codexbridge.execution=spoof alpine true",
        ] {
            let error = match validate_podman_tracking_command(command) {
                Ok(()) => panic!("unsafe form accepted: {command}"),
                Err(error) => error,
            };
            assert_eq!(error.code(), "INVALID_INPUT", "{command}");
        }
    }

    #[test]
    fn podman_cleanup_context_preserves_effective_daemon_overrides() {
        let configured = "unix:///run/user/configured.sock".to_owned();
        let override_host = "tcp://127.0.0.1:18888".to_owned();
        assert_eq!(
            cleanup_podman_host(Some(&override_host), Some(&configured), false).as_deref(),
            Some("tcp://127.0.0.1:18888")
        );
        assert_eq!(
            cleanup_podman_host(None, Some(&configured), false).as_deref(),
            Some("unix:///run/user/configured.sock")
        );
        let sandbox_socket = "unix:///run/podman.sock".to_owned();
        assert_eq!(
            cleanup_podman_host(Some(&sandbox_socket), Some(&configured), true).as_deref(),
            Some("unix:///run/user/configured.sock")
        );
    }

    #[tokio::test]
    async fn explicit_terminate_or_kill_requests_owned_podman_cleanup() {
        for signal in [ProcessSignal::Terminate, ProcessSignal::Kill] {
            let session = test_session(Some(0));
            let root = tempfile::tempdir().unwrap();
            let host_dir = root.path().join(format!("tracker-{}", signal.as_str()));
            std::fs::create_dir(&host_dir).unwrap();
            *session.podman_tracker.lock().unwrap() = Some(PodmanExecutionTracker {
                host_dir: host_dir.clone(),
                sandbox_dir: PathBuf::from("/run/unused"),
                cleanup_modes: Vec::new(),
                execution_label: format!("{PODMAN_EXECUTION_LABEL_KEY}=test"),
                configured_host: None,
                container_host: None,
                docker_host: None,
            });
            assert!(!session.podman_cleanup_requested.load(Ordering::Acquire));

            session.signal(signal).await.unwrap();

            assert!(
                session.podman_cleanup_requested.load(Ordering::Acquire),
                "{} did not request Podman cleanup",
                signal.as_str()
            );
            let requested_signal = *session.requested_signal.lock().unwrap();
            assert_eq!(
                requested_signal.map(ProcessSignal::as_str),
                Some(signal.as_str())
            );
            let mut wait_error = None;
            cleanup_podman_after_forced_exit(&session, None, &mut wait_error).await;
            assert!(wait_error.is_none());
            assert!(session.podman_tracker.lock().unwrap().is_none());
            assert!(
                !host_dir.exists(),
                "tracker survived {} cleanup",
                signal.as_str()
            );
        }
    }

    #[test]
    fn podman_cleanup_reads_only_valid_execution_cidfiles() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("cid.10.txt"), "0123456789abcdef\n").unwrap();
        std::fs::write(directory.path().join("cid.11.txt"), "fedcba9876543210\n").unwrap();
        std::fs::write(directory.path().join("cid.12.txt"), "not-a-container-id\n").unwrap();
        std::fs::write(directory.path().join("other.txt"), "aaaaaaaaaaaa\n").unwrap();
        assert_eq!(
            podman_container_ids_from_cidfiles(directory.path()).unwrap(),
            ["0123456789abcdef".to_owned(), "fedcba9876543210".to_owned()]
        );
    }

    #[cfg(target_os = "linux")]
    async fn podman_test_output(
        config: &crate::config::Config,
        args: &[&str],
    ) -> std::process::Output {
        let mut command = if std::env::var_os("CODEXBRIDGE_PODMAN_ROOTFUL_PROBE").is_some() {
            let mut command = TokioCommand::new("sudo");
            command.args(["-n", "env"]);
            if let Some(socket) = config.container_socket.as_ref() {
                command.arg(format!("CONTAINER_HOST=unix://{}", socket.display()));
            }
            command.arg("podman");
            command
        } else {
            let mut command = TokioCommand::new("podman");
            if let Some(socket) = config.container_socket.as_ref() {
                command.env("CONTAINER_HOST", format!("unix://{}", socket.display()));
            }
            command
        };
        command.args(args);
        command.output().await.unwrap_or_else(|error| {
            panic!("live Podman prerequisite failed to launch runtime: {error}")
        })
    }

    #[cfg(target_os = "linux")]
    async fn podman_container_ids(config: &crate::config::Config, name: &str) -> String {
        let filter = format!("name=^{name}$");
        let output = podman_test_output(config, &["ps", "-aq", "--filter", &filter]).await;
        assert!(output.status.success(), "{output:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires a live Podman daemon; run explicitly with CODEXBRIDGE_PODMAN_TIMEOUT_PROBE=1"]
    async fn regression_podman_timeout_and_cancel_remove_daemon_managed_containers() {
        let probe = std::env::var("CODEXBRIDGE_PODMAN_TIMEOUT_PROBE").unwrap_or_else(|_| {
            panic!(
                "live Podman test was explicitly selected but CODEXBRIDGE_PODMAN_TIMEOUT_PROBE is missing; set CODEXBRIDGE_PODMAN_TIMEOUT_PROBE=1"
            )
        });
        assert_eq!(
            probe, "1",
            "CODEXBRIDGE_PODMAN_TIMEOUT_PROBE must be exactly 1 for explicit live Podman execution"
        );
        use crate::{config::ConfigBuilder, project::ProjectKey, request_context::TransportMode};

        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let project_root = workspace.join("effective");
        let metadata_root = workspace.join(".metadata/projects/effective");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::create_dir_all(metadata_root.join("tmp")).unwrap();
        let socket = std::env::var("CODEXBRIDGE_PODMAN_TIMEOUT_SOCKET").unwrap_or_else(|_| {
            panic!(
                "live Podman test requires CODEXBRIDGE_PODMAN_TIMEOUT_SOCKET to name the daemon socket explicitly"
            )
        });
        assert!(
            !socket.trim().is_empty(),
            "CODEXBRIDGE_PODMAN_TIMEOUT_SOCKET must not be empty"
        );
        let socket_metadata = std::fs::metadata(&socket)
            .unwrap_or_else(|error| panic!("live Podman socket {socket} is unavailable: {error}"));
        use std::os::unix::fs::FileTypeExt as _;
        assert!(
            socket_metadata.file_type().is_socket(),
            "CODEXBRIDGE_PODMAN_TIMEOUT_SOCKET must point to a Unix socket: {socket}"
        );
        let config = ConfigBuilder::from_map(BTreeMap::from([
            ("MCP_AUTH_TOKEN".to_owned(), "1234567890abcdef".to_owned()),
            ("MCP_EXEC_SANDBOX".to_owned(), "none".to_owned()),
            ("MCP_CONTAINER_SOCKET".to_owned(), socket.clone()),
            ("EXEC_DEFAULT_TIMEOUT_MS".to_owned(), "5000".to_owned()),
            ("EXEC_MAX_TIMEOUT_MS".to_owned(), "5000".to_owned()),
        ]))
        .build()
        .unwrap();
        let project = ProjectContext {
            native_project_key: ProjectKey::new("native".to_owned()).unwrap(),
            effective_project_key: ProjectKey::new("effective".to_owned()).unwrap(),
            project_alias: None,
            project_root,
            metadata_root,
            transport_mode: TransportMode::Stateless,
            mcp_session_present: false,
        };
        let name = format!("tmp-codexbridge-timeout-{}", Uuid::now_v7().simple());
        let image = std::env::var("CODEXBRIDGE_PODMAN_TEST_IMAGE")
            .unwrap_or_else(|_| "docker.io/library/alpine:latest".to_owned());
        let runtime = podman_test_output(&config, &["info"]).await;
        assert!(
            runtime.status.success(),
            "live Podman runtime is unavailable through {socket}: {}",
            String::from_utf8_lossy(&runtime.stderr).trim()
        );
        let pull = podman_test_output(&config, &["pull", &image]).await;
        assert!(
            pull.status.success(),
            "live Podman test image {image} could not be prepared: {}",
            String::from_utf8_lossy(&pull.stderr).trim()
        );
        let podman = if std::env::var_os("CODEXBRIDGE_PODMAN_ROOTFUL_PROBE").is_some() {
            "sudo -n podman"
        } else {
            "podman"
        };
        let args = ExecCommandArgs {
            command: format!("{podman} run --rm --name {name} {image} sleep 30"),
            workdir: None,
            shell: None,
            timeout_ms: Some(5_000),
            yield_time_ms: Some(250),
            env: BTreeMap::new(),
            max_output_tokens: None,
            stdin: None,
            close_stdin: false,
            tty: false,
            rows: None,
            cols: None,
            extensions: BTreeMap::new(),
        };
        let registry = ProcessRegistry::new(4, Duration::from_secs(60), 4096);
        let global_permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
        let project_permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
        let (_id, session) = registry
            .start(&config, &project, &args, global_permit, project_permit)
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(4), async {
            while podman_container_ids(&config, &name).await.is_empty() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("Podman timeout probe container was never created before its deadline");

        tokio::time::timeout(Duration::from_secs(10), async {
            while !session.is_finished() {
                session.changed.notified().await;
            }
        })
        .await
        .expect("timed-out Podman session never published completion");
        assert_eq!(
            session.completion().unwrap().reason,
            CompletionReason::TimedOut
        );

        assert!(
            podman_container_ids(&config, &name).await.is_empty(),
            "daemon-managed container survived bridge timeout: {name}"
        );

        let cancel_name = format!("tmp-codexbridge-cancel-{}", Uuid::now_v7().simple());
        let cancel_args = ExecCommandArgs {
            command: format!("{podman} run --rm --name {cancel_name} {image} sleep 30"),
            timeout_ms: Some(5_000),
            ..args
        };
        let global_permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
        let project_permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
        let (_id, cancel_session) = registry
            .start(
                &config,
                &project,
                &cancel_args,
                global_permit,
                project_permit,
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while podman_container_ids(&config, &cancel_name).await.is_empty() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("Podman cancel probe container was never created");
        cancel_session.cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !cancel_session.is_finished() {
                cancel_session.changed.notified().await;
            }
        })
        .await
        .expect("cancelled Podman session never published completion");
        assert_eq!(
            cancel_session.completion().unwrap().reason,
            CompletionReason::Cancelled
        );
        assert!(
            podman_container_ids(&config, &cancel_name).await.is_empty(),
            "daemon-managed container survived bridge cancellation: {cancel_name}"
        );

        let terminate_name = format!("tmp-codexbridge-terminate-{}", Uuid::now_v7().simple());
        let terminate_args = ExecCommandArgs {
            command: format!("{podman} run --rm --name {terminate_name} {image} sleep 30"),
            timeout_ms: Some(5_000),
            ..cancel_args
        };
        let global_permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
        let project_permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
        let (_id, terminate_session) = registry
            .start(
                &config,
                &project,
                &terminate_args,
                global_permit,
                project_permit,
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while podman_container_ids(&config, &terminate_name)
                .await
                .is_empty()
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("Podman terminate probe container was never created");
        terminate_session
            .signal(ProcessSignal::Terminate)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !terminate_session.is_finished() {
                terminate_session.changed.notified().await;
            }
        })
        .await
        .expect("terminated Podman session never published completion");
        assert!(
            podman_container_ids(&config, &terminate_name)
                .await
                .is_empty(),
            "daemon-managed container survived explicit terminate: {terminate_name}"
        );

        let shutdown_name = format!("tmp-codexbridge-shutdown-{}", Uuid::now_v7().simple());
        let shutdown_args = ExecCommandArgs {
            command: format!("{podman} run --rm --name {shutdown_name} {image} sleep 30"),
            timeout_ms: Some(5_000),
            ..terminate_args
        };
        let global_permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
        let project_permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
        let (_id, _shutdown_session) = registry
            .start(
                &config,
                &project,
                &shutdown_args,
                global_permit,
                project_permit,
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while podman_container_ids(&config, &shutdown_name)
                .await
                .is_empty()
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("Podman shutdown probe container was never created");
        let (_, remaining) = registry.shutdown_and_wait(Duration::from_millis(500)).await;
        assert_eq!(remaining, 0, "shutdown left active process sessions");
        assert!(
            podman_container_ids(&config, &shutdown_name)
                .await
                .is_empty(),
            "daemon-managed container survived graceful registry shutdown: {shutdown_name}"
        );

        let user_cid_name = format!("tmp-codexbridge-usercid-{}", Uuid::now_v7().simple());
        let user_cid = directory.path().join("user-provided.cid");
        let user_cid_args = ExecCommandArgs {
            command: format!(
                "{podman} run --rm --cidfile {} --name {user_cid_name} {image} sleep 30",
                user_cid.display()
            ),
            workdir: None,
            shell: None,
            timeout_ms: Some(5_000),
            yield_time_ms: Some(250),
            env: BTreeMap::new(),
            max_output_tokens: None,
            stdin: None,
            close_stdin: false,
            tty: false,
            rows: None,
            cols: None,
            extensions: BTreeMap::new(),
        };
        let global_permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
        let project_permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
        let (_id, user_cid_session) = registry
            .start(
                &config,
                &project,
                &user_cid_args,
                global_permit,
                project_permit,
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(4), async {
            while podman_container_ids(&config, &user_cid_name)
                .await
                .is_empty()
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("Podman user-cidfile probe container was never created before its deadline");
        tokio::time::timeout(Duration::from_secs(10), async {
            while !user_cid_session.is_finished() {
                user_cid_session.changed.notified().await;
            }
        })
        .await
        .expect("user-cidfile Podman session never published completion");
        assert_eq!(
            user_cid_session.completion().unwrap().reason,
            CompletionReason::TimedOut
        );
        assert!(user_cid.exists(), "Podman did not honor the user cidfile");
        assert!(
            podman_container_ids(&config, &user_cid_name)
                .await
                .is_empty(),
            "label-based cleanup failed when no Bridge-owned cidfile was injected: {user_cid_name}"
        );
        if std::env::var_os("CODEXBRIDGE_PODMAN_SUDO_PROBE").is_some() {
            let sudo_name = format!("tmp-codexbridge-sudo-{}", Uuid::now_v7().simple());
            let sudo_args = ExecCommandArgs {
                command: format!("sudo -n podman run --rm --name {sudo_name} {image} sleep 30"),
                timeout_ms: Some(5_000),
                ..user_cid_args
            };
            let global_permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
            let project_permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
            let (_id, sudo_session) = registry
                .start(&config, &project, &sudo_args, global_permit, project_permit)
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(4), async {
                while podman_container_ids(&config, &sudo_name).await.is_empty() {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .expect("sudo Podman timeout probe container was never created before its deadline");
            tokio::time::timeout(Duration::from_secs(10), async {
                while !sudo_session.is_finished() {
                    sudo_session.changed.notified().await;
                }
            })
            .await
            .expect("sudo Podman session never published completion");
            assert_eq!(
                sudo_session.completion().unwrap().reason,
                CompletionReason::TimedOut
            );
            assert!(
                podman_container_ids(&config, &sudo_name).await.is_empty(),
                "sudo Podman workload survived timeout or used a mismatched daemon context: {sudo_name}"
            );
        }
    }

    #[test]
    fn cleanup_retains_finished_replay_from_completion_time() {
        let registry = ProcessRegistry::new(4, Duration::from_secs(900), 1024);
        let session = test_session(Some(0));
        session.replay_pending.store(true, Ordering::Relaxed);
        *session.last_activity.lock().unwrap() = Instant::now() - Duration::from_secs(3_600);
        *session.finished_at.lock().unwrap() = Some(Instant::now() - Duration::from_secs(61));
        registry.entries.insert("replay".to_owned(), session);

        registry.cleanup();
        assert!(registry.entries.contains_key("replay"));
    }

    #[test]
    fn cleanup_expires_normal_finished_session_from_completion_time() {
        let registry = ProcessRegistry::new(4, Duration::from_secs(900), 1024);
        let session = test_session(Some(0));
        *session.last_activity.lock().unwrap() = Instant::now() - Duration::from_secs(3_600);
        *session.finished_at.lock().unwrap() = Some(Instant::now() - Duration::from_secs(61));
        registry.entries.insert("finished".to_owned(), session);

        registry.cleanup();
        assert!(!registry.entries.contains_key("finished"));
    }

    #[test]
    fn cleanup_keeps_freshly_finished_session_even_when_last_activity_is_stale() {
        let registry = ProcessRegistry::new(4, Duration::from_secs(900), 1024);
        let session = test_session(Some(0));
        *session.last_activity.lock().unwrap() = Instant::now() - Duration::from_secs(86_400);
        *session.finished_at.lock().unwrap() = Some(Instant::now());
        registry.entries.insert("fresh-finish".to_owned(), session);

        registry.cleanup();
        assert!(registry.entries.contains_key("fresh-finish"));
    }

    #[test]
    fn capacity_pressure_prefers_finished_session_without_pending_replay() {
        let registry = ProcessRegistry::new(2, Duration::from_secs(900), 1024);
        let replay = test_session(Some(0));
        replay.replay_pending.store(true, Ordering::Relaxed);
        let ordinary = test_session(Some(0));
        registry.entries.insert("replay".to_owned(), replay);
        registry.entries.insert("ordinary".to_owned(), ordinary);

        registry.make_room_for_session();

        assert!(registry.entries.contains_key("replay"));
        assert!(!registry.entries.contains_key("ordinary"));
    }

    #[test]
    fn capacity_pressure_evicts_finished_replay_before_blocking_new_execution() {
        let registry = ProcessRegistry::new(1, Duration::from_secs(900), 1024);
        let replay = test_session(Some(0));
        replay.replay_pending.store(true, Ordering::Relaxed);
        registry.entries.insert("replay".to_owned(), replay);

        registry.make_room_for_session();

        assert!(registry.entries.is_empty());
        assert_eq!(registry.capacity.available_permits(), 1);
    }

    #[test]
    fn regression_finished_replays_do_not_consume_execution_capacity() {
        const DEFAULT_MAX_INTERACTIVE_PROCESSES: usize = 32;
        let registry = ProcessRegistry::new(
            DEFAULT_MAX_INTERACTIVE_PROCESSES,
            Duration::from_secs(900),
            1024,
        );

        for index in 0..DEFAULT_MAX_INTERACTIVE_PROCESSES {
            let registry_permit = registry.capacity.clone().try_acquire_owned().unwrap();
            let replay = test_session(Some(0));
            replay.replay_pending.store(true, Ordering::Relaxed);
            *replay.execution_capacity_permit.lock().unwrap() = Some(registry_permit);
            *replay.finished_at.lock().unwrap() = Some(Instant::now() - Duration::from_secs(899));
            replay.release_execution_permits();
            registry.entries.insert(format!("replay-{index}"), replay);
        }

        registry.cleanup();
        assert_eq!(
            registry.active(),
            0,
            "all replay sessions are already finished"
        );
        assert_eq!(registry.entries.len(), DEFAULT_MAX_INTERACTIVE_PROCESSES);
        assert_eq!(
            registry.capacity.available_permits(),
            DEFAULT_MAX_INTERACTIVE_PROCESSES,
            "finished replay sessions must retain output without retaining execution capacity"
        );
        let permit = registry
            .capacity
            .clone()
            .try_acquire_owned()
            .expect("a new process must be able to acquire execution capacity");
        drop(permit);
        assert_eq!(registry.entries.len(), DEFAULT_MAX_INTERACTIVE_PROCESSES);
    }

    #[test]
    fn output_buffer_is_bounded_and_marks_truncation() {
        let mut buffer = OutputBuffer::default();
        buffer.append(b"12345", 6);
        buffer.append(b"67890", 6);
        let (output, start, next, truncated) = buffer.render_window(None, true);
        assert!(output.starts_with("123"));
        assert!(output.ends_with("890"));
        assert!(output.contains("bytes omitted"));
        // The initial summary starts with the retained logical head at byte 0,
        // explicitly marks the evicted middle, and then includes the tail.
        assert_eq!((start, next), (0, 10));
        assert!(truncated);
        // Rendering is non-destructive: the same cursor replays the window.
        let (replayed, replay_start, replay_next, _) = buffer.render_window(Some(0), true);
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
        let (output, start, next, truncated) = buffer.render_window(Some(6), true);
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
        let (output, _, _, _) = session.output.lock().unwrap().render_window(Some(0), true);

        assert_eq!(output, "[stderr] error at src/main.rs:10: boom\n");
    }

    #[test]
    fn output_buffer_under_limit_round_trips_without_marker() {
        let mut buffer = OutputBuffer::default();
        buffer.append(b"hello", 16);
        buffer.append(b" world", 16);
        let (first, _, next, truncated) = buffer.render_window(None, true);
        assert_eq!(first, "hello world");
        assert_eq!(next, 11);
        assert!(!truncated);
        buffer.append(b" again", 16);
        let (second, start, next, _) = buffer.render_window(None, true);
        assert_eq!((second, start, next), (" again".to_owned(), 11, 17));
    }

    #[test]
    fn zero_byte_output_limit_retains_nothing_but_reports_omission() {
        let mut buffer = OutputBuffer::default();
        buffer.append(b"abcdef", 0);
        let (output, _, next, truncated) = buffer.render_window(None, true);
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
            process_deadline_exceeded: AtomicBool::new(false),
            replay_pending: AtomicBool::new(false),
            requested_signal: Mutex::new(None),
            started: Instant::now(),
            last_activity: Mutex::new(Instant::now()),
            finished_at: Mutex::new(None),
            pid: None,
            changed: Notify::new(),
            drains_remaining: AtomicUsize::new(1),
            drains_finished: Notify::new(),
            execution_capacity_permit: Mutex::new(None),
            process_permits: Mutex::new(None),
            podman_tracker: Mutex::new(None),
            podman_cleanup_requested: AtomicBool::new(false),
            cancellation: CancellationToken::new(),
        });
        let tasks = Arc::new(AtomicUsize::new(0));
        spawn_drain(reader, session.clone(), 1024, "", tasks.clone());
        writer.write_all(b"the-final-tail").await.unwrap();
        drop(writer);

        assert!(wait_for_drains(&session, Duration::from_secs(1)).await);
        let (output, _, bytes, truncated) =
            session.output.lock().unwrap().render_window(None, true);
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
        assert_eq!(value["process_deadline_exceeded"], false);
        assert!(value.get("deadline_exceeded").is_none());
        assert!(value.get("timed_out").is_none());
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

    #[tokio::test(start_paused = true)]
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
        // This is a virtual-time deadlock guard, not a wall-clock latency contract.
        // A completion notification must wake the waiter well before its 30s yield deadline.
        let value = tokio::time::timeout(
            Duration::from_secs(1),
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
    fn regression_poll_yield_is_below_common_transport_deadline() {
        assert_eq!(MAX_POLL_YIELD_MS, MAX_INITIAL_YIELD_MS);
        assert_eq!(MAX_POLL_YIELD_MS, 20_000);
    }

    #[test]
    fn output_buffer_replaces_invalid_utf8_without_losing_byte_accounting() {
        let mut buffer = OutputBuffer::default();
        buffer.append(&[b'a', 0xff, b'b'], 16);
        let (output, _, bytes, truncated) = buffer.render_window(None, true);
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
    fn process_signal_schema_documents_platform_specific_semantics() {
        let rendered = serde_json::to_string(&schemars::schema_for!(WriteStdinArgs)).unwrap();
        assert!(rendered.contains("Hidden non-TTY Windows"), "{rendered}");
        assert!(rendered.contains("forced fallback"), "{rendered}");
    }

    #[test]
    fn windows_signal_contract_rejects_hidden_interrupt_without_aliasing_termination() {
        assert_eq!(
            windows_signal_action(ProcessSignal::Interrupt),
            WindowsSignalAction::UnsupportedInterrupt
        );
        assert_eq!(
            windows_signal_action(ProcessSignal::Terminate),
            WindowsSignalAction::Taskkill { force: false }
        );
        assert_eq!(
            windows_signal_action(ProcessSignal::Kill),
            WindowsSignalAction::Taskkill { force: true }
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_taskkill_does_not_depend_on_path() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::process::tests::windows_taskkill_does_not_depend_on_path_child",
                "--ignored",
                "--nocapture",
            ])
            .env("PATH", "")
            .env("CODEXBRIDGE_TASKKILL_PATH_PROBE", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child probe failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "child harness: invoked by windows_taskkill_does_not_depend_on_path"]
    fn windows_taskkill_does_not_depend_on_path_child() {
        assert_eq!(
            std::env::var_os("CODEXBRIDGE_TASKKILL_PATH_PROBE").as_deref(),
            Some(std::ffi::OsStr::new("1")),
            "child harness must only run under windows_taskkill_does_not_depend_on_path"
        );
        let taskkill = std::path::PathBuf::from(
            crate::platform::windows_taskkill_program_for_test(u32::MAX, true),
        );
        assert!(
            taskkill.is_absolute() && taskkill.ends_with(r"System32\taskkill.exe"),
            "taskkill must be launched by absolute System32 path, got {}",
            taskkill.display()
        );
        let error = signal_tree(Some(u32::MAX), ProcessSignal::Kill).unwrap_err();
        assert!(
            error.message().contains("taskkill failed for process tree"),
            "taskkill executable resolution failed before launch: {error}"
        );
    }

    #[test]
    fn replay_cursor_below_eviction_head_discloses_omitted_prefix() {
        let mut buffer = OutputBuffer::default();
        // Push 10 KiB through a 4 KiB head+tail window. The first/last 2 KiB
        // remain addressable and the 6 KiB middle is omitted.
        for _ in 0..10 {
            buffer.append(&[b'x'; 1024], 4096);
        }
        let (text, start, next, truncated) = buffer.render_window(Some(0), true);
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
        let (text, start, next, _) = buffer.render_window(Some(999_999), true);
        assert_eq!((start, next), (5, 5));
        assert_eq!(text, "");
    }

    #[test]
    fn render_window_is_idempotent_for_the_same_explicit_cursor() {
        let mut buffer = OutputBuffer::default();
        buffer.append(b"stable", 64);
        let first = buffer.render_window(Some(2), true);
        let second = buffer.render_window(Some(2), true);
        assert_eq!(first, second);
        // Explicit-cursor renders still mark the stream delivered to its end,
        // so the next default render has nothing new to emit.
        let advanced = buffer.render_window(None, true);
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
        let first = buffer.render_window(None, true);
        assert_eq!((first.1, first.2), (0, 5));
        buffer.append(b"beta", 64);
        let second = buffer.render_window(None, true);
        assert_eq!((second.1, second.2), (5, 9));
        // Lost-response recovery replays from byte 0.
        let replay = buffer.render_window(Some(0), true);
        assert_eq!(replay.0, "alphabeta");
        // Default poll after recovery: nothing new, nothing duplicated.
        let next = buffer.render_window(None, true);
        assert_eq!(next.0, "");
        assert_eq!((next.1, next.2), (9, 9));
    }

    #[test]
    fn multibyte_output_never_splits_utf8_at_window_boundary() {
        let mut buffer = OutputBuffer::default();
        buffer.append("alphaβ".as_bytes(), 64);
        buffer.append("γomega".as_bytes(), 64);
        let (text, _, _, _) = buffer.render_window(None, true);
        assert!(text.contains("alphaβ"));
        assert!(text.contains("γomega"));
        assert!(!text.contains('\u{fffd}'));
    }

    #[test]
    fn regression_streaming_poll_waits_for_split_utf8_codepoint() {
        let mut buffer = OutputBuffer::default();
        buffer.append(&[0xF0], 64);

        let first = buffer.render_window(None, false);
        assert_eq!(first.0, "");
        assert_eq!((first.1, first.2), (0, 0));
        assert_eq!(buffer.delivered, 0);

        buffer.append(&[0x9F, 0x99, 0x82], 64);
        let second = buffer.render_window(None, false);
        assert_eq!(second.0, "🙂");
        assert_eq!((second.1, second.2), (0, 4));
        assert_eq!(buffer.delivered, 4);
        assert!(!second.0.contains('\u{fffd}'));
    }

    #[test]
    fn bounded_retention_never_cuts_valid_utf8_scalars() {
        let mut buffer = OutputBuffer::default();
        buffer.append("🙂".as_bytes(), 6);
        buffer.append("🙂".as_bytes(), 6);

        let (text, _, next, truncated) = buffer.render_window(Some(0), true);
        assert_eq!(next, 8);
        assert!(truncated);
        assert!(!text.contains('\u{fffd}'), "{text:?}");
        assert!(
            text.contains('🙂'),
            "complete retained scalar missing: {text:?}"
        );
    }

    #[test]
    fn podman_cleanup_parses_only_valid_labeled_container_ids() {
        assert_eq!(
            podman_container_ids_from_bytes(
                b"0123456789abcdef\nnot-an-id\r\nfedcba9876543210\n0123456789abcdef\n"
            ),
            ["0123456789abcdef".to_owned(), "fedcba9876543210".to_owned()]
        );
    }

    #[test]
    fn valid_utf8_never_gains_replacement_chars_across_small_retention_limits() {
        let source = "a🙂β界z🙂";
        for limit in 1..=24 {
            let mut buffer = OutputBuffer::default();
            for byte in source.as_bytes() {
                buffer.append(std::slice::from_ref(byte), limit);
                assert!(buffer.retained.len() <= limit, "limit={limit}");
            }
            let (text, _, next, truncated) = buffer.render_window(Some(0), true);
            assert_eq!(next, source.len(), "limit={limit}");
            assert_eq!(truncated, source.len() > limit, "limit={limit}");
            assert!(
                !text.contains('\u{fffd}'),
                "valid UTF-8 gained a replacement character at limit={limit}: {text:?}"
            );
        }
    }

    #[test]
    fn replay_cursor_inside_valid_utf8_scalar_advances_with_disclosure() {
        let mut buffer = OutputBuffer::default();
        buffer.append("a🙂b".as_bytes(), 64);

        let (text, start, next, truncated) = buffer.render_window(Some(2), true);
        assert_eq!(start, 5);
        assert_eq!(next, 6);
        assert!(!truncated);
        assert!(text.contains("3 UTF-8 boundary bytes omitted"), "{text:?}");
        assert!(text.ends_with('b'), "{text:?}");
        assert!(!text.contains('\u{fffd}'), "{text:?}");
    }

    #[test]
    fn token_window_never_splits_utf16_surrogate_pairs() {
        let (text, original) = token_window("a🙂bc🙂d".to_owned(), Some(1));
        assert_eq!(original, Some(2));
        assert!(!text.contains('\u{fffd}'), "{text:?}");
        assert!(text.starts_with('a'), "{text:?}");
        assert!(text.ends_with('d'), "{text:?}");
        assert!(text.contains("UTF-16 code units omitted"), "{text:?}");
    }

    #[test]
    fn finished_stream_flushes_incomplete_utf8_as_replacement() {
        let mut buffer = OutputBuffer::default();
        buffer.append(&[0xF0], 64);

        let running = buffer.render_window(None, false);
        assert_eq!((running.0.as_str(), running.2), ("", 0));

        let finished = buffer.render_window(None, true);
        assert_eq!(finished.0, "�");
        assert_eq!(finished.2, 1);
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
        let session = test_session(None);
        *session.completion.lock().unwrap() = Some(ProcessCompletion {
            reason: CompletionReason::TimedOut,
            exit_code: Some(9),
            signal: None,
            error: None,
        });
        *session.requested_signal.lock().unwrap() = Some(ProcessSignal::Interrupt);
        session
            .process_deadline_exceeded
            .store(true, Ordering::Relaxed);
        let value = yield_result("sig-timeout", &session, 10, None, None).await;
        assert_eq!(value["completion_reason"], "timed_out");
        assert_eq!(value["exit_code"], 9);
        assert_eq!(value["requested_signal"], "interrupt");
        assert_eq!(value["process_deadline_exceeded"], true);
        assert!(value.get("deadline_exceeded").is_none());
        assert!(value.get("timed_out").is_none());
    }
}
