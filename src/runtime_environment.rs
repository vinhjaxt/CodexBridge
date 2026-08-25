use serde::Serialize;

use crate::{config::Config, sandbox};

/// Identity-independent execution facts shared by MCP instructions, tool
/// descriptions, and startup diagnostics. Project paths and conversation
/// identifiers are intentionally excluded.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RuntimeEnvironment {
    pub os: &'static str,
    pub arch: &'static str,
    pub path_separator: char,
    pub executable_suffix: &'static str,
    pub shell: String,
    pub shell_kind: &'static str,
    pub shell_argv_prefix: Vec<String>,
    pub sandbox_backend: &'static str,
    pub(crate) podman_invocation: sandbox::PodmanInvocation,
}

impl RuntimeEnvironment {
    pub fn detect(config: &Config) -> Self {
        let (shell, shell_kind, shell_argv_prefix) = sandbox::default_exec_shell(config);
        Self {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            path_separator: std::path::MAIN_SEPARATOR,
            executable_suffix: std::env::consts::EXE_SUFFIX,
            shell,
            shell_kind,
            shell_argv_prefix,
            sandbox_backend: sandbox::effective_default_sandbox_backend(config),
            podman_invocation: sandbox::podman_invocation(),
        }
    }

    pub fn render_agent_summary(&self) -> String {
        let shell_advice = match self.shell_kind {
            "powershell" => "Write PowerShell syntax, not POSIX shell syntax.",
            "cmd" => "Write cmd.exe syntax, not POSIX shell syntax.",
            _ => "Write POSIX shell syntax.",
        };
        format!(
            "Environment (identity-independent, secret-free): OS={}, architecture={}, OS path separator=`{}`, executable suffix=`{}`, exec shell=`{}` ({}), default exec backend={}. {} {} Structured project-tool paths always use `/` separators on every OS and remain relative to the active project; use the reported OS separator only inside shell commands and OS-native paths. Project paths are disclosed only after chatgpt_turn_init; individual commands such as Podman may use a different effective backend when runtime capability probing requires it.",
            self.os,
            self.arch,
            self.path_separator,
            self.executable_suffix,
            self.shell,
            self.shell_kind,
            self.sandbox_backend,
            shell_advice,
            self.podman_invocation.agent_advice(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigBuilder;
    use std::collections::BTreeMap;

    #[test]
    fn rendered_environment_is_secret_free_and_names_effective_shell() {
        let config = ConfigBuilder::from_map(BTreeMap::from([(
            "MCP_AUTH_TOKEN".to_owned(),
            "1234567890abcdef".to_owned(),
        )]))
        .build()
        .unwrap();
        let environment = RuntimeEnvironment::detect(&config);
        let rendered = environment.render_agent_summary();
        assert!(rendered.contains(&environment.shell));
        assert!(rendered.contains(environment.sandbox_backend));
        assert!(rendered.contains(environment.podman_invocation.agent_advice()));
        assert!(rendered.contains("Structured project-tool paths always use `/`"));
        assert!(!rendered.contains(&config.auth_token));
        assert!(!rendered.contains(config.workspace_root.to_string_lossy().as_ref()));
    }

    #[test]
    fn verified_podman_sudo_fallback_is_explicit_in_agent_summary() {
        let environment = RuntimeEnvironment {
            os: "linux",
            arch: "x86_64",
            path_separator: '/',
            executable_suffix: "",
            shell: "/bin/sh".to_owned(),
            shell_kind: "posix",
            shell_argv_prefix: vec!["-c".to_owned()],
            sandbox_backend: "native",
            podman_invocation: sandbox::PodmanInvocation::DirectWithSudoFallback,
        };
        let rendered = environment.render_agent_summary();
        assert!(rendered.contains("retry the same Podman operation once"));
        assert!(rendered.contains("sudo -n podman"));
        assert!(rendered.contains("crun"));
    }
}
