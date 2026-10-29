//! Gemini `generateContent` response -> OpenAI chat response.

use rustllmroute_types::{
    AssistantMessage, ChatResponse, Choice, CompletionTokensDetails, Extra, FinishReason,
    FunctionCall, GatewayError, PromptTokensDetails, ToolCall, Usage,
};
use serde_json::{Value, json};

use crate::traits::CallContext;

pub const CONTENT_FILTER_REASONS: &[&str] = &[
    "SAFETY",
    "RECITATION",
    "BLOCKLIST",
    "PROHIBITED_CONTENT",
    "SPII",
    "IMAGE_SAFETY",
];

pub fn map_finish(reason: Option<&str>, has_tool_calls: bool) -> Option<FinishReason> {
    if has_tool_calls {
        return Some(FinishReason::ToolCalls);
    }
    match reason? {
        "STOP" | "FINISH_REASON_UNSPECIFIED" => Some(FinishReason::Stop),
        "MAX_TOKENS" => Some(FinishReason::Length),
        other if CONTENT_FILTER_REASONS.contains(&other) => Some(FinishReason::ContentFilter),
        _ => Some(FinishReason::Other),
    }
}

/// Gemini's cumulative `usageMetadata` -> OpenAI usage (`prompt_tokens` includes cached tokens,
/// `completion_tokens` includes thoughts).
pub fn usage_from_metadata(metadata: &Value) -> Usage {
    let count = |key: &str| metadata.get(key).and_then(Value::as_u64).unwrap_or(0);
    let prompt = count("promptTokenCount");
    let cached = count("cachedContentTokenCount");
    let thoughts = count("thoughtsTokenCount");
    let candidates = count("candidatesTokenCount");
    let completion = candidates + thoughts;
    let total = metadata
        .get("totalTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(prompt + completion)
        .max(prompt + completion);
    Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: total,
        prompt_tokens_details: (cached > 0).then(|| PromptTokensDetails {
            cached_tokens: Some(cached),
            ..PromptTokensDetails::default()
        }),
        completion_tokens_details: (thoughts > 0).then(|| CompletionTokensDetails {
            reasoning_tokens: Some(thoughts),
            ..CompletionTokensDetails::default()
        }),
        ..Usage::default()
    }
}

/// Text, reasoning, tool calls and thought signature pulled out of a candidate's parts.
#[derive(Debug, Default)]
pub struct Parsed {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub signature: Option<String>,
}

pub fn parse_parts(parts: &[Value], next_call_id: &mut dyn FnMut() -> String) -> Parsed {
    let mut parsed = Parsed::default();
    for part in parts {
        if let Some(signature) = part.get("thoughtSignature").and_then(Value::as_str) {
            parsed.signature = Some(signature.to_owned());
        }
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            if part.get("thought").and_then(Value::as_bool) == Some(true) {
                parsed.reasoning.push_str(text);
            } else {
                parsed.text.push_str(text);
            }
        }
        if let Some(call) = part.get("functionCall") {
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(&mut *next_call_id);
            parsed.tool_calls.push(ToolCall {
                id,
                kind: "function".to_owned(),
                function: FunctionCall {
                    name,
                    arguments: args.to_string(),
                },
                extra: Extra::new(),
            });
        }
    }
    parsed
}

pub fn new_call_id() -> String {
    format!("call_{}", uuid::Uuid::now_v7().simple())
}

pub fn blocked_prompt_error(body: &Value) -> Option<GatewayError> {
    let reason = body.pointer("/promptFeedback/blockReason")?.as_str()?;
    Some(GatewayError::ContentFilter(format!(
        "gemini blocked the prompt: {reason}"
    )))
}

