use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use rustllmroute_db::{KeyRow, TeamRow, UserRow};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};

/// `Option<Option<T>>` PATCH semantics: absent = untouched, `null` = clear, value = set.
pub fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

pub fn rfc3339(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

/// Accepts `YYYY-MM-DD` or RFC 3339; returns epoch milliseconds at the start of that instant/day.
pub fn parse_date_start(text: &str) -> Option<i64> {
    if let Ok(date) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return date
            .and_hms_opt(0, 0, 0)
            .map(|t| t.and_utc().timestamp_millis());
    }
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.timestamp_millis())
}

/// A bare date means the end of that day (UTC).
pub fn parse_date_end(text: &str) -> Option<i64> {
    if let Ok(date) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return date
            .and_hms_milli_opt(23, 59, 59, 999)
            .map(|t| t.and_utc().timestamp_millis());
    }
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.timestamp_millis())
}

pub fn day_string(ms: i64) -> String {
    rustllmroute_db::day_of(ms)
}

fn json_or(text: &str, fallback: Value) -> Value {
    serde_json::from_str(text).unwrap_or(fallback)
}

/// LiteLLM-shaped key info: JSON columns expanded, timestamps as RFC 3339, never the raw key
/// unless it was just created.
pub fn key_json(row: &KeyRow, raw_key: Option<&str>) -> Value {
    let mut value = json!({
        "token": row.token,
        "token_id": row.token,
        "key_name": row.key_name,
        "key_alias": row.key_alias,
        "user_id": row.user_id,
        "team_id": row.team_id,
        "spend": row.spend,
        "max_budget": row.max_budget,
        "soft_budget": row.soft_budget,
        "budget_duration": row.budget_duration,
        "budget_reset_at": row.budget_reset_at.map(rfc3339),
        "expires": row.expires_at.map(rfc3339),
        "models": json_or(&row.models, json!([])),
        "aliases": json_or(&row.aliases, json!({})),
        "model_max_budget": json_or(&row.model_max_budget, json!({})),
        "allowed_routes": json_or(&row.allowed_routes, json!([])),
        "metadata": json_or(&row.metadata, json!({})),
        "max_parallel_requests": row.max_parallel_requests,
        "tpm_limit": row.tpm_limit,
        "rpm_limit": row.rpm_limit,
        "blocked": row.blocked,
        "created_at": rfc3339(row.created_at),
        "updated_at": rfc3339(row.updated_at),
        "last_active_at": row.last_active_at.map(rfc3339),
        "created_by": row.created_by,
    });
    if let Some(raw) = raw_key {
        value["key"] = Value::String(raw.to_owned());
    }
    value
}

pub fn team_json(row: &TeamRow) -> Value {
    json!({
        "team_id": row.team_id,
        "team_alias": row.team_alias,
        "members_with_roles": json_or(&row.members, json!([])),
        "spend": row.spend,
        "max_budget": row.max_budget,
        "budget_duration": row.budget_duration,
        "budget_reset_at": row.budget_reset_at.map(rfc3339),
        "models": json_or(&row.models, json!([])),
        "model_max_budget": json_or(&row.model_max_budget, json!({})),
        "metadata": json_or(&row.metadata, json!({})),
        "max_parallel_requests": row.max_parallel_requests,
        "tpm_limit": row.tpm_limit,
        "rpm_limit": row.rpm_limit,
        "blocked": row.blocked,
        "created_at": rfc3339(row.created_at),
        "updated_at": rfc3339(row.updated_at),
    })
}

pub fn user_json(row: &UserRow) -> Value {
    json!({
        "user_id": row.user_id,
        "user_email": row.user_email,
        "user_alias": row.user_alias,
        "user_role": row.user_role,
        "spend": row.spend,
        "max_budget": row.max_budget,
        "budget_duration": row.budget_duration,
        "budget_reset_at": row.budget_reset_at.map(rfc3339),
        "models": json_or(&row.models, json!([])),
        "metadata": json_or(&row.metadata, json!({})),
        "max_parallel_requests": row.max_parallel_requests,
        "tpm_limit": row.tpm_limit,
        "rpm_limit": row.rpm_limit,
        "created_at": rfc3339(row.created_at),
        "updated_at": rfc3339(row.updated_at),
    })
}

pub fn json_text<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned())
}

pub fn object_text(value: &Option<Value>) -> Option<String> {
    value.as_ref().map(|v| match v {
        Value::Object(_) => v.to_string(),
        _ => "{}".to_owned(),
    })
}

