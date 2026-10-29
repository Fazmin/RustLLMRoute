use std::net::SocketAddr;
use std::path::Path;

use base64::Engine;
use bytes::{Bytes, BytesMut};
use clap::Parser;
use rustllmroute_config::{Cli, LoadedConfig};
use rustllmroute_framer::aws_event_stream::{AwsEventStreamCodec, Header, HeaderValue, Message};
use rustllmroute_server::{app, state};
use serde_json::{Value, json};
use tokio_util::codec::Encoder;
use wiremock::matchers::{header, method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MASTER: &str = "sk-master-test";

struct Mocks {
    gemini: MockServer,
    vertex: MockServer,
    bedrock: MockServer,
}

async fn boot(mocks: &Mocks) -> SocketAddr {
    let yaml = format!(
        r#"
model_list:
  - model_name: gemini
    litellm_params: {{ model: gemini/gemini-2.5-flash, api_key: g-key, api_base: {g} }}
    model_info: {{ id: dep-gemini }}
  - model_name: vgemini
    litellm_params: {{ model: vertex_ai/gemini-2.5-pro, vertex_project: proj, vertex_location: us-central1, api_key: ya29.token, api_base: {v} }}
    model_info: {{ id: dep-vgemini }}
  - model_name: vclaude
    litellm_params: {{ model: vertex_ai/claude-sonnet-4-5@20250929, vertex_project: proj, vertex_location: us-east5, api_key: ya29.token, api_base: {v} }}
    model_info: {{ id: dep-vclaude }}
  - model_name: bclaude
    litellm_params: {{ model: bedrock/anthropic.claude-sonnet-4-5-20250929-v1:0, aws_region_name: us-east-1, aws_access_key_id: AKIATEST, aws_secret_access_key: secret, api_base: {b} }}
    model_info: {{ id: dep-bclaude }}
general_settings:
  master_key: {MASTER}
"#,
        g = mocks.gemini.uri(),
        v = mocks.vertex.uri(),
        b = mocks.bedrock.uri()
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

async fn mocks() -> Mocks {
    Mocks {
        gemini: MockServer::start().await,
        vertex: MockServer::start().await,
        bedrock: MockServer::start().await,
    }
}

fn chat_body(model: &str, stream: bool) -> Value {
    json!({"model": model, "stream": stream, "messages": [{"role": "user", "content": "weather in Oslo?"}],
        "tools": [{"type": "function", "function": {"name": "get_weather", "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}}]})
}

fn sse_data(text: &str) -> Vec<Value> {
    text.split("\n\n")
        .filter_map(|f| f.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .map(|d| serde_json::from_str(d).unwrap())
        .collect()
}

fn event_stream(events: &[(&str, Value)]) -> Vec<u8> {
    let mut codec = AwsEventStreamCodec;
    let mut buffer = BytesMut::new();
    for (kind, payload) in events {
        let message = Message::new(Bytes::from(payload.to_string()))
            .add_header(Header::new(
                ":message-type",
                HeaderValue::String("event".into()),
            ))
            .add_header(Header::new(
                ":event-type",
                HeaderValue::String(kind.to_string().into()),
            ))
            .add_header(Header::new(
                ":content-type",
                HeaderValue::String("application/json".into()),
            ));
        codec.encode(message, &mut buffer).unwrap();
    }
    buffer.to_vec()
}

#[tokio::test]
async fn gemini_chat_is_translated_both_ways() {
    let mocks = mocks().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.5-flash:generateContent"))
        .and(header("x-goog-api-key", "g-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": "Let me check"}, {"functionCall": {"name": "get_weather", "args": {"city": "Oslo"}}}]}, "finishReason": "STOP", "index": 0}],
            "usageMetadata": {"promptTokenCount": 11, "candidatesTokenCount": 6, "totalTokenCount": 17},
            "modelVersion": "gemini-2.5-flash"
        })))
        .expect(1)
        .mount(&mocks.gemini)
        .await;
    let address = boot(&mocks).await;
    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&chat_body("gemini", false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let cost: f64 = response.headers()["x-litellm-response-cost"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(cost > 0.0, "gemini pricing resolves from the catalog");
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["model"], "gemini");
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(body["choices"][0]["message"]["content"], "Let me check");
    assert_eq!(
        body["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "get_weather"
    );
    assert_eq!(body["usage"]["prompt_tokens"], 11);
    let sent: Value =
        serde_json::from_slice(&mocks.gemini.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["contents"][0]["role"], "user");
    assert_eq!(sent["contents"][0]["parts"][0]["text"], "weather in Oslo?");
    assert_eq!(
        sent["tools"][0]["functionDeclarations"][0]["name"],
        "get_weather"
    );
}

#[tokio::test]
async fn gemini_streams_decode_to_openai_chunks() {
    let mocks = mocks().await;
    let stream = concat!(
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hel\"}],\"role\":\"model\"},\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":1,\"totalTokenCount\":6},\"modelVersion\":\"gemini-2.5-flash\"}\n\n",
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"lo\"}],\"role\":\"model\"},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":2,\"totalTokenCount\":7},\"modelVersion\":\"gemini-2.5-flash\"}\n\n"
    );
    Mock::given(method("POST"))
        .and(path(
            "/v1beta/models/gemini-2.5-flash:streamGenerateContent",
        ))
        .and(query_param("alt", "sse"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .mount(&mocks.gemini)
        .await;
    let address = boot(&mocks).await;
    let text = reqwest::Client::new()
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "gemini", "stream": true, "stream_options": {"include_usage": true}, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
    let chunks = sse_data(&text);
    let content: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(content, "Hello");
    assert!(
        chunks
            .iter()
            .any(|c| c["choices"][0]["finish_reason"] == "stop")
    );
    let usage = chunks
        .iter()
        .find_map(|c| c.get("usage").filter(|u| !u.is_null()))
        .unwrap();
    assert_eq!(
        usage["completion_tokens"], 2,
        "cumulative usage keeps the last value"
    );
}

#[tokio::test]
async fn vertex_gemini_uses_the_project_path_and_bearer_token() {
    let mocks = mocks().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/locations/us-central1/publishers/google/models/gemini-2.5-pro:generateContent"))
        .and(header("authorization", "Bearer ya29.token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": "hi from vertex"}]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 4, "totalTokenCount": 7}
        })))
        .expect(1)
        .mount(&mocks.vertex)
        .await;
    let address = boot(&mocks).await;
    let body: Value = reqwest::Client::new()
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "vgemini", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "hi from vertex");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
}

