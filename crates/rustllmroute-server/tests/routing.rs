use std::net::SocketAddr;
use std::path::Path;

use clap::Parser;
use rustllmroute_config::{Cli, LoadedConfig};
use rustllmroute_server::{app, state};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const MASTER: &str = "sk-master-test";

async fn boot(primary: &MockServer, secondary: &MockServer, fallback: &MockServer) -> SocketAddr {
    let yaml = format!(
        r#"
model_list:
  - model_name: gpt
    litellm_params: {{ model: openai/gpt-4o-mini, api_key: k, api_base: {a}, order: 0 }}
    model_info: {{ id: dep-a }}
  - model_name: gpt
    litellm_params: {{ model: openai/gpt-4o-mini, api_key: k, api_base: {b}, order: 1 }}
    model_info: {{ id: dep-b }}
  - model_name: backup
    litellm_params: {{ model: openai/gpt-4o-mini, api_key: k, api_base: {c} }}
    model_info: {{ id: dep-c }}
router_settings:
  cooldown_time: 60
  fallbacks: [{{"gpt": ["backup"]}}]
general_settings:
  master_key: {MASTER}
"#,
        a = primary.uri(),
        b = secondary.uri(),
        c = fallback.uri()
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

fn ok_body(content: &str) -> Value {
    json!({
        "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "gpt-4o-mini",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
    })
}

async fn chat(address: SocketAddr, model: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn rate_limited_primary_is_retried_on_the_next_tier_and_cooled_down() {
    let (primary, secondary, fallback) = (
        MockServer::start().await,
        MockServer::start().await,
        MockServer::start().await,
    );
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "30")
                .set_body_json(json!({"error": {"message": "slow"}})),
        )
        .mount(&primary)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body("from b")))
        .mount(&secondary)
        .await;
    let address = boot(&primary, &secondary, &fallback).await;

    let response = chat(address, "gpt").await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-litellm-model-id"], "dep-b");
    assert_eq!(response.headers()["x-litellm-attempted-retries"], "1");
    assert_eq!(response.headers()["x-litellm-attempted-fallbacks"], "0");
    assert_eq!(primary.received_requests().await.unwrap().len(), 1);

    let response = chat(address, "gpt").await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["x-litellm-attempted-retries"],
        "0",
        "dep-a is cooling down"
    );
    assert_eq!(
        primary.received_requests().await.unwrap().len(),
        1,
        "no second call to the cooled deployment"
    );
}

#[tokio::test]
async fn exhausted_group_falls_back_and_reports_it() {
    let (primary, secondary, fallback) = (
        MockServer::start().await,
        MockServer::start().await,
        MockServer::start().await,
    );
    for server in [&primary, &secondary] {
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(500).set_body_json(json!({"error": {"message": "boom"}})),
            )
            .mount(server)
            .await;
    }
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body("from c")))
        .mount(&fallback)
        .await;
    let address = boot(&primary, &secondary, &fallback).await;
    let response = chat(address, "gpt").await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-litellm-model-id"], "dep-c");
    assert_eq!(
        response.headers()["x-litellm-attempted-fallbacks"],
        "2",
        "the order-1 tier of the same group is tried before the backup group"
    );
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "from c");
    assert_eq!(
        body["model"], "gpt",
        "the client still sees the name it asked for"
    );
}

#[tokio::test]
async fn client_can_disable_fallbacks_per_request() {
    let (primary, secondary, fallback) = (
        MockServer::start().await,
        MockServer::start().await,
        MockServer::start().await,
    );
    for server in [&primary, &secondary] {
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(503).set_body_json(json!({"error": {"message": "down"}})),
            )
            .mount(server)
            .await;
    }
    let address = boot(&primary, &secondary, &fallback).await;
    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "gpt", "messages": [{"role": "user", "content": "hi"}], "disable_fallbacks": true, "num_retries": 0}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert!(fallback.received_requests().await.unwrap().is_empty());
    let sent: Value =
        serde_json::from_slice(&primary.received_requests().await.unwrap()[0].body).unwrap();
    assert!(
        sent.get("disable_fallbacks").is_none(),
        "gateway-only params are stripped before forwarding"
    );
}
