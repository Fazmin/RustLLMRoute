//! Bedrock Converse response -> OpenAI chat response.

use rustllmroute_types::{
    AssistantMessage, ChatResponse, Choice, Extra, FinishReason, FunctionCall, GatewayError,
    PromptTokensDetails, ToolCall, Usage,
};
use serde_json::{Value, json};

use crate::anthropic::request::JSON_TOOL_NAME;
use crate::traits::CallContext;

pub fn map_stop_reason(reason: Option<&str>) -> Option<FinishReason> {
    match reason? {
        "end_turn" | "stop_sequence" => Some(FinishReason::Stop),
        "tool_use" => Some(FinishReason::ToolCalls),
        "max_tokens" => Some(FinishReason::Length),
        "guardrail_intervened" | "content_filtered" => Some(FinishReason::ContentFilter),
        _ => Some(FinishReason::Other),
    }
}

/// `prompt_tokens` includes cache reads and writes, as LiteLLM reports them.
pub fn usage_from_converse(usage: &Value) -> Usage {
    let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let input = count("inputTokens");
    let cache_read = count("cacheReadInputTokens");
    let cache_write = count("cacheWriteInputTokens");
    let output = count("outputTokens");
    let prompt = input + cache_read + cache_write;
    Usage {
        prompt_tokens: prompt,
        completion_tokens: output,
        total_tokens: prompt + output,
        prompt_tokens_details: (cache_read > 0).then(|| PromptTokensDetails {
            cached_tokens: Some(cache_read),
            ..PromptTokensDetails::default()
        }),
        cache_creation_input_tokens: (cache_write > 0).then_some(cache_write),
        ..Usage::default()
    }
}

#[derive(Debug, Default)]
pub struct ParsedBlocks {
    pub text: String,
    pub reasoning: String,
    pub thinking_blocks: Vec<Value>,
    pub tool_calls: Vec<ToolCall>,
    /// The forced `json_tool_call` input when `response_format` was emulated.
    pub json_output: Option<String>,
}

pub fn parse_content(blocks: &[Value], cx: &CallContext<'_>) -> ParsedBlocks {
    let mut parsed = ParsedBlocks::default();
    for block in blocks {
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            parsed.text.push_str(text);
        }
        if let Some(reasoning) = block.get("reasoningContent") {
            if let Some(inner) = reasoning.get("reasoningText") {
                let text = inner
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                parsed.reasoning.push_str(text);
                parsed.thinking_blocks.push(json!({
                    "type": "thinking",
                    "thinking": text,
                    "signature": inner.get("signature").and_then(Value::as_str).unwrap_or_default(),
                }));
            }
            if let Some(data) = reasoning.get("redactedContent").and_then(Value::as_str) {
                parsed
                    .thinking_blocks
                    .push(json!({"type": "redacted_thinking", "data": data}));
            }
        }
        if let Some(tool) = block.get("toolUse") {
            let name = tool.get("name").and_then(Value::as_str).unwrap_or_default();
            let input = tool.get("input").cloned().unwrap_or_else(|| json!({}));
            if cx.json_tool_call && name == JSON_TOOL_NAME {
                parsed.json_output = Some(input.to_string());
                continue;
            }
            let client_name = cx
                .tool_name_map
                .get(name)
                .cloned()
                .unwrap_or_else(|| name.to_owned());
            parsed.tool_calls.push(ToolCall {
                id: tool
                    .get("toolUseId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                kind: "function".to_owned(),
                function: FunctionCall {
                    name: client_name,
                    arguments: input.to_string(),
                },
                extra: Extra::new(),
            });
        }
    }
    parsed
}