#[derive(Debug, Default, Deserialize)]
pub struct GenerateKeyRequest {
    pub key_alias: Option<String>,
    pub duration: Option<String>,
    pub models: Option<Vec<String>>,
    pub max_budget: Option<f64>,
    pub soft_budget: Option<f64>,
    pub budget_duration: Option<String>,
    pub tpm_limit: Option<i64>,
    pub rpm_limit: Option<i64>,
    pub max_parallel_requests: Option<i64>,
    pub team_id: Option<String>,
    pub user_id: Option<String>,
    pub metadata: Option<Value>,
    pub aliases: Option<HashMap<String, String>>,
    pub model_max_budget: Option<HashMap<String, f64>>,
    pub allowed_routes: Option<Vec<String>>,
    pub blocked: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub struct UpdateKeyRequest {
    pub key: String,
    #[serde(default, deserialize_with = "double_option")]
    pub key_alias: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub user_id: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub team_id: Option<Option<String>>,
    pub spend: Option<f64>,
    #[serde(default, deserialize_with = "double_option")]
    pub max_budget: Option<Option<f64>>,
    #[serde(default, deserialize_with = "double_option")]
    pub soft_budget: Option<Option<f64>>,
    #[serde(default, deserialize_with = "double_option")]
    pub budget_duration: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub duration: Option<Option<String>>,
    pub models: Option<Vec<String>>,
    pub aliases: Option<HashMap<String, String>>,
    pub model_max_budget: Option<HashMap<String, f64>>,
    pub allowed_routes: Option<Vec<String>>,
    pub metadata: Option<Value>,
    #[serde(default, deserialize_with = "double_option")]
    pub max_parallel_requests: Option<Option<i64>>,
    #[serde(default, deserialize_with = "double_option")]
    pub tpm_limit: Option<Option<i64>>,
    #[serde(default, deserialize_with = "double_option")]
    pub rpm_limit: Option<Option<i64>>,
    pub blocked: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub struct DeleteKeysRequest {
    #[serde(default)]
    pub keys: Vec<String>,
    #[serde(default)]
    pub key_aliases: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct KeyRef {
    pub key: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct RegenerateRequest {
    pub duration: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct KeyQuery {
    pub key: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    pub page: Option<u32>,
    pub size: Option<u32>,
    pub page_size: Option<u32>,
    pub user_id: Option<String>,
    pub team_id: Option<String>,
    pub key_alias: Option<String>,
    #[serde(default)]
    pub include_blocked: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct NewTeamRequest {
    pub team_id: Option<String>,
    pub team_alias: Option<String>,
    pub models: Option<Vec<String>>,
    pub max_budget: Option<f64>,
    pub budget_duration: Option<String>,
    pub tpm_limit: Option<i64>,
    pub rpm_limit: Option<i64>,
    pub max_parallel_requests: Option<i64>,
    pub metadata: Option<Value>,
    pub members_with_roles: Option<Vec<Value>>,
    pub blocked: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub struct UpdateTeamRequest {
    pub team_id: String,
    #[serde(default, deserialize_with = "double_option")]
    pub team_alias: Option<Option<String>>,
    pub models: Option<Vec<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub max_budget: Option<Option<f64>>,
    #[serde(default, deserialize_with = "double_option")]
    pub budget_duration: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub tpm_limit: Option<Option<i64>>,
    #[serde(default, deserialize_with = "double_option")]
    pub rpm_limit: Option<Option<i64>>,
    #[serde(default, deserialize_with = "double_option")]
    pub max_parallel_requests: Option<Option<i64>>,
    pub metadata: Option<Value>,
    pub blocked: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct TeamIds {
    pub team_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct TeamRef {
    pub team_id: String,
}

#[derive(Debug, Deserialize)]
pub struct TeamMemberAdd {
    pub team_id: String,
    pub member: Value,
}

#[derive(Debug, Deserialize)]
pub struct TeamMemberDelete {
    pub team_id: String,
    pub user_id: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct TeamQuery {
    pub team_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct NewUserRequest {
    pub user_id: Option<String>,
    pub user_email: Option<String>,
    pub user_alias: Option<String>,
    pub user_role: Option<String>,
    pub max_budget: Option<f64>,
    pub budget_duration: Option<String>,
    pub models: Option<Vec<String>>,
    pub tpm_limit: Option<i64>,
    pub rpm_limit: Option<i64>,
    pub max_parallel_requests: Option<i64>,
    pub metadata: Option<Value>,
    #[serde(default = "default_true")]
    pub auto_create_key: bool,
    pub key_alias: Option<String>,
    pub team_id: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Default, Deserialize)]
pub struct UpdateUserRequest {
    pub user_id: String,
    #[serde(default, deserialize_with = "double_option")]
    pub user_email: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub user_alias: Option<Option<String>>,
    pub user_role: Option<String>,
    pub models: Option<Vec<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub max_budget: Option<Option<f64>>,
    #[serde(default, deserialize_with = "double_option")]
    pub budget_duration: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub tpm_limit: Option<Option<i64>>,
    #[serde(default, deserialize_with = "double_option")]
    pub rpm_limit: Option<Option<i64>>,
    #[serde(default, deserialize_with = "double_option")]
    pub max_parallel_requests: Option<Option<i64>>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct UserIds {
    pub user_ids: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct UserQuery {
    pub user_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SpendLogsQuery {
    pub api_key: Option<String>,
    pub user_id: Option<String>,
    pub team_id: Option<String>,
    pub request_id: Option<String>,
    pub session_id: Option<String>,
    pub status_filter: Option<String>,
    pub model: Option<String>,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub page: Option<u32>,
    pub page_size: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SpendRangeQuery {
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub api_key: Option<String>,
    pub team_id: Option<String>,
    pub user_id: Option<String>,
    pub model: Option<String>,
    pub group_by: Option<String>,
    pub page: Option<u32>,
    pub page_size: Option<u32>,
}

pub fn empty_object() -> Map<String, Value> {
    Map::new()
}
