use axum::body::Body;
use axum::response::{IntoResponse, Response};
use http::{HeaderValue, header};
use rustllmroute_types::GatewayError;

fn render(error: &GatewayError, body: String) -> Response {
    let status = error.status();
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let GatewayError::GuardrailBlocked { guardrail, .. } = error
        && let Ok(value) = HeaderValue::from_str(guardrail)
    {
        response
            .headers_mut()
            .insert("x-rustllmroute-guardrail", value);
    }
    if let Some(retry_after) = error.retry_after() {
        let seconds = retry_after.as_secs().max(1).to_string();
        if let Ok(value) = HeaderValue::from_str(&seconds) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
    }
    if status.is_server_error() {
        tracing::error!(error = %error, "request failed");
    } else {
        tracing::debug!(error = %error, status = status.as_u16(), "request rejected");
    }
    response
}

/// Renders a [`GatewayError`] as an OpenAI-style error response.
pub struct ApiError(pub GatewayError);

impl From<GatewayError> for ApiError {
    fn from(error: GatewayError) -> Self {
        Self(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = self.0.to_openai_body().to_string();
        render(&self.0, body)
    }
}

/// Renders a [`GatewayError`] in the Anthropic error shape, for `/v1/messages` routes.
pub struct AnthropicApiError(pub GatewayError);

impl From<GatewayError> for AnthropicApiError {
    fn from(error: GatewayError) -> Self {
        Self(error)
    }
}

impl IntoResponse for AnthropicApiError {
    fn into_response(self) -> Response {
        let body = self.0.to_anthropic_body().to_string();
        render(&self.0, body)
    }
}
