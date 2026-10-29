use std::net::SocketAddr;
use std::path::Path;

use clap::Parser;
use rustllmroute_config::{Cli, LoadedConfig};
use rustllmroute_server::{app, state};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MASTER: &str = "sk-master-test";

struct Mocks {
    openai: MockServer,
    gemini: MockServer,
    bedrock: MockServer,
    groq: MockServer,
}

async fn mocks() -> Mocks {
    Mocks {
        openai: MockServer::start().await,
        gemini: MockServer::start().await,
        bedrock: MockServer::start().await,
        groq: MockServer::start().await,
    }
}

async fn boot(mocks: &Mocks) -> SocketAddr {
    let yaml = format!(
        r#"
model_list:
  - model_name: embed-oai
    litellm_params: {{ model: openai/text-embedding-3-small, api_key: sk-oai, api_base: {o} }}
  - model_name: embed-gemini
    litellm_params: {{ model: gemini/gemini-embedding-001, api_key: g-key, api_base: {g} }}
  - model_name: embed-titan
    litellm_params: {{ model: bedrock/amazon.titan-embed-text-v2:0, aws_region_name: us-east-1, aws_access_key_id: AKIATEST, aws_secret_access_key: secret, api_base: {b} }}
  - model_name: gpt
    litellm_params: {{ model: openai/gpt-4o-mini, api_key: sk-oai, api_base: {o} }}
    model_info: {{ id: dep-gpt }}
  - model_name: llama
    litellm_params: {{ model: groq/llama-3.3-70b-versatile, api_key: sk-groq, api_base: {q} }}
    model_info: {{ id: dep-llama }}
general_settings:
  master_key: {MASTER}
"#,
        o = mocks.openai.uri(),
        g = mocks.gemini.uri(),
        b = mocks.bedrock.uri(),
        q = mocks.groq.uri()
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

fn sse_events(text: &str) -> Vec<(String, Value)> {
    text.split("\n\n")
        .filter(|frame| !frame.is_empty() && !frame.starts_with(':'))
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

#[tokio::test]
async fn openai_embeddings_are_forwarded_and_priced() {
    let mocks = mocks().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list", "model": "text-embedding-3-small",
            "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2]}, {"object": "embedding", "index": 1, "embedding": [0.3, 0.4]}],
            "usage": {"prompt_tokens": 4, "total_tokens": 4}
        })))
        .expect(1)
        .mount(&mocks.openai)
        .await;
    let address = boot(&mocks).await;
    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/embeddings"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "embed-oai", "input": ["a", "b"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let cost: f64 = response.headers()["x-litellm-response-cost"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(cost > 0.0, "embedding pricing resolves from the catalog");
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["model"], "embed-oai");
    assert_eq!(body["data"].as_array().unwrap().len(), 2);
    assert_eq!(body["usage"]["prompt_tokens"], 4);
    let sent: Value =
        serde_json::from_slice(&mocks.openai.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["model"], "text-embedding-3-small");
    assert_eq!(sent["input"][1], "b");
}

