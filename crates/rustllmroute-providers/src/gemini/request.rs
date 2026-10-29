//! OpenAI chat request -> Gemini `generateContent` body.

use std::collections::HashMap;

use rustllmroute_types::{
    ChatMessage, ChatRequest, Content, ContentPart, GatewayError, Stop, ToolChoice, role,
};
use serde_json::{Map, Value, json};

use super::schema::clean_schema;
use crate::traits::CallContext;

/// `reasoning_effort` -> Gemini thinking budget (LiteLLM's mapping).
pub fn thinking_budget(effort: &str) -> Option<u64> {
    match effort {
        "none" => Some(0),
        "minimal" => Some(128),
        "low" => Some(1024),
        "medium" => Some(2048),
        "high" => Some(4096),
        "xhigh" | "max" => Some(8192),
        _ => None,
    }
}

fn mime_from_url(url: &str) -> &'static str {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    match path.rsplit('.').next() {
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("heic") => "image/heic",
        Some("pdf") => "application/pdf",
        Some("mp4") => "video/mp4",
        Some("mp3") => "audio/mp3",
        Some("wav") => "audio/wav",
        Some("txt") => "text/plain",
        _ => "image/jpeg",
    }
}

fn media_part(url: &str) -> Value {
    if let Some(rest) = url.strip_prefix("data:")
        && let Some((meta, data)) = rest.split_once(',')
    {
        let mime = meta.split(';').next().unwrap_or("image/jpeg");
        return json!({"inlineData": {"mimeType": mime, "data": data}});
    }
    json!({"fileData": {"mimeType": mime_from_url(url), "fileUri": url}})
}

fn content_parts(content: &Content) -> Vec<Value> {
    match content {
        Content::Text(text) => {
            if text.is_empty() {
                Vec::new()
            } else {
                vec![json!({"text": text})]
            }
        }
        Content::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part.known()? {
                ContentPart::Text { text, .. } if !text.is_empty() => Some(json!({"text": text})),
                ContentPart::ImageUrl { image_url, .. } => Some(media_part(&image_url.url)),
                ContentPart::File { file, .. } => {
                    let data = file.get("file_data").and_then(Value::as_str)?;
                    Some(media_part(data))
                }
                _ => None,
            })
            .collect(),
    }
}

fn parse_args(arguments: &str) -> Value {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn tool_result_value(message: &ChatMessage) -> Value {
    let text = message
        .content
        .as_ref()
        .map(Content::joined_text)
        .unwrap_or_default();
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(object)) => Value::Object(object),
        Ok(other) => json!({"content": other}),
        Err(_) => json!({"content": text}),
    }
}

