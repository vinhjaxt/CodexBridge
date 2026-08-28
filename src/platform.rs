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
fn windows_taskkill_command(pid: u32, force: bool) -> std::process::Command {
    use std::os::windows::process::CommandExt as _;

    let mut command = std::process::Command::new(windows_system32_executable("taskkill.exe"));
    command.args(["/T", "/PID", &pid.to_string()]);
    if force {
        command.arg("/F");
    }
    command
        .creation_flags(windows_hidden_creation_flags(false))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command
}

#[cfg(windows)]
pub(crate) fn windows_taskkill(pid: u32, force: bool) -> std::io::Result<std::process::ExitStatus> {
    windows_taskkill_command(pid, force).status()
}

#[cfg(all(windows, test))]
pub(crate) fn windows_taskkill_program_for_test(pid: u32, force: bool) -> OsString {
    windows_taskkill_command(pid, force)
        .get_program()
        .to_os_string()
}

#[cfg(windows)]
fn windows_hidden_creation_flags(new_process_group: bool) -> u32 {
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

    CREATE_NO_WINDOW
        | if new_process_group {
            CREATE_NEW_PROCESS_GROUP
        } else {
            0
        }
}

#[cfg(windows)]
pub(crate) fn configure_windows_non_tty_process(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;

    command
        .as_std_mut()
        .creation_flags(windows_hidden_creation_flags(true));
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
        let env = command
            .as_std()
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let get = |name: &str| {
            env.iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .and_then(|(_, value)| value.as_deref())
        };
        let path = get("PATH").expect("PATH must be explicitly provided");
        if cfg!(windows) {
            assert!(!path.contains("/usr/local/bin:/usr/bin:/bin"));
            assert!(!path.trim().is_empty());
            let system_root = get("SystemRoot").expect("SystemRoot baseline");
            assert!(!system_root.trim().is_empty());
            assert_eq!(get("WINDIR"), Some(system_root));
            assert!(get("TEMP").is_some_and(|value| !value.trim().is_empty()));
            assert!(get("TMP").is_some_and(|value| !value.trim().is_empty()));
        } else {
            assert_eq!(path, "/usr/local/bin:/usr/bin:/bin");
            assert_eq!(get("LANG"), Some("C.UTF-8"));
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

    #[cfg(windows)]
    #[test]
    fn hidden_creation_flags_cover_exec_and_internal_taskkill() {
        use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

        assert_eq!(
            windows_hidden_creation_flags(true),
            CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP
        );
        assert_eq!(windows_hidden_creation_flags(false), CREATE_NO_WINDOW);
    }

    #[cfg(windows)]
    #[test]
    fn non_tty_process_does_not_show_a_console_window() {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "platform::tests::non_tty_process_console_window_probe",
                "--ignored",
                "--nocapture",
            ])
            .env("CODEXBRIDGE_NO_WINDOW_PROBE", "1");
        configure_windows_non_tty_process(&mut command);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let output = runtime.block_on(command.output()).unwrap();

        assert!(
            output.status.success(),
            "hidden window probe failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(windows)]
    fn assert_current_process_has_no_console_window() {
        use windows_sys::Win32::System::Console::GetConsoleWindow;

        let window = unsafe { GetConsoleWindow() };
        assert!(
            window.is_null(),
            "CREATE_NO_WINDOW process has a console window"
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "child harness: invoked by non_tty_process_does_not_show_a_console_window"]
    fn non_tty_process_console_window_probe() {
        assert_eq!(
            std::env::var_os("CODEXBRIDGE_NO_WINDOW_PROBE").as_deref(),
            Some(std::ffi::OsStr::new("1")),
            "child harness must only run under non_tty_process_does_not_show_a_console_window"
        );
        assert_current_process_has_no_console_window();
    }
}
