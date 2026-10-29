//! Admin-only routes the dashboard uses: deployment and group info, config view and reload.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::LazyLock;

use axum::Extension;
use axum::Json;
use axum::extract::State;
use regex::Regex;
use rustllmroute_core::AuthContext;
use rustllmroute_router::{HealthStatus, Registry};
use rustllmroute_types::GatewayError;
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::state::AppState;

fn require_admin(auth: &AuthContext) -> Result<(), ApiError> {
    if auth.is_admin {
        Ok(())
    } else {
        Err(ApiError(GatewayError::Forbidden(
            "this route requires an admin key".to_owned(),
        )))
    }
}

pub async fn model_info(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&auth)?;
    let report = state.gateway.router.health_report();
    let mut health: BTreeMap<String, Value> = BTreeMap::new();
    for endpoint in report
        .healthy_endpoints
        .iter()
        .chain(&report.unhealthy_endpoints)
        .chain(&report.unknown_endpoints)
    {
        health.insert(
            endpoint.model_id.clone(),
            serde_json::to_value(endpoint).unwrap_or(Value::Null),
        );
    }
    let registry = state.gateway.router.registry();
    let mut data: Vec<Value> = registry
        .deployments()
        .map(|deployment| {
            let provider = &deployment.provider;
            let meta = &deployment.meta;
            let pricing = &deployment.pricing;
            json!({
                "model_name": deployment.model_group,
                "litellm_params": {
                    "model": format!("{}/{}", provider.provider_name, provider.model),
                    "api_base": deployment.api_base(),
                    "api_key": provider.api_key.as_ref().map(|_| "***"),
                    "rpm": meta.rpm,
                    "tpm": meta.tpm,
                    "weight": meta.weight,
                    "order": meta.order,
                    "tags": meta.tags,
                    "timeout": provider.timeout.as_secs_f64(),
                    "stream_timeout": provider.stream_idle_timeout.as_secs_f64(),
                    "drop_params": provider.drop_params,
                },
                "model_info": {
                    "id": deployment.id,
                    "db_model": false,
                    "mode": format!("{:?}", deployment.mode).to_ascii_lowercase(),
                    "base_model": deployment.base_model,
                    "provider": provider.provider_name,
                    "pricing_key": deployment.pricing_key,
                    "max_input_tokens": pricing.max_input_tokens,
                    "max_output_tokens": pricing.max_output_tokens,
                    "input_cost_per_token": pricing.input,
                    "output_cost_per_token": pricing.output,
                    "cache_read_input_token_cost": pricing.cache_read,
                    "blocked": meta.blocked,
                    "health": health.get(&*deployment.id).cloned().unwrap_or(Value::Null),
                },
            })
        })
        .collect();
    data.sort_by(|a, b| {
        a["model_name"]
            .as_str()
            .cmp(&b["model_name"].as_str())
            .then(
                a["model_info"]["id"]
                    .as_str()
                    .cmp(&b["model_info"]["id"].as_str()),
            )
    });
    Ok(Json(json!({ "data": data })))
}

pub async fn model_group_info(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&auth)?;
    let report = state.gateway.router.health_report();
    let status_of = |id: &str| -> (bool, bool) {
        let find = |list: &[rustllmroute_router::EndpointHealth]| {
            list.iter().find(|e| e.model_id == id).map(|e| {
                (
                    e.status == HealthStatus::Healthy,
                    e.cooldown_remaining_s.is_some(),
                )
            })
        };
        find(&report.healthy_endpoints)
            .or_else(|| find(&report.unhealthy_endpoints))
            .or_else(|| find(&report.unknown_endpoints))
            .unwrap_or((false, false))
    };
    let registry = state.gateway.router.registry();
    let mut groups: BTreeMap<String, Value> = BTreeMap::new();
    for deployment in registry.deployments() {
        let entry = groups
            .entry(deployment.model_group.to_string())
            .or_insert_with(|| {
                json!({
                    "model_group": deployment.model_group,
                    "providers": [],
                    "deployments": 0,
                    "healthy": 0,
                    "cooled_down": 0,
                    "max_input_tokens": null,
                    "max_output_tokens": null,
                    "input_cost_per_token": null,
                    "output_cost_per_token": null,
                    "mode": format!("{:?}", deployment.mode).to_ascii_lowercase(),
                    "supports_responses_api": deployment.responses.is_some(),
                    "supports_messages_api": deployment.messages.is_some(),
                })
            });
        let (healthy, cooled) = status_of(&deployment.id);
        entry["deployments"] = json!(entry["deployments"].as_u64().unwrap_or(0) + 1);
        if healthy {
            entry["healthy"] = json!(entry["healthy"].as_u64().unwrap_or(0) + 1);
        }
        if cooled {
            entry["cooled_down"] = json!(entry["cooled_down"].as_u64().unwrap_or(0) + 1);
        }
        if let Some(providers) = entry["providers"].as_array_mut()
            && !providers
                .iter()
                .any(|p| p == deployment.provider.provider_name)
        {
            providers.push(json!(deployment.provider.provider_name));
        }
        let pricing = &deployment.pricing;
        for (key, value) in [
            (
                "max_input_tokens",
                pricing.max_input_tokens.map(|v| json!(v)),
            ),
            (
                "max_output_tokens",
                pricing.max_output_tokens.map(|v| json!(v)),
            ),
            ("input_cost_per_token", pricing.input.map(|v| json!(v))),
            ("output_cost_per_token", pricing.output.map(|v| json!(v))),
        ] {
            if let Some(value) = value {
                let current = &entry[key];
                let take = match (current.as_f64(), value.as_f64()) {
                    (Some(a), Some(b)) => b > a,
                    (None, Some(_)) => true,
                    _ => false,
                };
                if take {
                    entry[key] = value;
                }
            }
        }
    }
    Ok(Json(
        json!({ "data": groups.into_values().collect::<Vec<_>>() }),
    ))
}

