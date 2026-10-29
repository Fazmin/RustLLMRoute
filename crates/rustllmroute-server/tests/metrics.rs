use std::net::SocketAddr;
use std::path::Path;

use clap::Parser;
use rustllmroute_config::{Cli, LoadedConfig};
use rustllmroute_server::{app, state};
use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const MASTER: &str = "sk-master-test";

async fn boot(upstream: &MockServer, metrics_auth: bool) -> SocketAddr {
    let yaml = format!(
        r#"
model_list:
  - model_name: gpt
    litellm_params: {{ model: openai/gpt-4o-mini, api_key: sk-a, api_base: {u} }}
    model_info: {{ id: dep-metrics }}
general_settings:
  master_key: {MASTER}
  metrics_auth: {metrics_auth}
"#,
        u = upstream.uri()
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

#[tokio::test]
async fn metrics_endpoint_exposes_request_deployment_and_spend_series() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "gpt-4o-mini",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 7, "completion_tokens": 2, "total_tokens": 9}
        })))
        .mount(&upstream)
        .await;
    let address = boot(&upstream, false).await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "gpt", "messages": [{"role": "user", "content": "hello"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let text = client
        .get(format!("http://{address}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for name in [
        "rustllmroute_build_info",
        "litellm_proxy_total_requests_metric",
        "litellm_requests_metric",
        "litellm_request_total_latency_metric",
        "litellm_llm_api_latency_metric",
        "litellm_total_tokens_metric",
        "litellm_input_tokens_metric",
        "litellm_output_tokens_metric",
        "litellm_spend_metric",
        "litellm_deployment_total_requests",
        "litellm_deployment_success_responses",
        "litellm_in_flight_requests",
    ] {
        assert!(text.contains(name), "missing {name} in:\n{text}");
    }
    assert!(text.contains("model_id=\"dep-metrics\""), "{text}");
    assert!(text.contains("hashed_api_key=\"master\""), "{text}");
}

#[tokio::test]
async fn metrics_endpoint_requires_a_key_by_default() {
    let upstream = MockServer::start().await;
    let address = boot(&upstream, true).await;
    let client = reqwest::Client::new();
    let anonymous = client
        .get(format!("http://{address}/metrics"))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), 401);
    let authed = client
        .get(format!("http://{address}/metrics"))
        .bearer_auth(MASTER)
        .send()
        .await
        .unwrap();
    assert_eq!(authed.status(), 200);
    assert!(
        authed.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/plain")
    );
}