fn signature_of(message: &ChatMessage) -> Option<String> {
    message
        .extra
        .get("thinking_blocks")
        .and_then(Value::as_array)
        .and_then(|blocks| {
            blocks
                .iter()
                .find_map(|b| b.get("signature").and_then(Value::as_str))
        })
        .or_else(|| {
            message
                .extra
                .get("thought_signature")
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
}

fn assistant_parts(message: &ChatMessage) -> Vec<Value> {
    let mut parts = Vec::new();
    if let Some(content) = &message.content {
        parts.extend(content_parts(content));
    }
    for call in message.tool_calls.iter().flatten() {
        parts.push(json!({
            "functionCall": {"name": call.function.name, "args": parse_args(&call.function.arguments)}
        }));
    }
    if let Some(signature) = signature_of(message) {
        let position = parts
            .iter()
            .position(|part| part.get("functionCall").is_some())
            .unwrap_or(0);
        if let Some(object) = parts.get_mut(position).and_then(Value::as_object_mut) {
            object.insert("thoughtSignature".to_owned(), Value::String(signature));
        }
    }
    parts
}

struct Contents {
    turns: Vec<Value>,
}

impl Contents {
    fn push(&mut self, role: &str, parts: Vec<Value>) {
        if parts.is_empty() {
            return;
        }
        if let Some(last) = self.turns.last_mut()
            && last.get("role").and_then(Value::as_str) == Some(role)
            && let Some(existing) = last.get_mut("parts").and_then(Value::as_array_mut)
        {
            existing.extend(parts);
            return;
        }
        self.turns.push(json!({"role": role, "parts": parts}));
    }
}

fn tool_choice_config(choice: &ToolChoice) -> Option<Value> {
    match choice {
        ToolChoice::Mode(mode) => match mode.as_str() {
            "auto" => Some(json!({"mode": "AUTO"})),
            "required" | "any" => Some(json!({"mode": "ANY"})),
            "none" => Some(json!({"mode": "NONE"})),
            _ => None,
        },
        ToolChoice::Named(named) => named
            .pointer("/function/name")
            .or_else(|| named.get("name"))
            .and_then(Value::as_str)
            .map(|name| json!({"mode": "ANY", "allowedFunctionNames": [name]})),
    }
}

fn generation_config(req: &ChatRequest) -> Map<String, Value> {
    let mut config = Map::new();
    let mut set = |key: &str, value: Option<Value>| {
        if let Some(value) = value {
            config.insert(key.to_owned(), value);
        }
    };
    set("temperature", req.temperature.map(Value::from));
    set("topP", req.top_p.map(Value::from));
    set("topK", req.extra.get("top_k").cloned());
    set("maxOutputTokens", req.output_token_cap().map(Value::from));
    set("candidateCount", req.n.map(Value::from));
    set("seed", req.seed.map(Value::from));
    set(
        "stopSequences",
        req.stop.as_ref().map(|stop| match stop {
            Stop::One(one) => json!([one]),
            Stop::Many(many) => json!(many),
        }),
    );
    set(
        "presencePenalty",
        req.extra.get("presence_penalty").cloned(),
    );
    set(
        "frequencyPenalty",
        req.extra.get("frequency_penalty").cloned(),
    );
    if req.logprobs == Some(true) {
        set("responseLogprobs", Some(Value::Bool(true)));
        set("logprobs", req.top_logprobs.map(Value::from));
    }
    if let Some(format) = &req.response_format {
        match format.get("type").and_then(Value::as_str) {
            Some("json_object") => set("responseMimeType", Some(json!("application/json"))),
            Some("json_schema") => {
                set("responseMimeType", Some(json!("application/json")));
                if let Some(schema) = format.pointer("/json_schema/schema") {
                    set("responseSchema", Some(clean_schema(schema)));
                }
            }
            _ => {}
        }
    }
    let mut thinking = Map::new();
    if let Some(effort) = req.reasoning_effort.as_deref()
        && let Some(budget) = thinking_budget(effort)
    {
        thinking.insert("thinkingBudget".to_owned(), Value::from(budget));
        thinking.insert("includeThoughts".to_owned(), Value::Bool(budget > 0));
    }
    if let Some(spec) = req.extra.get("thinking").and_then(Value::as_object) {
        match spec.get("type").and_then(Value::as_str) {
            Some("disabled") => {
                thinking.insert("thinkingBudget".to_owned(), Value::from(0));
                thinking.remove("includeThoughts");
            }
            _ => {
                if let Some(budget) = spec.get("budget_tokens").and_then(Value::as_u64) {
                    thinking.insert("thinkingBudget".to_owned(), Value::from(budget));
                    thinking.insert("includeThoughts".to_owned(), Value::Bool(true));
                }
            }
        }
    }
    if let Some(spec) = req.extra.get("thinking_config").and_then(Value::as_object) {
        thinking.extend(spec.clone());
    }
    if !thinking.is_empty() {
        config.insert("thinkingConfig".to_owned(), Value::Object(thinking));
    }
    config
}

/// Builds the `generateContent` body. Tool names already satisfy Gemini's rules when they satisfy
/// OpenAI's, so no sanitising is needed.
pub fn chat_to_generate_content(
    req: &ChatRequest,
    cx: &mut CallContext<'_>,
) -> Result<Value, GatewayError> {
    let mut system: Vec<Value> = Vec::new();
    let mut contents = Contents { turns: Vec::new() };
    let mut call_names: HashMap<&str, &str> = HashMap::new();
    for message in &req.messages {
        for call in message.tool_calls.iter().flatten() {
            call_names.insert(call.id.as_str(), call.function.name.as_str());
        }
    }
    for message in &req.messages {
        match message.role.as_str() {
            role::SYSTEM | role::DEVELOPER => {
                if let Some(content) = &message.content {
                    system.extend(
                        content_parts(content)
                            .into_iter()
                            .filter(|part| part.get("text").is_some()),
                    );
                }
            }
            role::ASSISTANT => contents.push("model", assistant_parts(message)),
            role::TOOL | role::FUNCTION => {
                let id = message.tool_call_id.as_deref().unwrap_or_default();
                let name = call_names
                    .get(id)
                    .copied()
                    .or(message.name.as_deref())
                    .unwrap_or(id);
                contents.push(
                    "user",
                    vec![json!({"functionResponse": {"name": name, "response": tool_result_value(message)}})],
                );
            }
            _ => {
                let parts = message
                    .content
                    .as_ref()
                    .map(content_parts)
                    .unwrap_or_default();
                contents.push("user", parts);
            }
        }
    }
    if contents.turns.is_empty() {
        return Err(GatewayError::BadRequest {
            message: "gemini needs at least one user or assistant message".to_owned(),
            param: Some("messages".to_owned()),
            code: None,
        });
    }
    if contents.turns[0].get("role").and_then(Value::as_str) == Some("model") {
        contents
            .turns
            .insert(0, json!({"role": "user", "parts": [{"text": "."}]}));
    }
    let mut body = Map::new();
    body.insert("contents".to_owned(), Value::Array(contents.turns));
    if !system.is_empty() {
        body.insert("systemInstruction".to_owned(), json!({"parts": system}));
    }
    let mut declarations = Vec::new();
    let mut native_tools = Vec::new();
    for tool in req.tools.iter().flatten() {
        match (&tool.function, tool.kind.as_str()) {
            (Some(function), _) => {
                let mut declaration = json!({"name": function.name});
                if let Some(description) = &function.description {
                    declaration["description"] = json!(description);
                }
                if let Some(parameters) = &function.parameters {
                    let cleaned = clean_schema(parameters);
                    if cleaned.get("properties").is_some() || cleaned.get("anyOf").is_some() {
                        declaration["parameters"] = cleaned;
                    }
                }
                declarations.push(declaration);
            }
            (None, _) if !tool.extra.is_empty() => {
                native_tools.push(Value::Object(tool.extra.clone()));
            }
            _ => {}
        }
    }
    let suppress_tools = matches!(&req.tool_choice, Some(ToolChoice::Mode(mode)) if mode == "none");
    if !suppress_tools {
        let mut tools = Vec::new();
        if !declarations.is_empty() {
            tools.push(json!({"functionDeclarations": declarations}));
        }
        tools.extend(native_tools);
        if !tools.is_empty() {
            body.insert("tools".to_owned(), Value::Array(tools));
            if let Some(config) = req.tool_choice.as_ref().and_then(tool_choice_config) {
                body.insert(
                    "toolConfig".to_owned(),
                    json!({"functionCallingConfig": config}),
                );
            }
        }
    }
    let generation = generation_config(req);
    if !generation.is_empty() {
        body.insert("generationConfig".to_owned(), Value::Object(generation));
    }
    if let Some(safety) = req.extra.get("safety_settings") {
        body.insert("safetySettings".to_owned(), safety.clone());
    }
    if let Some(cached) = req.extra.get("cached_content") {
        body.insert("cachedContent".to_owned(), cached.clone());
    }
    for (key, value) in &cx.config.extra_body {
        body.insert(key.clone(), value.clone());
    }
    Ok(Value::Object(body))
}

#[cfg(test)]
mod tests {
    use rustllmroute_types::{FunctionCall, FunctionDef, Recognized, Tool, ToolCall};
    use url::Url;

    use super::*;
    use crate::registry::ProviderKind;
    use crate::traits::ProviderConfig;

    fn config() -> ProviderConfig {
        ProviderConfig::new(
            ProviderKind::Gemini,
            "gemini-2.5-pro",
            Url::parse("https://generativelanguage.googleapis.com").unwrap(),
        )
    }

    #[test]
    fn translates_tools_results_and_generation_config() {
        let request = ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![
                ChatMessage::text(role::SYSTEM, "be brief"),
                ChatMessage::text(role::USER, "weather in Oslo?"),
                ChatMessage {
                    role: role::ASSISTANT.into(),
                    content: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "call_1".into(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "get_weather".into(),
                            arguments: "{\"city\":\"Oslo\"}".into(),
                        },
                        extra: Default::default(),
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
                    name: "get_weather".into(),
                    description: Some("d".into()),
                    parameters: Some(
                        json!({"type": "object", "properties": {"city": {"type": "string"}}, "additionalProperties": false}),
                    ),
                    strict: Some(true),
                    extra: Default::default(),
                }),
                extra: Default::default(),
            }]),
            tool_choice: Some(ToolChoice::Mode("required".into())),
            max_tokens: Some(50),
            temperature: Some(0.2),
            reasoning_effort: Some("low".into()),
            response_format: Some(json!({"type": "json_object"})),
            ..ChatRequest::default()
        };
        let config = config();
        let mut cx = CallContext::new(&config, "gemini-2.5-pro", false, "r");
        let body = chat_to_generate_content(&request, &mut cx).unwrap();
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be brief");
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(
            contents.len(),
            3,
            "tool result merges into the following user turn"
        );
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(
            contents[1]["parts"][0]["functionCall"]["args"]["city"],
            "Oslo"
        );
        assert_eq!(
            contents[2]["parts"][0]["functionResponse"]["name"],
            "get_weather"
        );
        assert_eq!(
            contents[2]["parts"][0]["functionResponse"]["response"]["content"],
            "12C"
        );
        assert_eq!(contents[2]["parts"][1]["text"], "thanks");
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["name"],
            "get_weather"
        );
        assert!(
            body["tools"][0]["functionDeclarations"][0]["parameters"]
                .get("additionalProperties")
                .is_none()
        );
        assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 50);
        assert_eq!(body["generationConfig"]["temperature"], 0.2);
        assert_eq!(
            body["generationConfig"]["responseMimeType"],
            "application/json"
        );
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            1024
        );
    }

    #[test]
    fn data_urls_become_inline_data_and_assistant_first_gets_a_placeholder() {
        let request = ChatRequest {
            model: "m".into(),
            messages: vec![
                ChatMessage::text(role::ASSISTANT, "hello"),
                ChatMessage {
                    role: role::USER.into(),
                    content: Some(Content::Parts(vec![
                        Recognized::Known(ContentPart::Text {
                            text: "look".into(),
                            extra: Default::default(),
                        }),
                        Recognized::Known(ContentPart::ImageUrl {
                            image_url: rustllmroute_types::ImageUrl {
                                url: "data:image/png;base64,AAAA".into(),
                                detail: None,
                                extra: Default::default(),
                            },
                            extra: Default::default(),
                        }),
                    ])),
                    ..ChatMessage::default()
                },
            ],
            ..ChatRequest::default()
        };
        let config = config();
        let mut cx = CallContext::new(&config, "m", false, "r");
        let body = chat_to_generate_content(&request, &mut cx).unwrap();
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(
            contents[2]["parts"][1]["inlineData"]["mimeType"],
            "image/png"
        );
        assert_eq!(contents[2]["parts"][1]["inlineData"]["data"], "AAAA");
    }
}
