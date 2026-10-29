use rustllmroute_types::{
    AssistantMessage, ChatResponse, Choice, ContentBlock, Extra, FinishReason, FunctionCall,
    MessagesResponse, ToolCall, stop_reason_to_finish,
};
use serde_json::{Value, json};

use super::request::JSON_TOOL_NAME;
use crate::traits::CallContext;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Anthropic Messages response -> OpenAI chat completion.
pub fn messages_to_chat(response: MessagesResponse, cx: &CallContext<'_>) -> ChatResponse {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut thinking_blocks: Vec<Value> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut json_payload: Option<String> = None;
    for block in &response.content {
        let Some(block) = block.known() else { continue };
        match block {
            ContentBlock::Text { text: t, .. } => text.push_str(t),
            ContentBlock::ToolUse {
                id, name, input, ..
            } => {
                if cx.json_tool_call && name == JSON_TOOL_NAME {
                    json_payload = Some(input.to_string());
                    continue;
                }
                let original = cx
                    .tool_name_map
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| name.clone());
                tool_calls.push(ToolCall {
                    id: id.clone(),
                    kind: "function".to_owned(),
                    function: FunctionCall {
                        name: original,
                        arguments: input.to_string(),
                    },
                    extra: Extra::new(),
                });
            }
            ContentBlock::Thinking {
                thinking,
                signature,
                ..
            } => {
                reasoning.push_str(thinking);
                thinking_blocks.push(
                    json!({"type": "thinking", "thinking": thinking, "signature": signature}),
                );
            }
            ContentBlock::RedactedThinking { data, .. } => {
                thinking_blocks.push(json!({"type": "redacted_thinking", "data": data}));
            }
            ContentBlock::Image { .. }
            | ContentBlock::Document { .. }
            | ContentBlock::ToolResult { .. } => {}
        }
    }
    let mut finish = response
        .stop_reason
        .as_deref()
        .map(stop_reason_to_finish)
        .unwrap_or(FinishReason::Stop);
    if let Some(payload) = json_payload {
        text = payload;
        if finish == FinishReason::ToolCalls {
            finish = FinishReason::Stop;
        }
    }
    let mut extra = Extra::new();
    if let Some(stop_sequence) = &response.stop_sequence {
        extra.insert(
            "stop_sequence".to_owned(),
            Value::String(stop_sequence.clone()),
        );
    }
    ChatResponse {
        id: response.id,
        object: "chat.completion".to_owned(),
        created: now_secs(),
        model: response.model,
        choices: vec![Choice {
            index: 0,
            message: AssistantMessage {
                content: Some(text),
                tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
                reasoning_content: (!reasoning.is_empty()).then_some(reasoning),
                thinking_blocks: (!thinking_blocks.is_empty()).then_some(thinking_blocks),
                ..AssistantMessage::default()
            },
            finish_reason: Some(finish),
            logprobs: None,
            extra,
        }],
        usage: Some(response.usage.to_openai()),
        system_fingerprint: None,
        extra: Extra::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use url::Url;

    use super::*;
    use crate::registry::ProviderKind;
    use crate::traits::ProviderConfig;

    fn config() -> ProviderConfig {
        ProviderConfig {
            kind: ProviderKind::Anthropic,
            provider_name: "anthropic",
            model: "claude".to_owned(),
            base_url: Url::parse("https://api.anthropic.com").unwrap(),
            api_key: None,
            api_version: None,
            extra_headers: Vec::new(),
            extra_body: serde_json::Map::new(),
            drop_params: true,
            connect_timeout: Duration::from_secs(5),
            timeout: Duration::from_secs(60),
            stream_idle_timeout: Duration::from_secs(60),
            default_max_tokens: None,
            vertex: None,
            aws: None,
            token_source: None,
            aws_credentials: None,
        }
    }

    #[test]
    fn tool_use_and_thinking_translate_back() {
        let response: MessagesResponse = serde_json::from_str(
            r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude","content":[
                {"type":"thinking","thinking":"hmm","signature":"sig"},
                {"type":"text","text":"Checking"},
                {"type":"tool_use","id":"toolu_1","name":"get_weather_","input":{"city":"Oslo"}}
            ],"stop_reason":"tool_use","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":4}}"#,
        )
        .unwrap();
        let config = config();
        let mut cx = CallContext::new(&config, "claude", false, "r");
        cx.tool_name_map
            .insert("get_weather_".into(), "get weather!".into());
        let chat = messages_to_chat(response, &cx);
        let choice = &chat.choices[0];
        assert_eq!(choice.finish_reason, Some(FinishReason::ToolCalls));
        assert_eq!(choice.message.content.as_deref(), Some("Checking"));
        assert_eq!(choice.message.reasoning_content.as_deref(), Some("hmm"));
        let calls = choice.message.tool_calls.as_ref().unwrap();
        assert_eq!(calls[0].function.name, "get weather!");
        assert_eq!(calls[0].function.arguments, "{\"city\":\"Oslo\"}");
        let usage = chat.usage.unwrap();
        assert_eq!(
            (
                usage.prompt_tokens,
                usage.completion_tokens,
                usage.cached_tokens()
            ),
            (14, 5, 4)
        );
    }

    #[test]
    fn json_tool_mode_unwraps_to_content() {
        let response: MessagesResponse = serde_json::from_str(
            r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude","content":[
                {"type":"tool_use","id":"toolu_1","name":"json_tool_call","input":{"k":"v"}}
            ],"stop_reason":"tool_use","usage":{"input_tokens":1,"output_tokens":1}}"#,
        )
        .unwrap();
        let config = config();
        let mut cx = CallContext::new(&config, "claude", false, "r");
        cx.json_tool_call = true;
        let chat = messages_to_chat(response, &cx);
        assert_eq!(
            chat.choices[0].message.content.as_deref(),
            Some("{\"k\":\"v\"}")
        );
        assert!(chat.choices[0].message.tool_calls.is_none());
        assert_eq!(chat.choices[0].finish_reason, Some(FinishReason::Stop));
    }
}
