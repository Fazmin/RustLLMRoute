use std::sync::LazyLock;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, StatusCode, header::RETRY_AFTER};
use regex::Regex;
use rustllmroute_types::{ErrorKind, GatewayError};
use serde_json::Value;

const MAX_MESSAGE_BYTES: usize = 1024;

static CONTEXT_WINDOW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)context_length_exceeded|context length|maximum context|prompt is too long|input is too long|too many tokens|token limit|max_tokens.*exceed|input token count exceeds")
        .expect("valid regex")
});
static CONTENT_POLICY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)content_policy_violation|content_filter|content management policy|blocked by safety|SAFETY|PROHIBITED_CONTENT|content policy")
        .expect("valid regex")
});

/// Maps an upstream status (plus the body text for the two 400 special cases) to an [`ErrorKind`].
pub fn classify(status: u16, message: &str) -> ErrorKind {
    match status {
        400 | 422 if CONTEXT_WINDOW.is_match(message) => ErrorKind::ContextWindow,
        400 | 422 if CONTENT_POLICY.is_match(message) => ErrorKind::ContentPolicy,
        401 | 403 => ErrorKind::Auth,
        404 => ErrorKind::NotFound,
        408 => ErrorKind::Timeout,
        429 => ErrorKind::RateLimit,
        500..=599 => ErrorKind::Upstream5xx,
        400..=499 => ErrorKind::BadRequest,
        _ => ErrorKind::Other,
    }
}

pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let raw = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(seconds) = raw.parse::<f64>() {
        return (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs_f64(seconds));
    }
    let date = httpdate_parse(raw)?;
    date.duration_since(std::time::SystemTime::now()).ok()
}

fn httpdate_parse(value: &str) -> Option<std::time::SystemTime> {
    // RFC 7231 IMF-fixdate only ("Sun, 06 Nov 1994 08:49:37 GMT"); other formats are rare in Retry-After.
    let parts: Vec<&str> = value.split_whitespace().collect();
    if parts.len() != 6 {
        return None;
    }
    let day: u64 = parts[1].parse().ok()?;
    let month = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: u64 = parts[3].parse().ok()?;
    let mut clock = parts[4].split(':');
    let hour: u64 = clock.next()?.parse().ok()?;
    let minute: u64 = clock.next()?.parse().ok()?;
    let second: u64 = clock.next()?.parse().ok()?;
    let days = days_from_civil(year as i64, month, day as i64);
    let secs = days * 86_400 + (hour * 3600 + minute * 60 + second) as i64;
    (secs >= 0).then(|| std::time::UNIX_EPOCH + Duration::from_secs(secs as u64))
}

fn days_from_civil(y: i64, m: u32, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Extracts a human message from the common provider error envelopes:
/// OpenAI `{error:{message}}`, Anthropic `{error:{type,message}}`, Gemini `{error:{message,status}}`,
/// or falls back to the (truncated) body text.
pub fn upstream_message(body: &[u8]) -> (String, Option<String>) {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        let error = value.get("error").unwrap_or(&value);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| error.as_str())
            .or_else(|| value.get("detail").and_then(Value::as_str))
            .map(str::to_owned);
        let code = error
            .get("code")
            .and_then(|code| {
                code.as_str()
                    .map(str::to_owned)
                    .or_else(|| code.as_i64().map(|n| n.to_string()))
            })
            .or_else(|| error.get("type").and_then(Value::as_str).map(str::to_owned))
            .or_else(|| {
                error
                    .get("status")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        if let Some(message) = message {
            return (truncate(&message), code);
        }
    }
    (truncate(&String::from_utf8_lossy(body)), None)
}

fn truncate(text: &str) -> String {
    if text.len() <= MAX_MESSAGE_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_MESSAGE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

pub fn upstream_error(
    provider: &'static str,
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
) -> GatewayError {
    let (message, code) = upstream_message(body);
    let kind = classify(status.as_u16(), &message);
    GatewayError::Upstream {
        provider,
        status: status.as_u16(),
        kind,
        code,
        message,
        retry_after: retry_after(headers),
        body: (!body.is_empty() && body.len() <= 64 * 1024).then(|| Bytes::copy_from_slice(body)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::ctx(
        400,
        "This model's maximum context length is 8192 tokens",
        ErrorKind::ContextWindow
    )]
    #[case::policy(
        400,
        "Your request was rejected as a result of our safety system: content_policy_violation",
        ErrorKind::ContentPolicy
    )]
    #[case::plain(400, "missing field", ErrorKind::BadRequest)]
    #[case::auth(401, "bad key", ErrorKind::Auth)]
    #[case::rate(429, "slow down", ErrorKind::RateLimit)]
    #[case::server(502, "bad gateway", ErrorKind::Upstream5xx)]
    fn classifies_status_and_body(
        #[case] status: u16,
        #[case] message: &str,
        #[case] expected: ErrorKind,
    ) {
        assert_eq!(classify(status, message), expected);
    }

    #[rstest]
    fn parses_openai_and_anthropic_envelopes() {
        let (message, code) = upstream_message(br#"{"error":{"message":"nope","type":"invalid_request_error","code":"model_not_found"}}"#);
        assert_eq!(message, "nope");
        assert_eq!(code.as_deref(), Some("model_not_found"));
        let (message, code) = upstream_message(
            br#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        );
        assert_eq!(message, "Overloaded");
        assert_eq!(code.as_deref(), Some("overloaded_error"));
        let (message, _) = upstream_message(b"<html>502</html>");
        assert_eq!(message, "<html>502</html>");
    }

    #[rstest]
    fn retry_after_accepts_seconds_and_http_dates() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, "2.5".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(Duration::from_millis(2500)));
        headers.insert(
            RETRY_AFTER,
            "Sun, 06 Nov 1994 08:49:37 GMT".parse().unwrap(),
        );
        assert_eq!(
            retry_after(&headers),
            None,
            "dates in the past yield no wait"
        );
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    }
}
