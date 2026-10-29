use std::time::Duration;

use bytes::Bytes;
use http::StatusCode;
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, strum::IntoStaticStr, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ErrorKind {
    ContextWindow,
    ContentPolicy,
    RateLimit,
    Auth,
    NotFound,
    BadRequest,
    Timeout,
    Upstream5xx,
    Connection,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateLimitScope {
    /// The virtual key, team or user of the caller hit a configured limit.
    Key,
    /// The provider rate-limited the gateway; another deployment may still work.
    Upstream,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeoutStage {
    Connect,
    Headers,
    FirstByte,
    Idle,
    Total,
}

/// The single error type that crosses crate boundaries and is rendered to clients.
#[derive(Clone, Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("{message}")]
    BadRequest {
        message: String,
        param: Option<String>,
        code: Option<&'static str>,
    },
    #[error("{0}")]
    Unauthorized(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    PayloadTooLarge(String),
    #[error("{0}")]
    BudgetExceeded(String),
    #[error("{0}")]
    ContextWindowExceeded(String),
    #[error("{0}")]
    ContentFilter(String),
    #[error("{message}")]
    RateLimited {
        message: String,
        retry_after: Option<Duration>,
        scope: RateLimitScope,
    },
    #[error("{provider} returned {status}: {message}")]
    Upstream {
        provider: &'static str,
        status: u16,
        kind: ErrorKind,
        code: Option<String>,
        message: String,
        retry_after: Option<Duration>,
        body: Option<Bytes>,
    },
    #[error("upstream timed out ({stage:?})")]
    Timeout { stage: TimeoutStage },
    #[error("{0}")]
    Transport(String),
    #[error("{0}")]
    InvalidUpstreamResponse(String),
    #[error("{message}")]
    NoDeployments {
        message: String,
        retry_after: Option<Duration>,
    },
    #[error("request blocked by guardrail {guardrail}: {reason}")]
    GuardrailBlocked { guardrail: String, reason: String },
    #[error("{0}")]
    GuardrailError(String),
    #[error("{0}")]
    StreamAborted(String),
    #[error("{0}")]
    Internal(String),
}

impl GatewayError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::BadRequest {
            message: message.into(),
            param: None,
            code: None,
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest { .. }
            | Self::BudgetExceeded(_)
            | Self::ContextWindowExceeded(_)
            | Self::ContentFilter(_)
            | Self::GuardrailBlocked { .. } => StatusCode::BAD_REQUEST,
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::Upstream { status, .. } => match *status {
                529 => StatusCode::SERVICE_UNAVAILABLE,
                400..=599 => StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY),
                _ => StatusCode::BAD_GATEWAY,
            },
            Self::Timeout { .. } => StatusCode::GATEWAY_TIMEOUT,
            Self::Transport(_) | Self::InvalidUpstreamResponse(_) | Self::StreamAborted(_) => {
                StatusCode::BAD_GATEWAY
            }
            Self::NoDeployments { retry_after, .. } => {
                if retry_after.is_some() {
                    StatusCode::TOO_MANY_REQUESTS
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                }
            }
            Self::GuardrailError(_) | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::BadRequest { .. }
            | Self::PayloadTooLarge(_)
            | Self::BudgetExceeded(_)
            | Self::GuardrailBlocked { .. } => ErrorKind::BadRequest,
            Self::Unauthorized(_) | Self::Forbidden(_) => ErrorKind::Auth,
            Self::NotFound(_) => ErrorKind::NotFound,
            Self::ContextWindowExceeded(_) => ErrorKind::ContextWindow,
            Self::ContentFilter(_) => ErrorKind::ContentPolicy,
            Self::RateLimited { .. } => ErrorKind::RateLimit,
            Self::Upstream { kind, .. } => *kind,
            Self::Timeout { .. } => ErrorKind::Timeout,
            Self::Transport(_) | Self::StreamAborted(_) => ErrorKind::Connection,
            Self::InvalidUpstreamResponse(_)
            | Self::NoDeployments { .. }
            | Self::GuardrailError(_)
            | Self::Internal(_) => ErrorKind::Other,
        }
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after, .. }
            | Self::Upstream { retry_after, .. }
            | Self::NoDeployments { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    pub fn upstream_status(&self) -> Option<u16> {
        match self {
            Self::Upstream { status, .. } => Some(*status),
            Self::Timeout { .. } => Some(408),
            _ => None,
        }
    }

    /// Whether the router may re-run the attempt on another (or the same) deployment.
    pub fn retryable(&self) -> bool {
        match self {
            Self::Timeout { .. } | Self::Transport(_) | Self::InvalidUpstreamResponse(_) => true,
            Self::RateLimited { scope, .. } => *scope == RateLimitScope::Upstream,
            Self::Upstream { status, .. } => {
                matches!(*status, 408 | 409 | 425 | 429 | 500 | 502 | 503 | 504 | 529)
            }
            _ => false,
        }
    }

    /// Status-only retryability, used for the skip set and the "non-retryable => cooldown" rule.
    pub fn retryable_status(&self) -> bool {
        match self.upstream_status() {
            Some(status) => matches!(status, 408 | 409 | 425 | 429 | 500..=599),
            None => matches!(self, Self::Transport(_) | Self::Timeout { .. }),
        }
    }

    pub fn cooldown_eligible(&self) -> bool {
        match self {
            Self::Timeout { .. } | Self::Transport(_) => true,
            Self::RateLimited { scope, .. } => *scope == RateLimitScope::Upstream,
            Self::Upstream { status, .. } => {
                matches!(*status, 401 | 402 | 404 | 408 | 429 | 500..=599)
            }
            _ => false,
        }
    }

    /// A plain client mistake: never retried, never falls back.
    pub fn is_client_bug(&self) -> bool {
        matches!(self, Self::BadRequest { .. } | Self::PayloadTooLarge(_))
            || matches!(
                self,
                Self::Upstream {
                    kind: ErrorKind::BadRequest,
                    ..
                }
            )
    }

    pub fn openai_type(&self) -> &'static str {
        match self {
            Self::BadRequest { .. }
            | Self::PayloadTooLarge(_)
            | Self::BudgetExceeded(_)
            | Self::ContextWindowExceeded(_)
            | Self::ContentFilter(_)
            | Self::GuardrailBlocked { .. } => "invalid_request_error",
            Self::Unauthorized(_) => "authentication_error",
            Self::Forbidden(_) => "permission_error",
            Self::NotFound(_) => "not_found_error",
            Self::RateLimited { .. } => "rate_limit_error",
            Self::NoDeployments { retry_after, .. } => {
                if retry_after.is_some() {
                    "rate_limit_error"
                } else {
                    "service_unavailable_error"
                }
            }
            Self::Upstream { status, .. } => match *status {
                401 => "authentication_error",
                403 => "permission_error",
                404 => "not_found_error",
                429 => "rate_limit_error",
                400..=499 => "invalid_request_error",
                _ => "api_error",
            },
            Self::Timeout { .. } => "timeout_error",
            Self::Transport(_) => "api_connection_error",
            Self::InvalidUpstreamResponse(_)
            | Self::StreamAborted(_)
            | Self::GuardrailError(_)
            | Self::Internal(_) => "api_error",
        }
    }

    pub fn code(&self) -> Option<String> {
        match self {
            Self::BadRequest { code, .. } => code.map(str::to_owned),
            Self::ContextWindowExceeded(_) => Some("context_length_exceeded".to_owned()),
            Self::ContentFilter(_) => Some("content_policy_violation".to_owned()),
            Self::BudgetExceeded(_) => Some("budget_exceeded".to_owned()),
            Self::NotFound(_) => Some("model_not_found".to_owned()),
            Self::GuardrailBlocked { .. } => Some("guardrail_violation".to_owned()),
            Self::RateLimited { .. } => Some("rate_limit_exceeded".to_owned()),
            Self::Upstream { code, .. } => code.clone(),
            _ => None,
        }
    }

    pub fn param(&self) -> Option<&str> {
        match self {
            Self::BadRequest { param, .. } => param.as_deref(),
            Self::GuardrailBlocked { guardrail, .. } => Some(guardrail.as_str()),
            _ => None,
        }
    }

    pub fn anthropic_type(&self) -> &'static str {
        match self.status().as_u16() {
            400 => "invalid_request_error",
            401 => "authentication_error",
            403 => "permission_error",
            404 => "not_found_error",
            413 => "request_too_large",
            429 => "rate_limit_error",
            503 | 529 => "overloaded_error",
            _ => "api_error",
        }
    }

    pub fn to_openai_body(&self) -> Value {
        json!({
            "error": {
                "message": self.to_string(),
                "type": self.openai_type(),
                "param": self.param(),
                "code": self.code(),
            }
        })
    }

    pub fn to_anthropic_body(&self) -> Value {
        json!({
            "type": "error",
            "error": {
                "type": self.anthropic_type(),
                "message": self.to_string(),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn upstream(status: u16, kind: ErrorKind) -> GatewayError {
        GatewayError::Upstream {
            provider: "test",
            status,
            kind,
            code: None,
            message: "boom".to_owned(),
            retry_after: None,
            body: None,
        }
    }

    #[rstest]
    #[case::rate_limit(upstream(429, ErrorKind::RateLimit), true, true)]
    #[case::server(upstream(503, ErrorKind::Upstream5xx), true, true)]
    #[case::auth(upstream(401, ErrorKind::Auth), false, true)]
    #[case::bad_request(upstream(400, ErrorKind::BadRequest), false, false)]
    #[case::not_found(upstream(404, ErrorKind::NotFound), false, true)]
    #[case::transport(GatewayError::Transport("reset".into()), true, true)]
    #[case::key_limit(GatewayError::RateLimited { message: "x".into(), retry_after: None, scope: RateLimitScope::Key }, false, false)]
    fn retry_and_cooldown_classification(
        #[case] error: GatewayError,
        #[case] retryable: bool,
        #[case] cooldown: bool,
    ) {
        assert_eq!(error.retryable(), retryable);
        assert_eq!(error.cooldown_eligible(), cooldown);
    }

    #[rstest]
    #[case::overloaded(upstream(529, ErrorKind::Upstream5xx), 503)]
    #[case::timeout(GatewayError::Timeout { stage: TimeoutStage::Headers }, 504)]
    #[case::all_cooled(GatewayError::NoDeployments { message: "x".into(), retry_after: Some(Duration::from_secs(5)) }, 429)]
    #[case::unknown_model(GatewayError::NotFound("m".into()), 404)]
    fn client_status_mapping(#[case] error: GatewayError, #[case] status: u16) {
        assert_eq!(error.status().as_u16(), status);
    }

    #[rstest]
    fn bodies_follow_openai_and_anthropic_shapes() {
        let error = GatewayError::ContextWindowExceeded("too long".into());
        let openai = error.to_openai_body();
        assert_eq!(openai["error"]["type"], "invalid_request_error");
        assert_eq!(openai["error"]["code"], "context_length_exceeded");
        let anthropic = error.to_anthropic_body();
        assert_eq!(anthropic["type"], "error");
        assert_eq!(anthropic["error"]["type"], "invalid_request_error");
    }
}
