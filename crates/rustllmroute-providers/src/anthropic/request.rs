use rustllmroute_types::{
    AnthropicMessage, ChatMessage, ChatRequest, Content, ContentBlock, ContentPart, Extra,
    GatewayError, MessageContent, MessagesRequest, Recognized, Stop, SystemPrompt, ToolChoice,
    role,
};
use serde_json::{Map, Value, json};

use super::AnthropicProvider;
use crate::traits::CallContext;

pub const JSON_TOOL_NAME: &str = "json_tool_call";
const MAX_TOOL_NAME: usize = 128;

/// Anthropic tool names must match `^[a-zA-Z0-9_-]{1,128}$`; the original name is remembered so
/// responses can be translated back.
pub fn sanitize_tool_name(name: &str, cx: &mut CallContext<'_>) -> String {
    let mut clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(MAX_TOOL_NAME)
        .collect();
    if clean.is_empty() {
        clean = "tool".to_owned();
    }
    if clean != name {
        cx.tool_name_map.insert(clean.clone(), name.to_owned());
    }
    clean
}

fn data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let media_type = meta.strip_suffix(";base64")?;
    Some((media_type.to_owned(), data.to_owned()))
}

fn image_block(url: &str, extra: &Extra) -> ContentBlock {
    let source = match data_url(url) {
        Some((media_type, data)) => {
            json!({"type": "base64", "media_type": media_type, "data": data})
        }
        None => json!({"type": "url", "url": url}),
    };
    ContentBlock::Image {
        source,
        extra: cache_control_only(extra),
    }
}

fn cache_control_only(extra: &Extra) -> Extra {
    let mut out = Extra::new();
    if let Some(cc) = extra.get("cache_control") {
        out.insert("cache_control".to_owned(), cc.clone());
    }
    out
}

fn user_blocks(message: &ChatMessage) -> Vec<Recognized<ContentBlock>> {
    let mut blocks = Vec::new();
    match &message.content {
        Some(Content::Text(text)) => blocks.push(Recognized::Known(ContentBlock::Text {
            text: text.clone(),
            extra: cache_control_only(&message.extra),
        })),
        Some(Content::Parts(parts)) => {
            for part in parts {
                let Some(part) = part.known() else { continue };
                match part {
                    ContentPart::Text { text, extra } => {
                        blocks.push(Recognized::Known(ContentBlock::Text {
                            text: text.clone(),
                            extra: cache_control_only(extra),
                        }))
                    }
                    ContentPart::ImageUrl { image_url, extra } => {
                        blocks.push(Recognized::Known(image_block(&image_url.url, extra)));
                    }
                    ContentPart::File { file, extra } => {
                        if let Some((media_type, data)) = file
                            .get("file_data")
                            .and_then(Value::as_str)
                            .and_then(data_url)
                        {
                            blocks.push(Recognized::Known(ContentBlock::Document {
                                source: json!({"type": "base64", "media_type": media_type, "data": data}),
                                extra: cache_control_only(extra),
                            }));
                        }
                    }
                    ContentPart::InputAudio { .. } => {}
                }
            }
            if let Some(cc) = message.extra.get("cache_control")
                && let Some(Recognized::Known(ContentBlock::Text { extra, .. })) = blocks.last_mut()
            {
                extra.insert("cache_control".to_owned(), cc.clone());
            }
        }
        None => {}
    }
    blocks
}

fn assistant_blocks(
    message: &ChatMessage,
    cx: &mut CallContext<'_>,
) -> Vec<Recognized<ContentBlock>> {
    let mut blocks = Vec::new();
    if let Some(Value::Array(thinking)) = message.extra.get("thinking_blocks") {
        for block in thinking {
            if let Ok(parsed) = serde_json::from_value::<ContentBlock>(block.clone()) {
                blocks.push(Recognized::Known(parsed));
            }
        }
    } else if let Some(reasoning) = &message.reasoning_content
        && !reasoning.is_empty()
    {
        blocks.push(Recognized::Known(ContentBlock::Thinking {
            thinking: reasoning.clone(),
            signature: None,
            extra: Extra::new(),
        }));
    }
    if let Some(content) = &message.content {
        for text in content.text_segments() {
            if !text.is_empty() {
                blocks.push(Recognized::Known(ContentBlock::text(text)));
            }
        }
    }
    if let Some(calls) = &message.tool_calls {
        for call in calls {
            let input = serde_json::from_str::<Value>(&call.function.arguments)
                .ok()
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({}));
            blocks.push(Recognized::Known(ContentBlock::ToolUse {
                id: call.id.clone(),
                name: sanitize_tool_name(&call.function.name, cx),
                input,
                extra: Extra::new(),
            }));
        }
    }
    blocks
}

