# Guardrails

Guardrails inspect request text before it reaches a provider and response text before it reaches
the client. They are configured under `guardrails:` with LiteLLM's shape and selected per request.

```yaml
guardrails:
  - guardrail_name: pii
    litellm_params:
      guardrail: content_filter
      mode: [pre_call, post_call]     # a string or a list; see modes below
      default_on: true                 # runs unless the key opts out
      patterns:
        - {name: email, action: MASK}  # bundled pattern (see the list below)
        - {name: us_ssn, action: BLOCK}
        - {name: ticket, pattern: "TCK-\\d+", action: BLOCK}   # custom regex
      blocked_words: [forbidden, {keyword: hush, action: MASK}]
      pattern_redaction_format: "[{pattern_name}_REDACTED]"
      keyword_redaction_tag: "[KEYWORD_REDACTED]"
      case_insensitive: true
      default_action: BLOCK            # for entries without an action
  - guardrail_name: tools
    litellm_params:
      guardrail: tool_permission
      mode: post_call
      default_on: true
      rules:
        - {id: reads, tool_name: "read_.*", action: allow}
        - {id: no-shell, tool_name: "shell|exec", action: deny}
      default_action: deny
  - guardrail_name: moderation
    litellm_params:
      guardrail: openai_moderation
      mode: pre_call
      api_key: os.environ/OPENAI_API_KEY
      model: omni-moderation-latest    # api_base and timeout (5s) are optional
  - guardrail_name: judge
    litellm_params:
      guardrail: llm_judge
      mode: pre_call
      judge_model: gpt-4o-mini         # a model_list group; the call bypasses hooks and spend logs
      prompt_template: "Answer SAFE or UNSAFE.\n\n{text}"
      max_tokens: 5
      block_on: [UNSAFE]
      timeout: 10
  - guardrail_name: external
    litellm_params:
      guardrail: generic_guardrail_api
      mode: during_call
      api_base: https://guard.example.com/check
      api_key: os.environ/GUARD_KEY    # sent as x-api-key
      headers: {x-team: core}
      additional_provider_specific_params: {policy: strict}
      timeout: 5
      on_error: fail_closed            # or fail_open (LiteLLM's unreachable_fallback is accepted too)
```

## Modes

| Mode | When | Outcomes |
|---|---|---|
| `pre_call` | Before routing, in config order, on every text segment of the request (system, user, assistant and tool messages; `instructions` and inputs on `/v1/responses`; embedding inputs). | `Modify` rewrites the text the provider sees; `Block` returns 400 `guardrail_violation` with `x-rustllmroute-guardrail: <name>`. |
| `during_call` | Concurrently with the provider call, on a snapshot of the request. | `Block` discards the provider's answer (its usage is still recorded) and returns 400. |
| `post_call` | On the provider's response text and tool calls. | `Modify` rewrites the reply; `Block` returns 400. For streaming clients the upstream call is buffered (sent non-streaming) and the result is replayed as a stream, so blocks are clean 400s and masks apply to the whole reply. |
| `logging_only` | Detached, after the request (and after the response when `post_call` text exists). | Logged, never enforced. |

Errors from a guardrail (network failures, bad responses) follow `on_error`: `fail_closed`
(default) returns 500, `fail_open` logs and continues.

## Selection

A guardrail runs when it is `default_on` or named in the request body (`"guardrails": ["name"]`,
stripped before forwarding) or in the key's metadata (`metadata.guardrails: [...]`). A key can opt
out of `default_on` guardrails with `metadata.disabled_guardrails: [...]`. Unknown names are a 400.

## Built-ins

- **content_filter**: regex and keyword rules with `BLOCK` or `MASK` actions. Block rules win
  over masks. Bundled patterns (from LiteLLM) include `us_ssn`, `us_ssn_no_dash`, `email`,
  `us_phone`, `credit_card`, `visa`, `mastercard`, `amex`, `aws_access_key`, `aws_secret_key`,
  `github_token`, `slack_token`, `generic_api_key`, `ipv4`, `ipv6`, `url`, passport numbers for
  several countries, and keyword-gated categories; lookaround patterns are supported.
- **tool_permission**: first matching `tool_name` regex rule decides, else `default_action`
  (deny by default). Runs on the tool calls in a response.
- **openai_moderation**: any flagged input blocks, naming the categories.
- **llm_judge**: sends `prompt_template` with `{text}` to a configured model group and blocks when
  the reply contains any `block_on` token.
- **generic_guardrail_api**: POSTs `{texts, input_type, model, tool_calls, metadata, additional_provider_specific_params}`
  and accepts `{action: NONE | BLOCKED | GUARDRAIL_INTERVENED, blocked_reason?, texts? | text?}`
  (lowercase `allow` / `block` / `modify` work too). This is the adapter for Presidio, Lakera and
  similar services behind a thin HTTP shim.

Provider-native guardrails (Bedrock Guardrails, Azure content safety) are not wired; use a
generic API shim or the provider's own configuration.
