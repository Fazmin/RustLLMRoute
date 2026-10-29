use std::collections::HashMap;

use rustllmroute_types::{
    ChatChunk, ChunkChoice, Delta, FunctionCallDelta, GatewayError, ToolCallDelta,
};
use serde_json::{Value, json};

use super::response::{
    blocked_prompt_error, map_finish, new_call_id, parse_parts, usage_from_metadata,
};
use crate::errors::{classify, upstream_message};
use crate::traits::{ChunkDecoder, Frame};

/// Decodes `streamGenerateContent?alt=sse` events: each `data:` carries a partial response whose
/// `usageMetadata` is cumulative, so the last one wins.
pub struct GeminiSseDecoder {
    provider: &'static str,
    id: String,
    created: u64,
    next_tool_index: HashMap<u32, u32>,
    saw_tools: HashMap<u32, bool>,
    started: bool,
}

impl GeminiSseDecoder {
    pub fn new(provider: &'static str, created: u64) -> Self {
        Self {
            provider,
            id: format!("chatcmpl-{}", uuid::Uuid::now_v7().simple()),
            created,
            next_tool_index: HashMap::new(),
            saw_tools: HashMap::new(),
            started: false,
        }
    }
}

impl ChunkDecoder for GeminiSseDecoder {
    fn decode(&mut self, frame: Frame<'_>, out: &mut Vec<ChatChunk>) -> Result<(), GatewayError> {
        let data = frame.data.trim_ascii();
        if data.is_empty() {
            return Ok(());
        }
        let body: Value = serde_json::from_slice(data).map_err(|error| {
            GatewayError::InvalidUpstreamResponse(format!(
                "{}: undecodable stream chunk: {error}",
                self.provider
            ))
        })?;
        if let Some(error) = body.get("error") {
            let (message, code) = upstream_message(data);
            let status = error
                .get("code")
                .and_then(Value::as_u64)
                .and_then(|code| u16::try_from(code).ok())
                .filter(|code| (400..600).contains(code))
                .unwrap_or(502);
            return Err(GatewayError::Upstream {
                provider: self.provider,
                status,
                kind: classify(status, &message),
                code,
                message,
                retry_after: None,
                body: None,
            });
        }
        let first = !self.started;
        if first && let Some(id) = body.get("responseId").and_then(Value::as_str) {
            self.id = format!("chatcmpl-{id}");
        }
        let mut chunk = ChatChunk::new(
            self.id.clone(),
            self.created,
            body.get("modelVersion")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        );
        let candidates = body
            .get("candidates")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if candidates.is_empty()
            && let Some(error) = blocked_prompt_error(&body)
        {
            return Err(error);
        }
        for (position, candidate) in candidates.iter().enumerate() {
            let index = candidate
                .get("index")
                .and_then(Value::as_u64)
                .map(|i| i as u32)
                .unwrap_or(position as u32);
            let finish = candidate.get("finishReason").and_then(Value::as_str);
            if finish == Some("MALFORMED_FUNCTION_CALL") {
                return Err(GatewayError::InvalidUpstreamResponse(
                    "gemini: the model produced a malformed function call".to_owned(),
                ));
            }
            let parts = candidate
                .pointer("/content/parts")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let parsed = parse_parts(&parts, &mut new_call_id);
            let mut delta = Delta::default();
            if first {
                delta.role = Some("assistant".to_owned());
            }
            if !parsed.text.is_empty() {
                delta.content = Some(parsed.text);
            }
            if !parsed.reasoning.is_empty() {
                delta.reasoning_content = Some(parsed.reasoning.clone());
            }
            if let Some(signature) = parsed.signature {
                delta.extra.insert(
                    "thinking_blocks".to_owned(),
                    json!([{"type": "thinking", "thinking": parsed.reasoning, "signature": signature}]),
                );
            }
            if !parsed.tool_calls.is_empty() {
                self.saw_tools.insert(index, true);
                let counter = self.next_tool_index.entry(index).or_insert(0);
                let mut calls = Vec::new();
                for call in parsed.tool_calls {
                    calls.push(ToolCallDelta {
                        index: *counter,
                        id: Some(call.id),
                        kind: Some("function".to_owned()),
                        function: FunctionCallDelta {
                            name: Some(call.function.name),
                            arguments: call.function.arguments,
                        },
                        extra: Default::default(),
                    });
                    *counter += 1;
                }
                delta.tool_calls = Some(calls);
            }
            let has_tools = self.saw_tools.get(&index).copied().unwrap_or(false);
            chunk.choices.push(ChunkChoice {
                index,
                delta,
                finish_reason: map_finish(finish, has_tools && finish.is_some()),
                logprobs: None,
                extra: Default::default(),
            });
        }
        self.started = true;
        if let Some(metadata) = body.get("usageMetadata") {
            let usage = usage_from_metadata(metadata);
            if usage.prompt_tokens > 0 || usage.completion_tokens > 0 {
                chunk.usage = Some(usage);
            }
        }
        if !chunk.choices.is_empty() || chunk.usage.is_some() {
            out.push(chunk);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use rustllmroute_types::FinishReason;

    use super::*;

    fn frame(data: &str) -> Frame<'_> {
        Frame {
            event: None,
            data: data.as_bytes(),
        }
    }

    #[test]
    fn decodes_text_then_function_call_with_cumulative_usage() {
        let mut decoder = GeminiSseDecoder::new("gemini", 1);
        let mut out = Vec::new();
        decoder
            .decode(
                frame(r#"{"candidates":[{"content":{"parts":[{"text":"Hel"}],"role":"model"},"index":0}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":1,"totalTokenCount":8},"modelVersion":"gemini-2.5-pro","responseId":"r1"}"#),
                &mut out,
            )
            .unwrap();
        decoder
            .decode(
                frame(r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"get_weather","args":{"city":"Oslo"}}}],"role":"model"},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":4,"totalTokenCount":11}}"#),
                &mut out,
            )
            .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "chatcmpl-r1");
        assert_eq!(out[0].choices[0].delta.role.as_deref(), Some("assistant"));
        assert_eq!(out[0].choices[0].delta.content.as_deref(), Some("Hel"));
        let call = &out[1].choices[0].delta.tool_calls.as_ref().unwrap()[0];
        assert_eq!(call.function.name.as_deref(), Some("get_weather"));
        assert_eq!(call.function.arguments, "{\"city\":\"Oslo\"}");
        assert_eq!(
            out[1].choices[0].finish_reason,
            Some(FinishReason::ToolCalls)
        );
        assert_eq!(out[1].usage.as_ref().unwrap().completion_tokens, 4);
    }

    #[test]
    fn in_band_errors_and_safety_blocks_fail_the_stream() {
        let mut decoder = GeminiSseDecoder::new("gemini", 1);
        let mut out = Vec::new();
        let error = decoder
            .decode(frame(r#"{"error":{"code":429,"message":"Resource exhausted","status":"RESOURCE_EXHAUSTED"}}"#), &mut out)
            .unwrap_err();
        assert!(matches!(error, GatewayError::Upstream { status: 429, .. }));
        let error = decoder
            .decode(
                frame(r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#),
                &mut out,
            )
            .unwrap_err();
        assert!(matches!(error, GatewayError::ContentFilter(_)));
    }
}
