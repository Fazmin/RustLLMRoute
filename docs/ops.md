# Operations runbook

## Running

```sh
docker run -p 4000:4000 -v rustllmroute_data:/data \
  -e LITELLM_MASTER_KEY=sk-... -e OPENAI_API_KEY=sk-... \
  -v ./config.yaml:/app/config.yaml:ro ghcr.io/<owner>/rustllmroute:latest
```

`docker compose up` uses `docker-compose.yml` (gateway + SQLite volume). The image is distroless;
`rustllmroute healthcheck` is the container health check.

## Health and readiness

- `GET /health/liveness` – process is up.
- `GET /health/readiness` – 200 with `{status: healthy, db: connected|disabled}`; 503 while
  draining after SIGTERM/SIGINT or when the database ping fails.
- `GET /health` (admin) – probes every deployment (or returns the last background result).

## Shutdown

A signal flips readiness to 503, the listener stops accepting, in-flight requests drain (up to
`graceful_shutdown_timeout`), the spend writer flushes its last batch, the database is
checkpointed and traces are flushed. `docker compose stop` with `stop_grace_period: 40s` is enough.
Crash loss is bounded by `spend.flush_interval_s` (default 10 s) of spend rows; Prometheus
already has those numbers.

## Common problems

| Symptom | Cause | Action |
|---|---|---|
| `config is invalid: ... environment variable X is not set` at startup | an `os.environ/` reference without a value | set the variable or add it to `environment_variables` |
| 401 `only the master key is accepted` | no `DATABASE_URL` | set one; virtual keys need the database |
| 429 `all deployments of X are cooling down` | upstream errors tripped cooldowns | check `/health` and `/model/info` for the error; `router_settings.disable_cooldowns: true` for debugging |
| 400 `context_length_exceeded` with `enable_pre_call_checks` | prompt larger than every deployment's `max_input_tokens` | raise `model_info.max_input_tokens` or add a larger deployment |
| 400 `guardrail_violation` | a guardrail blocked; header `x-rustllmroute-guardrail` names it | adjust the guardrail or opt the key out via `metadata.disabled_guardrails` |
| spend not increasing | `spend_events_dropped` in readiness > 0 (queue full) or writer errors in logs | raise `spend.queue_capacity`, check disk space / SQLite locks |
| `SQLITE_BUSY` errors | another process holds the file | one gateway per database file; use the WAL-friendly volume, not a network share |
| `no pricing found` warning | model missing from the pricing table | set `input_cost_per_token`/`output_cost_per_token` or `model_info.base_model` |

## Logs and metrics

JSON access logs on stdout (`docker logs`), one `request complete` line per call; `/metrics` for
Prometheus; OTLP traces with `OTEL_EXPORTER_OTLP_ENDPOINT`. See `docs/observability.md`.

## Backups

The SQLite file in `/data` (plus `-wal`/`-shm`) is the whole state. Copy it with the gateway
stopped, or use `sqlite3 rustllmroute.db ".backup out.db"` while running.

## Upgrades

Migrations run at startup (`rustllmroute --migrate-only` to apply them separately). Config
changes to `model_list` apply with `POST /config/reload`; other sections need a restart.
