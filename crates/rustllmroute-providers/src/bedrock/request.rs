//! OpenAI chat request -> Bedrock Converse body.

use std::sync::LazyLock;

use regex::Regex;
use rustllmroute_types::{
    ChatMessage, ChatRequest, Content, ContentPart, GatewayError, Stop, ToolChoice, role,
};
use serde_json::{Map, Value, json};

use crate::anthropic::request::JSON_TOOL_NAME;
use crate::traits::CallContext;

static TOOL_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^a-zA-Z0-9_-]").expect("valid regex"));

/// Bedrock tool names must match `^[a-zA-Z0-9_-]{1,64}$`; the reverse map lives in the context.
pub fn sanitize_tool_name(name: &str, cx: &mut CallContext<'_>) -> String {
    let mut cleaned = TOOL_NAME.replace_all(name, "_").into_owned();
    if cleaned.is_empty() {
        cleaned = "tool".to_owned();
    }
    cleaned.truncate(64);
    if cleaned != name {
        cx.tool_name_map.insert(cleaned.clone(), name.to_owned());
    }
    cleaned
}

fn image_block(url: &str) -> Result<Value, GatewayError> {
    let Some(rest) = url.strip_prefix("data:") else {
        return Err(GatewayError::BadRequest {
            message: "bedrock only accepts images as base64 data URLs".to_owned(),
            param: Some("messages".to_owned()),
            code: None,
        });
    };
    let (meta, data) = rest.split_once(',').unwrap_or((rest, ""));
    let mime = meta.split(';').next().unwrap_or("image/jpeg");
    let format = match mime {
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "jpeg",
    };
    Ok(json!({"image": {"format": format, "source": {"bytes": data}}}))
}

fn text_blocks(content: &Content) -> Result<Vec<Value>, GatewayError> {
    match content {
        Content::Text(text) => Ok(if text.is_empty() {
            Vec::new()
        } else {
            vec![json!({"text": text})]
        }),
        Content::Parts(parts) => {
            let mut blocks = Vec::new();
            for part in parts {
                match part.known() {
                    Some(ContentPart::Text { text, .. }) if !text.is_empty() => {
                        blocks.push(json!({"text": text}));
                    }
                    Some(ContentPart::ImageUrl { image_url, .. }) => {
                        blocks.push(image_block(&image_url.url)?);
                    }
                    _ => {}
                }
            }
            Ok(blocks)
        }
    }
}

