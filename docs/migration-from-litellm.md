# Migrating from LiteLLM

RustLLMroute keeps LiteLLM's public surface where it matters: the config file shape, the
`sk-` virtual keys and their SHA-256 storage, the `x-litellm-*` response headers, the
`litellm_*` metric names, the management routes and the OpenAI-shaped error bodies. Most
deployments switch by pointing the same clients at the new port.

## What carries over unchanged

- `config.yaml`: `model_list`, `litellm_params`, `model_info`, `router_settings`,
  `general_settings`, `guardrails`, `environment_variables`, `os.environ/` references.
- Clients: the OpenAI and Anthropic SDKs, Claude Code (`ANTHROPIC_BASE_URL`), anything that
  speaks `/v1/chat/completions`, `/v1/messages`, `/v1/responses` or `/v1/embeddings`.
- Virtual keys: rows from LiteLLM's `LiteLLM_VerificationToken` can be imported by copying
  `token` (already hashed), `key_alias`, `models`, `spend`, `max_budget`, `budget_duration`,
  `tpm_limit`, `rpm_limit`, `team_id`, `user_id`, `metadata`, `blocked`, `expires` into the
  `keys` table (`migrations/0001_init.sql`). The raw keys keep working because the hash is the
  same unsalted SHA-256.
- Management API shapes for keys, teams, users, spend logs and daily activity.
- Prometheus dashboards built on `litellm_*` metrics, with these exceptions: `litellm_spend_metric`
  is a monotonic gauge rather than a counter, and user labels are not emitted.

## What is different

| Area | LiteLLM | RustLLMroute |
|---|---|---|
| Database | Prisma on PostgreSQL (or SQLite dev) | SQLx on SQLite; PostgreSQL planned |
| Redis | needed for multi-instance correctness | not used; single node, in-process state |
| Providers | 100+ directories | OpenAI-compatible table, Anthropic, Gemini, Vertex AI, Bedrock |
| Guardrails | 65 vendors | content filter, tool permissions, OpenAI moderation, LLM judge, generic HTTP adapter |
| Model access denial | 401 | 403 |
| Timeouts / connection errors | 408 / 500 | 504 / 502 |
| `drop_params` | off by default | on by default |
| Connection errors and cooldowns | not cooled down | cooled down like 5xx |
| Pass-through routes, MCP, images/audio/files/batches, SSO, orgs, audit log, response caching | available | not included |
| `previous_response_id` | works everywhere | only for natively relayed `/v1/responses` |

## Steps

1. Run `rustllmroute --config config.yaml --check-config` and fix anything it reports
   (unknown providers, missing env vars, zero rpm/tpm).
2. Start with `DATABASE_URL=sqlite:///data/rustllmroute.db` and `LITELLM_MASTER_KEY`.
3. Import keys if needed (`sqlite3 rustllmroute.db` with the columns above), then verify one
   with `GET /key/info?key=sk-...`.
4. Point clients at the new port; compare `/spend/logs` entries and `/metrics` against the old
   dashboards for a day.
5. Switch Grafana data sources; keep LiteLLM running only until the comparison looks right.
