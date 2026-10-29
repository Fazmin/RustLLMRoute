# Observability

## Metrics

`GET /metrics` renders Prometheus text format. It requires a key unless
`general_settings.metrics_auth: false`. Histogram buckets match LiteLLM's so existing Grafana
dashboards keep working; series idle for an hour are dropped.

Request metrics (labels `model`, `hashed_api_key`, `api_key_alias`, `team`; `master` stands in
for the master key):

| Metric | Type | Notes |
|---|---|---|
| `litellm_proxy_total_requests_metric` | counter | plus `status_code` |
| `litellm_proxy_failed_requests_metric` | counter | plus `status_code`, `exception_class` |
| `litellm_requests_metric` | counter | successful calls |
| `litellm_llm_api_failed_requests_metric` | counter | upstream failures, labels `model`, `api_provider` |
| `litellm_request_total_latency_metric` | histogram | seconds, end to end |
| `litellm_llm_api_latency_metric` | histogram | seconds, labels `model`, `api_provider` |
| `litellm_llm_api_time_to_first_token_metric` | histogram | streaming calls |
| `litellm_total_tokens_metric`, `litellm_input_tokens_metric`, `litellm_output_tokens_metric`, `litellm_input_cached_tokens_metric` | counter | |
| `litellm_spend_metric` | gauge | running USD total per label set (monotonic; use `increase()`/`rate()`) |
| `litellm_zero_cost_requests_total` | counter | usage reported but no pricing known |
| `litellm_in_flight_requests` | gauge | |

Deployment and router metrics (`model_id`, `model_group`):
`litellm_deployment_total_requests`, `litellm_deployment_success_responses`,
`litellm_deployment_failure_responses`, `litellm_deployment_cooled_down`,
`litellm_deployment_successful_fallbacks`, `litellm_deployment_failed_fallbacks`,
`rustllmroute_retries_total`.

Budgets (refreshed on each authenticated call): `litellm_api_key_max_budget_metric`,
`litellm_remaining_api_key_budget_metric` (`hashed_api_key`, `api_key_alias`),
`litellm_remaining_team_budget_metric` (`team`, `team_alias`).

Guardrails: `litellm_guardrail_requests_total` (`guardrail_name`, `guardrail_mode`, `status` =
allow | modify | block | error) and `litellm_guardrail_latency_seconds`.

Internals: `rustllmroute_spend_events_total`, `rustllmroute_spend_events_dropped_total`,
`rustllmroute_spend_flush_duration_seconds`, `rustllmroute_spend_flush_failures_total`,
`rustllmroute_build_info{version}`.

## Traces

Set `OTEL_EXPORTER_OTLP_ENDPOINT` (OTLP over HTTP/protobuf; `/v1/traces` is appended) or add
`otel` to `litellm_settings.callbacks` to export spans. Every request produces an `llm.request`
span with `request_id`, `http.method`, `url.path`, `http.response.status_code` and, once the call
completes, `gen_ai.request.model`, `gen_ai.response.model`, `gen_ai.usage.input_tokens`,
`gen_ai.usage.output_tokens`, `llm.model_id`, `llm.key_alias`, `llm.cost_usd` and `llm.status`.
Streaming calls finish in a background task, so their usage reaches the metrics and the access
log but not the span.

## Logs

One `request complete` line per call (JSON when stdout is not a terminal, or `--log-format json`)
with `request_id`, `kind`, `status`, `http_status`, `key_alias`, `model`, `deployment`,
`provider`, token counts, `cost_usd`, `stream`, `retries`, `fallbacks`, `duration_ms` and
`error`. `LITELLM_LOG` / `--log-level` take `tracing` filter syntax (`info`, `debug`,
`rustllmroute_router=debug`).
