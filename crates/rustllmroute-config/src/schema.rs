use std::collections::BTreeMap;

use secrecy::SecretString;
use serde::Deserialize;
use serde_json::{Map, Value};

pub type Extra = Map<String, Value>;

/// LiteLLM-compatible proxy configuration. Unknown keys are preserved in `extra` maps so a
/// LiteLLM config file loads unchanged; validation reports what this gateway ignores.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ProxyConfig {
    #[serde(default)]
    pub model_list: Vec<ModelEntry>,
    #[serde(default)]
    pub litellm_settings: LitellmSettings,
    #[serde(default)]
    pub router_settings: RouterSettings,
    #[serde(default)]
    pub general_settings: GeneralSettings,
    #[serde(default)]
    pub guardrails: Vec<GuardrailSpec>,
    #[serde(default)]
    pub environment_variables: BTreeMap<String, String>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ModelEntry {
    pub model_name: String,
    pub litellm_params: LitellmParams,
    #[serde(default)]
    pub model_info: ModelInfo,
}

#[derive(Clone, Debug, Deserialize)]
pub struct LitellmParams {
    /// `provider/model`, `provider/*`, or a bare OpenAI model name.
    pub model: String,
    #[serde(default)]
    pub api_key: Option<SecretString>,
    #[serde(default)]
    pub api_base: Option<String>,
    #[serde(default)]
    pub api_version: Option<String>,
    #[serde(default)]
    pub rpm: Option<u32>,
    #[serde(default)]
    pub tpm: Option<u32>,
    #[serde(default)]
    pub weight: Option<u32>,
    #[serde(default)]
    pub order: Option<i32>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub timeout: Option<f64>,
    #[serde(default)]
    pub stream_timeout: Option<f64>,
    #[serde(default)]
    pub num_retries: Option<u32>,
    #[serde(default)]
    pub cooldown_time: Option<f64>,
    #[serde(default)]
    pub max_parallel_requests: Option<u32>,
    #[serde(default)]
    pub input_cost_per_token: Option<f64>,
    #[serde(default)]
    pub output_cost_per_token: Option<f64>,
    #[serde(default)]
    pub drop_params: Option<bool>,
    #[serde(default)]
    pub extra_headers: BTreeMap<String, String>,
    /// Provider-specific parameters (`aws_region_name`, `vertex_project`, `vertex_location`,
    /// `azure_ad_token`, ...). Secrets inside are still `os.environ/` resolved.
    #[serde(flatten)]
    pub extra: Extra,
}

impl LitellmParams {
    /// `(provider, model)` split on the first `/`; a bare name is treated as OpenAI.
    pub fn provider_and_model(&self) -> (&str, &str) {
        match self.model.split_once('/') {
            Some((provider, model)) => (provider, model),
            None => ("openai", self.model.as_str()),
        }
    }

