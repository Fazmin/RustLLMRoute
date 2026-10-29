# Changelog

## 0.1.0 (unreleased)

First release of RustLLMroute, a Rust replacement for LiteLLM's core proxy.

- OpenAI-compatible `/v1/chat/completions`, `/v1/messages`, `/v1/responses`, `/v1/embeddings`,
  `/v1/models`, streaming with raw relay where the provider speaks the client's format.
- Providers: the OpenAI-compatible family (OpenAI, Azure, Groq, Mistral, Together, OpenRouter,
  DeepSeek, xAI, Fireworks, Perplexity, Cerebras, SambaNova, NVIDIA NIM, Ollama, LM Studio,
  vLLM and more), Anthropic, Google Gemini, Vertex AI (Gemini and Claude), AWS Bedrock (Converse,
  Claude invoke, Titan and Cohere embeddings).
- Router: weighted shuffle, least-busy, latency, usage and cost strategies; order tiers, retries,
  fallbacks, cooldowns, tag filtering, pre-call context and rpm checks, health probes.
- Control plane on SQLite: virtual keys, teams, users, budgets with resets, rate limits, spend
  logs and daily aggregates, LiteLLM-shaped management API.
- Guardrails: content filter (LiteLLM's pattern catalog), tool permissions, OpenAI moderation,
  LLM judge, generic HTTP guardrail adapter; pre/during/post-call and logging-only modes.
- Observability: Prometheus `/metrics` with LiteLLM metric names, OTLP traces, JSON access logs.
- Admin dashboard at `/ui`, embedded in the binary.
- Distroless Docker image with SQLite volume; `rustllmroute healthcheck` subcommand.
