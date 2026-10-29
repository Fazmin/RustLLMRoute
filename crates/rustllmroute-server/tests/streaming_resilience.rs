use std::net::SocketAddr;
use std::path::Path;

use clap::Parser;
use rustllmroute_config::{Cli, LoadedConfig};
use rustllmroute_server::{app, state};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const MASTER: &str = "sk-master-test";

const GOOD_STREAM: &str = concat!(
    "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n"
);

async fn boot(primary: &str, secondary: &str) -> SocketAddr {
    let yaml = format!(
        r#"
model_list:
  - model_name: gpt
    litellm_params: {{ model: openai/gpt-4o-mini, api_key: k, api_base: {primary}, order: 0 }}
    model_info: {{ id: dep-a }}
  - model_name: gpt
    litellm_params: {{ model: openai/gpt-4o-mini, api_key: k, api_base: {secondary}, order: 1 }}
    model_info: {{ id: dep-b }}
general_settings:
  master_key: {MASTER}
"#
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

async fn stream_chat(address: SocketAddr) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(MASTER)
        .json(&json!({"model": "gpt", "messages": [{"role": "user", "content": "hi"}], "stream": true}))
        .send()
        .await
        .unwrap()
}

fn frames(text: &str) -> Vec<String> {
    text.split("\n\n")
        .filter_map(|frame| frame.strip_prefix("data: "))
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn an_error_in_the_first_frame_is_retried_on_the_next_deployment() {
    let (primary, secondary) = (MockServer::start().await, MockServer::start().await);
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"error\":{\"message\":\"overloaded\",\"code\":503}}\n\n",
            "text/event-stream",
        ))
        .mount(&primary)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(GOOD_STREAM, "text/event-stream"))
        .mount(&secondary)
        .await;
    let address = boot(&primary.uri(), &secondary.uri()).await;
    let response = stream_chat(address).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-litellm-model-id"], "dep-b");
    assert_eq!(
        response.headers()["x-litellm-attempted-retries"],
        "2",
        "dep-a is retried within its order tier first"
    );
    assert_eq!(
        response.headers()["x-litellm-attempted-fallbacks"],
        "1",
        "then the next tier is a fallback"
    );
    let text = response.text().await.unwrap();
    let frames = frames(&text);
    assert_eq!(frames.last().map(String::as_str), Some("[DONE]"));
    let first: Value = serde_json::from_str(&frames[0]).unwrap();
    assert_eq!(first["choices"][0]["delta"]["content"], "ok");
}

/// A raw TCP upstream that sends two SSE frames inside a chunked body and then drops the socket.
async fn faulty_upstream() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 65536];
                let mut seen = Vec::new();
                loop {
                    let Ok(n) = socket.read(&mut buffer).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    seen.extend_from_slice(&buffer[..n]);
                    if let Some(end) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&seen[..end]).to_string();
                        let length = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if seen.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
                let _ = socket.write_all(head.as_bytes()).await;
                for content in ["Hel", "lo"] {
                    let frame = format!(
                        "data: {{\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{content}\"}}}}]}}\n\n"
                    );
                    let chunk = format!("{:x}\r\n{frame}\r\n", frame.len());
                    let _ = socket.write_all(chunk.as_bytes()).await;
                    let _ = socket.flush().await;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                drop(socket);
            });
        }
    });
    format!("http://{address}")
}

#[tokio::test]
async fn a_connection_cut_mid_stream_ends_with_an_in_band_error_and_no_retry() {
    let faulty = faulty_upstream().await;
    let secondary = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(GOOD_STREAM, "text/event-stream"))
        .mount(&secondary)
        .await;
    let address = boot(&faulty, &secondary.uri()).await;
    let response = stream_chat(address).await;
    assert_eq!(
        response.status(),
        200,
        "headers were committed before the cut"
    );
    assert_eq!(response.headers()["x-litellm-model-id"], "dep-a");
    let text = response.text().await.unwrap();
    let frames = frames(&text);
    let contents: Vec<&str> = frames
        .iter()
        .filter_map(|f| serde_json::from_str::<Value>(f).ok())
        .filter_map(|v| {
            v["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_owned)
        })
        .collect::<Vec<_>>()
        .leak()
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(contents, vec!["Hel", "lo"]);
    let error_frame = frames
        .iter()
        .find(|f| f.contains("\"error\""))
        .expect("in-band error frame");
    let error: Value = serde_json::from_str(error_frame).unwrap();
    assert_eq!(error["error"]["type"], "api_connection_error");
    assert_eq!(frames.last().map(String::as_str), Some("[DONE]"));
    assert!(
        secondary.received_requests().await.unwrap().is_empty(),
        "nothing is retried after the first byte"
    );
}
