use std::{
    ffi::OsString,
    io::{Read, Write},
    sync::{Arc, Mutex},
    time::Duration,
};

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::error::{AppError, Result as AppResult};

pub(super) type SharedPtyMaster = Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>;

pub(super) struct PtyProcess {
    pub child: Box<dyn portable_pty::Child + Send>,
    pub killer: Box<dyn ChildKiller + Send + Sync>,
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    pub master: SharedPtyMaster,
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PtyExitStatus {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
}

pub(super) fn wait_pty_process(
    mut child: Box<dyn portable_pty::Child + Send>,
    pid: Option<u32>,
) -> AppResult<PtyExitStatus> {
    #[cfg(not(unix))]
    let _ = pid;
    #[cfg(unix)]
    if let Some(pid) = pid {
        let mut status = 0_i32;
        loop {
            let waited = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) };
            if waited < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(AppError::new("PROCESS_FAILED", error.to_string()));
            }
            if libc::WIFEXITED(status) {
                return Ok(PtyExitStatus {
                    exit_code: Some(libc::WEXITSTATUS(status)),
                    signal: None,
                });
            }
            if libc::WIFSIGNALED(status) {
                return Ok(PtyExitStatus {
                    exit_code: None,
                    signal: Some(libc::WTERMSIG(status)),
                });
            }
        }
    }

    let status = child
        .wait()
        .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))?;
    Ok(PtyExitStatus {
        exit_code: Some(status.exit_code().min(i32::MAX as u32) as i32),
        signal: None,
    })
}

pub(super) fn terminal_dimensions(rows: Option<u16>, cols: Option<u16>) -> AppResult<(u16, u16)> {
    match (rows, cols) {
        (None, None) => Ok((24, 80)),
        (Some(rows), Some(cols)) if (1..=1000).contains(&rows) && (1..=1000).contains(&cols) => {
            Ok((rows, cols))
        }
        (Some(_), Some(_)) => Err(AppError::new(
            "INVALID_INPUT",
            "PTY rows and cols must be between 1 and 1000",
        )),
        _ => Err(AppError::new(
            "INVALID_INPUT",
            "PTY rows and cols must be supplied together",
        )),
    }
}

pub(super) fn pty_size(rows: u16, cols: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn pty_bootstrap_input_for_platform(windows: bool) -> &'static [u8] {
    if windows {
        // portable-pty 0.9 creates ConPTY with PSEUDOCONSOLE_INHERIT_CURSOR.
        // A headless host must answer ConPTY's initial cursor-position query or
        // cmd.exe/PowerShell can remain blocked waiting for the terminal reply.
        b"\x1b[1;1R"
    } else {
        &[]
    }
}

fn pty_argv(command: &tokio::process::Command, timeout: Duration) -> Vec<OsString> {
    #[cfg(not(unix))]
    let _ = timeout;
    let command = command.as_std();
    let original: Vec<OsString> = std::iter::once(command.get_program().to_os_string())
        .chain(command.get_args().map(ToOwned::to_owned))
        .collect();
    #[cfg(target_os = "linux")]
    if std::path::Path::new("/usr/bin/prlimit").is_file() {
        let mut argv = vec![
            OsString::from("/usr/bin/prlimit"),
            OsString::from(format!(
                "--cpu={}:{}",
                timeout.as_secs().saturating_add(2).max(2),
                timeout.as_secs().saturating_add(3).max(3)
            )),
            OsString::from("--nofile=256:256"),
            OsString::from("--"),
        ];
        argv.extend(original);
        return argv;
    }
    #[cfg(unix)]
    {
        let mut argv = vec![
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from(format!(
                "ulimit -t {}; ulimit -n 256; exec \"$@\"",
                timeout.as_secs().saturating_add(2).max(2)
            )),
            OsString::from("pty-resource-limits"),
        ];
        argv.extend(original);
        argv
    }
    #[cfg(not(unix))]
    original
}

pub(super) fn spawn_pty_process(
    command: &tokio::process::Command,
    timeout: Duration,
    rows: u16,
    cols: u16,
) -> AppResult<PtyProcess> {
    let command_std = command.as_std();
    let argv = pty_argv(command, timeout);
    let mut builder = CommandBuilder::from_argv(argv);
    builder.env_clear();
    for (key, value) in command_std.get_envs() {
        if let Some(value) = value {
            builder.env(key, value);
        }
    }
    if let Some(cwd) = command_std.get_current_dir() {
        builder.cwd(cwd);
    }
    let pair = native_pty_system()
        .openpty(pty_size(rows, cols))
        .map_err(|error| AppError::new("SANDBOX_UNAVAILABLE", error.to_string()))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))?;
    let bootstrap = pty_bootstrap_input_for_platform(cfg!(windows));
    if !bootstrap.is_empty() {
        writer
            .write_all(bootstrap)
            .and_then(|_| writer.flush())
            .map_err(|error| {
                AppError::new(
                    "PROCESS_FAILED",
                    format!("failed to initialize PTY input: {error}"),
                )
            })?;
    }
    // Seed the ConPTY cursor response before spawning the child. On Windows the
    // inherited-cursor handshake can otherwise block shell startup inside
    // spawn_command, which is too early for a post-spawn writer to recover it.
    let child = pair
        .slave
        .spawn_command(builder)
        .map_err(|error| AppError::new("SANDBOX_UNAVAILABLE", error.to_string()))?;
    drop(pair.slave);
    let pid = child.process_id();
    let killer = child.clone_killer();
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))?;
    Ok(PtyProcess {
        child,
        killer,
        reader,
        writer,
        master: Arc::new(Mutex::new(Some(pair.master))),
        pid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_wrapper_does_not_apply_uid_wide_nproc_limit() {
        let mut command = tokio::process::Command::new("/bin/sh");
        command.arg("-c").arg("true");
        let argv = pty_argv(&command, Duration::from_secs(30));
        let rendered = argv
            .iter()
            .map(|value| value.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!rendered.contains("--nproc"));
        assert!(!rendered.contains("ulimit -u"));
    }

    #[test]
    fn windows_conpty_bootstrap_answers_inherited_cursor_query() {
        assert_eq!(pty_bootstrap_input_for_platform(true), b"\x1b[1;1R");
        assert!(pty_bootstrap_input_for_platform(false).is_empty());
    }
}