#[tokio::test]
async fn vertex_claude_relays_messages_with_the_vertex_version() {
    let mocks = mocks().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/locations/us-east5/publishers/anthropic/models/claude-sonnet-4-5@20250929:rawPredict"))
        .and(header("authorization", "Bearer ya29.token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_v", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5",
            "content": [{"type": "text", "text": "hello"}], "stop_reason": "end_turn",
            "usage": {"input_tokens": 4, "output_tokens": 2}
        })))
        .expect(1)
        .mount(&mocks.vertex)
        .await;
    let address = boot(&mocks).await;
    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/messages"))
        .header("x-api-key", MASTER)
        .json(&json!({"model": "vclaude", "max_tokens": 32, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "hello");
    let sent: Value =
        serde_json::from_slice(&mocks.vertex.received_requests().await.unwrap()[0].body).unwrap();
    assert!(sent.get("model").is_none());
    assert_eq!(sent["anthropic_version"], "vertex-2023-10-16");
    assert_eq!(sent["max_tokens"], 32);
}

#[tokio::test]
async fn bedrock_converse_is_signed_and_translated() {
    let mocks = mocks().await;
    Mock::given(method("POST"))
        .and(path_regex(r"/model/.*/converse$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "output": {"message": {"role": "assistant", "content": [{"text": "Let me check"}, {"toolUse": {"toolUseId": "tooluse_1", "name": "get_weather", "input": {"city": "Oslo"}}}]}},
            "stopReason": "tool_use",
            "usage": {"inputTokens": 20, "outputTokens": 7, "totalTokens": 27}
        })))
        .expect(1)
        .mount(&mocks.bedrock)
        .await;
    let address = boot(&mocks).await;
    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&chat_body("bclaude", false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-litellm-model-id"], "dep-bclaude");
    let cost: f64 = response.headers()["x-litellm-response-cost"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(cost > 0.0, "bedrock pricing resolves from the catalog");
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        body["choices"][0]["message"]["tool_calls"][0]["id"],
        "tooluse_1"
    );
    assert_eq!(
        body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
        "{\"city\":\"Oslo\"}"
    );
    assert_eq!(body["usage"]["prompt_tokens"], 20);
    let request = &mocks.bedrock.received_requests().await.unwrap()[0];
    assert_eq!(
        request.url.path(),
        "/model/anthropic.claude-sonnet-4-5-20250929-v1%3A0/converse"
    );
    let authorization = request.headers["authorization"].to_str().unwrap();
    assert!(
        authorization.starts_with("AWS4-HMAC-SHA256 Credential=AKIATEST/"),
        "{authorization}"
    );
    assert!(
        authorization.contains("/us-east-1/bedrock/aws4_request"),
        "{authorization}"
    );
    assert!(request.headers.contains_key("x-amz-date"));
    let sent: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(
        sent["messages"][0]["content"][0]["text"],
        "weather in Oslo?"
    );
    assert_eq!(
        sent["toolConfig"]["tools"][0]["toolSpec"]["name"],
        "get_weather"
    );
}