#[tokio::test]
async fn gemini_embeddings_use_batch_embed_and_estimate_tokens() {
    let mocks = mocks().await;
    Mock::given(method("POST"))
        .and(path(
            "/v1beta/models/gemini-embedding-001:batchEmbedContents",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "embeddings": [{"values": [0.1]}, {"values": [0.2]}, {"values": [0.3]}]
        })))
        .expect(1)
        .mount(&mocks.gemini)
        .await;
    let address = boot(&mocks).await;
    let body: Value = reqwest::Client::new()
        .post(format!("http://{address}/v1/embeddings"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "embed-gemini", "input": ["alpha", "beta", "gamma"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["model"], "embed-gemini");
    assert_eq!(body["data"].as_array().unwrap().len(), 3);
    assert_eq!(body["data"][2]["index"], 2);
    assert!(
        body["usage"]["prompt_tokens"].as_u64().unwrap() >= 3,
        "{body}"
    );
}

#[tokio::test]
async fn titan_embeddings_fan_out_one_call_per_input() {
    let mocks = mocks().await;
    Mock::given(method("POST"))
        .and(path_regex(r"/model/.*/invoke$"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"embedding": [0.5, 0.5], "inputTextTokenCount": 2})),
        )
        .expect(2)
        .mount(&mocks.bedrock)
        .await;
    let address = boot(&mocks).await;
    let body: Value = reqwest::Client::new()
        .post(format!("http://{address}/v1/embeddings"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "embed-titan", "input": ["hello", "world"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["data"].as_array().unwrap().len(), 2);
    assert_eq!(body["data"][1]["index"], 1);
    assert_eq!(body["usage"]["prompt_tokens"], 4, "{body}");
    let requests = mocks.bedrock.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let mut texts: Vec<String> = requests
        .iter()
        .map(|r| {
            serde_json::from_slice::<Value>(&r.body).unwrap()["inputText"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    texts.sort();
    assert_eq!(texts, vec!["hello", "world"]);
    assert!(
        requests[0].headers["authorization"]
            .to_str()
            .unwrap()
            .starts_with("AWS4-HMAC-SHA256")
    );
}

#[tokio::test]
async fn responses_native_relay_encodes_ids_and_routes_previous_response_id() {
    let mocks = mocks().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_up1", "object": "response", "created_at": 1, "status": "completed", "model": "gpt-4o-mini",
            "output": [{"type": "message", "id": "msg_1", "status": "completed", "role": "assistant", "content": [{"type": "output_text", "text": "hi", "annotations": []}]}],
            "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}
        })))
        .expect(2)
        .mount(&mocks.openai)
        .await;
    let address = boot(&mocks).await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{address}/v1/responses"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "gpt", "input": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-litellm-model-id"], "dep-gpt");
    let cost: f64 = response.headers()["x-litellm-response-cost"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(cost > 0.0);
    let body: Value = response.json().await.unwrap();
    let id = body["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("resp_") && id != "resp_up1", "{id}");
    assert_eq!(body["model"], "gpt");
    assert_eq!(body["output"][0]["content"][0]["text"], "hi");
    let follow_up: Value = client
        .post(format!("http://{address}/v1/responses"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "gpt", "input": "again", "previous_response_id": id}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(follow_up["model"], "gpt");
    let requests = mocks.openai.received_requests().await.unwrap();
    let second: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(
        second["previous_response_id"], "resp_up1",
        "the upstream id is restored"
    );
    assert_eq!(second["model"], "gpt-4o-mini");
}

#[tokio::test]
async fn responses_native_stream_rewrites_lifecycle_events_only() {
    let mocks = mocks().await;
    let stream = concat!(
        "event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_up2\",\"object\":\"response\",\"status\":\"in_progress\",\"model\":\"gpt-4o-mini\",\"output\":[]}}\n\n",
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"sequence_number\":1,\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"Hi\"}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"sequence_number\":2,\"response\":{\"id\":\"resp_up2\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"gpt-4o-mini\",\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":1,\"total_tokens\":4}}}\n\n"
    );
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .mount(&mocks.openai)
        .await;
    let address = boot(&mocks).await;
    let text = reqwest::Client::new()
        .post(format!("http://{address}/v1/responses"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "gpt", "input": "hi", "stream": true}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let events = sse_events(&text);
    assert_eq!(events.len(), 3, "{text}");
    assert_eq!(events[0].0, "response.created");
    let created_id = events[0].1["response"]["id"].as_str().unwrap();
    assert!(created_id.starts_with("resp_") && created_id != "resp_up2");
    assert_eq!(events[0].1["response"]["model"], "gpt");
    assert!(text.contains("data: {\"type\":\"response.output_text.delta\",\"sequence_number\":1,\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"Hi\"}"), "delta frames are forwarded untouched");
    assert_eq!(events[2].1["response"]["id"], created_id);
}

#[tokio::test]
async fn responses_bridge_translates_chat_completions() {
    let mocks = mocks().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-7", "object": "chat.completion", "created": 1, "model": "llama-3.3-70b-versatile",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": null, "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Oslo\"}"}}]}, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 9, "completion_tokens": 4, "total_tokens": 13}
        })))
        .expect(1)
        .mount(&mocks.groq)
        .await;
    let address = boot(&mocks).await;
    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/responses"))
        .bearer_auth(MASTER)
        .json(&json!({
            "model": "llama", "instructions": "be brief",
            "input": [{"role": "user", "content": "weather in Oslo?"}],
            "tools": [{"type": "function", "name": "get_weather", "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert!(body["id"].as_str().unwrap().starts_with("resp_"));
    assert_eq!(body["object"], "response");
    assert_eq!(body["status"], "completed");
    assert_eq!(body["model"], "llama");
    assert_eq!(body["output"][0]["type"], "function_call");
    assert_eq!(body["output"][0]["call_id"], "call_1");
    assert_eq!(body["output"][0]["arguments"], "{\"city\":\"Oslo\"}");
    assert_eq!(body["usage"]["input_tokens"], 9);
    assert_eq!(body["usage"]["total_tokens"], 13);
    let sent: Value =
        serde_json::from_slice(&mocks.groq.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["messages"][0]["role"], "system");
    assert_eq!(sent["messages"][1]["content"], "weather in Oslo?");
    assert_eq!(sent["tools"][0]["function"]["name"], "get_weather");
}

#[tokio::test]
async fn responses_bridge_streams_responses_events() {
    let mocks = mocks().await;
    let stream = concat!(
        "data: {\"id\":\"chatcmpl-7\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"llama\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-7\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"llama\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-7\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"llama\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"id\":\"chatcmpl-7\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"llama\",\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n",
        "data: [DONE]\n\n"
    );
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .mount(&mocks.groq)
        .await;
    let address = boot(&mocks).await;
    let text = reqwest::Client::new()
        .post(format!("http://{address}/v1/responses"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "llama", "input": "hi", "stream": true}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let events = sse_events(&text);
    let kinds: Vec<&str> = events.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(kinds.first().copied(), Some("response.created"), "{text}");
    assert_eq!(kinds.last().copied(), Some("response.completed"), "{text}");
    let deltas: String = events
        .iter()
        .filter(|(k, _)| k == "response.output_text.delta")
        .filter_map(|(_, v)| v["delta"].as_str())
        .collect();
    assert_eq!(deltas, "Hello");
    let completed = &events.last().unwrap().1["response"];
    assert_eq!(completed["model"], "llama");
    assert_eq!(completed["output"][0]["content"][0]["text"], "Hello");
    assert_eq!(completed["usage"]["input_tokens"], 5);
    let sequence: Vec<u64> = events
        .iter()
        .map(|(_, v)| v["sequence_number"].as_u64().unwrap())
        .collect();
    assert!(
        sequence.windows(2).all(|w| w[1] == w[0] + 1),
        "{sequence:?}"
    );
}
