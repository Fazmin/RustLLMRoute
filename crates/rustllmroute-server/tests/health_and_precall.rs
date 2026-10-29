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
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
}

#[tokio::test]
async fn on_demand_health_probes_report_each_deployment() {
    let healthy = MockServer::start().await;
    let broken = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_json("ok")))
        .expect(1)
        .mount(&healthy)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_json(
            json!({"error": {"message": "bad key", "type": "invalid_request_error"}}),
        ))
        .expect(1)
        .mount(&broken)
        .await;
    let yaml = format!(
        r#"
model_list:
  - model_name: gpt
    litellm_params: {{ model: openai/gpt-4o-mini, api_key: sk-a, api_base: {a} }}
    model_info: {{ id: dep-a }}
  - model_name: gpt
    litellm_params: {{ model: openai/gpt-4o-mini, api_key: sk-b, api_base: {b} }}
    model_info: {{ id: dep-b }}
  - model_name: "wild/*"
    litellm_params: {{ model: "openai/*", api_key: sk-a, api_base: {a} }}
    model_info: {{ id: dep-wild }}
general_settings:
  master_key: {MASTER}
"#,
        a = healthy.uri(),
        b = broken.uri()
    );
    let address = boot(&yaml).await;
    let report: Value = reqwest::Client::new()
        .get(format!("http://{address}/health"))
        .bearer_auth(MASTER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(report["status"], "degraded", "{report}");
    let healthy_ids: Vec<&str> = report["healthy_endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["model_id"].as_str().unwrap())
        .collect();
    let unhealthy_ids: Vec<&str> = report["unhealthy_endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["model_id"].as_str().unwrap())
        .collect();
    let unknown_ids: Vec<&str> = report["unknown_endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["model_id"].as_str().unwrap())
        .collect();
    assert_eq!(healthy_ids, vec!["dep-a"]);
    assert_eq!(unhealthy_ids, vec!["dep-b"]);
    assert_eq!(unknown_ids, vec!["dep-wild"], "wildcards are not probed");
    let unhealthy = &report["unhealthy_endpoints"][0];
    assert!(
        unhealthy["error"].as_str().unwrap().contains("bad key"),
        "{unhealthy}"
    );
    assert!(report["healthy_endpoints"][0]["latency_ms"].is_number());
    let probe: Value =
        serde_json::from_slice(&healthy.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(probe["max_tokens"], 1);
    assert_eq!(probe["messages"][0]["content"], "test");
}

#[tokio::test]
async fn pre_call_context_check_routes_long_prompts_to_the_larger_window() {
    let small = MockServer::start().await;
    let large = MockServer::start().await;
    for server in [&small, &large] {
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(chat_json("ok")))
            .mount(server)
            .await;
    }
    let yaml = format!(
        r#"
model_list:
  - model_name: gpt
    litellm_params: {{ model: openai/gpt-4o-mini, api_key: sk-a, api_base: {s} }}
    model_info: {{ id: dep-small, max_input_tokens: 50 }}
  - model_name: gpt
    litellm_params: {{ model: openai/gpt-4o-mini, api_key: sk-b, api_base: {l} }}
    model_info: {{ id: dep-large, max_input_tokens: 400 }}
router_settings:
  enable_pre_call_checks: true
general_settings:
  master_key: {MASTER}
"#,
        s = small.uri(),
        l = large.uri()
    );
    let address = boot(&yaml).await;
    let client = reqwest::Client::new();
    let long_prompt = "lorem ipsum dolor sit amet ".repeat(30);
    for _ in 0..4 {
        let response = client
            .post(format!("http://{address}/v1/chat/completions"))
            .bearer_auth(MASTER)
            .json(&json!({"model": "gpt", "messages": [{"role": "user", "content": long_prompt}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["x-litellm-model-id"], "dep-large");
    }
    assert!(
        small.received_requests().await.unwrap().is_empty(),
        "the small window never sees the long prompt"
    );
    assert_eq!(large.received_requests().await.unwrap().len(), 4);

    let huge_prompt = "lorem ipsum dolor sit amet ".repeat(200);
    let response = client
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "gpt", "messages": [{"role": "user", "content": huge_prompt}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "context_length_exceeded", "{body}");
}
