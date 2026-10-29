# Providers

Every deployment is a `model_list` entry whose `litellm_params.model` is `provider/model`. The
prefixes, their credentials and the parameters they read are listed below. Secrets can be given
inline or as `os.environ/NAME`.

| Prefix | Wire format | Default endpoint | Credentials |
|---|---|---|---|
| `openai`, `groq`, `mistral`, `together_ai`, `openrouter`, `deepseek`, `xai`, `fireworks_ai`, `perplexity`, `cerebras`, `sambanova`, `nvidia_nim`, `ollama`, `lm_studio`, `hosted_vllm`, `openai_like`, ... | OpenAI chat completions | per provider | `api_key` or the provider's `*_API_KEY` env var |
| `azure` | OpenAI chat completions (deployments URL style) | `api_base` required | `api_key` or `AZURE_API_KEY`; `api_version` |
| `anthropic` | Anthropic Messages | `https://api.anthropic.com` | `api_key` or `ANTHROPIC_API_KEY` |
| `gemini` | Gemini `generateContent` | `https://generativelanguage.googleapis.com` | `api_key`, `GEMINI_API_KEY` or `GOOGLE_API_KEY` |
| `vertex_ai` | Gemini `generateContent`; Anthropic Messages for `claude-*` models | `https://{location}-aiplatform.googleapis.com` | OAuth, see below |
| `bedrock` | Converse; Anthropic Messages (`invoke`) for `anthropic.*` models | `https://bedrock-runtime.{region}.amazonaws.com` | SigV4, see below |

`/v1/chat/completions` works against every provider. `/v1/messages` is relayed natively to
`anthropic`, Vertex Claude and Bedrock Claude deployments and translated through chat completions
for everything else, so the Anthropic SDK and Claude Code can use any model.

## Google AI Studio (`gemini/`)

```yaml
- model_name: gemini-flash
  litellm_params:
    model: gemini/gemini-2.5-flash
    api_key: os.environ/GEMINI_API_KEY
```

Translation notes: `reasoning_effort` becomes `thinkingConfig.thinkingBudget` (low 1024, medium
2048, high 4096); `thinking: {type: enabled, budget_tokens: N}` is honoured; `response_format`
maps to `responseMimeType`/`responseSchema`; JSON schemas are reduced to the OpenAPI subset Gemini
accepts (`$ref` expanded, `additionalProperties`/`strict` dropped, `null` unions become `nullable`);
`safety_settings`, `top_k`, `cached_content` pass through from the request body. Usage reports
`prompt_tokens` including cached tokens and `completion_tokens` including thoughts.

## Vertex AI (`vertex_ai/`)

```yaml
- model_name: gemini-pro
  litellm_params:
    model: vertex_ai/gemini-2.5-pro
    vertex_project: my-project          # or VERTEXAI_PROJECT / GOOGLE_CLOUD_PROJECT
    vertex_location: us-central1        # or VERTEXAI_LOCATION; `global` is supported
    vertex_credentials: os.environ/GCP_SA_JSON   # optional, see below
- model_name: claude-on-vertex
  litellm_params:
    model: vertex_ai/claude-sonnet-4-5@20250929
    vertex_project: my-project
    vertex_location: us-east5
```

Credentials, in order: `api_key` (used as a pre-minted bearer token), `vertex_credentials` (inline
service-account JSON, base64 JSON, or a file path), `GOOGLE_APPLICATION_CREDENTIALS`, the gcloud
application-default credentials file, then the GCE metadata server. Service-account keys are
exchanged for access tokens with a signed JWT and cached until two minutes before expiry.

Models containing `claude` go to `publishers/anthropic` with `rawPredict`/`streamRawPredict` and
`anthropic_version: vertex-2023-10-16`; `/v1/messages/count_tokens` is forwarded to Vertex.

## AWS Bedrock (`bedrock/`)

```yaml
- model_name: claude-bedrock
  litellm_params:
    model: bedrock/us.anthropic.claude-sonnet-4-5-20250929-v1:0
    aws_region_name: us-east-1          # or AWS_REGION / AWS_DEFAULT_REGION
    aws_access_key_id: os.environ/AWS_ACCESS_KEY_ID        # optional
    aws_secret_access_key: os.environ/AWS_SECRET_ACCESS_KEY
    aws_session_token: os.environ/AWS_SESSION_TOKEN        # optional
    aws_profile_name: prod                                 # optional
    aws_bedrock_runtime_endpoint: https://vpce-...         # optional
- model_name: nova
  litellm_params:
    model: bedrock/amazon.nova-pro-v1:0
```

Credential chain: deployment parameters, `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`
(`AWS_SESSION_TOKEN`), the shared credentials file (`AWS_PROFILE`), web identity
(`AWS_WEB_IDENTITY_TOKEN_FILE` + `AWS_ROLE_ARN`, EKS), container credentials (ECS), then IMDSv2.
Requests are SigV4-signed per attempt with `service=bedrock`. Model ids and ARNs are URL-encoded
in the path; `bedrock/converse/...` and `bedrock/invoke/...` prefixes are accepted.

Translation notes: tools become `toolConfig.toolSpec` with names sanitised to
`[a-zA-Z0-9_-]{1,64}`; `reasoning_effort` on Claude models becomes
`additionalModelRequestFields.thinking` (temperature/top_p are dropped and `maxTokens` is raised
above the budget); `response_format` is emulated with a forced tool; images must be base64 data
URLs. Streams use the binary AWS event stream and are decoded into OpenAI chunks; Claude
`invoke-with-response-stream` bodies are re-framed into Anthropic SSE for `/v1/messages`.
Errors carry the Bedrock exception name (`ThrottlingException` → 429, `ValidationException` → 400,
`ModelNotReadyException`/`ServiceUnavailableException` → 503, `ModelTimeoutException` → 408).

## Endpoints per provider

| Endpoint | OpenAI-compatible | Anthropic | Gemini | Vertex AI | Bedrock |
|---|---|---|---|---|---|
| `/v1/chat/completions` | native | translated | translated | translated | translated (Converse) |
| `/v1/messages` | translated | native relay | translated | native relay for Claude, translated for Gemini | native relay for Claude, translated otherwise |
| `/v1/responses` | native relay where the host serves it (OpenAI, Azure, xAI, OpenRouter, or `model_info.supports_responses_api: true`), bridged otherwise | bridged | bridged | bridged | bridged |
| `/v1/embeddings` | native | no | `batchEmbedContents` | `predict` | Titan (one call per input), Cohere |

Bridged Responses cover text, function tools, `instructions`, `text.format` JSON schemas and
`reasoning.effort`; `previous_response_id` only works for natively relayed responses, whose ids
encode the deployment that produced them.
