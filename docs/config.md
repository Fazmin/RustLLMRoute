# Configuration reference

The config file keeps LiteLLM's shape so an existing `config.yaml` loads unchanged. Values written
as `os.environ/NAME` are read from the process environment, then from the file's
`environment_variables` map; a missing name is a startup error naming the YAML path. Validation
reports every problem at once and exits with status 2.

## Command line and environment

| Flag | Env | Default | Meaning |
|---|---|---|---|
| `--config PATH` | `LITELLM_CONFIG` | required | config file |
| `--port` | `PORT` | 4000 | listen port |
| `--host` | `HOST` | 0.0.0.0 | listen address |
| `--database-url` | `DATABASE_URL` | none | `sqlite://path.db`; without it only the master key works |
| `--log-level` | `LITELLM_LOG` | info | `tracing` filter |
| `--log-format` | | auto | `json`, `pretty` or `auto` (JSON when stdout is not a terminal) |
| `--check-config` | | | validate and exit |
| `--migrate-only` | | | apply migrations and exit |
| | `LITELLM_MASTER_KEY` | | master key when not in the file |
| | `UI_USERNAME`, `UI_PASSWORD` | admin / master key | dashboard login |
| | `OTEL_EXPORTER_OTLP_ENDPOINT` | | enables traces |
| | `LITELLM_MODEL_COST_MAP_URL` | | pricing table refresh source (`LITELLM_MODEL_COST_MAP_REFRESH_SECS`, default 86400) |
| | `GEMINI_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GROQ_API_KEY`, ... | | provider defaults when `api_key` is absent |

Subcommands: `rustllmroute healthcheck` (Docker `HEALTHCHECK`), `rustllmroute generate-master-key`.

## `model_list`

```yaml
model_list:
  - model_name: gpt-4o                 # public name; repeat it to load-balance
    litellm_params:
      model: openai/gpt-4o             # provider/model (see docs/providers.md)
      api_key: os.environ/OPENAI_API_KEY
      api_base: https://api.openai.com/v1   # optional override
      api_version: "2024-10-21"        # azure
      rpm: 500                         # per-deployment request budget per minute
      tpm: 100000
      weight: 2                        # simple-shuffle weight
      order: 0                         # lower tiers are tried first
      tags: [prod]                     # with router_settings.enable_tag_filtering
      timeout: 600                     # seconds to headers / whole non-stream body
      stream_timeout: 120              # idle seconds between stream frames
      num_retries: 2
      cooldown_time: 30
      max_parallel_requests: 100
      input_cost_per_token: 0.0000025  # pricing overrides
      output_cost_per_token: 0.00001
      drop_params: true
      extra_headers: {x-team: core}
      # provider-specific: vertex_project, vertex_location, vertex_credentials,
      # aws_region_name, aws_access_key_id, aws_secret_access_key, aws_session_token,
      # aws_profile_name, aws_bedrock_runtime_endpoint; anything else is sent in the body
    model_info:
      id: gpt-4o-primary               # stable deployment id (default: model_name)
      base_model: gpt-4o               # pricing lookup for azure deployments
      mode: chat                       # chat | embedding
      max_input_tokens: 128000         # pre-call context check
      max_output_tokens: 16384
      supports_responses_api: true     # relay /v1/responses natively
      blocked: false
```

Wildcards: `model_name: "openrouter/*"` with `model: "openrouter/*"` forwards any `openrouter/x`
request with `x` substituted.

## `litellm_settings`

`drop_params` (default true), `num_retries`, `request_timeout`, `callbacks` (`prometheus` is
always on; `otel` enables traces), `store_prompts_in_spend_logs`.

## `router_settings`

`routing_strategy` (`simple-shuffle`, `least-busy`, `latency-based-routing`,
`usage-based-routing-v2`, `cost-based-routing`), `num_retries`, `retry_after`, `allowed_fails`,
`cooldown_time`, `disable_cooldowns`, `timeout`, `stream_timeout`, `max_fallbacks`,
`fallbacks: [{"gpt-4o": ["gpt-4o-mini"]}]`, `context_window_fallbacks`,
`content_policy_fallbacks`, `default_fallbacks`, `model_group_alias: {"gpt-4": "gpt-4o"}`,
`enable_pre_call_checks`, `enable_tag_filtering`, `enable_health_check_routing`,
`lowest_latency_buffer`.

## `general_settings`

| Key | Default | Meaning |
|---|---|---|
| `master_key` | | admin key (`sk-…`) |
| `database_url` | | SQLite URL |
| `background_health_checks` | false | probe deployments every `health_check_interval` seconds (min 30, default 300) |
| `metrics_auth` | true | require a key for `/metrics` |
| `ui_secure_cookie` | false | mark the dashboard cookie `Secure` |
| `maximum_spend_logs_retention_period` | | e.g. `90d`; `maximum_spend_logs_retention_interval` default `1d` |
| `disable_spend_logs` | false | keep daily aggregates only |
| `graceful_shutdown_timeout` | 30 | seconds |
| `max_body_mb` | 32 | request body limit |
| `keepalive_interval` | 15 | SSE keepalive seconds |
| `spend.flush_interval_s` | 10 | spend writer cadence (`queue_capacity` 10000, `batch_rows` 1000) |
| `database.reader_connections` | 4 | SQLite reader pool; `sqlite_synchronous: normal|full`; `query_timeout_s` 10 |

## `guardrails`

See `docs/guardrails.md`.

## Hot reload

`POST /config/reload` (admin) re-reads the file: `model_list` and `model_group_alias` are
applied live; `router_settings`, `guardrails` and `general_settings` need a restart. Invalid
files are rejected and the running config stays.