pub fn generate_content_to_chat(
    body: &Value,
    cx: &CallContext<'_>,
    created: u64,
) -> Result<ChatResponse, GatewayError> {
    let candidates = body
        .get("candidates")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if candidates.is_empty() {
        if let Some(error) = blocked_prompt_error(body) {
            return Err(error);
        }
        return Err(GatewayError::InvalidUpstreamResponse(
            "gemini: response has no candidates".to_owned(),
        ));
    }
    let mut choices = Vec::with_capacity(candidates.len());
    for (index, candidate) in candidates.iter().enumerate() {
        let finish_reason = candidate.get("finishReason").and_then(Value::as_str);
        if finish_reason == Some("MALFORMED_FUNCTION_CALL") {
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
        let has_tools = !parsed.tool_calls.is_empty();
        let thinking_blocks = parsed.signature.as_ref().map(|signature| {
            vec![json!({"type": "thinking", "thinking": parsed.reasoning, "signature": signature})]
        });
        choices.push(Choice {
            index: candidate
                .get("index")
                .and_then(Value::as_u64)
                .map(|i| i as u32)
                .unwrap_or(index as u32),
            message: AssistantMessage {
                content: (!parsed.text.is_empty() || !has_tools).then_some(parsed.text),
                tool_calls: has_tools.then_some(parsed.tool_calls),
                reasoning_content: (!parsed.reasoning.is_empty()).then_some(parsed.reasoning),
                thinking_blocks,
                ..AssistantMessage::default()
            },
            finish_reason: map_finish(finish_reason, has_tools),
            logprobs: None,
            extra: Extra::new(),
        });
    }
    let usage = body.get("usageMetadata").map(usage_from_metadata);
    Ok(ChatResponse {
        id: body
            .get("responseId")
            .and_then(Value::as_str)
            .map(|id| format!("chatcmpl-{id}"))
            .unwrap_or_else(|| format!("chatcmpl-{}", uuid::Uuid::now_v7().simple())),
        object: "chat.completion".to_owned(),
        created,
        model: body
            .get("modelVersion")
            .and_then(Value::as_str)
            .unwrap_or(cx.model)
            .to_owned(),
        choices,
        usage,
        system_fingerprint: None,
        extra: Extra::new(),
    })
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::*;
    use crate::registry::ProviderKind;
    use crate::traits::ProviderConfig;

    #[test]
    fn function_calls_and_thoughts_translate_back() {
        let body: Value = serde_json::from_str(
            r#"{"candidates":[{"content":{"role":"model","parts":[
                {"text":"thinking...","thought":true},
                {"text":"Let me check","thoughtSignature":"sig"},
                {"functionCall":{"name":"get_weather","args":{"city":"Oslo"}}}
            ]},"finishReason":"STOP","index":0}],
            "usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"thoughtsTokenCount":3,"cachedContentTokenCount":4,"totalTokenCount":18},
            "modelVersion":"gemini-2.5-pro-001","responseId":"abc"}"#,
        )
        .unwrap();
        let config = ProviderConfig::new(
            ProviderKind::Gemini,
            "gemini-2.5-pro",
            Url::parse("https://x").unwrap(),
        );
        let cx = CallContext::new(&config, "gemini-2.5-pro", false, "r");
        let response = generate_content_to_chat(&body, &cx, 1).unwrap();
        assert_eq!(response.id, "chatcmpl-abc");
        assert_eq!(response.model, "gemini-2.5-pro-001");
        let choice = &response.choices[0];
        assert_eq!(choice.finish_reason, Some(FinishReason::ToolCalls));
        assert_eq!(choice.message.content.as_deref(), Some("Let me check"));
        assert_eq!(
            choice.message.reasoning_content.as_deref(),
            Some("thinking...")
        );
        let call = &choice.message.tool_calls.as_ref().unwrap()[0];
        assert!(call.id.starts_with("call_"));
        assert_eq!(call.function.arguments, "{\"city\":\"Oslo\"}");
        assert_eq!(
            choice.message.thinking_blocks.as_ref().unwrap()[0]["signature"],
            "sig"
        );
        let usage = response.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 8);
        assert_eq!(usage.cached_tokens(), 4);
        assert_eq!(usage.reasoning_tokens(), 3);
    }

    #[test]
    fn blocked_prompts_are_content_filter_errors() {
        let body = json!({"promptFeedback": {"blockReason": "SAFETY"}});
        let config =
            ProviderConfig::new(ProviderKind::Gemini, "m", Url::parse("https://x").unwrap());
        let cx = CallContext::new(&config, "m", false, "r");
        let error = generate_content_to_chat(&body, &cx, 1).unwrap_err();
        assert!(matches!(error, GatewayError::ContentFilter(_)));
        assert_eq!(
            map_finish(Some("SAFETY"), false),
            Some(FinishReason::ContentFilter)
        );
        assert_eq!(
            map_finish(Some("MAX_TOKENS"), false),
            Some(FinishReason::Length)
        );
    }
}
