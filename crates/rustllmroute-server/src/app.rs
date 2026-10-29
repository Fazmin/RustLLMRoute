use std::sync::Arc;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use http::HeaderValue;
use rustllmroute_types::metrics as m;
use secrecy::ExposeSecret;
use tracing::Instrument;

use crate::error::{AnthropicApiError, ApiError};
use crate::state::AppState;
use crate::{admin, handlers, session, ui};

#[derive(Clone, Debug)]
pub struct RequestId(pub String);

pub fn router(state: Arc<AppState>) -> Router {
    let gateway = Router::new()
        .route("/v1/chat/completions", post(handlers::chat))
        .route("/chat/completions", post(handlers::chat))
        .route("/v1/messages", post(handlers::messages))
        .route("/v1/messages/count_tokens", post(handlers::count_tokens))
        .route("/v1/embeddings", post(handlers::embeddings))
        .route("/embeddings", post(handlers::embeddings))
        .route("/v1/responses", post(handlers::responses))
        .route("/responses", post(handlers::responses))
        .route("/v1/models", get(handlers::models))
        .route("/models", get(handlers::models))
        .route("/v1/models/{id}", get(handlers::model))
        .route("/health", get(handlers::health))
        .route("/model/info", get(admin::model_info))
        .route("/v1/model/info", get(admin::model_info))
        .route("/model_group/info", get(admin::model_group_info))
        .route("/config/yaml", get(admin::config_yaml))
        .route("/config/reload", post(admin::config_reload))
        .with_state(state.clone());
    let mut protected = gateway;
    if state.metrics_auth {
        protected = protected.route("/metrics", get(handlers::metrics));
    }
    if let Some(plane) = &state.control {
        protected = protected.merge(rustllmroute_control::management::router(plane.clone()));
    }
    let protected = protected.route_layer(middleware::from_fn_with_state(
        state.clone(),
        auth_middleware,
    ));
    let public = Router::new()
        .route("/health/liveliness", get(handlers::liveness))
        .route("/health/liveness", get(handlers::liveness))
        .route("/health/readiness", get(handlers::readiness))
        .route("/login", get(ui::redirect_to_ui).post(session::login))
        .route("/logout", post(session::logout))
        .route("/ui", get(ui::redirect_to_ui))
        .route("/ui/", get(ui::index))
        .route("/ui/{*path}", get(ui::asset))
        .with_state(state.clone());
    let public = if state.metrics_auth {
        public
    } else {
        public.route("/metrics", get(handlers::metrics))
    };
    Router::new()
        .merge(protected)
        .merge(public)
        .fallback(handlers::not_found)
        .layer(middleware::from_fn(request_id_middleware))
        .layer(DefaultBodyLimit::max(state.max_body_bytes))
}

async fn request_id_middleware(mut request: Request, next: Next) -> Response {
    let id = request
        .headers()
        .get("x-litellm-call-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .map(str::to_owned)
        .unwrap_or_else(rustllmroute_core::new_request_id);
    request.extensions_mut().insert(RequestId(id.clone()));
    let span = tracing::info_span!(
        "llm.request",
        request_id = %id,
        http.method = %request.method(),
        url.path = %request.uri().path(),
        http.response.status_code = tracing::field::Empty,
        gen_ai.request.model = tracing::field::Empty,
        gen_ai.response.model = tracing::field::Empty,
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        llm.model_id = tracing::field::Empty,
        llm.key_alias = tracing::field::Empty,
        llm.cost_usd = tracing::field::Empty,
        llm.status = tracing::field::Empty,
    );
    metrics::gauge!(m::IN_FLIGHT_REQUESTS).increment(1.0);
    let mut response = next.run(request).instrument(span.clone()).await;
    metrics::gauge!(m::IN_FLIGHT_REQUESTS).decrement(1.0);
    span.record("http.response.status_code", response.status().as_u16());
    if let Ok(value) = HeaderValue::from_str(&id) {
        response
            .headers_mut()
            .entry("x-litellm-call-id")
            .or_insert(value);
    }
    response
}

async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_owned();
    if let Some(auth) =
        session::session_from_headers(request.headers(), state.master_key.expose_secret())
    {
        request.extensions_mut().insert(auth);
        return next.run(request).await;
    }
    match state
        .gateway
        .hooks
        .authenticate(request.headers(), &path)
        .await
    {
        Ok(auth) => {
            request.extensions_mut().insert(auth);
            next.run(request).await
        }
        Err(error) if path.starts_with("/v1/messages") => AnthropicApiError(error).into_response(),
        Err(error) => ApiError(error).into_response(),
    }
}