#[tokio::test]
async fn bedrock_converse_stream_is_decoded_from_event_stream() {
    let mocks = mocks().await;
    let body = event_stream(&[
        ("messageStart", json!({"role": "assistant"})),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex": 0, "delta": {"text": "Hel"}}),
        ),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex": 0, "delta": {"text": "lo"}}),
        ),
        ("contentBlockStop", json!({"contentBlockIndex": 0})),
        ("messageStop", json!({"stopReason": "end_turn"})),
        (
            "metadata",
            json!({"usage": {"inputTokens": 8, "outputTokens": 2, "totalTokens": 10}, "metrics": {"latencyMs": 3}}),
        ),
    ]);
    Mock::given(method("POST"))
        .and(path_regex(r"/model/.*/converse-stream$"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body, "application/vnd.amazon.eventstream"),
        )
        .mount(&mocks.bedrock)
        .await;
    let address = boot(&mocks).await;
    let text = reqwest::Client::new()
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "bclaude", "stream": true, "stream_options": {"include_usage": true}, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
    let chunks = sse_data(&text);
    let content: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(content, "Hello");
    assert_eq!(chunks[0]["model"], "bclaude");
    assert!(
        chunks
            .iter()
            .any(|c| c["choices"][0]["finish_reason"] == "stop")
    );
    let usage = chunks
        .iter()
        .find_map(|c| c.get("usage").filter(|u| !u.is_null()))
        .unwrap();
    assert_eq!(usage["prompt_tokens"], 8);
    assert_eq!(usage["completion_tokens"], 2);
}

#[tokio::test]
async fn bedrock_claude_messages_stream_is_reframed_as_anthropic_sse() {
    let mocks = mocks().await;
    let events = [
        r#"{"type":"message_start","message":{"id":"msg_b","type":"message","role":"assistant","model":"claude","content":[],"stop_reason":null,"usage":{"input_tokens":6,"output_tokens":1}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":3}}"#,
        r#"{"type":"message_stop"}"#,
    ];
    let chunks: Vec<(&str, Value)> = events
        .iter()
        .map(|inner| {
            (
                "chunk",
                json!({"bytes": base64::engine::general_purpose::STANDARD.encode(inner)}),
            )
        })
        .collect();
    Mock::given(method("POST"))
        .and(path_regex(r"/model/.*/invoke-with-response-stream$"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(event_stream(&chunks), "application/vnd.amazon.eventstream"),
        )
        .mount(&mocks.bedrock)
        .await;
    let address = boot(&mocks).await;
    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/messages"))
        .header("x-api-key", MASTER)
        .json(&json!({"model": "bclaude", "max_tokens": 16, "stream": true, "messages": [{"role": "user", "content": "hi"}]}))
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
    let expected: String = events
        .iter()
        .map(|inner| {
            let kind = serde_json::from_str::<Value>(inner).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_owned();
            format!("event: {kind}\ndata: {inner}\n\n")
        })
        .collect();
    assert_eq!(text, expected);
    let sent: Value =
        serde_json::from_slice(&mocks.bedrock.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["anthropic_version"], "bedrock-2023-05-31");
    assert!(sent.get("model").is_none());
    assert!(sent.get("stream").is_none());
}

#[tokio::test]
async fn bedrock_errors_keep_their_exception_type() {
    let mocks = mocks().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header(
                    "x-amzn-errortype",
                    "ThrottlingException:http://internal.amazon.com/coral/com.amazon.bedrock/",
                )
                .set_body_json(
                    json!({"message": "Too many requests, please wait before trying again."}),
                ),
        )
        .mount(&mocks.bedrock)
        .await;
    let address = boot(&mocks).await;
    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "bclaude", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 429);
    let body: Value = response.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("ThrottlingException"),
        "{body}"
    );
    assert!(
        mocks.bedrock.received_requests().await.unwrap().len() >= 2,
        "retries stay on the only deployment"
    );
}
