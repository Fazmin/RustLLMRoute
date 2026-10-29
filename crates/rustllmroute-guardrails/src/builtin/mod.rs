pub mod content_filter;
pub mod generic_api;
pub mod llm_judge;
pub mod openai_moderation;
pub mod tool_permission;

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::{Guardrail, GuardrailSpec, JudgeClient};

/// Builds the guardrail named by `litellm_params.guardrail`.
pub fn build(
    spec: &GuardrailSpec,
    judge: Option<Arc<dyn JudgeClient>>,
) -> Result<Arc<dyn Guardrail>, String> {
    let kind = spec
        .litellm_params
        .get("guardrail")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            format!(
                "guardrail {}: litellm_params.guardrail is required",
                spec.guardrail_name
            )
        })?;
    Ok(match kind {
        "content_filter" | "litellm_content_filter" => {
            Arc::new(content_filter::ContentFilter::from_spec(spec)?)
        }
        "tool_permission" => Arc::new(tool_permission::ToolPermission::from_spec(spec)?),
        "openai_moderation" => Arc::new(openai_moderation::OpenAiModeration::from_spec(spec)?),
        "llm_judge" | "llm_as_a_judge" => {
            let judge = judge.ok_or_else(|| {
                format!(
                    "guardrail {}: llm_judge needs the gateway judge client",
                    spec.guardrail_name
                )
            })?;
            Arc::new(llm_judge::LlmJudge::from_spec(spec, judge)?)
        }
        "generic_guardrail_api" => Arc::new(generic_api::GenericApi::from_spec(spec)?),
        other => {
            return Err(format!(
                "guardrail {}: unsupported guardrail type {other:?} (supported: content_filter, tool_permission, openai_moderation, llm_judge, generic_guardrail_api)",
                spec.guardrail_name
            ));
        }
    })
}

pub(crate) fn timeout_from(params: &serde_json::Map<String, Value>, default_secs: f64) -> Duration {
    params
        .get("timeout")
        .and_then(Value::as_f64)
        .filter(|t| *t > 0.0)
        .map(Duration::from_secs_f64)
        .unwrap_or(Duration::from_secs_f64(default_secs))
}

pub(crate) fn http_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(timeout)
        .user_agent(concat!("rustllmroute/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_default()
}

pub(crate) fn secret_param(
    params: &serde_json::Map<String, Value>,
    key: &str,
    env: &str,
) -> Option<String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .or_else(|| std::env::var(env).ok().filter(|v| !v.is_empty()))
}
