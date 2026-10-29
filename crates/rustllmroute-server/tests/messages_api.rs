use std::net::SocketAddr;
use std::path::Path;

use clap::Parser;
use rustllmroute_config::{Cli, LoadedConfig};
use rustllmroute_server::{app, state};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MASTER: &str = "sk-master-test";

async fn boot(anthropic: &MockServer, openai: &MockServer) -> SocketAddr {
    let yaml = format!(
        r#"
model_list:
  - model_name: claude
    litellm_params: {{ model: anthropic/claude-sonnet-4-5, api_key: sk-ant, api_base: {a} }}
    model_info: {{ id: dep-claude }}
  - model_name: gpt
    litellm_params: {{ model: openai/gpt-4o-mini, api_key: sk-oai, api_base: {b} }}
    model_info: {{ id: dep-gpt }}
general_settings:
  master_key: {MASTER}
"#,
        a = anthropic.uri(),
        b = openai.uri()
    );
    let loaded = LoadedConfig::parse(&yaml, Path::new("test.yaml")).unwrap();
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

fn anthropic_response() -> Value {
    json!({
        "id": "msg_01", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5",
        "content": [{"type": "text", "text": "Hello from Claude"}],
        "stop_reason": "end_turn", "stop_sequence": null,
        "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 2}
    })
}

const ANTHROPIC_STREAM: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-4-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: ping\ndata: {\"type\":\"ping\"}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":4}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
);

fn openai_response() -> Value {
    json!({
        "id": "chatcmpl-7", "object": "chat.completion", "created": 1, "model": "gpt-4o-mini",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Oslo\"}"}}]}, "finish_reason": "tool_calls"}],
        "usage": {"prompt_tokens": 20, "completion_tokens": 8, "total_tokens": 28}
    })
}

const OPENAI_STREAM: &str = concat!(
    "data: {\"id\":\"chatcmpl-7\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-7\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-7\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: {\"id\":\"chatcmpl-7\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-mini\",\"choices\":[],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":2,\"total_tokens\":22}}\n\n",
    "data: [DONE]\n\n"
);

fn sse_events(text: &str) -> Vec<(String, Value)> {
    text.split("\n\n")
        .filter(|frame| !frame.is_empty())
        .map(|frame| {
            let mut event = String::new();
            let mut data = String::new();
            for line in frame.lines() {
                if let Some(rest) = line.strip_prefix("event: ") {
                    event = rest.to_owned();
                } else if let Some(rest) = line.strip_prefix("data: ") {
                    data = rest.to_owned();
                }
            }
            (event, serde_json::from_str(&data).unwrap_or(Value::Null))
        })
        .collect()
}

fn messages_body(model: &str, stream: bool) -> Value {
    json!({"model": model, "max_tokens": 64, "stream": stream, "messages": [{"role": "user", "content": "hi"}]})
}

#[tokio::test]
async fn chat_completions_against_an_anthropic_deployment() {
    let (anthropic, openai) = (MockServer::start().await, MockServer::start().await);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-ant"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response()))
        .expect(1)
        .mount(&anthropic)
        .await;
    let address = boot(&anthropic, &openai).await;
    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "claude", "messages": [{"role": "system", "content": "be brief"}, {"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-litellm-model-id"], "dep-claude");
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["model"], "claude");
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "Hello from Claude"
    );
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(
        body["usage"]["prompt_tokens"], 12,
        "cache reads are folded into prompt tokens"
    );
    let sent: Value =
        serde_json::from_slice(&anthropic.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["model"], "claude-sonnet-4-5");
    assert_eq!(sent["system"][0]["text"], "be brief");
    assert_eq!(sent["messages"][0]["role"], "user");
    assert!(
        sent["max_tokens"].as_u64().unwrap() > 0,
        "max_tokens is always sent"
    );
}

#[tokio::test]
async fn chat_completions_stream_against_an_anthropic_deployment() {
    let (anthropic, openai) = (MockServer::start().await, MockServer::start().await);
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(ANTHROPIC_STREAM, "text/event-stream"),
        )
        .mount(&anthropic)
        .await;
    let address = boot(&anthropic, &openai).await;
    let text = reqwest::Client::new()
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "claude", "stream": true, "stream_options": {"include_usage": true}, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
    let chunks: Vec<Value> = text
        .split("\n\n")
        .filter_map(|f| f.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .map(|d| serde_json::from_str(d).unwrap())
        .collect();
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    let content: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(content, "Hi");
    assert!(
        chunks
            .iter()
            .any(|c| c["choices"][0]["finish_reason"] == "stop")
    );
    let usage = chunks
        .iter()
        .find_map(|c| c.get("usage").filter(|u| !u.is_null()))
        .unwrap();
    assert_eq!(usage["prompt_tokens"], 10);
    assert_eq!(usage["completion_tokens"], 4);
}

