use std::net::SocketAddr;
use std::path::Path;

use clap::Parser;
use rustllmroute_config::{Cli, LoadedConfig};
use rustllmroute_server::{app, state};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MASTER: &str = "sk-master-test";

async fn boot(upstream: &MockServer) -> SocketAddr {
    let yaml = format!(
        r#"
model_list:
  - model_name: test-model
    litellm_params: {{ model: openai/gpt-4o-mini, api_key: sk-upstream, api_base: {base} }}
    model_info: {{ id: dep-a }}
  - model_name: "wild/*"
    litellm_params: {{ model: "groq/*", api_key: sk-upstream, api_base: {base} }}
general_settings:
  master_key: {MASTER}
"#,
        base = upstream.uri()
    );
    let loaded = LoadedConfig::parse(&yaml, Path::new("test.yaml")).unwrap();
    let cli = Cli::parse_from(["rustllmroute"]);
    let state = state::build(loaded, &cli).await.unwrap();
    let router = app::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    address
}

fn chat_body(model: &str, stream: bool) -> Value {
    json!({
        "model": model,
        "messages": [{"role": "user", "content": "hi"}],
        "stream": stream,
    })
}

fn completion_json() -> Value {
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 1,
        "model": "gpt-4o-mini",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hello"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
    })
}

const STREAM_BODY: &str = concat!(
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-mini\",\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1,\"total_tokens\":6}}\n\n",
    "data: [DONE]\n\n"
);

#[tokio::test]
async fn non_streaming_chat_is_forwarded_and_accounted() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer sk-upstream"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion_json()))
        .expect(1)
        .mount(&upstream)
        .await;
    let address = boot(&upstream).await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&chat_body("test-model", false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let headers = response.headers().clone();
    assert!(headers.contains_key("x-litellm-call-id"));
    assert_eq!(headers["x-litellm-model-id"], "dep-a");
    let cost: f64 = headers["x-litellm-response-cost"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(cost > 0.0, "gpt-4o-mini has catalog pricing");
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        body["model"], "test-model",
        "the client sees the name it asked for"
    );
    assert_eq!(body["choices"][0]["message"]["content"], "Hello");
    let sent = &upstream.received_requests().await.unwrap()[0];
    let sent: Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(
        sent["model"], "gpt-4o-mini",
        "the provider sees the deployment model"
    );
}

#[tokio::test]
async fn streaming_chat_normalises_chunks_and_terminates() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(STREAM_BODY, "text/event-stream"),
        )
        .mount(&upstream)
        .await;
    let address = boot(&upstream).await;
    let response = reqwest::Client::new()
        .post(format!("http://{address}/chat/completions"))
        .header("x-api-key", MASTER)
        .json(&chat_body("test-model", true))
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
    assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
    let chunks: Vec<Value> = text
        .split("\n\n")
        .filter_map(|frame| frame.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).unwrap())
        .collect();
    assert!(chunks.iter().all(|chunk| chunk["model"] == "test-model"));
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "Hello");
    let last = chunks.last().unwrap();
    assert_eq!(last["choices"][0]["finish_reason"], "stop");
    assert!(
        chunks.iter().all(|chunk| chunk.get("usage").is_none()),
        "usage is only forwarded on request"
    );
    let sent: Value =
        serde_json::from_slice(&upstream.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(
        sent["stream_options"]["include_usage"], true,
        "usage is requested upstream for accounting"
    );
}

#[tokio::test]
async fn streaming_usage_is_forwarded_when_requested() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(STREAM_BODY, "text/event-stream"))
        .mount(&upstream)
        .await;
    let address = boot(&upstream).await;
    let mut body = chat_body("test-model", true);
    body["stream_options"] = json!({"include_usage": true});
    let text = reqwest::Client::new()
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&body)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(text.contains("\"total_tokens\":6"), "{text}");
}

#[tokio::test]
async fn wildcard_groups_substitute_the_suffix() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion_json()))
        .mount(&upstream)
        .await;
    let address = boot(&upstream).await;
    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&chat_body("wild/llama-3.3-70b", false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let sent: Value =
        serde_json::from_slice(&upstream.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["model"], "llama-3.3-70b");
}

#[tokio::test]
async fn auth_and_routing_errors_use_openai_error_bodies() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "7")
                .set_body_json(
                    json!({"error": {"message": "slow down", "type": "rate_limit_error"}}),
                ),
        )
        .mount(&upstream)
        .await;
    let address = boot(&upstream).await;
    let client = reqwest::Client::new();
    let url = format!("http://{address}/v1/chat/completions");

    let missing = client
        .post(&url)
        .json(&chat_body("test-model", false))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 401);
    let body: Value = missing.json().await.unwrap();
    assert_eq!(body["error"]["type"], "authentication_error");

    let wrong = client
        .post(&url)
        .bearer_auth("sk-wrong")
        .json(&chat_body("test-model", false))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);

    let unknown = client
        .post(&url)
        .bearer_auth(MASTER)
        .json(&chat_body("nope", false))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);
    let body: Value = unknown.json().await.unwrap();
    assert_eq!(body["error"]["code"], "model_not_found");

    let limited = client
        .post(&url)
        .bearer_auth(MASTER)
        .json(&chat_body("test-model", false))
        .send()
        .await
        .unwrap();
    assert_eq!(limited.status(), 429);
    assert_eq!(limited.headers()["retry-after"], "7");
    let body: Value = limited.json().await.unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error");

    let models: Value = client
        .get(format!("http://{address}/v1/models"))
        .bearer_auth(MASTER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["test-model"]);

    let liveness = client
        .get(format!("http://{address}/health/liveliness"))
        .send()
        .await
        .unwrap();
    assert_eq!(liveness.status(), 200);
}