    pub fn extra_str(&self, key: &str) -> Option<&str> {
        self.extra.get(key).and_then(Value::as_str)
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ModelInfo {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub base_model: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub max_input_tokens: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub access_groups: Vec<String>,
    #[serde(default)]
    pub supports_responses_api: Option<bool>,
    #[serde(default)]
    pub blocked: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct LitellmSettings {
    #[serde(default)]
    pub drop_params: Option<bool>,
    #[serde(default)]
    pub num_retries: Option<u32>,
    #[serde(default)]
    pub request_timeout: Option<f64>,
    #[serde(default)]
    pub callbacks: Vec<String>,
    #[serde(default)]
    pub success_callback: Vec<String>,
    #[serde(default)]
    pub store_prompts_in_spend_logs: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `fallbacks: [{"gpt-4o": ["gpt-4o-mini"]}, {"*": ["default"]}]`, LiteLLM's list-of-single-key-maps shape.
pub type FallbackRules = Vec<BTreeMap<String, Vec<String>>>;

pub fn fallback_targets<'a>(rules: &'a FallbackRules, group: &str) -> Option<&'a [String]> {
    rules
        .iter()
        .find_map(|rule| rule.get(group))
        .map(Vec::as_slice)
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct RouterSettings {
    #[serde(default)]
    pub routing_strategy: Option<String>,
    #[serde(default)]
    pub num_retries: Option<u32>,
    #[serde(default)]
    pub retry_after: Option<f64>,
    #[serde(default)]
    pub allowed_fails: Option<u32>,
    #[serde(default)]
    pub cooldown_time: Option<f64>,
    #[serde(default)]
    pub disable_cooldowns: bool,
    #[serde(default)]
    pub timeout: Option<f64>,
    #[serde(default)]
    pub stream_timeout: Option<f64>,
    #[serde(default)]
    pub max_fallbacks: Option<u32>,
    #[serde(default)]
    pub fallbacks: FallbackRules,
    #[serde(default)]
    pub context_window_fallbacks: FallbackRules,
    #[serde(default)]
    pub content_policy_fallbacks: FallbackRules,
    #[serde(default)]
    pub default_fallbacks: Vec<String>,
    #[serde(default)]
    pub model_group_alias: BTreeMap<String, String>,
    #[serde(default)]
    pub enable_pre_call_checks: bool,
    #[serde(default)]
    pub enable_tag_filtering: bool,
    #[serde(default)]
    pub enable_health_check_routing: bool,
    #[serde(default)]
    pub lowest_latency_buffer: Option<f64>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Debug, Deserialize)]
pub struct GeneralSettings {
    #[serde(default)]
    pub master_key: Option<SecretString>,
    #[serde(default)]
    pub database_url: Option<SecretString>,
    #[serde(default)]
    pub store_model_in_db: bool,
    #[serde(default)]
    pub background_health_checks: bool,
    #[serde(default = "default_health_check_interval")]
    pub health_check_interval: u64,
    #[serde(default = "default_true")]
    pub metrics_auth: bool,
    #[serde(default = "default_true")]
    pub ui_enabled: bool,
    #[serde(default)]
    pub ui_secure_cookie: Option<bool>,
    #[serde(default)]
    pub maximum_spend_logs_retention_period: Option<String>,
    #[serde(default)]
    pub maximum_spend_logs_retention_interval: Option<String>,
    #[serde(default)]
    pub disable_spend_logs: bool,
    #[serde(default = "default_graceful_shutdown_timeout")]
    pub graceful_shutdown_timeout: u64,
    #[serde(default = "default_max_body_mb")]
    pub max_body_mb: u64,
    #[serde(default = "default_keepalive_interval")]
    pub keepalive_interval: u64,
    #[serde(default)]
    pub spend: SpendSettings,
    #[serde(default)]
    pub database: DatabaseSettings,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            master_key: None,
            database_url: None,
            store_model_in_db: false,
            background_health_checks: false,
            health_check_interval: default_health_check_interval(),
            metrics_auth: true,
            ui_enabled: true,
            ui_secure_cookie: None,
            maximum_spend_logs_retention_period: None,
            maximum_spend_logs_retention_interval: None,
            disable_spend_logs: false,
            graceful_shutdown_timeout: default_graceful_shutdown_timeout(),
            max_body_mb: default_max_body_mb(),
            keepalive_interval: default_keepalive_interval(),
            spend: SpendSettings::default(),
            database: DatabaseSettings::default(),
            extra: Extra::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct SpendSettings {
    #[serde(default = "default_flush_interval")]
    pub flush_interval_s: u64,
    #[serde(default = "default_queue_capacity")]
    pub queue_capacity: usize,
    #[serde(default = "default_batch_rows")]
    pub batch_rows: usize,
}

impl Default for SpendSettings {
    fn default() -> Self {
        Self {
            flush_interval_s: default_flush_interval(),
            queue_capacity: default_queue_capacity(),
            batch_rows: default_batch_rows(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct DatabaseSettings {
    #[serde(default = "default_reader_connections")]
    pub reader_connections: u32,
    #[serde(default = "default_sqlite_synchronous")]
    pub sqlite_synchronous: String,
    #[serde(default = "default_query_timeout")]
    pub query_timeout_s: u64,
}

impl Default for DatabaseSettings {
    fn default() -> Self {
        Self {
            reader_connections: default_reader_connections(),
            sqlite_synchronous: default_sqlite_synchronous(),
            query_timeout_s: default_query_timeout(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct GuardrailSpec {
    pub guardrail_name: String,
    #[serde(default)]
    pub litellm_params: Extra,
}

fn default_true() -> bool {
    true
}
fn default_health_check_interval() -> u64 {
    300
}
fn default_graceful_shutdown_timeout() -> u64 {
    30
}
fn default_max_body_mb() -> u64 {
    32
}
fn default_keepalive_interval() -> u64 {
    15
}
fn default_flush_interval() -> u64 {
    10
}
fn default_queue_capacity() -> usize {
    10_000
}
fn default_batch_rows() -> usize {
    1_000
}
fn default_reader_connections() -> u32 {
    4
}
fn default_sqlite_synchronous() -> String {
    "normal".to_owned()
}
fn default_query_timeout() -> u64 {
    10
}
