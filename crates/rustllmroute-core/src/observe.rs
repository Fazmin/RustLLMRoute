//! Per-call metrics, the access log line and span attributes; one entry point for every path
//! that completes a call.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rustllmroute_types::GatewayError;
use rustllmroute_types::metrics as m;

use crate::hooks::{AuthContext, CallRecord, CallStatus, CallTicket, Hooks};

/// Records metrics and the access log, then hands the record to the control plane.
pub fn complete(
    hooks: &Arc<dyn Hooks>,
    ticket: Box<dyn CallTicket>,
    auth: &AuthContext,
    record: CallRecord,
) {
    record_call(&record, auth);
    log_record(auth, &record);
    hooks.on_complete(ticket, auth, record);
}

type SpendKey = (String, String, String, String);

/// Prometheus counters are integers in the `metrics` facade, so spend is exposed as a
/// monotonically increasing gauge of the running USD total per label set.
fn spend_totals() -> &'static Mutex<HashMap<SpendKey, f64>> {
    static TOTALS: std::sync::OnceLock<Mutex<HashMap<SpendKey, f64>>> = std::sync::OnceLock::new();
    TOTALS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn is_upstream_failure(error: &GatewayError) -> bool {
    matches!(
        error,
        GatewayError::Upstream { .. }
            | GatewayError::Timeout { .. }
            | GatewayError::Transport(_)
            | GatewayError::InvalidUpstreamResponse(_)
            | GatewayError::StreamAborted(_)
    )
}

pub fn record_call(record: &CallRecord, auth: &AuthContext) {
    let model = record.requested_model.clone();
    let key = auth
        .key_hash
        .as_deref()
        .map(str::to_owned)
        .unwrap_or_else(|| "master".to_owned());
    let alias = auth.key_alias.clone().unwrap_or_default();
    let team = auth.team_id.clone().unwrap_or_default();
    let status_code = record.http_status.to_string();
    let seconds = record.duration_ms() as f64 / 1000.0;
    metrics::counter!(
        m::PROXY_TOTAL_REQUESTS,
        "model" => model.clone(),
        "hashed_api_key" => key.clone(),
        "api_key_alias" => alias.clone(),
        "team" => team.clone(),
        "status_code" => status_code.clone()
    )
    .increment(1);
    metrics::histogram!(
        m::REQUEST_TOTAL_LATENCY,
        "model" => model.clone(),
        "hashed_api_key" => key.clone(),
        "api_key_alias" => alias.clone(),
        "team" => team.clone()
    )
    .record(seconds);
    match &record.status {
        CallStatus::Error(error) => {
            metrics::counter!(
                m::PROXY_FAILED_REQUESTS,
                "model" => model.clone(),
                "hashed_api_key" => key.clone(),
                "api_key_alias" => alias.clone(),
                "team" => team.clone(),
                "status_code" => status_code,
                "exception_class" => format!("{:?}", error.kind())
            )
            .increment(1);
            if is_upstream_failure(error) {
                metrics::counter!(
                    m::LLM_API_FAILED_REQUESTS,
                    "model" => model.clone(),
                    "api_provider" => record.provider.unwrap_or("-").to_owned()
                )
                .increment(1);
            }
        }
        CallStatus::Ok | CallStatus::ClientDisconnected => {
            metrics::counter!(
                m::REQUESTS,
                "model" => model.clone(),
                "hashed_api_key" => key.clone(),
                "api_key_alias" => alias.clone(),
                "team" => team.clone()
            )
            .increment(1);
        }
    }
    if let Some(provider) = record.provider {
        metrics::histogram!(
            m::LLM_API_LATENCY,
            "model" => model.clone(),
            "api_provider" => provider.to_owned()
        )
        .record(seconds);
        if record.stream
            && let Some(first) = record.first_byte_at
        {
            let ttft = first.duration_since(record.started_instant).as_secs_f64();
            metrics::histogram!(
                m::LLM_API_TTFT,
                "model" => model.clone(),
                "api_provider" => provider.to_owned()
            )
            .record(ttft);
        }
    }
    let usage = &record.usage;
    if usage.total_tokens > 0 || usage.prompt_tokens > 0 || usage.completion_tokens > 0 {
        let total = usage
            .total_tokens
            .max(usage.prompt_tokens + usage.completion_tokens);
        for (name, value) in [
            (m::TOTAL_TOKENS, total),
            (m::INPUT_TOKENS, usage.prompt_tokens),
            (m::OUTPUT_TOKENS, usage.completion_tokens),
            (m::INPUT_CACHED_TOKENS, usage.cached_tokens()),
        ] {
            if value > 0 {
                metrics::counter!(
                    name,
                    "model" => model.clone(),
                    "hashed_api_key" => key.clone(),
                    "api_key_alias" => alias.clone(),
                    "team" => team.clone()
                )
                .increment(value);
            }
        }
        if record.cost_usd > 0.0 {
            let total = {
                let mut totals = spend_totals().lock().unwrap_or_else(|e| e.into_inner());
                let entry = totals
                    .entry((model.clone(), key.clone(), alias.clone(), team.clone()))
                    .or_insert(0.0);
                *entry += record.cost_usd;
                *entry
            };
            metrics::gauge!(
                m::SPEND,
                "model" => model.clone(),
                "hashed_api_key" => key.clone(),
                "api_key_alias" => alias.clone(),
                "team" => team.clone()
            )
            .set(total);
        } else if !record.pricing_known {
            metrics::counter!(m::ZERO_COST_REQUESTS, "model" => model.clone()).increment(1);
        }
    }
    let span = tracing::Span::current();
    span.record("gen_ai.request.model", record.requested_model.as_str());
    if let Some(provider_model) = record.provider_model.as_deref() {
        span.record("gen_ai.response.model", provider_model);
    }
    span.record("gen_ai.usage.input_tokens", usage.prompt_tokens);
    span.record("gen_ai.usage.output_tokens", usage.completion_tokens);
    if let Some(id) = record.deployment_id.as_deref() {
        span.record("llm.model_id", id);
    }
    if let Some(alias) = auth.key_alias.as_deref() {
        span.record("llm.key_alias", alias);
    }
    span.record("llm.cost_usd", record.cost_usd);
    span.record("llm.status", status_label(&record.status));
}

fn status_label(status: &CallStatus) -> &'static str {
    match status {
        CallStatus::Ok => "ok",
        CallStatus::Error(_) => "error",
        CallStatus::ClientDisconnected => "client_disconnected",
    }
}

/// One access-log line per call.
pub fn log_record(auth: &AuthContext, record: &CallRecord) {
    let kind: &'static str = record.kind.into();
    let error = match &record.status {
        CallStatus::Error(error) => error.to_string(),
        _ => String::new(),
    };
    tracing::info!(
        request_id = %record.request_id,
        kind,
        status = status_label(&record.status),
        http_status = record.http_status,
        key_alias = auth.key_alias.as_deref().unwrap_or("-"),
        model = %record.requested_model,
        deployment = record.deployment_id.as_deref().unwrap_or("-"),
        provider = record.provider.unwrap_or("-"),
        prompt_tokens = record.usage.prompt_tokens,
        completion_tokens = record.usage.completion_tokens,
        cost_usd = record.cost_usd,
        stream = record.stream,
        retries = record.attempted_retries,
        fallbacks = record.attempted_fallbacks,
        duration_ms = record.duration_ms(),
        error,
        "request complete"
    );
}
