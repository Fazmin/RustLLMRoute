//! A fast OpenAI-compatible upstream for load tests: canned chat completions (streaming and not),
//! embeddings and Responses, with an optional fixed latency.

use std::convert::Infallible;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::response::Response;
use axum::routing::post;
use clap::Parser;
use http::{HeaderValue, header};
use serde_json::{Value, json};

#[derive(Clone, Debug, Parser)]
#[command(
    name = "fake-upstream",
    about = "Canned OpenAI-compatible upstream for load tests"
)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:18080")]
    bind: String,
    /// Artificial latency before answering, in milliseconds.
    #[arg(long, default_value_t = 0)]
    latency_ms: u64,
    /// Number of text chunks in a streaming reply.
    #[arg(long, default_value_t = 20)]
    chunks: usize,
}

#[derive(Clone)]
struct Shared {
    latency: Duration,
    chunks: usize,
    completion: Bytes,
    embedding: Bytes,
}

fn completion_body() -> Bytes {
    Bytes::from(
        json!({
            "id": "chatcmpl-fake", "object": "chat.completion", "created": 1, "model": "gpt-4o-mini",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "The quick brown fox jumps over the lazy dog. ".repeat(20)}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 120, "completion_tokens": 200, "total_tokens": 320}
        })
        .to_string(),
    )
}

fn embedding_body() -> Bytes {
    let vector: Vec<f32> = (0..256).map(|i| (i as f32) / 256.0).collect();
    Bytes::from(
        json!({
            "object": "list", "model": "text-embedding-3-small",
            "data": [{"object": "embedding", "index": 0, "embedding": vector}],
            "usage": {"prompt_tokens": 8, "total_tokens": 8}
        })
        .to_string(),
    )
}

fn json_response(body: Bytes) -> Response {
    let mut response = Response::new(Body::from(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

async fn chat(State(shared): State<Shared>, body: Bytes) -> Response {
    if !shared.latency.is_zero() {
        tokio::time::sleep(shared.latency).await;
    }
    let stream = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v.get("stream").and_then(Value::as_bool))
        .unwrap_or(false);
    if !stream {
        return json_response(shared.completion.clone());
    }
    let chunks = shared.chunks.max(1);
    let frames = futures_util::stream::iter((0..=chunks).map(move |i| {
        let frame = if i == chunks {
            "data: {\"id\":\"chatcmpl-fake\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":120,\"completion_tokens\":200,\"total_tokens\":320}}\n\ndata: [DONE]\n\n".to_owned()
        } else if i == 0 {
            "data: {\"id\":\"chatcmpl-fake\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"The quick \"},\"finish_reason\":null}]}\n\n".to_owned()
        } else {
            "data: {\"id\":\"chatcmpl-fake\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"brown fox \"},\"finish_reason\":null}]}\n\n".to_owned()
        };
        Ok::<Bytes, Infallible>(Bytes::from(frame))
    }));
    let mut response = Response::new(Body::from_stream(frames));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response
}

async fn embeddings(State(shared): State<Shared>) -> Response {
    if !shared.latency.is_zero() {
        tokio::time::sleep(shared.latency).await;
    }
    json_response(shared.embedding.clone())
}

async fn responses(State(shared): State<Shared>, body: Bytes) -> Response {
    if !shared.latency.is_zero() {
        tokio::time::sleep(shared.latency).await;
    }
    let model = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v.get("model").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| "gpt-4o-mini".to_owned());
    json_response(Bytes::from(
        json!({
            "id": "resp_fake", "object": "response", "created_at": 1, "status": "completed", "model": model,
            "output": [{"type": "message", "id": "msg_fake", "status": "completed", "role": "assistant", "content": [{"type": "output_text", "text": "hello", "annotations": []}]}],
            "usage": {"input_tokens": 10, "output_tokens": 2, "total_tokens": 12}
        })
        .to_string(),
    ))
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let shared = Shared {
        latency: Duration::from_millis(args.latency_ms),
        chunks: args.chunks,
        completion: completion_body(),
        embedding: embedding_body(),
    };
    let app = Router::new()
        .route("/chat/completions", post(chat))
        .route("/v1/chat/completions", post(chat))
        .route("/embeddings", post(embeddings))
        .route("/v1/embeddings", post(embeddings))
        .route("/responses", post(responses))
        .route("/v1/responses", post(responses))
        .with_state(shared);
    let listener = tokio::net::TcpListener::bind(&args.bind)
        .await
        .unwrap_or_else(|error| panic!("cannot bind {}: {error}", args.bind));
    eprintln!(
        "fake-upstream listening on http://{} (latency {} ms)",
        args.bind, args.latency_ms
    );
    axum::serve(listener, app).await.expect("server");
}
