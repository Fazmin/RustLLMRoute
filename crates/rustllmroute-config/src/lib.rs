mod cli;
mod env;
mod handle;
mod schema;
mod validate;

pub use cli::{Cli, Command, LogFormat};
pub use env::resolve_env_references;
pub use handle::{ConfigHandle, LoadedConfig, ReloadReport};
pub use schema::*;
pub use validate::validate;

pub use secrecy::{ExposeSecret, SecretString};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config file {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("config file {path} is not valid YAML: {source}")]
    Yaml {
        path: String,
        #[source]
        source: serde_yaml_ng::Error,
    },
    #[error("config file {path} has an invalid shape: {source}")]
    Shape {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("config is invalid:\n  - {}", .0.join("\n  - "))]
    Invalid(Vec<String>),
}
