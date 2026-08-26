use std::path::PathBuf;

use clap::Parser;
use codex_bridge::{config::ConfigBuilder, server};

const DEFAULT_RUST_LOG_FILTER: &str = "warn";

#[derive(Debug, Parser)]
#[command(
    name = "codex-bridge",
    about = "CodexBridge: production MCP coding-agent bridge"
)]
struct Cli {
    /// Root used to host isolated ChatGPT conversation workspaces. Defaults to /workspace.
    workspace: Option<PathBuf>,

    /// Listener socket, equivalent to MCP_BIND.
    #[arg(long)]
    bind: Option<String>,
}

fn build_config(cli: &Cli) -> codex_bridge::error::Result<codex_bridge::config::Config> {
    let mut builder = ConfigBuilder::from_process()?;
    if let Some(bind) = &cli.bind {
        builder = builder.override_value("MCP_BIND", bind.clone());
    }
    if let Some(root) = &cli.workspace {
        builder = builder.override_value("WORKSPACE_ROOT", root.display().to_string());
    }
    builder.build()
}

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| DEFAULT_RUST_LOG_FILTER.into()),
        )
        .try_init();

    let cli = Cli::parse();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(65535)
        .thread_stack_size(2 * 1024 * 1024)
        .build()
        .unwrap();
    let outcome = rt.block_on(async {
        match build_config(&cli) {
            Ok(config) => server::run(config).await,
            Err(error) => Err(error),
        }
    });
    if let Err(error) = outcome {
        eprintln!("server failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_log_defaults_to_warning() {
        assert_eq!(DEFAULT_RUST_LOG_FILTER, "warn");
    }
}