fn tool_result_block(message: &ChatMessage) -> Recognized<ContentBlock> {
    let text = message
        .content
        .as_ref()
        .map(Content::joined_text)
        .unwrap_or_default();
    Recognized::Known(ContentBlock::ToolResult {
        tool_use_id: message.tool_call_id.clone().unwrap_or_default(),
        content: Some(Value::String(text)),
        is_error: None,
        extra: Extra::new(),
    })
}

fn push_message(
    messages: &mut Vec<AnthropicMessage>,
    role: &str,
    blocks: Vec<Recognized<ContentBlock>>,
) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = messages.last_mut()
        && last.role == role
    {
        let mut existing =
            std::mem::replace(&mut last.content, MessageContent::Text(String::new())).into_blocks();
        existing.extend(blocks);
        last.content = MessageContent::Blocks(existing);
        return;
    }
    messages.push(AnthropicMessage {
        role: role.to_owned(),
        content: MessageContent::Blocks(blocks),
        extra: Extra::new(),
    });
}

fn function_tool(tool: &rustllmroute_types::Tool, cx: &mut CallContext<'_>) -> Option<Value> {
    let function = tool.function.as_ref()?;
    let mut out = Map::new();
    out.insert(
        "name".to_owned(),
        Value::String(sanitize_tool_name(&function.name, cx)),
    );
    if let Some(description) = &function.description {
        out.insert("description".to_owned(), Value::String(description.clone()));
    }
    out.insert(
        "input_schema".to_owned(),
        function
            .parameters
            .clone()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
    );
    if let Some(cc) = tool.extra.get("cache_control") {
        out.insert("cache_control".to_owned(), cc.clone());
    }
    Some(Value::Object(out))
}

fn thinking_budget(effort: &str) -> Option<u32> {
    match effort {
        "none" => None,
        "minimal" | "low" => Some(1024),
        "medium" => Some(2048),
        "high" => Some(4096),
        "xhigh" | "max" => Some(8192),
        _ => None,
    }
}

