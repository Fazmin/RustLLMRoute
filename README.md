# RustLLMroute

One binary. One config file. Every LLM behind one OpenAI-compatible URL.

RustLLMroute is an LLM gateway written in Rust: drop-in OpenAI compatibility, 100+ providers
through one API, virtual keys with budgets and rate limits, spend tracking, guardrails, load
balancing with retries and fallbacks, Prometheus metrics, OpenTelemetry traces and an admin
dashboard. It is a single executable of about 30 MB (or a 75 MB distroless container) with SQLite inside,
so there is nothing else to install, no Redis, no database server, no Python.

## Up and running in 60 seconds

**Docker** (the image ships with `config.minimal.yaml` as its default config)

```sh
docker build -t rustllmroute .   # or pull ghcr.io/fazmin/rustllmroute once a release is tagged
docker run -p 4000:4000 \
  -e LITELLM_MASTER_KEY=sk-anything-you-like \
  -e OPENAI_API_KEY=sk-... -e ANTHROPIC_API_KEY=sk-ant-... \
  rustllmroute
```

Your own config instead of the default: add `-v "$PWD/config.yaml:/app/config.yaml:ro"`.

**Binary** (from a release download or `cargo build --release`)

```sh
export LITELLM_MASTER_KEY=sk-anything-you-like OPENAI_API_KEY=sk-... ANTHROPIC_API_KEY=sk-ant-...
rustllmroute --config config.minimal.yaml
```

That is the whole install. `config.minimal.yaml` lists one OpenAI model and one Anthropic model;
delete the one you have no key for (an `os.environ/` reference to an unset variable is a startup
error, on purpose), or add more later.

**Call it** with anything that speaks OpenAI:

```sh
curl http://localhost:4000/v1/chat/completions \
  -H "Authorization: Bearer sk-anything-you-like" \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hello"}]}'
```

```python
from openai import OpenAI
client = OpenAI(base_url="http://localhost:4000/v1", api_key="sk-anything-you-like")
client.chat.completions.create(model="gpt-4o-mini", messages=[{"role": "user", "content": "hello"}])
```

Point Claude Code or the Anthropic SDK at it too: `ANTHROPIC_BASE_URL=http://localhost:4000`,
`ANTHROPIC_AUTH_TOKEN=sk-anything-you-like`. The model name can be any entry in your config,
including an OpenAI one; the gateway translates.

Open `http://localhost:4000/ui/` and sign in with user `admin` and the master key as password.

## Adding it to your environment

Three things grow with you, each optional:

1. **More models.** Add entries to `model_list`. The same `model_name` twice means
   load balancing; `order`, `weight`, `rpm` and `fallbacks` shape the routing. Any config written for
   LiteLLM loads unchanged.
2. **Virtual keys and spend tracking.** Set `DATABASE_URL=sqlite:///data/rustllmroute.db`
   (the Docker image sets this already and keeps it in the `/data` volume). Then create keys in the
   dashboard or with `POST /key/generate`, each with its own models, budget, expiry and rate limits.
3. **Guardrails, metrics, traces.** Add a `guardrails:` block to mask PII or block content,
   scrape `/metrics` with Prometheus, set `OTEL_EXPORTER_OTLP_ENDPOINT` for traces.

With Docker Compose:

```sh
cp .env.example .env                      # master key and provider keys
cp config.minimal.yaml config.yaml        # or config.example.yaml for the full tour
docker compose up -d
```

## What you get

| Area | Details |
|---|---|
| Endpoints | `/v1/chat/completions`, `/v1/messages`, `/v1/responses`, `/v1/embeddings`, `/v1/models`, all with streaming |
| Providers | OpenAI, Azure OpenAI, Anthropic, Google Gemini, Vertex AI (Gemini and Claude), AWS Bedrock, Groq, Mistral, Together, OpenRouter, DeepSeek, xAI, Fireworks, Perplexity, Cerebras, SambaNova, NVIDIA NIM, Ollama, LM Studio, vLLM and any OpenAI-compatible host |
| Routing | weighted shuffle, least-busy, latency, usage and cost strategies; order tiers, retries with backoff, fallbacks, cooldowns, context-window pre-checks, tag routing, health probes |
| Keys | virtual keys, teams and users with budgets (with periodic resets), rate limits, model allow-lists, expiry; LiteLLM-shaped management API |
| Spend | per-request logs with token counts and USD cost from a bundled, refreshable pricing table; daily aggregates for dashboards |
| Guardrails | content filter (PII and secret patterns), tool permissions, OpenAI moderation, LLM judge, generic HTTP adapter; pre-call, during-call, post-call and logging-only |
| Observability | Prometheus `/metrics` with LiteLLM metric names, OTLP traces, JSON access logs |
| Dashboard | keys, teams, users, models with health, request logs, usage charts, config view and hot reload |
| Compatibility | LiteLLM config files, `sk-` keys (same hash, importable), `x-litellm-*` headers, `litellm_*` metrics |

Measured overhead: about 1 ms added at p99 at 2 000 requests per second on a laptop, see
`docs/perf.md`.

## Documentation

- `docs/config.md` – every setting, flag and environment variable
- `docs/providers.md` – provider prefixes, credentials (Vertex OAuth, Bedrock SigV4) and endpoint coverage
- `docs/guardrails.md` – guardrail modes, selection and built-ins
- `docs/observability.md` – metrics, traces and logs
- `docs/dashboard.md` – the admin UI and sessions
- `docs/ops.md` – runbook: health, shutdown, backups, common problems
- `docs/migration-from-litellm.md` – what carries over and what differs
- `docs/perf.md` – load-test method and numbers

## Building from source

```sh
npm --prefix ui install && npm --prefix ui run build   # dashboard (optional)
cargo build --release                                  # target/release/rustllmroute
cargo test --workspace                                 # 317 tests
```

Without the dashboard build the binary still runs and serves a placeholder page at `/ui`.

## Not yet

PostgreSQL (the schema and SQL are written portably; the pool layer is SQLite-only today),
provider-native guardrails, image and audio endpoints, multi-instance coordination.

## Contributing and license

See `CONTRIBUTING.md` for setup, the test gate and how changes land. MIT licensed (`LICENSE`);
material derived from LiteLLM is credited in `NOTICE`.
