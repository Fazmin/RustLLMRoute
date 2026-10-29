# Contributing to RustLLMroute

Thanks for helping. This page covers how to set up, what the gate is, and how changes land.

## Setup

- Rust stable (the workspace pins `stable` in `rust-toolchain.toml`; minimum 1.94).
- Node 22+ only if you touch the dashboard (`ui/`).
- Docker only if you want to build the image.

```sh
git clone https://github.com/fazmin/RustLLMRoute
cd RustLLMRoute
cargo build
cargo test --workspace
```

To run a local gateway while developing:

```sh
export LITELLM_MASTER_KEY=sk-dev OPENAI_API_KEY=sk-...
cargo run -- --config config.minimal.yaml --log-level debug
```

Dashboard work: `npm --prefix ui install`, then `npm --prefix ui run dev` serves the app with
API calls proxied to `localhost:4000`; `npm --prefix ui run build` produces `ui/dist`, which the
binary embeds (release) or reads from disk (debug).

## The gate

Every change must pass, and CI runs exactly this:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Add tests with the change. Conventions in this repository:

- Unit tests live in a `#[cfg(test)]` module at the bottom of the file they test; `rstest`
  with named `#[case::...]` for tables.
- Cross-crate behaviour is tested in `crates/rustllmroute-server/tests/` by booting the app
  in-process against `wiremock` upstreams (see any file there for the `boot` helper).
- Provider translations get a request-shape test, a response-shape test and a decoder test.
- Anything that parses bytes from the network must not panic on garbage;
  `crates/rustllmroute-providers/tests/decoder_fuzz.rs` is the place to extend.
- Router tests use `tokio::time` with `start_paused`, so no real sleeps.

## Layout

| Crate | Owns |
|---|---|
| `rustllmroute-types` | wire types, `GatewayError`, metric names |
| `rustllmroute-providers` | pure request/response transforms per provider, no I/O |
| `rustllmroute-core` | HTTP executor, streaming, guardrail orchestration, gateway entry points |
| `rustllmroute-router` | selection strategies, retries, fallbacks, cooldowns |
| `rustllmroute-control` | keys, budgets, rate limits, spend writer, management API |
| `rustllmroute-db` | SQLx repositories and migrations |
| `rustllmroute-guardrails` | guardrail trait and built-ins |
| `rustllmroute-observe` | Prometheus, OpenTelemetry, logging |
| `rustllmroute-server` | axum routes, dashboard embedding, binary |
| `ui/` | the React dashboard |

Design rules that keep the hot path fast: providers never touch sockets (the executor does);
nothing on the request path awaits the database (spend is batched through a channel); unknown
JSON fields are preserved in `extra` maps rather than dropped; LiteLLM's names (config keys,
`x-litellm-*` headers, `litellm_*` metrics) are kept so existing setups keep working.

## Adding a provider

1. If it speaks the OpenAI wire format, add a row to
   `crates/rustllmroute-providers/src/openai_compat/table.rs` and a name to
   `KNOWN_PROVIDERS` in `crates/rustllmroute-config/src/validate.rs`. That is usually all.
2. Otherwise add a module implementing `ChatProvider` (and `EmbeddingProvider`,
   `MessagesProvider` or `ResponsesProvider` where native), register it in
   `registry.rs`, and document its parameters in `docs/providers.md`.

## Pull requests

- One topic per PR; keep refactors separate from behaviour changes.
- Use conventional commit messages (`feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`).
- Describe what changed and how you verified it. A wiremock test or a reproduced curl session
  is the usual evidence; for provider work, say whether you ran it against the live API.
- Update `CHANGELOG.md` under the unreleased heading for user-visible changes.
- Never commit secrets. `config.yaml`, `.env` and `*.db` are ignored; keep it that way.

## Reporting bugs and security issues

Open an issue with the config (secrets redacted), the request, the response headers
(`x-litellm-call-id` helps) and the gateway log line for that request. For anything that could
expose keys, prompts or spend data, email the maintainer instead of opening a public issue.

## License

By contributing you agree that your contributions are licensed under the MIT License in
`LICENSE`.
