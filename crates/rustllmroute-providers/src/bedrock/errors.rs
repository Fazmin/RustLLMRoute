use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use rustllmroute_types::{ErrorKind, GatewayError};
use serde_json::Value;

use super::PROVIDER;
use crate::errors::{classify, retry_after};

/// HTTP status Bedrock serves a given exception type with (also used for event-stream exceptions).
pub fn exception_status(name: &str) -> u16 {
    let name = name.rsplit('#').next().unwrap_or(name);
    let name = name.split(':').next().unwrap_or(name);
    match name.to_ascii_lowercase().as_str() {
        "throttlingexception" | "toomanyrequestsexception" => 429,
        "validationexception" => 400,
        "accessdeniedexception" => 403,
        "resourcenotfoundexception" => 404,
        "modeltimeoutexception" => 408,
        "modelnotreadyexception" | "serviceunavailableexception" => 503,
        "servicequotaexceededexception" => 429,
        "modelstreamerrorexception" | "modelerrorexception" | "internalserverexception" => 500,
        _ => 500,
    }
}

fn kind_for(status: u16, message: &str) -> ErrorKind {
    let lower = message.to_ascii_lowercase();
    if status == 400
        && (lower.contains("blocked by")
            || lower.contains("content filter")
            || lower.contains("guardrail"))
    {
        return ErrorKind::ContentPolicy;
    }
    classify(status, message)
}

/// Builds an upstream error from an event-stream exception payload (`{"message": ...}`).
pub fn stream_exception(name: &str, payload: &[u8]) -> GatewayError {
    let status = exception_status(name);
    let message = serde_json::from_slice::<Value>(payload)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .or_else(|| value.get("Message"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| String::from_utf8_lossy(payload).into_owned());
    GatewayError::Upstream {
        provider: PROVIDER,
        status,
        kind: kind_for(status, &message),
        code: Some(name.to_owned()),
        message: format!("{name}: {message}"),
        retry_after: None,
        body: None,
    }
}

/// Non-streaming error responses: `x-amzn-errortype` names the exception, the body carries `message`.
pub fn parse_error(status: StatusCode, headers: &HeaderMap, body: &[u8]) -> GatewayError {
    let exception = headers
        .get("x-amzn-errortype")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(':').next().unwrap_or(value).to_owned());
    let message = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .or_else(|| value.get("Message"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| String::from_utf8_lossy(body).chars().take(1024).collect());
    let status = match &exception {
        Some(name)
            if status.is_success() || status.as_u16() == 400 && exception_status(name) == 429 =>
        {
            exception_status(name)
        }
        _ => status.as_u16(),
    };
    GatewayError::Upstream {
        provider: PROVIDER,
        status,
        kind: kind_for(status, &message),
        code: exception.clone(),
        message: match &exception {
            Some(name) => format!("{name}: {message}"),
            None => message,
        },
        retry_after: retry_after(headers),
        body: (!body.is_empty() && body.len() <= 64 * 1024).then(|| Bytes::copy_from_slice(body)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_exception_types_and_headers() {
        assert_eq!(exception_status("ThrottlingException"), 429);
        assert_eq!(exception_status("throttlingException"), 429);
        assert_eq!(exception_status("ValidationException:http://internal"), 400);
        assert_eq!(exception_status("com.amazon#ModelNotReadyException"), 503);
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-amzn-errortype",
            "ValidationException:http://x".parse().unwrap(),
        );
        let error = parse_error(
            StatusCode::BAD_REQUEST,
            &headers,
            br#"{"message":"Input is too long for requested model."}"#,
        );
        assert!(matches!(
            error,
            GatewayError::Upstream {
                status: 400,
                kind: ErrorKind::ContextWindow,
                ..
            }
        ));
        let error = stream_exception("throttlingException", br#"{"message":"Too many requests"}"#);
        assert!(matches!(
            error,
            GatewayError::Upstream {
                status: 429,
                kind: ErrorKind::RateLimit,
                ..
            }
        ));
        assert!(error.retryable());
    }
}
