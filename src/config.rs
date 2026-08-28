use std::{
    collections::BTreeMap,
    env, fmt,
    fs::OpenOptions,
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use serde::Deserialize;

use crate::error::{AppError, Result};

const MAX_UPSTREAM_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_UPSTREAM_SERVERS: usize = 64;
const MAX_UPSTREAM_ARGS: usize = 256;
const MAX_UPSTREAM_ENV_VARS: usize = 256;

#[derive(Debug, Clone)]
pub struct Limits {
    pub request_body_bytes: usize,
    pub input_string_bytes: usize,
    pub write_bytes: usize,
    pub patch_bytes: usize,
    pub multi_path_count: usize,
    pub results: usize,
    pub traversed_entries: usize,
    pub process_output_bytes: usize,
    pub max_concurrent_tools: usize,
    pub max_concurrent_cpu: usize,
    pub max_concurrent_processes: usize,
    pub max_concurrent_searches: usize,
    pub max_concurrent_patches: usize,
    pub per_project_tools: usize,
    pub per_project_processes: usize,
    pub overload_wait: Duration,
}

#[derive(Debug, Clone)]
pub struct OutputLimits {
    /// Default bytes returned by one read_file call when the caller omits limit.
    pub file_bytes: usize,
    /// Aggregate bytes returned by read_files before the caller must narrow the request.
    pub multi_file_bytes: usize,
    /// Default page size for listing/search/tree tools. Hard MAX_RESULTS still applies.
    pub results: usize,
    /// Default aggregate text payload retained by grep/search_files.
    pub search_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct LogConfig {
    pub root: PathBuf,
    pub queue_capacity: usize,
    pub queue_max_bytes: usize,
    pub console_param_bytes: usize,
    pub console_result_bytes: usize,
    pub file_event_bytes: usize,
    pub max_file_bytes: u64,
    pub max_files: usize,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub config_source: String,
    pub bind: BindAddress,
    pub unix_socket_mode: u32,
    pub auth_token: String,
    pub auth_mode: AuthMode,
    pub workspace_root: PathBuf,
    pub limits: Limits,
    pub output: OutputLimits,
    pub logs: LogConfig,
    pub max_sessions: usize,
    pub session_idle: Duration,
    pub status_interval: Duration,
    pub exec_default_timeout: Duration,
    pub exec_max_timeout: Duration,
    pub sandbox_backend: String,
    pub allow_unsandboxed_exec: bool,
    pub allowed_hosts: Vec<String>,
    pub max_interactive_processes: usize,
    pub interactive_process_idle: Duration,
    pub container_socket: Option<PathBuf>,
    pub container_config_root: Option<PathBuf>,
    pub upstream_config: Option<PathBuf>,
    pub upstreams: BTreeMap<String, UpstreamSpec>,
    pub upstream_call_timeout: Duration,
    pub max_concurrent_upstream_calls: usize,
    pub max_gateway_skill_bytes: usize,
    /// Optional project-local instruction filenames checked after AGENTS.md.
    pub project_doc_fallbacks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindAddress {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

impl fmt::Display for BindAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp(address) => address.fmt(formatter),
            Self::Unix(path) => path.display().fmt(formatter),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    Path,
    Bearer,
    Either,
}

impl AuthMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Bearer => "bearer",
            Self::Either => "either",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamMode {
    Direct,
    Gateway,
}

fn default_upstream_mode() -> UpstreamMode {
    // Keep the default model surface compact. Operators can opt into `direct`
    // when individual upstream tools are important enough to expose verbatim.
    UpstreamMode::Gateway
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpstreamSpec {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(rename = "type", default)]
    pub transport: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    /// Name of the environment variable holding a bearer token sent as
    /// `Authorization: Bearer <value>` to Streamable-HTTP upstreams. The secret
    /// itself never lives in the config file (mirrors codex's
    /// `bearer_token_env_var`).
    #[serde(default)]
    pub bearer_token_env_var: Option<String>,
    /// Static header names whose values are read from the environment at
    /// connect time, e.g. `X-Tenant-Id: TENANT_ENV_VAR`.
    #[serde(default)]
    pub env_http_headers: BTreeMap<String, String>,
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    #[serde(default = "default_upstream_mode")]
    pub mode: UpstreamMode,
}

#[derive(Debug, Default, Deserialize)]
struct UpstreamConfigFile {
    #[serde(
        default,
        rename = "mcpServers",
        alias = "mcp_servers",
        alias = "upstreams"
    )]
    servers: BTreeMap<String, UpstreamSpec>,
}

fn validate_upstream_spec(name: &str, spec: &UpstreamSpec) -> Result<()> {
    if name.is_empty()
        || name.len() > 128
        || name
            .chars()
            .any(|character| character.is_control() || character == '\0')
    {
        return Err(AppError::config("upstream MCP server name is invalid"));
    }
    if spec.args.len() > MAX_UPSTREAM_ARGS || spec.env.len() > MAX_UPSTREAM_ENV_VARS {
        return Err(AppError::config(format!(
            "upstream MCP `{name}` exceeds argument or environment limits"
        )));
    }
    if spec.command.as_ref().is_some_and(|command| {
        command.is_empty() || command.len() > 4096 || command.contains(['\0', '\n', '\r'])
    }) {
        return Err(AppError::config(format!(
            "upstream MCP `{name}` has an invalid command"
        )));
    }
    if spec.args.iter().any(|argument| {
        argument.len() > 8192 || argument.contains('\0') || argument.contains(['\n', '\r'])
    }) {
        return Err(AppError::config(format!(
            "upstream MCP `{name}` has an invalid argument"
        )));
    }
    if spec.env.iter().any(|(key, value)| {
        key.is_empty()
            || key.len() > 128
            || key.contains(['=', '\0', '\n', '\r'])
            || value.len() > 8192
            || value.contains('\0')
    }) {
        return Err(AppError::config(format!(
            "upstream MCP `{name}` has an invalid environment entry"
        )));
    }
    if spec.tools.as_ref().is_some_and(|tools| {
        tools.len() > 512
            || tools.iter().any(|tool| {
                tool.is_empty()
                    || tool.len() > 128
                    || tool
                        .chars()
                        .any(|character| character.is_control() || character == '\0')
            })
    }) {
        return Err(AppError::config(format!(
            "upstream MCP `{name}` has an invalid tool allowlist"
        )));
    }
    if spec.bearer_token_env_var.as_ref().is_some_and(|name| {
        name.is_empty() || name.len() > 128 || name.contains(['=', '\0', '\n', '\r'])
    }) {
        return Err(AppError::config(format!(
            "upstream MCP `{name}` has an invalid bearer_token_env_var"
        )));
    }
    if spec.env_http_headers.iter().any(|(header, variable)| {
        header.is_empty()
            || header.len() > 256
            || header.contains(['\0', '\n', '\r'])
            || variable.is_empty()
            || variable.len() > 128
            || variable.contains(['=', '\0', '\n', '\r'])
    }) {
        return Err(AppError::config(format!(
            "upstream MCP `{name}` has an invalid env_http_headers entry"
        )));
    }
    Ok(())
}

fn load_upstream_config(
    path: Option<String>,
) -> Result<(Option<PathBuf>, BTreeMap<String, UpstreamSpec>)> {
    let Some(path) = path.filter(|value| !value.trim().is_empty()) else {
        return Ok((None, BTreeMap::new()));
    };
    let path = PathBuf::from(path);
    let bytes = std::fs::read(&path).map_err(|error| {
        AppError::config(format!(
            "cannot read MCP_UPSTREAM_CONFIG {}: {error}",
            path.display()
        ))
    })?;
    if bytes.len() > MAX_UPSTREAM_CONFIG_BYTES {
        return Err(AppError::config(format!(
            "MCP_UPSTREAM_CONFIG exceeds {MAX_UPSTREAM_CONFIG_BYTES} bytes"
        )));
    }
    let file: UpstreamConfigFile = serde_yaml::from_slice(&bytes).map_err(|error| {
        AppError::config(format!(
            "invalid MCP_UPSTREAM_CONFIG {}: {error}",
            path.display()
        ))
    })?;
    if file.servers.len() > MAX_UPSTREAM_SERVERS {
        return Err(AppError::config(format!(
            "MCP_UPSTREAM_CONFIG may define at most {MAX_UPSTREAM_SERVERS} servers"
        )));
    }
    for (name, spec) in &file.servers {
        validate_upstream_spec(name, spec)?;
    }
    Ok((Some(path), file.servers))
}

fn load_or_create_auth_token(workspace_root: &Path) -> Result<String> {
    let metadata_root = workspace_root.join(".metadata");
    std::fs::create_dir_all(&metadata_root).map_err(|error| {
        AppError::config(format!(
            "cannot create CodexBridge metadata directory {}: {error}",
            metadata_root.display()
        ))
    })?;
    #[cfg(unix)]
    {
        let mut permissions = std::fs::metadata(&metadata_root)
            .map_err(|error| AppError::config(error.to_string()))?
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&metadata_root, permissions)
            .map_err(|error| AppError::config(error.to_string()))?;
    }
    let path = metadata_root.join("auth-token");
    let read_existing = || -> Result<String> {
        let value = std::fs::read_to_string(&path).map_err(|error| {
            AppError::config(format!(
                "cannot read CodexBridge auth token {}: {error}",
                path.display()
            ))
        })?;
        #[cfg(unix)]
        {
            let mut permissions = std::fs::metadata(&path)
                .map_err(|error| AppError::config(error.to_string()))?
                .permissions();
            permissions.set_mode(0o600);
            std::fs::set_permissions(&path, permissions)
                .map_err(|error| AppError::config(error.to_string()))?;
        }
        Ok(value.trim().to_owned())
    };
    if path.exists() {
        return read_existing();
    }

    let token = format!(
        "cb_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    match options.open(&path) {
        Ok(mut file) => {
            file.write_all(token.as_bytes()).map_err(|error| {
                AppError::config(format!(
                    "cannot write CodexBridge auth token {}: {error}",
                    path.display()
                ))
            })?;
            file.write_all(b"\n")
                .map_err(|error| AppError::config(error.to_string()))?;
            Ok(token)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => read_existing(),
        Err(error) => Err(AppError::config(format!(
            "cannot create CodexBridge auth token {}: {error}",
            path.display()
        ))),
    }
}

fn parse_unix_socket_mode(value: &str) -> Result<u32> {
    let digits = value.strip_prefix("0o").unwrap_or(value);
    let mode = u32::from_str_radix(digits, 8).map_err(|_| {
        AppError::config("MCP_UNIX_SOCKET_MODE must be an octal mode between 0000 and 0777")
    })?;
    if mode > 0o777 {
        return Err(AppError::config(
            "MCP_UNIX_SOCKET_MODE must be an octal mode between 0000 and 0777",
        ));
    }
    Ok(mode)
}

#[derive(Debug, Clone)]
pub struct ConfigBuilder {
    environment: BTreeMap<String, String>,
    overrides: BTreeMap<String, String>,
}

impl ConfigBuilder {
    pub fn from_process() -> Result<Self> {
        Ok(Self {
            environment: env::vars().collect(),
            overrides: BTreeMap::new(),
        })
    }

    pub fn from_map(environment: BTreeMap<String, String>) -> Self {
        Self {
            environment,
            overrides: BTreeMap::new(),
        }
    }

    pub fn override_value(mut self, name: &str, value: impl Into<String>) -> Self {
        self.overrides.insert(name.to_owned(), value.into());
        self
    }

    fn value(&self, name: &str, default: &str) -> String {
        self.overrides
            .get(name)
            .or_else(|| self.environment.get(name))
            .cloned()
            .unwrap_or_else(|| default.to_owned())
    }

    fn optional_value(&self, name: &str) -> Option<String> {
        self.overrides
            .get(name)
            .or_else(|| self.environment.get(name))
            .cloned()
    }

    fn usize_value(&self, name: &str, default: usize) -> Result<usize> {
        self.value(name, &default.to_string())
            .parse()
            .map_err(|_| AppError::config(format!("{name} must be a positive integer")))
    }

    fn bool_value(&self, name: &str, default: bool) -> Result<bool> {
        self.value(name, if default { "true" } else { "false" })
            .parse()
            .map_err(|_| AppError::config(format!("{name} must be true or false")))
    }

    pub fn build(self) -> Result<Config> {
        let workspace_root = PathBuf::from(self.value("WORKSPACE_ROOT", "/workspace"));
        let auth_mode = match self.value("MCP_AUTH_MODE", "path").as_str() {
            "path" => AuthMode::Path,
            "bearer" => AuthMode::Bearer,
            "either" => AuthMode::Either,
            _ => {
                return Err(AppError::config(
                    "MCP_AUTH_MODE must be one of path, bearer, or either",
                ));
            }
        };
        let auth_token = match self
            .optional_value("MCP_AUTH_TOKEN")
            .filter(|value| !value.is_empty())
        {
            Some(value) => value,
            None => load_or_create_auth_token(&workspace_root)?,
        };
        if auth_token.len() < 16 || auth_token.len() > 512 {
            return Err(AppError::config(
                "MCP_AUTH_TOKEN must contain between 16 and 512 bytes",
            ));
        }
        if matches!(auth_mode, AuthMode::Path | AuthMode::Either)
            && (auth_token.contains('/') || auth_token.contains('\\'))
        {
            return Err(AppError::config(
                "MCP_AUTH_TOKEN cannot contain path separators",
            ));
        }

        let bind_value = self.value("MCP_BIND", "0.0.0.0:3000");
        let bind = if bind_value.contains('/') {
            BindAddress::Unix(PathBuf::from(bind_value))
        } else {
            BindAddress::Tcp(
                bind_value
                    .parse()
                    .map_err(|_| AppError::config("MCP_BIND must be a valid socket address"))?,
            )
        };
        let unix_socket_mode = parse_unix_socket_mode(&self.value("MCP_UNIX_SOCKET_MODE", "0777"))?;
        let cpu_default = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(2)
            .max(2);
        let max_processes = self.usize_value("MAX_CONCURRENT_PROCESSES", cpu_default.min(8))?;

        let project_doc_fallbacks = self
            .optional_value("MCP_PROJECT_DOC_FALLBACKS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if project_doc_fallbacks.len() > 16
            || project_doc_fallbacks.iter().any(|name| {
                name.len() > 128
                    || name == "."
                    || name == ".."
                    || name.contains('/')
                    || name.contains('\\')
                    || name.contains('\0')
            })
        {
            return Err(AppError::config(
                "MCP_PROJECT_DOC_FALLBACKS must contain at most 16 simple filenames",
            ));
        }
        let (upstream_config, upstreams) =
            load_upstream_config(self.optional_value("MCP_UPSTREAM_CONFIG"))?;

        let log_queue_capacity = self.usize_value("LOG_QUEUE_CAPACITY", 4096)?;
        if log_queue_capacity == 0 {
            return Err(AppError::config(
                "LOG_QUEUE_CAPACITY must be greater than zero",
            ));
        }
        let log_queue_max_bytes = self.usize_value("LOG_QUEUE_MAX_BYTES", 64 * 1024 * 1024)?;
        if log_queue_max_bytes == 0 {
            return Err(AppError::config(
                "LOG_QUEUE_MAX_BYTES must be greater than zero",
            ));
        }

        let config = Config {
            config_source: "builtins+environment+cli".to_owned(),
            bind,
            unix_socket_mode,
            auth_token,
            auth_mode,
            logs: LogConfig {
                root: self
                    .optional_value("LOG_ROOT")
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| workspace_root.join(".metadata/logs")),
                queue_capacity: log_queue_capacity,
                queue_max_bytes: log_queue_max_bytes,
                console_param_bytes: self.usize_value("CONSOLE_PARAM_EXCERPT_BYTES", 4096)?,
                console_result_bytes: self.usize_value("CONSOLE_RESULT_EXCERPT_BYTES", 8192)?,
                file_event_bytes: self.usize_value("LOG_MAX_EVENT_BYTES", 1024 * 1024)?,
                max_file_bytes: self.usize_value("LOG_MAX_FILE_SIZE_MB", 100)? as u64 * 1024 * 1024,
                max_files: self.usize_value("LOG_MAX_FILES", 10)?,
            },
            limits: Limits {
                request_body_bytes: self.usize_value("MAX_REQUEST_BODY_BYTES", 16 * 1024 * 1024)?,
                input_string_bytes: self.usize_value("MAX_INPUT_STRING_BYTES", 1024 * 1024)?,
                write_bytes: self.usize_value("MAX_WRITE_BYTES", 8 * 1024 * 1024)?,
                patch_bytes: self.usize_value("MAX_PATCH_BYTES", 4 * 1024 * 1024)?,
                multi_path_count: self.usize_value("MAX_MULTI_PATHS", 64)?,
                results: self.usize_value("MAX_RESULTS", 1000)?,
                traversed_entries: self.usize_value("MAX_TRAVERSED_ENTRIES", 100_000)?,
                process_output_bytes: self
                    .usize_value("MAX_PROCESS_OUTPUT_BYTES", 4 * 1024 * 1024)?,
                max_concurrent_tools: self.usize_value("MAX_CONCURRENT_TOOL_CALLS", 64)?,
                max_concurrent_cpu: self.usize_value("MAX_CONCURRENT_CPU_TASKS", cpu_default)?,
                max_concurrent_processes: max_processes,
                max_concurrent_searches: self
                    .usize_value("MAX_CONCURRENT_SEARCHES", cpu_default)?,
                max_concurrent_patches: self.usize_value("MAX_CONCURRENT_PATCHES", 4)?,
                per_project_tools: self.usize_value("MAX_PROJECT_TOOL_CALLS", 8)?,
                per_project_processes: self.usize_value(
                    "MAX_PROJECT_PROCESSES",
                    max_processes.saturating_sub(1).max(1),
                )?,
                overload_wait: Duration::from_millis(
                    self.usize_value("OVERLOAD_WAIT_MS", 500)? as u64
                ),
            },
            output: OutputLimits {
                file_bytes: self.usize_value("OUTPUT_FILE_BYTES", 256 * 1024)?,
                multi_file_bytes: self.usize_value("OUTPUT_MULTI_FILE_BYTES", 1024 * 1024)?,
                results: self.usize_value("OUTPUT_MAX_RESULTS", 500)?,
                search_bytes: self.usize_value("OUTPUT_SEARCH_BYTES", 512 * 1024)?,
            },
            workspace_root,
            max_sessions: self.usize_value("MAX_LEGACY_MCP_SESSIONS", 1024)?,
            session_idle: Duration::from_secs(
                self.usize_value("MCP_SESSION_IDLE_SECS", 3600)? as u64
            ),
            status_interval: Duration::from_secs(
                self.usize_value("STATUS_INTERVAL_SECS", 0)? as u64
            ),
            exec_default_timeout: Duration::from_millis(
                self.usize_value("EXEC_DEFAULT_TIMEOUT_MS", 120_000)? as u64,
            ),
            exec_max_timeout: Duration::from_millis(
                self.usize_value("EXEC_MAX_TIMEOUT_MS", 3_600_000)? as u64,
            ),
            sandbox_backend: self.value("MCP_EXEC_SANDBOX", "auto"),
            allow_unsandboxed_exec: self.bool_value("MCP_ALLOW_UNSANDBOXED_EXEC", true)?,
            allowed_hosts: self
                .value("MCP_ALLOWED_HOSTS", "")
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect(),
            max_interactive_processes: self.usize_value("MAX_INTERACTIVE_PROCESSES", 32)?,
            interactive_process_idle: Duration::from_secs(
                self.usize_value("INTERACTIVE_PROCESS_IDLE_SECS", 900)? as u64,
            ),
            container_socket: self
                .optional_value("MCP_CONTAINER_SOCKET")
                .map(PathBuf::from),
            container_config_root: self
                .optional_value("MCP_CONTAINER_CONFIG_ROOT")
                .map(PathBuf::from),
            upstream_config,
            upstreams,
            upstream_call_timeout: Duration::from_millis(
                self.usize_value("UPSTREAM_CALL_TIMEOUT_MS", 120_000)? as u64,
            ),
            max_concurrent_upstream_calls: self.usize_value("MAX_CONCURRENT_UPSTREAM_CALLS", 8)?,
            max_gateway_skill_bytes: self.usize_value("MAX_GATEWAY_SKILL_BYTES", 1024 * 1024)?,
            project_doc_fallbacks,
        };
        config.validate()?;
        Ok(config)
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        ConfigBuilder::from_process()?.build()
    }

    pub fn diagnostic_summary(&self) -> serde_json::Value {
        serde_json::json!({
            "config_source": self.config_source,
            "bind": self.bind.to_string(),
            "workspace_root": self.workspace_root.display().to_string(),
            "log_root": self.logs.root.display().to_string(),
            "auth": self.auth_mode.as_str(),
            "init_required": true,
            "yolo_tools": true,
            "sandbox_backend": self.sandbox_backend,
            "allow_unsandboxed_exec": self.allow_unsandboxed_exec,
            "allowed_hosts": self.allowed_hosts,
            "upstream_config": self.upstream_config.as_ref().map(|path| path.display().to_string()),
            "configured_upstreams": self.upstreams.len(),
            "project_doc_fallbacks": self.project_doc_fallbacks,
            "limits": {
                "max_concurrent_tools": self.limits.max_concurrent_tools,
                "max_concurrent_processes": self.limits.max_concurrent_processes,
                "max_write_bytes": self.limits.write_bytes,
                "max_process_output_bytes": self.limits.process_output_bytes,
                "exec_default_timeout_ms": self.exec_default_timeout.as_millis(),
                "exec_max_timeout_ms": self.exec_max_timeout.as_millis()
            },
            "output": {
                "file_bytes": self.output.file_bytes,
                "multi_file_bytes": self.output.multi_file_bytes,
                "max_results": self.output.results,
                "search_bytes": self.output.search_bytes
            }
        })
    }

    fn validate(&self) -> Result<()> {
        let limits = &self.limits;
        if !matches!(self.sandbox_backend.as_str(), "auto" | "bwrap" | "none") {
            return Err(AppError::config(
                "MCP_EXEC_SANDBOX must be one of auto, bwrap, or none",
            ));
        }
        let positive = [
            ("MAX_REQUEST_BODY_BYTES", limits.request_body_bytes),
            ("MAX_INPUT_STRING_BYTES", limits.input_string_bytes),
            ("MAX_WRITE_BYTES", limits.write_bytes),
            ("MAX_PATCH_BYTES", limits.patch_bytes),
            ("MAX_MULTI_PATHS", limits.multi_path_count),
            ("MAX_RESULTS", limits.results),
            ("MAX_TRAVERSED_ENTRIES", limits.traversed_entries),
            ("MAX_PROCESS_OUTPUT_BYTES", limits.process_output_bytes),
            ("MAX_CONCURRENT_TOOL_CALLS", limits.max_concurrent_tools),
            ("MAX_CONCURRENT_CPU_TASKS", limits.max_concurrent_cpu),
            ("MAX_CONCURRENT_PROCESSES", limits.max_concurrent_processes),
            ("MAX_CONCURRENT_SEARCHES", limits.max_concurrent_searches),
            ("MAX_CONCURRENT_PATCHES", limits.max_concurrent_patches),
            ("MAX_PROJECT_TOOL_CALLS", limits.per_project_tools),
            ("MAX_PROJECT_PROCESSES", limits.per_project_processes),
        ];
        if let Some((name, _)) = positive.into_iter().find(|(_, amount)| *amount == 0) {
            return Err(AppError::config(format!(
                "{name} must be greater than zero"
            )));
        }
        for (name, amount) in [
            ("MAX_INPUT_STRING_BYTES", limits.input_string_bytes),
            ("MAX_WRITE_BYTES", limits.write_bytes),
            ("MAX_PATCH_BYTES", limits.patch_bytes),
        ] {
            if amount > limits.request_body_bytes {
                return Err(AppError::config(format!(
                    "{name} cannot exceed MAX_REQUEST_BODY_BYTES"
                )));
            }
        }
        if limits.per_project_processes > limits.max_concurrent_processes {
            return Err(AppError::config(
                "MAX_PROJECT_PROCESSES cannot exceed MAX_CONCURRENT_PROCESSES",
            ));
        }
        if self.max_sessions == 0 || self.max_interactive_processes == 0 {
            return Err(AppError::config(
                "session and interactive process limits must be greater than zero",
            ));
        }
        if self.max_concurrent_upstream_calls == 0 {
            return Err(AppError::config(
                "MAX_CONCURRENT_UPSTREAM_CALLS must be greater than zero",
            ));
        }
        if self.exec_default_timeout > self.exec_max_timeout {
            return Err(AppError::config(
                "EXEC_DEFAULT_TIMEOUT_MS cannot exceed EXEC_MAX_TIMEOUT_MS",
            ));
        }
        if self.logs.max_files == 0 || self.logs.max_file_bytes == 0 {
            return Err(AppError::config(
                "log rotation limits must be greater than zero",
            ));
        }
        if self.output.file_bytes == 0
            || self.output.multi_file_bytes == 0
            || self.output.results == 0
            || self.output.search_bytes == 0
        {
            return Err(AppError::config(
                "output presentation limits must be greater than zero",
            ));
        }
        if self.output.file_bytes > limits.write_bytes
            || self.output.multi_file_bytes > limits.write_bytes
        {
            return Err(AppError::config(
                "OUTPUT_FILE_BYTES and OUTPUT_MULTI_FILE_BYTES cannot exceed MAX_WRITE_BYTES",
            ));
        }
        if self.output.results > limits.results {
            return Err(AppError::config(
                "OUTPUT_MAX_RESULTS cannot exceed MAX_RESULTS",
            ));
        }
        if self.output.search_bytes > limits.process_output_bytes {
            return Err(AppError::config(
                "OUTPUT_SEARCH_BYTES cannot exceed MAX_PROCESS_OUTPUT_BYTES",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_builder_precedence_is_override_env_builtin() {
        let environment = BTreeMap::from([
            ("MCP_AUTH_TOKEN".to_owned(), "1234567890abcdef".to_owned()),
            ("MCP_BIND".to_owned(), "127.0.0.1:3102".to_owned()),
            ("OUTPUT_FILE_BYTES".to_owned(), "65536".to_owned()),
        ]);
        let config = ConfigBuilder::from_map(environment)
            .override_value("MCP_BIND", "127.0.0.1:3103")
            .build()
            .unwrap();
        assert_eq!(config.bind.to_string(), "127.0.0.1:3103");
        assert_eq!(config.output.file_bytes, 65536);
        assert_eq!(config.output.results, 500);
    }

    #[test]
    fn slash_bind_value_selects_unix_socket() {
        let environment = BTreeMap::from([
            ("MCP_AUTH_TOKEN".to_owned(), "1234567890abcdef".to_owned()),
            ("MCP_BIND".to_owned(), "/tmp/codexbridge.sock".to_owned()),
        ]);
        let config = ConfigBuilder::from_map(environment).build().unwrap();
        assert_eq!(
            config.bind,
            BindAddress::Unix(PathBuf::from("/tmp/codexbridge.sock"))
        );
        assert_eq!(config.bind.to_string(), "/tmp/codexbridge.sock");
        assert_eq!(config.unix_socket_mode, 0o777);
    }

    #[test]
    fn unix_socket_mode_is_configured_as_octal() {
        let environment = BTreeMap::from([
            ("MCP_AUTH_TOKEN".to_owned(), "1234567890abcdef".to_owned()),
            ("MCP_UNIX_SOCKET_MODE".to_owned(), "0750".to_owned()),
        ]);
        let config = ConfigBuilder::from_map(environment).build().unwrap();
        assert_eq!(config.unix_socket_mode, 0o750);

        for value in ["0899", "1000", "invalid"] {
            let environment = BTreeMap::from([
                ("MCP_AUTH_TOKEN".to_owned(), "1234567890abcdef".to_owned()),
                ("MCP_UNIX_SOCKET_MODE".to_owned(), value.to_owned()),
            ]);
            let error = ConfigBuilder::from_map(environment).build().unwrap_err();
            assert!(error.message().contains("MCP_UNIX_SOCKET_MODE"), "{value}");
        }
    }

    #[test]
    fn invalid_non_path_bind_value_is_still_rejected() {
        let environment = BTreeMap::from([
            ("MCP_AUTH_TOKEN".to_owned(), "1234567890abcdef".to_owned()),
            ("MCP_BIND".to_owned(), "not-a-socket-address".to_owned()),
        ]);
        let error = ConfigBuilder::from_map(environment).build().unwrap_err();
        assert!(error.message().contains("MCP_BIND"));
    }

    #[test]
    fn config_builds_are_independent_in_one_process() {
        let first = ConfigBuilder::from_map(BTreeMap::from([
            (
                "MCP_AUTH_TOKEN".to_owned(),
                "first-env-token-0001".to_owned(),
            ),
            ("MCP_BIND".to_owned(), "127.0.0.1:3201".to_owned()),
            ("MCP_AUTH_MODE".to_owned(), "either".to_owned()),
            ("OUTPUT_MAX_RESULTS".to_owned(), "101".to_owned()),
            (
                "MCP_PROJECT_DOC_FALLBACKS".to_owned(),
                "FIRST.md,FIRST.local.md".to_owned(),
            ),
        ]))
        .override_value("MCP_AUTH_TOKEN", "first-override-token-0001")
        .override_value("OUTPUT_MAX_RESULTS", "111")
        .build()
        .unwrap();
        let second = ConfigBuilder::from_map(BTreeMap::from([
            (
                "MCP_AUTH_TOKEN".to_owned(),
                "second-env-token-0002".to_owned(),
            ),
            ("MCP_BIND".to_owned(), "127.0.0.1:3202".to_owned()),
            ("MCP_AUTH_MODE".to_owned(), "bearer".to_owned()),
            ("OUTPUT_MAX_RESULTS".to_owned(), "222".to_owned()),
            (
                "MCP_PROJECT_DOC_FALLBACKS".to_owned(),
                "SECOND.md".to_owned(),
            ),
        ]))
        .build()
        .unwrap();

        assert_eq!(first.auth_token, "first-override-token-0001");
        assert_eq!(first.bind.to_string(), "127.0.0.1:3201");
        assert_eq!(first.auth_mode, AuthMode::Either);
        assert_eq!(first.output.results, 111);
        assert_eq!(
            first.project_doc_fallbacks,
            vec!["FIRST.md".to_owned(), "FIRST.local.md".to_owned()]
        );

        assert_eq!(second.auth_token, "second-env-token-0002");
        assert_eq!(second.bind.to_string(), "127.0.0.1:3202");
        assert_eq!(second.auth_mode, AuthMode::Bearer);
        assert_eq!(second.output.results, 222);
        assert_eq!(second.project_doc_fallbacks, vec!["SECOND.md".to_owned()]);
    }

    #[test]
    fn generated_auth_token_is_stable_per_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let environment = BTreeMap::from([(
            "WORKSPACE_ROOT".to_owned(),
            directory.path().display().to_string(),
        )]);
        let first = ConfigBuilder::from_map(environment.clone())
            .build()
            .unwrap();
        let second = ConfigBuilder::from_map(environment).build().unwrap();
        assert_eq!(first.auth_token, second.auth_token);
        assert!(first.auth_token.starts_with("cb_"));
        assert!(directory.path().join(".metadata/auth-token").is_file());
    }

    #[test]
    fn upstream_config_loads_standard_mcp_servers_shape() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("upstreams.yaml");
        std::fs::write(
            &path,
            "mcpServers:\n  demo:\n    command: mock-mcp\n    args: [--stdio]\n    tools: [echo]\n    mode: gateway\n",
        )
        .unwrap();
        let environment = BTreeMap::from([
            ("MCP_AUTH_TOKEN".to_owned(), "1234567890abcdef".to_owned()),
            ("MCP_UPSTREAM_CONFIG".to_owned(), path.display().to_string()),
        ]);
        let config = ConfigBuilder::from_map(environment).build().unwrap();
        assert_eq!(config.upstream_config.as_deref(), Some(path.as_path()));
        let demo = config.upstreams.get("demo").unwrap();
        assert_eq!(demo.command.as_deref(), Some("mock-mcp"));
        assert_eq!(demo.args, vec!["--stdio"]);
        assert_eq!(demo.tools.as_ref().unwrap(), &vec!["echo".to_owned()]);
        assert!(matches!(demo.mode, UpstreamMode::Gateway));
    }

    #[test]
    fn upstream_config_rejects_control_characters_in_commands() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("upstreams.yaml");
        std::fs::write(
            &path,
            "mcpServers:\n  demo:\n    command: \"mock\\nother\"\n",
        )
        .unwrap();
        let environment = BTreeMap::from([
            ("MCP_AUTH_TOKEN".to_owned(), "1234567890abcdef".to_owned()),
            ("MCP_UPSTREAM_CONFIG".to_owned(), path.display().to_string()),
        ]);
        assert!(ConfigBuilder::from_map(environment).build().is_err());
    }

    #[test]
    fn upstream_mode_defaults_to_gateway_to_protect_tool_context() {
        let spec: UpstreamSpec = serde_yaml::from_str("command: mock\n").unwrap();
        assert!(matches!(spec.mode, UpstreamMode::Gateway));
    }

    #[test]
    fn upstream_config_rejects_oversized_files_before_parsing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("upstreams.yaml");
        std::fs::write(&path, vec![b' '; MAX_UPSTREAM_CONFIG_BYTES + 1]).unwrap();
        let error = load_upstream_config(Some(path.display().to_string())).unwrap_err();
        assert_eq!(error.code(), "CONFIG_ERROR");
        assert!(error.message().contains("exceeds"));
    }

    #[test]
    fn upstream_config_rejects_invalid_environment_keys() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("upstreams.yaml");
        std::fs::write(
            &path,
            "mcpServers:\n  demo:\n    command: mock\n    env:\n      BAD=KEY: value\n",
        )
        .unwrap();
        let error = load_upstream_config(Some(path.display().to_string())).unwrap_err();
        assert_eq!(error.code(), "CONFIG_ERROR");
        assert!(error.message().contains("environment"));
    }

    fn base_environment() -> BTreeMap<String, String> {
        BTreeMap::from([("MCP_AUTH_TOKEN".to_owned(), "1234567890abcdef".to_owned())])
    }

    #[test]
    fn default_exec_policy_allows_native_yolo_fallback_without_sandbox() {
        let config = ConfigBuilder::from_map(base_environment()).build().unwrap();
        assert!(
            config.allow_unsandboxed_exec,
            "YOLO mode should fall back to native execution when Bubblewrap is unavailable"
        );
    }

    #[test]
    fn invalid_sandbox_backend_is_rejected() {
        let mut environment = base_environment();
        environment.insert("MCP_EXEC_SANDBOX".to_owned(), "magic".to_owned());
        let error = ConfigBuilder::from_map(environment).build().unwrap_err();
        assert_eq!(error.code(), "CONFIG_ERROR");
        assert!(error.message().contains("auto, bwrap, or none"));
    }

    #[test]
    fn zero_positive_limits_are_rejected_with_the_specific_variable_name() {
        for name in [
            "MAX_REQUEST_BODY_BYTES",
            "MAX_WRITE_BYTES",
            "MAX_RESULTS",
            "MAX_CONCURRENT_TOOL_CALLS",
            "MAX_CONCURRENT_PROCESSES",
            "MAX_PROJECT_TOOL_CALLS",
        ] {
            let mut environment = base_environment();
            environment.insert(name.to_owned(), "0".to_owned());
            let error = ConfigBuilder::from_map(environment).build().unwrap_err();
            assert_eq!(error.code(), "CONFIG_ERROR", "{name}");
            assert!(
                error.message().contains(name),
                "{name}: {}",
                error.message()
            );
        }
    }

    #[test]
    fn nested_limits_cannot_exceed_their_parent_budget() {
        let cases = [
            (
                [
                    ("MAX_REQUEST_BODY_BYTES", "5242880"),
                    ("MAX_WRITE_BYTES", "6291456"),
                ],
                "MAX_WRITE_BYTES cannot exceed MAX_REQUEST_BODY_BYTES",
            ),
            (
                [
                    ("MAX_CONCURRENT_PROCESSES", "2"),
                    ("MAX_PROJECT_PROCESSES", "3"),
                ],
                "MAX_PROJECT_PROCESSES cannot exceed MAX_CONCURRENT_PROCESSES",
            ),
            (
                [("MAX_WRITE_BYTES", "1024"), ("OUTPUT_FILE_BYTES", "2048")],
                "OUTPUT_FILE_BYTES and OUTPUT_MULTI_FILE_BYTES cannot exceed MAX_WRITE_BYTES",
            ),
            (
                [("MAX_RESULTS", "5"), ("OUTPUT_MAX_RESULTS", "6")],
                "OUTPUT_MAX_RESULTS cannot exceed MAX_RESULTS",
            ),
        ];
        for (overrides, expected) in cases {
            let mut environment = base_environment();
            for (name, value) in overrides {
                environment.insert(name.to_owned(), value.to_owned());
            }
            let error = ConfigBuilder::from_map(environment).build().unwrap_err();
            assert!(error.message().contains(expected), "{}", error.message());
        }
    }

    #[test]
    fn default_exec_timeout_cannot_exceed_max_timeout() {
        let mut environment = base_environment();
        environment.insert("EXEC_DEFAULT_TIMEOUT_MS".to_owned(), "2000".to_owned());
        environment.insert("EXEC_MAX_TIMEOUT_MS".to_owned(), "1000".to_owned());
        let error = ConfigBuilder::from_map(environment).build().unwrap_err();
        assert!(error.message().contains("EXEC_DEFAULT_TIMEOUT_MS"));
    }

    #[test]
    fn diagnostic_summary_never_contains_auth_token() {
        let config = ConfigBuilder::from_map(base_environment()).build().unwrap();
        let rendered = serde_json::to_string(&config.diagnostic_summary()).unwrap();
        assert!(!rendered.contains(&config.auth_token));
        assert!(rendered.contains("\"init_required\":true"));
        assert!(rendered.contains("\"yolo_tools\":true"));
    }

    #[test]
    fn upstream_config_accepts_supported_top_level_aliases() {
        for key in ["mcpServers", "mcp_servers", "upstreams"] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("upstreams.yaml");
            std::fs::write(&path, format!("{key}:\n  demo:\n    command: mock\n")).unwrap();
            let loaded = load_upstream_config(Some(path.display().to_string())).unwrap();
            assert!(loaded.1.contains_key("demo"), "{key}");
        }
    }

    #[test]
    fn upstream_validation_rejects_invalid_server_tool_and_argument_shapes() {
        let invalid = [
            "mcpServers:\n  '':\n    command: mock\n".to_owned(),
            "mcpServers:\n  demo:\n    command: mock\n    args: [\"bad\\narg\"]\n".to_owned(),
            "mcpServers:\n  demo:\n    command: mock\n    tools: ['']\n".to_owned(),
        ];
        for body in invalid {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("upstreams.yaml");
            std::fs::write(&path, body).unwrap();
            assert_eq!(
                load_upstream_config(Some(path.display().to_string()))
                    .unwrap_err()
                    .code(),
                "CONFIG_ERROR"
            );
        }
    }
}