static SECRET_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)((?:[A-Za-z0-9_]*(?:api_key|master_key|database_url|secret|password|token|credentials|KEY|SECRET|PASSWORD|TOKEN|URL)[A-Za-z0-9_]*)\s*:\s*)([^,}
]+)",
    )
    .expect("valid regex")
});

/// The config file with secret values replaced; `os.environ/NAME` references are kept. Both
/// block style (`api_key: x`) and flow style (`{ api_key: x, ... }`) are covered.
pub fn redact_yaml(text: &str) -> String {
    SECRET_FIELD
        .replace_all(text, |caps: &regex::Captures<'_>| {
            let value = caps[2].trim();
            if value.starts_with("os.environ/")
                || value.starts_with('[')
                || value.starts_with('{')
                || value.is_empty()
            {
                caps[0].to_owned()
            } else {
                format!("{}\"***\"", &caps[1])
            }
        })
        .into_owned()
}

pub async fn config_yaml(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&auth)?;
    let snapshot = state.config.snapshot();
    let text = std::fs::read_to_string(&snapshot.path).map_err(|error| {
        ApiError(GatewayError::internal(format!(
            "reading {}: {error}",
            snapshot.path.display()
        )))
    })?;
    Ok(Json(json!({
        "path": snapshot.path.display().to_string(),
        "yaml": redact_yaml(&text),
        "loaded_at": snapshot
            .loaded_at
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "models": snapshot.config.model_list.len(),
        "guardrails": state.gateway.guardrails.names(),
    })))
}

/// Re-reads the config file; `model_list` and `model_group_alias` are applied live, everything
/// else is reported as needing a restart.
pub async fn config_reload(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&auth)?;
    let report = state.config.reload().map_err(|error| {
        ApiError(GatewayError::BadRequest {
            message: format!("config reload rejected: {error}"),
            param: None,
            code: None,
        })
    })?;
    let snapshot = state.config.snapshot();
    let catalog = rustllmroute_catalog::bundled()
        .map_err(|error| ApiError(GatewayError::internal(format!("pricing table: {error}"))))?;
    let deployments = crate::state::build_deployments(&snapshot.config, &catalog)?;
    let aliases = snapshot
        .config
        .router_settings
        .model_group_alias
        .iter()
        .map(|(alias, target)| (alias.clone(), target.clone()))
        .collect();
    let count = deployments.len();
    state
        .gateway
        .router
        .replace_registry(Registry::new(deployments, aliases));
    tracing::info!(models = count, "configuration reloaded");
    let mut requires_restart: Vec<&str> = report.requires_restart.clone();
    requires_restart.extend(["router_settings", "guardrails", "general_settings"]);
    Ok(Json(json!({
        "models": report.models,
        "applied": ["model_list", "router_settings.model_group_alias"],
        "requires_restart": requires_restart,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secret_lines_but_keeps_env_references() {
        let yaml = "model_list:\n  - litellm_params:\n      api_key: sk-live-123\n      api_base: https://x\n      aws_secret_access_key: os.environ/AWS_SECRET_ACCESS_KEY\ngeneral_settings:\n  master_key: top-secret\nenvironment_variables:\n  AZURE_API_KEY: replace-me\n  DATABASE_URL: sqlite://./x.db\n";
        let redacted = redact_yaml(yaml);
        assert!(!redacted.contains("sk-live-123"));
        assert!(!redacted.contains("top-secret"));
        assert!(!redacted.contains("replace-me"));
        assert!(redacted.contains("api_key: \"***\""));
        assert!(redacted.contains("api_base: https://x"));
        assert!(redacted.contains("os.environ/AWS_SECRET_ACCESS_KEY"));
        assert!(redacted.contains("DATABASE_URL: \"***\""));
    }
}