fn parse_args(arguments: &str) -> Value {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn reasoning_blocks(message: &ChatMessage) -> Vec<Value> {
    let mut blocks = Vec::new();
    if let Some(Value::Array(thinking)) = message.extra.get("thinking_blocks") {
        for block in thinking {
            match block.get("type").and_then(Value::as_str) {
                Some("thinking") => {
                    if let (Some(text), Some(signature)) = (
                        block.get("thinking").and_then(Value::as_str),
                        block.get("signature").and_then(Value::as_str),
                    ) {
                        blocks.push(json!({"reasoningContent": {"reasoningText": {"text": text, "signature": signature}}}));
                    }
                }
                Some("redacted_thinking") => {
                    if let Some(data) = block.get("data").and_then(Value::as_str) {
                        blocks.push(json!({"reasoningContent": {"redactedContent": data}}));
                    }
                }
                _ => {}
            }
        }
    }
    blocks
}

fn assistant_blocks(
    message: &ChatMessage,
    cx: &mut CallContext<'_>,
) -> Result<Vec<Value>, GatewayError> {
    let mut blocks = reasoning_blocks(message);
    if let Some(content) = &message.content {
        blocks.extend(text_blocks(content)?);
    }
    for call in message.tool_calls.iter().flatten() {
        blocks.push(json!({
            "toolUse": {
                "toolUseId": call.id,
                "name": sanitize_tool_name(&call.function.name, cx),
                "input": parse_args(&call.function.arguments),
            }
        }));
    }
    Ok(blocks)
}

fn tool_result_block(message: &ChatMessage) -> Value {
    let text = message
        .content
        .as_ref()
        .map(Content::joined_text)
        .unwrap_or_default();
    let content = match serde_json::from_str::<Value>(&text) {
        Ok(value @ Value::Object(_)) => json!([{"json": value}]),
        _ => json!([{"text": if text.is_empty() { " ".to_owned() } else { text }}]),
    };
    json!({"toolResult": {"toolUseId": message.tool_call_id.clone().unwrap_or_default(), "content": content}})
}

struct Turns {
    messages: Vec<Value>,
}

impl Turns {
    fn push(&mut self, role: &str, blocks: Vec<Value>) {
        if blocks.is_empty() {
            return;
        }
        if let Some(last) = self.messages.last_mut()
            && last.get("role").and_then(Value::as_str) == Some(role)
            && let Some(existing) = last.get_mut("content").and_then(Value::as_array_mut)
        {
            existing.extend(blocks);
            return;
        }
        self.messages.push(json!({"role": role, "content": blocks}));
    }
}

fn anthropic_budget(effort: &str) -> Option<u64> {
    match effort {
        "low" => Some(1024),
        "medium" => Some(2048),
        "high" => Some(4096),
        _ => None,
    }
}

pub fn is_anthropic_model(model: &str) -> bool {
    model.contains("anthropic.")
}

/// Builds the Converse body; `cx.json_tool_call` is set when `response_format` is emulated.
pub fn chat_to_converse(
    req: &ChatRequest,
    cx: &mut CallContext<'_>,
) -> Result<Value, GatewayError> {
    let mut system: Vec<Value> = Vec::new();
    let mut turns = Turns {
        messages: Vec::new(),
    };
    for message in &req.messages {
        match message.role.as_str() {
            role::SYSTEM | role::DEVELOPER => {
                if let Some(content) = &message.content {
                    system.extend(
                        text_blocks(content)?
                            .into_iter()
                            .filter(|b| b.get("text").is_some()),
                    );
                }
            }
            role::ASSISTANT => {
                let blocks = assistant_blocks(message, cx)?;
                turns.push("assistant", blocks);
            }
            role::TOOL | role::FUNCTION => turns.push("user", vec![tool_result_block(message)]),
            _ => {
                let blocks = message
                    .content
                    .as_ref()
                    .map(text_blocks)
                    .transpose()?
                    .unwrap_or_default();
                turns.push("user", blocks);
            }
        }
    }
    if turns.messages.is_empty() {
        return Err(GatewayError::BadRequest {
            message: "bedrock needs at least one user or assistant message".to_owned(),
            param: Some("messages".to_owned()),
            code: None,
        });
    }
    if turns.messages[0].get("role").and_then(Value::as_str) == Some("assistant") {
        turns
            .messages
            .insert(0, json!({"role": "user", "content": [{"text": "."}]}));
    }
    let mut body = Map::new();
    body.insert("messages".to_owned(), Value::Array(turns.messages));
    if !system.is_empty() {
        body.insert("system".to_owned(), Value::Array(system));
    }

    let mut inference = Map::new();
    let anthropic = is_anthropic_model(cx.model);
    let mut additional = Map::new();
    let mut thinking_enabled = false;
    if let Some(spec) = req.extra.get("thinking") {
        additional.insert("thinking".to_owned(), spec.clone());
        thinking_enabled = spec.get("type").and_then(Value::as_str) == Some("enabled");
    } else if let Some(effort) = req.reasoning_effort.as_deref() {
        if anthropic {
            if let Some(budget) = anthropic_budget(effort) {
                additional.insert(
                    "thinking".to_owned(),
                    json!({"type": "enabled", "budget_tokens": budget}),
                );
                thinking_enabled = true;
            }
        } else {
            additional.insert("reasoning_effort".to_owned(), json!(effort));
        }
    }
    if let Some(max) = req.output_token_cap() {
        let max = if thinking_enabled {
            let budget = additional
                .get("thinking")
                .and_then(|t| t.get("budget_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32;
            max.max(budget + 1)
        } else {
            max
        };
        inference.insert("maxTokens".to_owned(), Value::from(max));
    }
    if !thinking_enabled {
        if let Some(temperature) = req.temperature {
            inference.insert("temperature".to_owned(), Value::from(temperature));
        }
        if let Some(top_p) = req.top_p {
            inference.insert("topP".to_owned(), Value::from(top_p));
        }
    }
    if let Some(stop) = &req.stop {
        inference.insert(
            "stopSequences".to_owned(),
            match stop {
                Stop::One(one) => json!([one]),
                Stop::Many(many) => json!(many),
            },
        );
    }
    if !inference.is_empty() {
        body.insert("inferenceConfig".to_owned(), Value::Object(inference));
    }
    if anthropic && let Some(top_k) = req.extra.get("top_k") {
        additional.insert("top_k".to_owned(), top_k.clone());
    }
    if let Some(betas) = req.extra.get("anthropic_beta") {
        additional.insert("anthropic_beta".to_owned(), betas.clone());
    }

    let mut tools: Vec<Value> = Vec::new();
    for tool in req.tools.iter().flatten() {
        let Some(function) = &tool.function else {
            continue;
        };
        let schema = function
            .parameters
            .clone()
            .filter(|p| p.is_object())
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
        let mut spec = json!({"name": sanitize_tool_name(&function.name, cx), "inputSchema": {"json": schema}});
        if let Some(description) = &function.description {
            spec["description"] = json!(description);
        }
        tools.push(json!({"toolSpec": spec}));
    }
    let mut tool_choice = match &req.tool_choice {
        Some(ToolChoice::Mode(mode)) => match mode.as_str() {
            "none" => {
                tools.clear();
                None
            }
            "required" | "any" => Some(json!({"any": {}})),
            _ => Some(json!({"auto": {}})),
        },
        Some(ToolChoice::Named(named)) => named
            .pointer("/function/name")
            .or_else(|| named.get("name"))
            .and_then(Value::as_str)
            .map(|name| json!({"tool": {"name": sanitize_tool_name(name, cx)}})),
        None => None,
    };
    if let Some(format) = &req.response_format {
        let schema = match format.get("type").and_then(Value::as_str) {
            Some("json_schema") => format
                .pointer("/json_schema/schema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"})),
            Some("json_object") => json!({"type": "object"}),
            _ => Value::Null,
        };
        if !schema.is_null() {
            tools.push(json!({"toolSpec": {"name": JSON_TOOL_NAME, "description": "Respond with a JSON object matching the schema", "inputSchema": {"json": schema}}}));
            tool_choice = Some(json!({"tool": {"name": JSON_TOOL_NAME}}));
            cx.json_tool_call = true;
        }
    }
    if !tools.is_empty() {
        let mut config = json!({"tools": tools});
        if let Some(choice) = tool_choice {
            config["toolChoice"] = choice;
        }
        body.insert("toolConfig".to_owned(), config);
    }
    if !additional.is_empty() {
        body.insert(
            "additionalModelRequestFields".to_owned(),
            Value::Object(additional),
        );
    }
    for (key, value) in &cx.config.extra_body {
        body.insert(key.clone(), value.clone());
    }
    Ok(Value::Object(body))
}

#[cfg(test)]
mod tests {
    use rustllmroute_types::{FunctionCall, FunctionDef, Tool, ToolCall};
    use url::Url;

    use super::*;
    use crate::registry::ProviderKind;
    use crate::traits::ProviderConfig;

    fn config(model: &str) -> ProviderConfig {
        ProviderConfig::new(
            ProviderKind::Bedrock,
            model,
            Url::parse("https://bedrock-runtime.us-east-1.amazonaws.com").unwrap(),
        )
    }

    #[test]
    fn translates_history_tools_and_inference_config() {
        let request = ChatRequest {
            model: "x".into(),
            messages: vec![
                ChatMessage::text(role::SYSTEM, "be brief"),
                ChatMessage::text(role::USER, "weather?"),
                ChatMessage {
                    role: role::ASSISTANT.into(),
                    content: Some(Content::Text("checking".into())),
                    tool_calls: Some(vec![ToolCall {
                        id: "tooluse_1".into(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "get.weather".into(),
                            arguments: "{\"city\":\"Oslo\"}".into(),
                        },
                        extra: Default::default(),
                    }]),
                    ..ChatMessage::default()
                },
                ChatMessage {
                    role: role::TOOL.into(),
                    tool_call_id: Some("tooluse_1".into()),
                    content: Some(Content::Text("{\"temp\": 12}".into())),
                    ..ChatMessage::default()
                },
                ChatMessage::text(role::USER, "and tomorrow?"),
            ],
            tools: Some(vec![Tool {
                kind: "function".into(),
                function: Some(FunctionDef {
                    name: "get.weather".into(),
                    description: Some("d".into()),
                    parameters: Some(
                        json!({"type": "object", "properties": {"city": {"type": "string"}}}),
                    ),
                    strict: None,
                    extra: Default::default(),
                }),
                extra: Default::default(),
            }]),
            tool_choice: Some(ToolChoice::Mode("auto".into())),
            max_tokens: Some(100),
            temperature: Some(0.5),
            stop: Some(Stop::One("END".into())),
            ..ChatRequest::default()
        };
        let config = config("anthropic.claude-sonnet-4-5-20250929-v1:0");
        let mut cx = CallContext::new(
            &config,
            "anthropic.claude-sonnet-4-5-20250929-v1:0",
            false,
            "r",
        );
        let body = chat_to_converse(&request, &mut cx).unwrap();
        assert_eq!(body["system"][0]["text"], "be brief");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["content"][0]["text"], "checking");
        assert_eq!(messages[1]["content"][1]["toolUse"]["name"], "get_weather");
        assert_eq!(
            messages[1]["content"][1]["toolUse"]["input"]["city"],
            "Oslo"
        );
        assert_eq!(
            messages[2]["content"][0]["toolResult"]["toolUseId"],
            "tooluse_1"
        );
        assert_eq!(
            messages[2]["content"][0]["toolResult"]["content"][0]["json"]["temp"],
            12
        );
        assert_eq!(messages[2]["content"][1]["text"], "and tomorrow?");
        assert_eq!(
            body["toolConfig"]["tools"][0]["toolSpec"]["name"],
            "get_weather"
        );
        assert_eq!(body["toolConfig"]["toolChoice"]["auto"], json!({}));
        assert_eq!(body["inferenceConfig"]["maxTokens"], 100);
        assert_eq!(body["inferenceConfig"]["temperature"], 0.5);
        assert_eq!(body["inferenceConfig"]["stopSequences"][0], "END");
        assert_eq!(cx.tool_name_map["get_weather"], "get.weather");
    }

    #[test]
    fn reasoning_effort_and_json_mode_for_claude() {
        let request = ChatRequest {
            model: "x".into(),
            messages: vec![ChatMessage::text(role::USER, "hi")],
            reasoning_effort: Some("medium".into()),
            temperature: Some(0.1),
            max_tokens: Some(100),
            response_format: Some(
                json!({"type": "json_schema", "json_schema": {"schema": {"type": "object", "properties": {"a": {"type": "string"}}}}}),
            ),
            ..ChatRequest::default()
        };
        let config = config("us.anthropic.claude-sonnet-4-5-20250929-v1:0");
        let mut cx = CallContext::new(
            &config,
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            false,
            "r",
        );
        let body = chat_to_converse(&request, &mut cx).unwrap();
        assert_eq!(
            body["additionalModelRequestFields"]["thinking"]["budget_tokens"],
            2048
        );
        assert!(
            body["inferenceConfig"].get("temperature").is_none(),
            "temperature is dropped with thinking"
        );
        assert_eq!(body["inferenceConfig"]["maxTokens"], 2049);
        assert_eq!(
            body["toolConfig"]["toolChoice"]["tool"]["name"],
            JSON_TOOL_NAME
        );
        assert!(cx.json_tool_call);
    }
}