/// OpenAI chat completion request -> Anthropic Messages request.
pub fn chat_to_messages(
    req: &ChatRequest,
    cx: &mut CallContext<'_>,
) -> Result<MessagesRequest, GatewayError> {
    let mut system: Vec<Recognized<ContentBlock>> = Vec::new();
    let mut messages: Vec<AnthropicMessage> = Vec::new();
    for message in &req.messages {
        match message.role.as_str() {
            role::SYSTEM | role::DEVELOPER => {
                for text in message.text_segments() {
                    system.push(Recognized::Known(ContentBlock::Text {
                        text: text.to_owned(),
                        extra: cache_control_only(&message.extra),
                    }));
                }
            }
            role::ASSISTANT => {
                let blocks = assistant_blocks(message, cx);
                push_message(&mut messages, "assistant", blocks);
            }
            role::TOOL | role::FUNCTION => {
                push_message(&mut messages, "user", vec![tool_result_block(message)])
            }
            _ => push_message(&mut messages, "user", user_blocks(message)),
        }
    }
    if messages.is_empty() {
        return Err(GatewayError::BadRequest {
            message: "at least one user or assistant message is required".to_owned(),
            param: Some("messages".to_owned()),
            code: None,
        });
    }
    if messages[0].role != "user" {
        messages.insert(
            0,
            AnthropicMessage {
                role: "user".to_owned(),
                content: MessageContent::Text(".".to_owned()),
                extra: Extra::new(),
            },
        );
    }
    let mut max_tokens = req
        .output_token_cap()
        .unwrap_or_else(|| AnthropicProvider::default_max_tokens(cx));
    let mut tools: Vec<Value> = Vec::new();
    let mut tool_choice: Option<Value> = None;
    let choice_mode = match &req.tool_choice {
        Some(ToolChoice::Mode(mode)) => mode.as_str(),
        _ => "auto",
    };
    if let Some(defined) = &req.tools
        && choice_mode != "none"
    {
        tools.extend(defined.iter().filter_map(|tool| function_tool(tool, cx)));
        tool_choice = match &req.tool_choice {
            Some(ToolChoice::Mode(mode)) if mode == "required" => Some(json!({"type": "any"})),
            Some(ToolChoice::Named(named)) => named
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .map(|name| json!({"type": "tool", "name": sanitize_tool_name(name, cx)})),
            _ => None,
        };
    }
    if let Some(format) = &req.response_format {
        let kind = format.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "json_schema" || kind == "json_object" {
            let schema = format
                .get("json_schema")
                .and_then(|s| s.get("schema"))
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"}));
            tools.push(json!({
                "name": JSON_TOOL_NAME,
                "description": "Respond with a JSON object matching the schema",
                "input_schema": schema,
            }));
            tool_choice = Some(json!({"type": "tool", "name": JSON_TOOL_NAME}));
            cx.json_tool_call = true;
        }
    }
    if req.parallel_tool_calls == Some(false) && !tools.is_empty() {
        let mut choice = tool_choice.unwrap_or_else(|| json!({"type": "auto"}));
        if let Value::Object(map) = &mut choice {
            map.insert("disable_parallel_tool_use".to_owned(), Value::Bool(true));
        }
        tool_choice = Some(choice);
    }
    let mut thinking = req.extra.get("thinking").cloned();
    if thinking.is_none()
        && let Some(budget) = req.reasoning_effort.as_deref().and_then(thinking_budget)
    {
        thinking = Some(json!({"type": "enabled", "budget_tokens": budget}));
        if max_tokens <= budget {
            max_tokens = budget + 1024;
        }
    }
    let thinking_enabled = thinking
        .as_ref()
        .and_then(|t| t.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|t| t == "enabled" || t == "adaptive");
    let stop_sequences = match &req.stop {
        Some(Stop::One(stop)) => Some(vec![stop.clone()]),
        Some(Stop::Many(stops)) => Some(stops.clone()),
        None => None,
    };
    let mut extra = Extra::new();
    if let Some(tier) = req.extra.get("service_tier") {
        extra.insert("service_tier".to_owned(), tier.clone());
    }
    let mut metadata = req.extra.get("metadata").cloned().filter(Value::is_object);
    if let Some(user) = &req.user {
        let mut map = match metadata.take() {
            Some(Value::Object(map)) => map,
            _ => Map::new(),
        };
        map.entry("user_id").or_insert(Value::String(user.clone()));
        metadata = Some(Value::Object(map));
    }
    Ok(MessagesRequest {
        model: cx.model.to_owned(),
        messages,
        max_tokens: Some(max_tokens),
        system: (!system.is_empty()).then_some(SystemPrompt::Blocks(system)),
        stream: req.stream,
        temperature: if thinking_enabled {
            None
        } else {
            req.temperature
        },
        top_p: if thinking_enabled { None } else { req.top_p },
        top_k: if thinking_enabled {
            None
        } else {
            req.extra
                .get("top_k")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
        },
        stop_sequences,
        tools: (!tools.is_empty()).then_some(tools),
        tool_choice,
        metadata,
        thinking,
        extra,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rustllmroute_types::{FunctionCall, FunctionDef, Tool, ToolCall};
    use secrecy::SecretString;
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
            api_key: Some(SecretString::from("k")),
            api_version: None,
            extra_headers: Vec::new(),
            extra_body: Map::new(),
            drop_params: true,
            connect_timeout: Duration::from_secs(5),
            timeout: Duration::from_secs(60),
            stream_idle_timeout: Duration::from_secs(60),
            default_max_tokens: Some(2048),
            vertex: None,
            aws: None,
            token_source: None,
            aws_credentials: None,
        }
    }

    fn translate(
        req: &ChatRequest,
    ) -> (
        MessagesRequest,
        bool,
        std::collections::HashMap<String, String>,
    ) {
        let config = config();
        let mut cx = CallContext::new(&config, "claude-sonnet-4-5", req.stream, "req");
        let out = chat_to_messages(req, &mut cx).unwrap();
        (out, cx.json_tool_call, cx.tool_name_map)
    }

    #[test]
    fn system_merging_tool_calls_and_results() {
        let req = ChatRequest {
            model: "x".into(),
            messages: vec![
                ChatMessage::text(role::SYSTEM, "be brief"),
                ChatMessage::text(role::USER, "weather?"),
                ChatMessage {
                    role: role::ASSISTANT.into(),
                    tool_calls: Some(vec![ToolCall {
                        id: "call_1".into(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "get weather!".into(),
                            arguments: "{\"city\":\"Oslo\"}".into(),
                        },
                        extra: Extra::new(),
                    }]),
                    ..ChatMessage::default()
                },
                ChatMessage {
                    role: role::TOOL.into(),
                    tool_call_id: Some("call_1".into()),
                    content: Some(Content::Text("12C".into())),
                    ..ChatMessage::default()
                },
                ChatMessage::text(role::USER, "thanks"),
            ],
            tools: Some(vec![Tool {
                kind: "function".into(),
                function: Some(FunctionDef {
                    name: "get weather!".into(),
                    description: Some("d".into()),
                    parameters: None,
                    strict: None,
                    extra: Extra::new(),
                }),
                extra: Extra::new(),
            }]),
            max_tokens: Some(100),
            ..ChatRequest::default()
        };
        let (out, json_mode, names) = translate(&req);
        assert_eq!(out.model, "claude-sonnet-4-5");
        assert_eq!(out.max_tokens, Some(100));
        assert_eq!(
            out.system.as_ref().map(SystemPrompt::text).as_deref(),
            Some("be brief")
        );
        let roles: Vec<&str> = out.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(
            roles,
            vec!["user", "assistant", "user"],
            "tool result merges into the following user turn"
        );
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(v["messages"][1]["content"][0]["name"], "get_weather_");
        assert_eq!(v["messages"][1]["content"][0]["input"]["city"], "Oslo");
        assert_eq!(v["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(v["messages"][2]["content"][0]["tool_use_id"], "call_1");
        assert_eq!(v["messages"][2]["content"][1]["text"], "thanks");
        assert_eq!(v["tools"][0]["name"], "get_weather_");
        assert_eq!(v["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(
            names.get("get_weather_").map(String::as_str),
            Some("get weather!")
        );
        assert!(!json_mode);
    }

    #[test]
    fn response_format_becomes_a_forced_tool_and_thinking_drops_sampling_params() {
        let req = ChatRequest {
            model: "x".into(),
            messages: vec![ChatMessage::text(role::USER, "hi")],
            response_format: Some(
                json!({"type": "json_schema", "json_schema": {"name": "a", "schema": {"type": "object", "properties": {"k": {"type": "string"}}}}}),
            ),
            reasoning_effort: Some("high".into()),
            temperature: Some(0.2),
            ..ChatRequest::default()
        };
        let (out, json_mode, _) = translate(&req);
        assert!(json_mode);
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v["tools"][0]["name"], JSON_TOOL_NAME);
        assert_eq!(v["tool_choice"]["name"], JSON_TOOL_NAME);
        assert_eq!(v["thinking"]["budget_tokens"], 4096);
        assert_eq!(
            out.max_tokens,
            Some(4096 + 1024),
            "max_tokens is raised above the thinking budget"
        );
        assert!(v.get("temperature").is_none());
    }

    #[test]
    fn leading_assistant_gets_a_user_placeholder_and_images_become_sources() {
        let req = ChatRequest {
            model: "x".into(),
            messages: vec![
                ChatMessage::text(role::ASSISTANT, "hello"),
                ChatMessage {
                    role: role::USER.into(),
                    content: Some(Content::Parts(vec![
                        Recognized::Known(ContentPart::Text {
                            text: "look".into(),
                            extra: Extra::new(),
                        }),
                        Recognized::Known(ContentPart::ImageUrl {
                            image_url: rustllmroute_types::ImageUrl {
                                url: "data:image/png;base64,AAAA".into(),
                                detail: None,
                                extra: Extra::new(),
                            },
                            extra: Extra::new(),
                        }),
                        Recognized::Known(ContentPart::ImageUrl {
                            image_url: rustllmroute_types::ImageUrl {
                                url: "https://x/y.png".into(),
                                detail: None,
                                extra: Extra::new(),
                            },
                            extra: Extra::new(),
                        }),
                    ])),
                    ..ChatMessage::default()
                },
            ],
            ..ChatRequest::default()
        };
        let (out, _, _) = translate(&req);
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["messages"][0]["content"], ".");
        assert_eq!(v["messages"][2]["content"][1]["source"]["type"], "base64");
        assert_eq!(
            v["messages"][2]["content"][1]["source"]["media_type"],
            "image/png"
        );
        assert_eq!(v["messages"][2]["content"][2]["source"]["type"], "url");
        assert_eq!(out.max_tokens, Some(2048), "falls back to the catalog cap");
    }
}
