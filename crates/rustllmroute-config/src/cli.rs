use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    /// JSON when stdout is not a terminal, pretty otherwise.
    #[default]
    Auto,
    Json,
    Pretty,
}

/// RustLLMroute: a fast, dependable OpenAI-compatible LLM gateway.
#[derive(Debug, Parser)]
#[command(name = "rustllmroute", version, about)]
pub struct Cli {
    /// Path to the YAML configuration file (LiteLLM-compatible shape).
    #[arg(short, long, env = "LITELLM_CONFIG")]
    pub config: Option<PathBuf>,
    #[arg(long, env = "PORT", default_value_t = 4000)]
    pub port: u16,
    #[arg(long, env = "HOST", default_value = "0.0.0.0")]
    pub host: String,
    /// Overrides general_settings.database_url and DATABASE_URL.
    #[arg(long)]
    pub database_url: Option<String>,
    /// Log filter, e.g. `info`, `debug`, `rustllmroute=debug,tower_http=info`.
    #[arg(long, env = "LITELLM_LOG", default_value = "info")]
    pub log_level: String,
    #[arg(long, value_enum, default_value_t = LogFormat::Auto)]
    pub log_format: LogFormat,
    /// Shortcut for `--log-level debug`.
    #[arg(long)]
    pub detailed_debug: bool,
    /// Load and validate the configuration, then exit.
    #[arg(long)]
    pub check_config: bool,
    /// Run database migrations, then exit.
    #[arg(long)]
    pub migrate_only: bool,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Probe a running gateway's liveness endpoint (used by the Docker HEALTHCHECK).
    Healthcheck {
        #[arg(long, env = "PORT", default_value_t = 4000)]
        port: u16,
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
    },
    /// Print a fresh random master key.
    GenerateMasterKey,
}

impl Cli {
    pub fn effective_log_level(&self) -> &str {
        if self.detailed_debug {
            "debug"
        } else {
            self.log_level.as_str()
        }
    }
}
