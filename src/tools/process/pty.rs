use std::{
    ffi::OsString,
    io::{Read, Write},
    sync::{Arc, Mutex},
    time::Duration,
};

#[cfg(not(windows))]
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::error::{AppError, Result as AppResult};

pub(super) trait PtyMasterControl: Send {
    fn resize(&mut self, rows: u16, cols: u16) -> AppResult<()>;
}

#[cfg(not(windows))]
struct PortablePtyMaster(Box<dyn MasterPty + Send>);

#[cfg(not(windows))]
impl PtyMasterControl for PortablePtyMaster {
    fn resize(&mut self, rows: u16, cols: u16) -> AppResult<()> {
        self.0
            .resize(portable_pty_size(rows, cols))
            .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))
    }
}

#[cfg(windows)]
struct WindowsPtyMaster(conpty_oxide::PtyController);

#[cfg(windows)]
impl PtyMasterControl for WindowsPtyMaster {
    fn resize(&mut self, rows: u16, cols: u16) -> AppResult<()> {
        let size = conpty_oxide::Size::try_new(cols, rows)
            .map_err(|error| AppError::new("INVALID_INPUT", error.to_string()))?;
        self.0
            .resize(size)
            .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))
    }
}

pub(super) type SharedPtyMaster = Arc<Mutex<Option<Box<dyn PtyMasterControl>>>>;

#[cfg(not(windows))]
type PtyChild = Box<dyn portable_pty::Child + Send>;

#[cfg(windows)]
type PtyChild = conpty_oxide::blocking::Child;

pub(super) struct PtyKiller {
    #[cfg(not(windows))]
    inner: Box<dyn ChildKiller + Send + Sync>,
    #[cfg(windows)]
    pid: u32,
}

impl PtyKiller {
    pub(super) fn kill(&mut self) -> AppResult<()> {
        #[cfg(not(windows))]
        {
            self.inner
                .kill()
                .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))
        }
        #[cfg(windows)]
        {
            let status = crate::platform::windows_taskkill(self.pid, true)?;
            if status.success() {
                Ok(())
            } else {
                Err(AppError::new(
                    "PROCESS_FAILED",
                    format!(
                        "taskkill failed for ConPTY process tree {} with status {status}",
                        self.pid
                    ),
                ))
            }
        }
    }
}

pub(super) struct PtyProcess {
    pub child: PtyChild,
    pub killer: PtyKiller,
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

#[cfg(not(windows))]
pub(super) fn wait_pty_process(mut child: PtyChild, pid: Option<u32>) -> AppResult<PtyExitStatus> {
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

#[cfg(windows)]
pub(super) fn wait_pty_process(mut child: PtyChild, pid: Option<u32>) -> AppResult<PtyExitStatus> {
    let _ = pid;
    let status = child
        .wait()
        .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))?;
    Ok(PtyExitStatus {
        exit_code: Some(status.code().min(i32::MAX as u32) as i32),
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

#[cfg(not(windows))]
fn portable_pty_size(rows: u16, cols: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

#[cfg(not(windows))]
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

#[cfg(windows)]
#[derive(Debug, PartialEq, Eq)]
struct WindowsPtyCommandLine {
    program: OsString,
    args: Vec<OsString>,
    raw_arg: Option<OsString>,
}

#[cfg(windows)]
fn windows_pty_command_line(
    command: &tokio::process::Command,
    shell_command_text: &str,
) -> WindowsPtyCommandLine {
    use std::path::Path;

    let command = command.as_std();
    let program = command.get_program().to_os_string();
    let is_cmd = Path::new(command.get_program())
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.eq_ignore_ascii_case("cmd") || name.eq_ignore_ascii_case("cmd.exe")
        });
    let args = if is_cmd {
        ["/d", "/s", "/c"].into_iter().map(OsString::from).collect()
    } else {
        command.get_args().map(ToOwned::to_owned).collect()
    };
    let raw_arg = is_cmd.then(|| OsString::from(format!("\"{shell_command_text}\"")));
    WindowsPtyCommandLine {
        program,
        args,
        raw_arg,
    }
}

#[cfg(windows)]
fn windows_conpty_current_dir(path: &std::path::Path) -> std::path::PathBuf {
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};

    const BACKSLASH: u16 = b'\\' as u16;
    const QUESTION: u16 = b'?' as u16;
    let wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if !wide.starts_with(&[BACKSLASH, BACKSLASH, QUESTION, BACKSLASH]) {
        return path.to_path_buf();
    }

    let rest = &wide[4..];
    let is_unc = rest.len() >= 4
        && matches!(rest[0], value if value == b'U' as u16 || value == b'u' as u16)
        && matches!(rest[1], value if value == b'N' as u16 || value == b'n' as u16)
        && matches!(rest[2], value if value == b'C' as u16 || value == b'c' as u16)
        && rest[3] == BACKSLASH;
    let normalized = if is_unc {
        let mut value = Vec::with_capacity(rest.len().saturating_sub(2));
        value.extend_from_slice(&[BACKSLASH, BACKSLASH]);
        value.extend_from_slice(&rest[4..]);
        value
    } else {
        rest.to_vec()
    };
    std::path::PathBuf::from(std::ffi::OsString::from_wide(&normalized))
}

#[cfg(not(windows))]
pub(super) fn spawn_pty_process(
    command: &tokio::process::Command,
    timeout: Duration,
    rows: u16,
    cols: u16,
    shell_command_text: &str,
) -> AppResult<PtyProcess> {
    let _ = shell_command_text;
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
        .openpty(portable_pty_size(rows, cols))
        .map_err(|error| AppError::new("SANDBOX_UNAVAILABLE", error.to_string()))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))?;
    let child = pair
        .slave
        .spawn_command(builder)
        .map_err(|error| AppError::new("SANDBOX_UNAVAILABLE", error.to_string()))?;
    drop(pair.slave);
    let pid = child.process_id();
    let killer = PtyKiller {
        inner: child.clone_killer(),
    };
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))?;
    Ok(PtyProcess {
        child,
        killer,
        reader,
        writer,
        master: Arc::new(Mutex::new(Some(Box::new(PortablePtyMaster(pair.master))))),
        pid,
    })
}

