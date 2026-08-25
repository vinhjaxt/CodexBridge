use std::{ffi::OsString, path::PathBuf};

use tokio::process::Command;

fn nonempty(value: Option<OsString>) -> Option<OsString> {
    value.filter(|value| !value.is_empty())
}

fn user_home_dir_from_values(
    home: Option<OsString>,
    user_profile: Option<OsString>,
    home_drive: Option<OsString>,
    home_path: Option<OsString>,
    windows: bool,
) -> Option<PathBuf> {
    if let Some(home) = nonempty(home) {
        return Some(PathBuf::from(home));
    }
    if !windows {
        return None;
    }
    if let Some(profile) = nonempty(user_profile) {
        return Some(PathBuf::from(profile));
    }
    let mut drive = nonempty(home_drive)?;
    drive.push(nonempty(home_path)?);
    Some(PathBuf::from(drive))
}

pub(crate) fn user_home_dir() -> Option<PathBuf> {
    user_home_dir_from_values(
        std::env::var_os("HOME"),
        std::env::var_os("USERPROFILE"),
        std::env::var_os("HOMEDRIVE"),
        std::env::var_os("HOMEPATH"),
        cfg!(windows),
    )
}

/// Stdio upstreams intentionally receive a small environment rather than the
/// daemon's complete environment. The baseline itself must still be native to
/// the host OS so a Windows upstream and its child processes can resolve the
/// normal system tools.
pub(crate) fn configure_upstream_stdio_environment(command: &mut Command) {
    command.env_clear();
    #[cfg(windows)]
    {
        let system_root = nonempty(std::env::var_os("SystemRoot"))
            .unwrap_or_else(|| OsString::from(r"C:\Windows"));
        let path = nonempty(std::env::var_os("PATH")).unwrap_or_else(|| {
            OsString::from(format!(
                r"{}\System32;{};{}\System32\WindowsPowerShell\v1.0",
                system_root.to_string_lossy(),
                system_root.to_string_lossy(),
                system_root.to_string_lossy()
            ))
        });
        command.env("PATH", path);
        command.env("SystemRoot", &system_root);
        command.env("WINDIR", &system_root);
        if let Some(comspec) = nonempty(std::env::var_os("ComSpec")) {
            command.env("ComSpec", comspec);
        }
        let temporary = std::env::temp_dir();
        command.env("TEMP", &temporary);
        command.env("TMP", &temporary);
    }
    #[cfg(not(windows))]
    {
        command.env("PATH", "/usr/local/bin:/usr/bin:/bin");
        command.env("LANG", "C.UTF-8");
    }
}

#[cfg(windows)]
pub(crate) fn windows_system32_executable(name: &str) -> PathBuf {
    let system_root = nonempty(std::env::var_os("SystemRoot"))
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    system_root.join("System32").join(name)
}

#[cfg(windows)]
pub(crate) fn windows_taskkill(pid: u32, force: bool) -> std::io::Result<std::process::ExitStatus> {
    let mut command = std::process::Command::new(windows_system32_executable("taskkill.exe"));
    command.args(["/T", "/PID", &pid.to_string()]);
    if force {
        command.arg("/F");
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
}

#[cfg(windows)]
pub(crate) fn configure_windows_process_group(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;

    command
        .as_std_mut()
        .creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);
}

#[cfg(windows)]
pub(crate) fn windows_send_interrupt(process_group_id: u32) -> std::io::Result<()> {
    let sent = unsafe {
        windows_sys::Win32::System::Console::GenerateConsoleCtrlEvent(
            windows_sys::Win32::System::Console::CTRL_BREAK_EVENT,
            process_group_id,
        )
    };
    if sent == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_resolution_prefers_home_then_windows_fallbacks() {
        assert_eq!(
            user_home_dir_from_values(
                Some(OsString::from("/home/agent")),
                Some(OsString::from(r"C:\Users\agent")),
                None,
                None,
                true,
            ),
            Some(PathBuf::from("/home/agent"))
        );
        assert_eq!(
            user_home_dir_from_values(
                None,
                Some(OsString::from(r"C:\Users\agent")),
                None,
                None,
                true,
            ),
            Some(PathBuf::from(r"C:\Users\agent"))
        );
        assert_eq!(
            user_home_dir_from_values(
                None,
                None,
                Some(OsString::from("C:")),
                Some(OsString::from(r"\Users\agent")),
                true,
            ),
            Some(PathBuf::from(r"C:\Users\agent"))
        );
        assert_eq!(
            user_home_dir_from_values(None, None, None, None, false),
            None
        );
    }

    #[test]
    fn upstream_environment_is_platform_native() {
        let mut command = Command::new("placeholder");
        configure_upstream_stdio_environment(&mut command);
        let path = command
            .as_std()
            .get_envs()
            .find(|(key, _)| key.to_string_lossy().eq_ignore_ascii_case("PATH"))
            .and_then(|(_, value)| value)
            .expect("PATH must be explicitly provided")
            .to_string_lossy();
        if cfg!(windows) {
            assert!(!path.contains("/usr/local/bin:/usr/bin:/bin"));
        } else {
            assert_eq!(path, "/usr/local/bin:/usr/bin:/bin");
        }
    }

    #[cfg(windows)]
    #[test]
    fn taskkill_is_resolved_to_system32_without_path_lookup() {
        let taskkill = windows_system32_executable("taskkill.exe");
        assert!(taskkill.is_absolute());
        assert!(taskkill.ends_with(r"System32\taskkill.exe"));
        // A deliberately invalid PID still proves the executable itself could
        // be launched without consulting PATH.
        assert!(windows_taskkill(u32::MAX, true).is_ok());
    }
}