pub fn converse_to_chat(
    body: &Value,
    cx: &CallContext<'_>,
    created: u64,
) -> Result<ChatResponse, GatewayError> {
    let blocks = body
        .pointer("/output/message/content")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| {
            GatewayError::InvalidUpstreamResponse(
                "bedrock: converse response has no output message".to_owned(),
            )
        })?;
    let parsed = parse_content(&blocks, cx);
    let stop_reason = body.get("stopReason").and_then(Value::as_str);
    let (content, tool_calls, finish) = match parsed.json_output {
        Some(json) => (Some(json), None, Some(FinishReason::Stop)),
        None => {
            let has_tools = !parsed.tool_calls.is_empty();
            (
                (!parsed.text.is_empty() || !has_tools).then_some(parsed.text),
                has_tools.then_some(parsed.tool_calls),
                if has_tools {
                    Some(FinishReason::ToolCalls)
                } else {
                    map_stop_reason(stop_reason)
                },
            )
        }
    };
    Ok(ChatResponse {
        id: format!("chatcmpl-{}", uuid::Uuid::now_v7().simple()),
        object: "chat.completion".to_owned(),
        created,
        model: cx.model.to_owned(),
        choices: vec![Choice {
            index: 0,
            message: AssistantMessage {
                content,
                tool_calls,
                reasoning_content: (!parsed.reasoning.is_empty()).then_some(parsed.reasoning),
                thinking_blocks: (!parsed.thinking_blocks.is_empty())
                    .then_some(parsed.thinking_blocks),
                ..AssistantMessage::default()
            },
            finish_reason: finish,
            logprobs: None,
            extra: Extra::new(),
        }],
        usage: body.get("usage").map(usage_from_converse),
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
    fn tool_use_and_reasoning_translate_back() {
        let body: Value = serde_json::from_str(
            r#"{"output":{"message":{"role":"assistant","content":[
                {"reasoningContent":{"reasoningText":{"text":"hmm","signature":"sig"}}},
                {"text":"Checking"},
                {"toolUse":{"toolUseId":"tooluse_1","name":"get_weather","input":{"city":"Oslo"}}}
            ]}},"stopReason":"tool_use",
            "usage":{"inputTokens":10,"outputTokens":5,"totalTokens":15,"cacheReadInputTokens":4,"cacheWriteInputTokens":2}}"#,
        )
        .unwrap();
        let config =
            ProviderConfig::new(ProviderKind::Bedrock, "m", Url::parse("https://x").unwrap());
        let mut cx = CallContext::new(&config, "m", false, "r");
        cx.tool_name_map
            .insert("get_weather".into(), "get.weather".into());
        let response = converse_to_chat(&body, &cx, 1).unwrap();
        let choice = &response.choices[0];
        assert_eq!(choice.finish_reason, Some(FinishReason::ToolCalls));
        assert_eq!(choice.message.content.as_deref(), Some("Checking"));
        assert_eq!(choice.message.reasoning_content.as_deref(), Some("hmm"));
        assert_eq!(
            choice.message.thinking_blocks.as_ref().unwrap()[0]["signature"],
            "sig"
        );
        let call = &choice.message.tool_calls.as_ref().unwrap()[0];
        assert_eq!(call.id, "tooluse_1");
        assert_eq!(call.function.name, "get.weather");
        let usage = response.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 16);
        assert_eq!(usage.cached_tokens(), 4);
        assert_eq!(usage.cache_creation_tokens(), 2);
    }

    #[test]
    fn json_tool_output_becomes_content() {
        let body = json!({"output": {"message": {"content": [{"toolUse": {"toolUseId": "t", "name": JSON_TOOL_NAME, "input": {"a": 1}}}]}}, "stopReason": "tool_use"});
        let config =
            ProviderConfig::new(ProviderKind::Bedrock, "m", Url::parse("https://x").unwrap());
        let mut cx = CallContext::new(&config, "m", false, "r");
        cx.json_tool_call = true;
        let response = converse_to_chat(&body, &cx, 1).unwrap();
        assert_eq!(
            response.choices[0].message.content.as_deref(),
            Some("{\"a\":1}")
        );
        assert!(response.choices[0].message.tool_calls.is_none());
        assert_eq!(response.choices[0].finish_reason, Some(FinishReason::Stop));
        assert_eq!(
            map_stop_reason(Some("guardrail_intervened")),
            Some(FinishReason::ContentFilter)
        );
    }
}
