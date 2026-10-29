//! Anthropic Messages <-> OpenAI chat adapters for deployments that do not speak Messages natively.

use rustllmroute_types::{
    AnthropicUsage, ChatMessage, ChatRequest, ChatResponse, Content, ContentBlock, ContentPart,
    Extra, FunctionDef, GatewayError, ImageUrl, MessageContent, MessagesRequest, MessagesResponse,
    Recognized, Stop, Tool, ToolCall, ToolChoice, finish_to_stop_reason, role,
};
use serde_json::{Value, json};

fn image_url_from_source(source: &Value) -> Option<String> {
    match source.get("type").and_then(Value::as_str)? {
        "base64" => {
            let media_type = source.get("media_type").and_then(Value::as_str)?;
            let data = source.get("data").and_then(Value::as_str)?;
            Some(format!("data:{media_type};base64,{data}"))
        }
        "url" => source.get("url").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

fn tool_result_text(content: &Option<Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn reasoning_effort_for(thinking: &Value) -> Option<String> {
    let kind = thinking.get("type").and_then(Value::as_str)?;
    if kind != "enabled" && kind != "adaptive" {
        return None;
    }
    let budget = thinking
        .get("budget_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(2048);
    Some(
        match budget {
            0..=1024 => "low",
            1025..=2048 => "medium",
            _ => "high",
        }
        .to_owned(),
    )
}

/// Anthropic Messages request -> OpenAI chat request (model filled in by the provider).
pub fn messages_to_chat_request(
    req: &MessagesRequest,
    model: &str,
) -> Result<ChatRequest, GatewayError> {
    let mut messages: Vec<ChatMessage> = Vec::new();
    if let Some(system) = &req.system {
        let text = system.text();
        if !text.is_empty() {
            messages.push(ChatMessage::text(role::SYSTEM, text));
        }
    }
    for message in &req.messages {
        let blocks = message.content.clone().into_blocks();
        match message.role.as_str() {
            "assistant" => {
                let mut text = String::new();
                let mut reasoning = String::new();
                let mut tool_calls = Vec::new();
                for block in &blocks {
                    match block.known() {
                        Some(ContentBlock::Text { text: t, .. }) => text.push_str(t),
                        Some(ContentBlock::Thinking { thinking, .. }) => {
                            reasoning.push_str(thinking)
                        }
                        Some(ContentBlock::ToolUse {
                            id, name, input, ..
                        }) => tool_calls.push(ToolCall {
                            id: id.clone(),
                            kind: "function".to_owned(),
                            function: rustllmroute_types::FunctionCall {
                                name: name.clone(),
                                arguments: input.to_string(),
                            },
                            extra: Extra::new(),
                        }),
                        _ => {}
                    }
                }
                messages.push(ChatMessage {
                    role: role::ASSISTANT.to_owned(),
                    content: (!text.is_empty()).then_some(Content::Text(text)),
                    tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
                    reasoning_content: (!reasoning.is_empty()).then_some(reasoning),
                    ..ChatMessage::default()
                });
            }
            _ => {
                let mut parts: Vec<Recognized<ContentPart>> = Vec::new();
                for block in &blocks {
                    match block.known() {
                        Some(ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        }) => messages.push(ChatMessage {
                            role: role::TOOL.to_owned(),
                            tool_call_id: Some(tool_use_id.clone()),
                            content: Some(Content::Text(tool_result_text(content))),
                            ..ChatMessage::default()
                        }),
                        Some(ContentBlock::Text { text, .. }) => {
                            parts.push(Recognized::Known(ContentPart::Text {
                                text: text.clone(),
                                extra: Extra::new(),
                            }))
                        }
                        Some(ContentBlock::Image { source, .. }) => {
                            if let Some(url) = image_url_from_source(source) {
                                parts.push(Recognized::Known(ContentPart::ImageUrl {
                                    image_url: ImageUrl {
                                        url,
                                        detail: None,
                                        extra: Extra::new(),
                                    },
                                    extra: Extra::new(),
                                }));
                            }
                        }
                        _ => {}
                    }
                }
                if parts.is_empty() {
                    continue;
                }
                let content = match parts.as_slice() {
                    [Recognized::Known(ContentPart::Text { text, .. })] => {
                        Content::Text(text.clone())
                    }
                    _ => Content::Parts(parts),
                };
                messages.push(ChatMessage {
                    role: role::USER.to_owned(),
                    content: Some(content),
                    ..ChatMessage::default()
                });
            }
        }
    }
    if messages.iter().all(|m| m.role == role::SYSTEM) {
        return Err(GatewayError::BadRequest {
            message: "messages must contain at least one user or assistant turn".to_owned(),
            param: Some("messages".to_owned()),
            code: None,
        });
    }
    let tools: Vec<Tool> = req
        .tools
        .iter()
        .flatten()
        .filter_map(|tool| {
            let name = tool.get("name").and_then(Value::as_str)?;
            let schema = tool.get("input_schema")?;
            Some(Tool {
                kind: "function".to_owned(),
                function: Some(FunctionDef {
                    name: name.to_owned(),
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    parameters: Some(schema.clone()),
                    strict: None,
                    extra: Extra::new(),
                }),
                extra: Extra::new(),
            })
        })
        .collect();
    let mut parallel_tool_calls = None;
    let tool_choice = req.tool_choice.as_ref().and_then(|choice| {
        if choice
            .get("disable_parallel_tool_use")
            .and_then(Value::as_bool)
            == Some(true)
        {
            parallel_tool_calls = Some(false);
        }
        match choice.get("type").and_then(Value::as_str)? {
            "auto" => Some(ToolChoice::Mode("auto".to_owned())),
            "any" => Some(ToolChoice::Mode("required".to_owned())),
            "none" => Some(ToolChoice::Mode("none".to_owned())),
            "tool" => choice.get("name").and_then(Value::as_str).map(|name| {
                ToolChoice::Named(json!({"type": "function", "function": {"name": name}}))
            }),
            _ => None,
        }
    });
    let mut extra = Extra::new();
    if let Some(top_k) = req.top_k {
        extra.insert("top_k".to_owned(), json!(top_k));
    }
    Ok(ChatRequest {
        model: model.to_owned(),
        messages,
        stream: req.stream,
        stream_options: None,
        tools: (!tools.is_empty()).then_some(tools),
        tool_choice,
        parallel_tool_calls,
        max_tokens: req.max_tokens,
        max_completion_tokens: None,
        temperature: req.temperature,
        top_p: req.top_p,
        n: None,
        stop: req.stop_sequences.clone().map(Stop::Many),
        seed: None,
        user: req
            .metadata
            .as_ref()
            .and_then(|m| m.get("user_id"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        response_format: None,
        reasoning_effort: req.thinking.as_ref().and_then(reasoning_effort_for),
        logprobs: None,
        top_logprobs: None,
        metadata: None,
        extra,
    })
}

/// OpenAI chat response -> Anthropic Messages response.
pub fn chat_to_messages_response(response: &ChatResponse, model: &str) -> MessagesResponse {
    let mut content: Vec<Recognized<ContentBlock>> = Vec::new();
    let mut finish = None;
    if let Some(choice) = response.choices.first() {
        finish = choice.finish_reason;
        let message = &choice.message;
        if let Some(blocks) = &message.thinking_blocks {
            for block in blocks {
                if let Ok(parsed) = serde_json::from_value::<ContentBlock>(block.clone()) {
                    content.push(Recognized::Known(parsed));
                }
            }
        } else if let Some(reasoning) = message
            .reasoning_content
            .as_deref()
            .filter(|r| !r.is_empty())
        {
            content.push(Recognized::Known(ContentBlock::Thinking {
                thinking: reasoning.to_owned(),
                signature: None,
                extra: Extra::new(),
            }));
        }
        if let Some(text) = message.content.as_deref().filter(|t| !t.is_empty()) {
            content.push(Recognized::Known(ContentBlock::text(text)));
        }
        for call in message.tool_calls.iter().flatten() {
            let input = serde_json::from_str::<Value>(&call.function.arguments)
                .ok()
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({}));
            content.push(Recognized::Known(ContentBlock::ToolUse {
                id: call.id.clone(),
                name: call.function.name.clone(),
                input,
                extra: Extra::new(),
            }));
        }
    }
    let id = if response.id.starts_with("msg_") {
        response.id.clone()
    } else {
        format!("msg_{}", response.id.trim_start_matches("chatcmpl-"))
    };
    MessagesResponse {
        id,
        kind: "message".to_owned(),
        role: "assistant".to_owned(),
        content,
        model: model.to_owned(),
        stop_reason: Some(
            finish_to_stop_reason(finish.unwrap_or(rustllmroute_types::FinishReason::Stop))
                .to_owned(),
        ),
        stop_sequence: None,
        usage: response
            .usage
            .as_ref()
            .map(AnthropicUsage::from_openai)
            .unwrap_or_default(),
        extra: Extra::new(),
    }
}

/// Rough prompt size for backends without a count endpoint: 4 characters per token plus a small
/// per-message overhead. Replaced by real tokenisation in a later phase.
pub fn estimate_input_tokens(req: &MessagesRequest) -> u64 {
    let mut chars = req.system.as_ref().map(|s| s.text().len()).unwrap_or(0);
    for message in &req.messages {
        chars += message
            .content
            .text_segments()
            .iter()
            .map(|s| s.len())
            .sum::<usize>();
        if let MessageContent::Blocks(blocks) = &message.content {
            for block in blocks {
                if let Some(ContentBlock::ToolUse { input, .. }) = block.known() {
                    chars += input.to_string().len();
                }
            }
        }
    }
    for tool in req.tools.iter().flatten() {
        chars += tool.to_string().len();
    }
    ((chars as u64).div_ceil(4) + 3 * req.messages.len() as u64).max(1)
}

#[cfg(test)]
mod tests {
    use rustllmroute_types::{AssistantMessage, Choice, FinishReason, FunctionCall, Usage};

    use super::*;

    #[test]
    fn messages_request_maps_to_chat_with_tools_and_tool_results() {
        let req: MessagesRequest = serde_json::from_str(
            r#"{"model":"claude","max_tokens":50,"system":"be brief","stream":true,
                "messages":[
                  {"role":"user","content":"weather?"},
                  {"role":"assistant","content":[{"type":"text","text":"checking"},{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{"city":"Oslo"}}]},
                  {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"12C"},{"type":"text","text":"and tomorrow?"}]}
                ],
                "tools":[{"name":"get_weather","description":"d","input_schema":{"type":"object"}},{"type":"web_search_20250305","name":"web_search"}],
                "tool_choice":{"type":"any","disable_parallel_tool_use":true},
                "metadata":{"user_id":"u1"},"thinking":{"type":"enabled","budget_tokens":4000}}"#,
        )
        .unwrap();
        let chat = messages_to_chat_request(&req, "gpt-4o").unwrap();
        let roles: Vec<&str> = chat.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["system", "user", "assistant", "tool", "user"]);
        assert_eq!(
            chat.messages[2].tool_calls.as_ref().unwrap()[0]
                .function
                .arguments,
            "{\"city\":\"Oslo\"}"
        );
        assert_eq!(chat.messages[3].tool_call_id.as_deref(), Some("toolu_1"));
        assert_eq!(
            chat.tools.as_ref().unwrap().len(),
            1,
            "server tools without a schema are dropped"
        );
        assert_eq!(chat.tool_choice, Some(ToolChoice::Mode("required".into())));
        assert_eq!(chat.parallel_tool_calls, Some(false));
        assert_eq!(chat.user.as_deref(), Some("u1"));
        assert_eq!(chat.reasoning_effort.as_deref(), Some("high"));
        assert!(chat.stream);
        assert!(estimate_input_tokens(&req) > 10);
    }

    #[test]
    fn chat_response_maps_to_messages_with_tool_use() {
        let response = ChatResponse {
            id: "chatcmpl-9".into(),
            object: "chat.completion".into(),
            created: 1,
            model: "gpt-4o".into(),
            choices: vec![Choice {
                index: 0,
                message: AssistantMessage {
                    content: Some("Let me check".into()),
                    tool_calls: Some(vec![ToolCall {
                        id: "call_1".into(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "get_weather".into(),
                            arguments: "{\"city\":\"Oslo\"}".into(),
                        },
                        extra: Extra::new(),
                    }]),
                    ..AssistantMessage::default()
                },
                finish_reason: Some(FinishReason::ToolCalls),
                logprobs: None,
                extra: Extra::new(),
            }],
            usage: Some(Usage::new(12, 4)),
            system_fingerprint: None,
            extra: Extra::new(),
        };
        let out = chat_to_messages_response(&response, "claude");
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v["id"], "msg_9");
        assert_eq!(v["model"], "claude");
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][1]["type"], "tool_use");
        assert_eq!(v["content"][1]["input"]["city"], "Oslo");
        assert_eq!(v["stop_reason"], "tool_use");
        assert_eq!(v["usage"]["input_tokens"], 12);
        assert_eq!(v["usage"]["output_tokens"], 4);
    }
}
