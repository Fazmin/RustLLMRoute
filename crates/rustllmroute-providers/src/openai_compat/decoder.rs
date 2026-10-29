use rustllmroute_types::{ChatChunk, ErrorKind, GatewayError};
use serde_json::Value;

use crate::traits::{ChunkDecoder, Frame};

/// Decodes OpenAI-style `data: {chunk}` events; `[DONE]` ends the stream and an in-band
/// `{"error": ...}` becomes an upstream error.
pub struct OpenAiSseDecoder {
    provider: &'static str,
    done: bool,
}

impl OpenAiSseDecoder {
    pub fn new(provider: &'static str) -> Self {
        Self {
            provider,
            done: false,
        }
    }
}

impl ChunkDecoder for OpenAiSseDecoder {
    fn decode(&mut self, frame: Frame<'_>, out: &mut Vec<ChatChunk>) -> Result<(), GatewayError> {
        if self.done {
            return Ok(());
        }
        let data = frame.data.trim_ascii();
        if data.is_empty() {
            return Ok(());
        }
        if data == b"[DONE]" {
            self.done = true;
            return Ok(());
        }
        match serde_json::from_slice::<ChatChunk>(data) {
            Ok(chunk) => {
                out.push(chunk);
                Ok(())
            }
            Err(parse_error) => {
                if let Ok(value) = serde_json::from_slice::<Value>(data) {
                    if let Some(error) = value.get("error") {
                        let message = error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("upstream error inside stream")
                            .to_owned();
                        let status = error
                            .get("code")
                            .and_then(Value::as_u64)
                            .and_then(|code| u16::try_from(code).ok())
                            .filter(|code| (400..600).contains(code))
                            .unwrap_or(502);
                        return Err(GatewayError::Upstream {
                            provider: self.provider,
                            status,
                            kind: crate::errors::classify(status, &message),
                            code: error.get("type").and_then(Value::as_str).map(str::to_owned),
                            message,
                            retry_after: None,
                            body: None,
                        });
                    }
                    if value.get("choices").is_none() && value.get("usage").is_none() {
                        // Keepalive-ish JSON some hosts emit; ignore it.
                        return Ok(());
                    }
                }
                Err(GatewayError::InvalidUpstreamResponse(format!(
                    "{}: undecodable stream chunk: {parse_error}",
                    self.provider
                )))
            }
        }
    }

    fn is_done(&self) -> bool {
        self.done
    }
}

impl From<ErrorKind> for OpenAiSseDecoder {
    fn from(_: ErrorKind) -> Self {
        Self::new("openai")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn frame(data: &str) -> Frame<'_> {
        Frame {
            event: None,
            data: data.as_bytes(),
        }
    }

    #[rstest]
    fn decodes_chunks_and_stops_at_done() {
        let mut decoder = OpenAiSseDecoder::new("openai");
        let mut out = Vec::new();
        decoder
            .decode(frame(r#"{"id":"c","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"hi"}}]}"#), &mut out)
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].choices[0].delta.content.as_deref(), Some("hi"));
        decoder.decode(frame("[DONE]"), &mut out).unwrap();
        assert!(decoder.is_done());
        decoder.decode(frame(r#"{"id":"late"}"#), &mut out).unwrap();
        assert_eq!(out.len(), 1, "frames after [DONE] are ignored");
    }

    #[rstest]
    fn in_band_error_becomes_upstream_error() {
        let mut decoder = OpenAiSseDecoder::new("groq");
        let mut out = Vec::new();
        let error = decoder
            .decode(
                frame(
                    r#"{"error":{"message":"rate limited","type":"rate_limit_error","code":429}}"#,
                ),
                &mut out,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            GatewayError::Upstream {
                status: 429,
                kind: ErrorKind::RateLimit,
                ..
            }
        ));
    }
}
