use std::net::SocketAddr;
use std::path::Path;

use clap::Parser;
use rustllmroute_config::{Cli, LoadedConfig};
use rustllmroute_server::{app, state};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MASTER: &str = "sk-master-test";

async fn boot(yaml: &str) -> SocketAddr {
    let loaded = LoadedConfig::parse(yaml, Path::new("test.yaml")).unwrap();
    let state = state::build(loaded, &Cli::parse_from(["rustllmroute"]))
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = app::router(state);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    address
}

fn chat_json(content: &str) -> Value {
    json!({
        "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "gpt-4o-mini",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
    })
}

fn config(openai: &str, anthropic: &str, guardrails: &str) -> String {
    format!(
        r#"
model_list:
  - model_name: gpt
    litellm_params: {{ model: openai/gpt-4o-mini, api_key: sk-a, api_base: {openai} }}
    model_info: {{ id: dep-gpt }}
  - model_name: claude
    litellm_params: {{ model: anthropic/claude-sonnet-4-5, api_key: sk-ant, api_base: {anthropic} }}
    model_info: {{ id: dep-claude }}
general_settings:
  master_key: {MASTER}
guardrails:
{guardrails}
"#
    )
}

#[tokio::test]
async fn pre_call_masks_upstream_and_blocks_with_a_400() {
    let openai = MockServer::start().await;
    let anthropic = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_json("ok")))
        .mount(&openai)
        .await;
    let yaml = config(
        &openai.uri(),
        &anthropic.uri(),
        r#"  - guardrail_name: pii
    litellm_params:
      guardrail: content_filter
      mode: pre_call
      default_on: true
      patterns: [{name: email, action: MASK}]
      blocked_words: [forbidden]
  - guardrail_name: strict
    litellm_params:
      guardrail: content_filter
      mode: pre_call
      blocked_words: [banana]"#,
    );
    let address = boot(&yaml).await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "gpt", "messages": [{"role": "user", "content": "mail me at a.b@example.com please"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let sent: Value =
        serde_json::from_slice(&openai.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(
        sent["messages"][0]["content"],
        "mail me at [EMAIL_REDACTED] please"
    );

    let blocked = client
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "gpt", "messages": [{"role": "user", "content": "this is forbidden"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(blocked.status(), 400);
    assert_eq!(blocked.headers()["x-rustllmroute-guardrail"], "pii");
    let body: Value = blocked.json().await.unwrap();
    assert_eq!(body["error"]["code"], "guardrail_violation", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("forbidden")
    );

    let allowed = client
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "gpt", "messages": [{"role": "user", "content": "banana split"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status(), 200, "strict is not default_on");
    let selected = client
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "gpt", "guardrails": ["strict"], "messages": [{"role": "user", "content": "banana split"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(selected.status(), 400);
    assert_eq!(selected.headers()["x-rustllmroute-guardrail"], "strict");
    let unknown = client
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "gpt", "guardrails": ["nope"], "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 400);
    assert_eq!(
        openai.received_requests().await.unwrap().len(),
        2,
        "only the two allowed calls reached upstream"
    );
}

#[tokio::test]
async fn post_call_guardrails_buffer_streams_and_filter_responses() {
    let openai = MockServer::start().await;
    let anthropic = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(chat_json("the secret code is 123-45-6789 ok")),
        )
        .mount(&openai)
        .await;
    let yaml = config(
        &openai.uri(),
        &anthropic.uri(),
        r#"  - guardrail_name: outbound
    litellm_params:
      guardrail: content_filter
      mode: post_call
      default_on: true
      patterns: [{name: us_ssn, action: MASK}]
  - guardrail_name: no-secrets
    litellm_params:
      guardrail: content_filter
      mode: post_call
      blocked_words: [secret]"#,
    );
    let address = boot(&yaml).await;
    let client = reqwest::Client::new();
    let text = client
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "gpt", "stream": true, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
    let content: String = text
        .split("\n\n")
        .filter_map(|f| f.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .map(|d| serde_json::from_str::<Value>(d).unwrap())
        .filter_map(|c| {
            c["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_owned)
        })
        .collect();
    assert_eq!(content, "the secret code is [US_SSN_REDACTED] ok");
    let sent: Value =
        serde_json::from_slice(&openai.received_requests().await.unwrap()[0].body).unwrap();
    assert_ne!(
        sent["stream"], true,
        "post-call guardrails buffer the upstream call"
    );

    let blocked = client
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "gpt", "stream": true, "guardrails": ["no-secrets"], "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(blocked.status(), 400);
    assert_eq!(blocked.headers()["x-rustllmroute-guardrail"], "no-secrets");

    let json_reply: Value = client
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "gpt", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        json_reply["choices"][0]["message"]["content"],
        "the secret code is [US_SSN_REDACTED] ok"
    );
}

#[tokio::test]
async fn messages_endpoint_masks_before_native_relay() {
    let openai = MockServer::start().await;
    let anthropic = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5",
            "content": [{"type": "text", "text": "reach me at c.d@example.com"}], "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 4}
        })))
        .mount(&anthropic)
        .await;
    let yaml = config(
        &openai.uri(),
        &anthropic.uri(),
        r#"  - guardrail_name: pii
    litellm_params:
      guardrail: content_filter
      mode: [pre_call, post_call]
      default_on: true
      patterns: [{name: email, action: MASK}]"#,
    );
    let address = boot(&yaml).await;
    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/messages"))
        .header("x-api-key", MASTER)
        .json(&json!({"model": "claude", "max_tokens": 16, "system": "you know a.b@example.com", "messages": [{"role": "user", "content": [{"type": "text", "text": "write to x.y@example.com"}]}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        body["content"][0]["text"], "reach me at [EMAIL_REDACTED]",
        "post-call masks the native reply"
    );
    let sent: Value =
        serde_json::from_slice(&anthropic.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["system"], "you know [EMAIL_REDACTED]");
    assert_eq!(
        sent["messages"][0]["content"][0]["text"],
        "write to [EMAIL_REDACTED]"
    );
}

#[tokio::test]
async fn tool_permission_and_generic_api_failure_modes() {
    let openai = MockServer::start().await;
    let anthropic = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "gpt-4o-mini",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "shell", "arguments": "{}"}}]}, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
        })))
        .mount(&openai)
        .await;
    let yaml = config(
        &openai.uri(),
        &anthropic.uri(),
        r#"  - guardrail_name: tools
    litellm_params:
      guardrail: tool_permission
      mode: post_call
      default_on: true
      rules: [{id: reads, tool_name: "read_.*", action: allow}]
      default_action: deny
  - guardrail_name: ext-open
    litellm_params:
      guardrail: generic_guardrail_api
      mode: pre_call
      api_base: http://127.0.0.1:9/check
      on_error: fail_open
  - guardrail_name: ext-closed
    litellm_params:
      guardrail: generic_guardrail_api
      mode: pre_call
      api_base: http://127.0.0.1:9/check
      on_error: fail_closed"#,
    );
    let address = boot(&yaml).await;
    let client = reqwest::Client::new();
    let denied = client
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "gpt", "messages": [{"role": "user", "content": "run it"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 400);
    let body: Value = denied.json().await.unwrap();
    assert!(
        body["error"]["message"].as_str().unwrap().contains("shell"),
        "{body}"
    );

    let open = client
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "gpt", "guardrails": ["ext-open"], "messages": [{"role": "user", "content": "read_file please"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        open.status(),
        400,
        "fail_open lets the request reach the model; tool_permission then denies the shell call"
    );

    let closed = client
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "gpt", "guardrails": ["ext-closed"], "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(closed.status(), 500);
    let body: Value = closed.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("ext-closed"),
        "{body}"
    );
}