#[cfg(windows)]
pub(super) fn spawn_pty_process(
    command: &tokio::process::Command,
    timeout: Duration,
    rows: u16,
    cols: u16,
    shell_command_text: &str,
) -> AppResult<PtyProcess> {
    let _ = timeout;
    let command_std = command.as_std();
    let command_line = windows_pty_command_line(command, shell_command_text);
    let mut builder = conpty_oxide::blocking::Command::new(&command_line.program);
    builder.args(&command_line.args);
    if let Some(raw_arg) = &command_line.raw_arg {
        builder.raw_arg(raw_arg);
    }
    builder.env_clear();
    for (key, value) in command_std.get_envs() {
        if let Some(value) = value {
            builder.env(key, value);
        }
    }
    if let Some(cwd) = command_std.get_current_dir() {
        builder.current_dir(windows_conpty_current_dir(cwd));
    }
    let size = conpty_oxide::Size::try_new(cols, rows)
        .map_err(|error| AppError::new("INVALID_INPUT", error.to_string()))?;
    let session = builder
        .spawn_with(conpty_oxide::SessionOptions::new().size(size))
        .map_err(|error| AppError::new("SANDBOX_UNAVAILABLE", error.to_string()))?;
    let conpty_oxide::blocking::SessionParts {
        child,
        output,
        input,
        controller,
        ..
    } = session.into_parts();
    let pid = child.id();
    Ok(PtyProcess {
        child,
        killer: PtyKiller { pid },
        reader: Box::new(output),
        writer: Box::new(input),
        master: Arc::new(Mutex::new(Some(Box::new(WindowsPtyMaster(controller))))),
        pid: Some(pid),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(not(windows))]
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

    #[cfg(windows)]
    #[test]
    fn windows_cmd_pty_conversion_preserves_raw_command_tail() {
        use std::os::windows::process::CommandExt as _;

        let command_text = r#"echo ok & "C:\Program Files\tool.exe" --probe"#;
        let expected_raw_arg = format!("\"{command_text}\"");
        let mut command = tokio::process::Command::new(r"C:\Windows\System32\cmd.exe");
        command.args(["/d", "/s", "/c"]);
        command.as_std_mut().raw_arg(&expected_raw_arg);

        let converted = windows_pty_command_line(&command, command_text);
        assert_eq!(converted.program, command.as_std().get_program());
        assert_eq!(
            converted.args,
            [
                OsString::from("/d"),
                OsString::from("/s"),
                OsString::from("/c")
            ]
        );
        assert_eq!(converted.raw_arg, Some(OsString::from(expected_raw_arg)));
    }

    #[cfg(windows)]
    #[test]
    fn windows_conpty_current_dir_removes_verbatim_prefixes() {
        assert_eq!(
            windows_conpty_current_dir(std::path::Path::new(r"\\?\C:\Temp\codexbridge")),
            std::path::PathBuf::from(r"C:\Temp\codexbridge")
        );
        assert_eq!(
            windows_conpty_current_dir(std::path::Path::new(r"\\?\UNC\server\share\codexbridge")),
            std::path::PathBuf::from(r"\\server\share\codexbridge")
        );
        assert_eq!(
            windows_conpty_current_dir(std::path::Path::new(r"C:\Temp\codexbridge")),
            std::path::PathBuf::from(r"C:\Temp\codexbridge")
        );
    }
}
