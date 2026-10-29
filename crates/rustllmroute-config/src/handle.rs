use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use arc_swap::ArcSwap;
use secrecy::SecretString;
use serde_json::Value;
use tokio::sync::watch;

use crate::{ConfigError, ProxyConfig, resolve_env_references, validate};

#[derive(Debug)]
pub struct LoadedConfig {
    pub config: ProxyConfig,
    pub path: PathBuf,
    pub loaded_at: SystemTime,
    /// `general_settings.master_key`, else `LITELLM_MASTER_KEY`.
    pub master_key: Option<SecretString>,
    /// `general_settings.database_url`, else `DATABASE_URL`.
    pub database_url: Option<SecretString>,
}

impl LoadedConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let display = path.display().to_string();
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: display.clone(),
            source,
        })?;
        Self::parse(&text, path)
    }

    pub fn parse(text: &str, path: &Path) -> Result<Self, ConfigError> {
        let display = path.display().to_string();
        let mut tree: Value =
            serde_yaml_ng::from_str(text).map_err(|source| ConfigError::Yaml {
                path: display.clone(),
                source,
            })?;
        if tree.is_null() {
            tree = Value::Object(Default::default());
        }
        let file_env = tree
            .get("environment_variables")
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        let mut errors = Vec::new();
        resolve_env_references(&mut tree, &file_env, &mut errors);
        let mut config: ProxyConfig =
            serde_json::from_value(tree).map_err(|source| ConfigError::Shape {
                path: display.clone(),
                source,
            })?;
        errors.extend(validate(&mut config));
        let master_key = config.general_settings.master_key.clone().or_else(|| {
            std::env::var("LITELLM_MASTER_KEY")
                .ok()
                .map(SecretString::from)
        });
        if master_key.is_none() {
            errors.push(
                "no master key: set general_settings.master_key or the LITELLM_MASTER_KEY environment variable"
                    .to_owned(),
            );
        }
        if !errors.is_empty() {
            return Err(ConfigError::Invalid(errors));
        }
        let database_url = config.general_settings.database_url.clone().or_else(|| {
            std::env::var("DATABASE_URL")
                .ok()
                .filter(|url| !url.is_empty())
                .map(SecretString::from)
        });
        Ok(Self {
            config,
            path: path.to_path_buf(),
            loaded_at: SystemTime::now(),
            master_key,
            database_url,
        })
    }
}

#[derive(Clone, Debug)]
pub struct ReloadReport {
    pub models: usize,
    pub requires_restart: Vec<&'static str>,
}

/// The live configuration. Readers take a cheap snapshot; reloads swap atomically and notify watchers.
#[derive(Clone)]
pub struct ConfigHandle {
    current: Arc<ArcSwap<LoadedConfig>>,
    sender: Arc<watch::Sender<Arc<LoadedConfig>>>,
}

impl ConfigHandle {
    pub fn new(loaded: LoadedConfig) -> Self {
        let loaded = Arc::new(loaded);
        let (sender, _) = watch::channel(loaded.clone());
        Self {
            current: Arc::new(ArcSwap::new(loaded)),
            sender: Arc::new(sender),
        }
    }

    pub fn snapshot(&self) -> Arc<LoadedConfig> {
        self.current.load_full()
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<LoadedConfig>> {
        self.sender.subscribe()
    }

    /// Re-reads the same file. On failure the old config stays live and the errors are returned.
    pub fn reload(&self) -> Result<ReloadReport, ConfigError> {
        let previous = self.snapshot();
        let next = LoadedConfig::load(&previous.path)?;
        let mut requires_restart = Vec::new();
        let same_secret = |a: &Option<SecretString>, b: &Option<SecretString>| match (a, b) {
            (Some(a), Some(b)) => {
                secrecy::ExposeSecret::expose_secret(a) == secrecy::ExposeSecret::expose_secret(b)
            }
            (None, None) => true,
            _ => false,
        };
        if !same_secret(&previous.database_url, &next.database_url) {
            requires_restart.push("general_settings.database_url");
        }
        if !same_secret(&previous.master_key, &next.master_key) {
            requires_restart.push("general_settings.master_key");
        }
        let report = ReloadReport {
            models: next.config.model_list.len(),
            requires_restart,
        };
        let next = Arc::new(next);
        self.current.store(next.clone());
        let _ = self.sender.send(next);
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    fn parse_resolves_env_and_requires_master_key() {
        let yaml = r#"
model_list:
  - model_name: gpt-4o
    litellm_params: { model: openai/gpt-4o, api_key: os.environ/RLR_KEY }
environment_variables:
  RLR_KEY: from-file
general_settings:
  master_key: sk-master
"#;
        let loaded = LoadedConfig::parse(yaml, Path::new("test.yaml")).unwrap();
        let key = loaded.config.model_list[0]
            .litellm_params
            .api_key
            .as_ref()
            .unwrap();
        assert_eq!(secrecy::ExposeSecret::expose_secret(key), "from-file");
        let without = yaml.replace("general_settings:\n  master_key: sk-master\n", "");
        assert!(!without.contains("master_key"));
        let error = LoadedConfig::parse(&without, Path::new("test.yaml"));
        if std::env::var("LITELLM_MASTER_KEY").is_err() {
            assert!(
                matches!(error, Err(ConfigError::Invalid(errors)) if errors.iter().any(|e| e.contains("master key")))
            );
        }
    }
}