#[tokio::test]
async fn messages_are_relayed_natively_to_anthropic() {
    let (anthropic, openai) = (MockServer::start().await, MockServer::start().await);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("anthropic-beta", "tools-2024-04-04"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response()))
        .mount(&anthropic)
        .await;
    let address = boot(&anthropic, &openai).await;
    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/messages"))
        .header("x-api-key", MASTER)
        .header("anthropic-beta", "tools-2024-04-04")
        .json(&messages_body("claude", false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let cost: f64 = response.headers()["x-litellm-response-cost"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(cost > 0.0);
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        body,
        anthropic_response(),
        "native responses are forwarded byte for byte"
    );
    let sent: Value =
        serde_json::from_slice(&anthropic.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["model"], "claude-sonnet-4-5");
    assert_eq!(sent["max_tokens"], 64);
}

#[tokio::test]
async fn messages_streams_are_relayed_raw() {
    let (anthropic, openai) = (MockServer::start().await, MockServer::start().await);
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(ANTHROPIC_STREAM, "text/event-stream"),
        )
        .mount(&anthropic)
        .await;
    let address = boot(&anthropic, &openai).await;
    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/messages"))
        .header("x-api-key", MASTER)
        .json(&messages_body("claude", true))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let text = response.text().await.unwrap();
    assert_eq!(
        text, ANTHROPIC_STREAM,
        "every byte of the upstream stream is forwarded"
    );
}

#[tokio::test]
async fn messages_are_translated_for_openai_deployments() {
    let (anthropic, openai) = (MockServer::start().await, MockServer::start().await);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_response()))
        .mount(&openai)
        .await;
    let address = boot(&anthropic, &openai).await;
    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/messages"))
        .header("x-api-key", MASTER)
        .json(&json!({
            "model": "gpt", "max_tokens": 64, "system": "be brief",
            "messages": [{"role": "user", "content": "weather in Oslo?"}],
            "tools": [{"name": "get_weather", "description": "d", "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["type"], "message");
    assert_eq!(body["model"], "gpt");
    assert_eq!(body["stop_reason"], "tool_use");
    assert_eq!(body["content"][0]["type"], "tool_use");
    assert_eq!(body["content"][0]["name"], "get_weather");
    assert_eq!(body["content"][0]["input"]["city"], "Oslo");
    assert_eq!(body["usage"]["input_tokens"], 20);
    assert_eq!(body["usage"]["output_tokens"], 8);
    let sent: Value =
        serde_json::from_slice(&openai.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["messages"][0]["role"], "system");
    assert_eq!(sent["tools"][0]["function"]["name"], "get_weather");
    assert_eq!(sent["max_tokens"], 64);
}

#[tokio::test]
async fn messages_streams_are_encoded_as_anthropic_events_for_openai_deployments() {
    let (anthropic, openai) = (MockServer::start().await, MockServer::start().await);
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(OPENAI_STREAM, "text/event-stream"))
        .mount(&openai)
        .await;
    let address = boot(&anthropic, &openai).await;
    let text = reqwest::Client::new()
        .post(format!("http://{address}/v1/messages"))
        .header("x-api-key", MASTER)
        .json(&messages_body("gpt", true))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let events = sse_events(&text);
    let kinds: Vec<&str> = events.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        kinds,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop"
        ],
        "{text}"
    );
    assert_eq!(events[0].1["message"]["model"], "gpt");
    assert_eq!(events[2].1["delta"]["text"], "Hel");
    assert_eq!(events[5].1["delta"]["stop_reason"], "end_turn");
    assert_eq!(events[5].1["usage"]["output_tokens"], 2);
    assert_eq!(events[5].1["usage"]["input_tokens"], 20);
}

#[tokio::test]
async fn count_tokens_forwards_natively_and_estimates_otherwise() {
    let (anthropic, openai) = (MockServer::start().await, MockServer::start().await);
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 42})))
        .mount(&anthropic)
        .await;
    let address = boot(&anthropic, &openai).await;
    let client = reqwest::Client::new();
    let native: Value = client
        .post(format!("http://{address}/v1/messages/count_tokens"))
        .header("x-api-key", MASTER)
        .json(&json!({"model": "claude", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(native["input_tokens"], 42);
    let estimated: Value = client
        .post(format!("http://{address}/v1/messages/count_tokens"))
        .header("x-api-key", MASTER)
        .json(&json!({"model": "gpt", "messages": [{"role": "user", "content": "hello there, how are you today?"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(estimated["input_tokens"].as_u64().unwrap() >= 5);

    let unauthenticated = client
        .post(format!("http://{address}/v1/messages"))
        .json(&messages_body("claude", false))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), 401);
    let body: Value = unauthenticated.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "authentication_error");
}
