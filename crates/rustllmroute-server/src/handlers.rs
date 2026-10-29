use std::convert::Infallible;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use bytes::Bytes;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use rustllmroute_core::{AuthContext, ChatReply, MessagesReply, ResponsesReply};
use rustllmroute_types::{
    ChatRequest, EmbeddingRequest, GatewayError, MessagesRequest, ModelList, ModelObject,
    ResponsesRequest,
};
use serde_json::{Value, json};

use crate::app::RequestId;
use crate::error::{AnthropicApiError, ApiError};
use crate::state::AppState;

fn parse_body<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, GatewayError> {
    serde_json::from_slice(body).map_err(|error| GatewayError::BadRequest {
        message: format!("invalid request body: {error}"),
        param: None,
        code: None,
    })
}

fn json_response(bytes: Bytes, headers: HeaderMap) -> Response {
    let mut reply = Response::new(Body::from(bytes));
    *reply.headers_mut() = headers;
    reply.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    reply
}

fn sse_response(
    body: futures_util::stream::BoxStream<'static, Bytes>,
    headers: HeaderMap,
) -> Response {
    let mut reply = Response::new(Body::from_stream(body.map(Ok::<Bytes, Infallible>)));
    *reply.headers_mut() = headers;
    let h = reply.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    reply
}

/// Client headers that may travel to the provider on `/v1/messages`.
fn forwarded_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for name in ["anthropic-beta"] {
        for value in headers.get_all(name) {
            out.append(name, value.clone());
        }
    }
    out
}

pub async fn chat(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let request: ChatRequest = parse_body(&body)?;
    if request.messages.is_empty() {
        return Err(ApiError(GatewayError::BadRequest {
            message: "messages must not be empty".to_owned(),
            param: Some("messages".to_owned()),
            code: None,
        }));
    }
    match state.gateway.chat(&auth, &request_id, request).await? {
        ChatReply::Json {
            response,
            raw,
            headers,
        } => {
            let bytes = match raw {
                Some(raw) => raw,
                None => Bytes::from(serde_json::to_vec(&response).map_err(|error| {
                    ApiError(GatewayError::internal(format!(
                        "serialising response: {error}"
                    )))
                })?),
            };
            Ok(json_response(bytes, headers))
        }
        ChatReply::Stream { body, headers } => Ok(sse_response(body, headers)),
    }
}

pub async fn messages(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AnthropicApiError> {
    let request: MessagesRequest = parse_body(&body)?;
    if request.messages.is_empty() {
        return Err(AnthropicApiError(GatewayError::BadRequest {
            message: "messages must not be empty".to_owned(),
            param: Some("messages".to_owned()),
            code: None,
        }));
    }
    let forwarded = forwarded_headers(&headers);
    match state
        .gateway
        .messages(&auth, &request_id, request, forwarded)
        .await?
    {
        MessagesReply::Json { body, headers } => Ok(json_response(body, headers)),
        MessagesReply::Stream { body, headers } => Ok(sse_response(body, headers)),
    }
}

pub async fn count_tokens(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AnthropicApiError> {
    let request: MessagesRequest = parse_body(&body)?;
    let forwarded = forwarded_headers(&headers);
    let bytes = state
        .gateway
        .count_tokens(&auth, &request_id, request, forwarded)
        .await?;
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        headers.insert("x-litellm-call-id", value);
    }
    Ok(json_response(bytes, headers))
}

pub async fn embeddings(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let request: EmbeddingRequest = parse_body(&body)?;
    let reply = state
        .gateway
        .embeddings(&auth, &request_id, request)
        .await?;
    Ok(json_response(reply.body, reply.headers))
}

pub async fn responses(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let request: ResponsesRequest = parse_body(&body)?;
    match state.gateway.responses(&auth, &request_id, request).await? {
        ResponsesReply::Json { body, headers } => Ok(json_response(body, headers)),
        ResponsesReply::Stream { body, headers } => Ok(sse_response(body, headers)),
    }
}

pub async fn models(State(state): State<Arc<AppState>>) -> Json<ModelList> {
    let registry = state.gateway.router.registry();
    let data = registry
        .model_names()
        .into_iter()
        .map(|name| ModelObject::new(name, state.started_epoch))
        .collect();
    Json(ModelList::new(data))
}

pub async fn model(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<ModelObject>, ApiError> {
    let registry = state.gateway.router.registry();
    if registry.model_names().contains(&id) {
        return Ok(Json(ModelObject::new(id, state.started_epoch)));
    }
    Err(ApiError(GatewayError::NotFound(format!(
        "model {id:?} is not configured"
    ))))
}

pub async fn health(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Json<Value>, ApiError> {
    if !auth.is_admin {
        return Err(ApiError(GatewayError::Forbidden(
            "/health requires an admin key".to_owned(),
        )));
    }
    let background = state
        .config
        .snapshot()
        .config
        .general_settings
        .background_health_checks;
    let report = if background {
        state.gateway.router.health_report()
    } else {
        rustllmroute_core::health::run_health_checks(
            &state.gateway,
            std::time::Duration::from_secs(30),
        )
        .await
    };
    Ok(Json(serde_json::to_value(report).unwrap_or(Value::Null)))
}

pub async fn liveness() -> &'static str {
    "I'm alive!"
}

pub async fn readiness(State(state): State<Arc<AppState>>) -> Response {
    let config = state.config.snapshot();
    let draining = state.draining.load(std::sync::atomic::Ordering::Relaxed);
    let (db, healthy) = match &state.control {
        Some(control) => match control.db().ping().await {
            Ok(()) => ("connected", true),
            Err(_) => ("disconnected", false),
        },
        None => ("disabled", true),
    };
    let healthy = healthy && !draining;
    let body = json!({
        "status": if draining { "draining" } else if healthy { "healthy" } else { "unhealthy" },
        "db": db,
        "spend_events_dropped": state.control.as_ref().map(|c| c.sink().dropped()).unwrap_or(0),
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_s": state.started.elapsed().as_secs(),
        "deployments": config.config.model_list.len(),
        "config_path": config.path.display().to_string(),
    });
    let status = if healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(body)).into_response()
}

pub async fn not_found() -> ApiError {
    ApiError(GatewayError::NotFound("no such route".to_owned()))
}

pub async fn metrics() -> Response {
    let mut reply = Response::new(Body::from(rustllmroute_observe::render()));
    reply.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    reply
}
