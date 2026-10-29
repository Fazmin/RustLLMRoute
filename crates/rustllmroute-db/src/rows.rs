use serde::{Deserialize, Serialize};
use serde_json::Value;

fn parse_json<T: for<'de> Deserialize<'de> + Default>(text: &str) -> T {
    serde_json::from_str(text).unwrap_or_default()
}

#[derive(Clone, Debug, PartialEq, sqlx::FromRow, Serialize)]
pub struct KeyRow {
    pub token: String,
    pub key_name: Option<String>,
    pub key_alias: Option<String>,
    pub user_id: Option<String>,
    pub team_id: Option<String>,
    pub spend: f64,
    pub max_budget: Option<f64>,
    pub soft_budget: Option<f64>,
    pub budget_duration: Option<String>,
    pub budget_reset_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub models: String,
    pub aliases: String,
    pub model_max_budget: String,
    pub allowed_routes: String,
    pub metadata: String,
    pub max_parallel_requests: Option<i64>,
    pub tpm_limit: Option<i64>,
    pub rpm_limit: Option<i64>,
    pub blocked: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_active_at: Option<i64>,
    pub created_by: Option<String>,
}

impl KeyRow {
    pub fn new(token: String, now: i64) -> Self {
        Self {
            token,
            key_name: None,
            key_alias: None,
            user_id: None,
            team_id: None,
            spend: 0.0,
            max_budget: None,
            soft_budget: None,
            budget_duration: None,
            budget_reset_at: None,
            expires_at: None,
            models: "[]".to_owned(),
            aliases: "{}".to_owned(),
            model_max_budget: "{}".to_owned(),
            allowed_routes: "[]".to_owned(),
            metadata: "{}".to_owned(),
            max_parallel_requests: None,
            tpm_limit: None,
            rpm_limit: None,
            blocked: false,
            created_at: now,
            updated_at: now,
            last_active_at: None,
            created_by: None,
        }
    }

    pub fn models_list(&self) -> Vec<String> {
        parse_json(&self.models)
    }

    pub fn aliases_map(&self) -> std::collections::HashMap<String, String> {
        parse_json(&self.aliases)
    }

    pub fn model_max_budget_map(&self) -> std::collections::HashMap<String, f64> {
        parse_json(&self.model_max_budget)
    }

    pub fn allowed_routes_list(&self) -> Vec<String> {
        parse_json(&self.allowed_routes)
    }

    pub fn metadata_value(&self) -> Value {
        serde_json::from_str(&self.metadata).unwrap_or(Value::Object(Default::default()))
    }
}

#[derive(Clone, Debug, PartialEq, sqlx::FromRow, Serialize)]
pub struct TeamRow {
    pub team_id: String,
    pub team_alias: Option<String>,
    pub members: String,
    pub spend: f64,
    pub max_budget: Option<f64>,
    pub budget_duration: Option<String>,
    pub budget_reset_at: Option<i64>,
    pub models: String,
    pub model_max_budget: String,
    pub metadata: String,
    pub max_parallel_requests: Option<i64>,
    pub tpm_limit: Option<i64>,
    pub rpm_limit: Option<i64>,
    pub blocked: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

impl TeamRow {
    pub fn new(team_id: String, now: i64) -> Self {
        Self {
            team_id,
            team_alias: None,
            members: "[]".to_owned(),
            spend: 0.0,
            max_budget: None,
            budget_duration: None,
            budget_reset_at: None,
            models: "[]".to_owned(),
            model_max_budget: "{}".to_owned(),
            metadata: "{}".to_owned(),
            max_parallel_requests: None,
            tpm_limit: None,
            rpm_limit: None,
            blocked: false,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn models_list(&self) -> Vec<String> {
        parse_json(&self.models)
    }

    pub fn members_list(&self) -> Vec<TeamMember> {
        parse_json(&self.members)
    }

    pub fn metadata_value(&self) -> Value {
        serde_json::from_str(&self.metadata).unwrap_or(Value::Object(Default::default()))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TeamMember {
    pub user_id: String,
    #[serde(default = "default_member_role")]
    pub role: String,
}

fn default_member_role() -> String {
    "user".to_owned()
}

#[derive(Clone, Debug, PartialEq, sqlx::FromRow, Serialize)]
pub struct UserRow {
    pub user_id: String,
    pub user_email: Option<String>,
    pub user_alias: Option<String>,
    pub user_role: String,
    pub spend: f64,
    pub max_budget: Option<f64>,
    pub budget_duration: Option<String>,
    pub budget_reset_at: Option<i64>,
    pub models: String,
    pub metadata: String,
    pub max_parallel_requests: Option<i64>,
    pub tpm_limit: Option<i64>,
    pub rpm_limit: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl UserRow {
    pub fn new(user_id: String, now: i64) -> Self {
        Self {
            user_id,
            user_email: None,
            user_alias: None,
            user_role: "internal_user".to_owned(),
            spend: 0.0,
            max_budget: None,
            budget_duration: None,
            budget_reset_at: None,
            models: "[]".to_owned(),
            metadata: "{}".to_owned(),
            max_parallel_requests: None,
            tpm_limit: None,
            rpm_limit: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn models_list(&self) -> Vec<String> {
        parse_json(&self.models)
    }
}

#[derive(Clone, Debug, PartialEq, sqlx::FromRow, Serialize)]
pub struct SpendLogRow {
    pub request_id: String,
    pub call_type: String,
    pub api_key: String,
    pub key_alias: Option<String>,
    pub user_id: Option<String>,
    pub team_id: Option<String>,
    pub end_user: Option<String>,
    pub spend: f64,
    pub total_tokens: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub start_time: i64,
    pub end_time: i64,
    pub completion_start_time: Option<i64>,
    pub request_duration_ms: Option<i64>,
    pub model: String,
    pub model_id: Option<String>,
    pub model_group: Option<String>,
    pub custom_llm_provider: Option<String>,
    pub api_base: Option<String>,
    pub status: String,
    pub error: Option<String>,
    pub cache_hit: bool,
    pub stream: bool,
    pub partial: bool,
    pub request_tags: String,
    pub metadata: String,
    pub request_body: Option<String>,
    pub response_body: Option<String>,
    pub session_id: Option<String>,
    pub litellm_call_id: Option<String>,
    pub created_at: i64,
}
