//! OpenAI Responses API: response-id encoding (so `previous_response_id` can find the deployment
//! that produced it) and the chat-completions bridge for providers without a native endpoint.

use base64::Engine;
use rustllmroute_types::{
    AssistantMessage, ChatMessage, ChatRequest, ChatResponse, Content, ContentPart, Extra,
    FinishReason, FunctionCall, FunctionDef, GatewayError, ImageUrl, Recognized, ResponsesRequest,
    Stop, Tool, ToolCall, ToolChoice, Usage, role, usage_to_responses,
};
use serde_json::{Map, Value, json};

const ID_PREFIX: &str = "resp_";

/// `resp_` + base64url(`{"id": upstream, "d": deployment}`): opaque to clients, decodable by us.
pub fn encode_response_id(upstream_id: &str, deployment_id: &str) -> String {
    let payload = json!({"id": upstream_id, "d": deployment_id}).to_string();
    format!(
        "{ID_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
    )
}

/// `(upstream_id, deployment_id)` when the id was minted by [`encode_response_id`].
pub fn decode_response_id(id: &str) -> Option<(String, String)> {
    let encoded = id.strip_prefix(ID_PREFIX)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    Some((
        value.get("id")?.as_str()?.to_owned(),
        value.get("d")?.as_str()?.to_owned(),
    ))
}

pub fn new_item_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::now_v7().simple())
}

fn text_of(content: &Value) -> Option<Content> {
    match content {
        Value::String(text) => Some(Content::Text(text.clone())),
        Value::Array(parts) => {
            let mut out: Vec<Recognized<ContentPart>> = Vec::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("input_text" | "output_text" | "text") => {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            out.push(Recognized::Known(ContentPart::Text {
                                text: text.to_owned(),
                                extra: Extra::new(),
                            }));
                        }
                    }
                    Some("input_image") => {
                        let url = match part.get("image_url") {
                            Some(Value::String(url)) => Some(url.clone()),
                            Some(object) => {
                                object.get("url").and_then(Value::as_str).map(str::to_owned)
                            }
                            None => None,
                        };
                        if let Some(url) = url {
                            out.push(Recognized::Known(ContentPart::ImageUrl {
                                image_url: ImageUrl {
                                    url,
                                    detail: part
                                        .get("detail")
                                        .and_then(Value::as_str)
                                        .map(str::to_owned),
                                    extra: Extra::new(),
                                },
                                extra: Extra::new(),
                            }));
                        }
                    }
                    _ => {}
                }
            }
            match out.as_slice() {
                [] => None,
                [Recognized::Known(ContentPart::Text { text, .. })] => {
                    Some(Content::Text(text.clone()))
                }
                _ => Some(Content::Parts(out)),
            }
        }
        _ => None,
    }
}

fn push_tool_call(messages: &mut Vec<ChatMessage>, call: ToolCall) {
    if let Some(last) = messages.last_mut()
        && last.role == role::ASSISTANT
        && last.content.is_none()
        && let Some(calls) = last.tool_calls.as_mut()
    {
        calls.push(call);
        return;
    }
    messages.push(ChatMessage {
        role: role::ASSISTANT.to_owned(),
        tool_calls: Some(vec![call]),
        ..ChatMessage::default()
    });
}

/// Responses request -> chat request for deployments without a native `/responses`.
pub fn responses_to_chat_request(
    req: &ResponsesRequest,
    model: &str,
) -> Result<ChatRequest, GatewayError> {
    let mut messages: Vec<ChatMessage> = Vec::new();
    if let Some(instructions) = req.instructions.as_deref().filter(|i| !i.is_empty()) {
        messages.push(ChatMessage::text(role::SYSTEM, instructions));
    }
    match &req.input {
        Value::String(text) => messages.push(ChatMessage::text(role::USER, text.clone())),
        Value::Array(items) => {
            for item in items {
                let kind = item.get("type").and_then(Value::as_str);
                match kind {
                    Some("function_call") => push_tool_call(
                        &mut messages,
                        ToolCall {
                            id: item
                                .get("call_id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            kind: "function".to_owned(),
                            function: FunctionCall {
                                name: item
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_owned(),
                                arguments: item
                                    .get("arguments")
                                    .and_then(Value::as_str)
                                    .unwrap_or("{}")
                                    .to_owned(),
                            },
                            extra: Extra::new(),
                        },
                    ),
                    Some("function_call_output") => {
                        let output = match item.get("output") {
                            Some(Value::String(text)) => text.clone(),
                            Some(other) => other.to_string(),
                            None => String::new(),
                        };
                        messages.push(ChatMessage {
                            role: role::TOOL.to_owned(),
                            tool_call_id: item
                                .get("call_id")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            content: Some(Content::Text(output)),
                            ..ChatMessage::default()
                        });
                    }
                    Some("message") | None => {
                        let item_role = item
                            .get("role")
                            .and_then(Value::as_str)
                            .unwrap_or(role::USER);
                        let chat_role = match item_role {
                            "developer" | "system" => role::SYSTEM,
                            "assistant" => role::ASSISTANT,
                            _ => role::USER,
                        };
                        if let Some(content) = item.get("content").and_then(text_of) {
                            messages.push(ChatMessage {
                                role: chat_role.to_owned(),
                                content: Some(content),
                                ..ChatMessage::default()
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        Value::Null => {}
        _ => {
            return Err(GatewayError::BadRequest {
                message: "input must be a string or an array of items".to_owned(),
                param: Some("input".to_owned()),
                code: None,
            });
        }
    }
    if !messages.iter().any(|m| m.role != role::SYSTEM) {
        return Err(GatewayError::BadRequest {
            message: "input must contain at least one message".to_owned(),
            param: Some("input".to_owned()),
            code: None,
        });
    }
    let tools: Vec<Tool> = req
        .tools
        .iter()
        .flatten()
        .filter(|tool| tool.get("type").and_then(Value::as_str) == Some("function"))
        .filter_map(|tool| {
            Some(Tool {
                kind: "function".to_owned(),
                function: Some(FunctionDef {
                    name: tool.get("name")?.as_str()?.to_owned(),
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    parameters: tool.get("parameters").cloned(),
                    strict: tool.get("strict").and_then(Value::as_bool),
                    extra: Extra::new(),
                }),
                extra: Extra::new(),
            })
        })
        .collect();
    let tool_choice = req.tool_choice.as_ref().and_then(|choice| match choice {
        Value::String(mode) => Some(ToolChoice::Mode(mode.clone())),
        Value::Object(object) => object
            .get("name")
            .and_then(Value::as_str)
            .map(|name| ToolChoice::Named(json!({"type": "function", "function": {"name": name}}))),
        _ => None,
    });
    let response_format = req
        .text
        .as_ref()
        .and_then(|text| text.get("format"))
        .and_then(|format| match format.get("type").and_then(Value::as_str)? {
            "json_schema" => Some(json!({
                "type": "json_schema",
                "json_schema": {
                    "name": format.get("name").cloned().unwrap_or_else(|| json!("response")),
                    "schema": format.get("schema").cloned().unwrap_or_else(|| json!({"type": "object"})),
                    "strict": format.get("strict").cloned().unwrap_or(Value::Bool(false)),
                }
            })),
            "json_object" => Some(json!({"type": "json_object"})),
            _ => None,
        });
    let mut extra = Extra::new();
    if let Some(stop) = req.extra.get("stop") {
        extra.insert("stop".to_owned(), stop.clone());
    }
    let stop = extra
        .remove("stop")
        .and_then(|stop| serde_json::from_value::<Stop>(stop).ok());
    Ok(ChatRequest {
        model: model.to_owned(),
        messages,
        stream: req.stream,
        stream_options: None,
        tools: (!tools.is_empty()).then_some(tools),
        tool_choice,
        parallel_tool_calls: req.parallel_tool_calls,
        max_tokens: req.max_output_tokens,
        max_completion_tokens: None,
        temperature: req.temperature,
        top_p: req.top_p,
        n: None,
        stop,
        seed: None,
        user: req.user.clone(),
        response_format,
        reasoning_effort: req
            .reasoning
            .as_ref()
            .and_then(|r| r.get("effort"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        logprobs: None,
        top_logprobs: None,
        metadata: req.metadata.clone(),
        extra,
    })
}

/// Output items for one assistant message: reasoning, message, function calls.
pub fn output_items(message: &AssistantMessage) -> Vec<Value> {
    let mut items = Vec::new();
    if let Some(reasoning) = message
        .reasoning_content
        .as_deref()
        .filter(|r| !r.is_empty())
    {
        items.push(json!({
            "type": "reasoning",
            "id": new_item_id("rs"),
            "summary": [{"type": "summary_text", "text": reasoning}],
        }));
    }
    let has_calls = message.tool_calls.as_ref().is_some_and(|c| !c.is_empty());
    if let Some(text) = message
        .content
        .as_deref()
        .filter(|t| !t.is_empty() || !has_calls)
    {
        items.push(message_item(&new_item_id("msg"), text, "completed"));
    }
    for call in message.tool_calls.iter().flatten() {
        items.push(function_call_item(
            &new_item_id("fc"),
            &call.id,
            &call.function.name,
            &call.function.arguments,
            "completed",
        ));
    }
    items
}

pub fn message_item(id: &str, text: &str, status: &str) -> Value {
    json!({
        "type": "message",
        "id": id,
        "status": status,
        "role": "assistant",
        "content": [{"type": "output_text", "text": text, "annotations": []}],
    })
}

pub fn function_call_item(
    id: &str,
    call_id: &str,
    name: &str,
    arguments: &str,
    status: &str,
) -> Value {
    json!({
        "type": "function_call",
        "id": id,
        "call_id": call_id,
        "name": name,
        "arguments": arguments,
        "status": status,
    })
}

/// The full response object, echoing the request parameters the way OpenAI does.
#[allow(clippy::too_many_arguments)]
pub fn response_object(
    id: &str,
    model: &str,
    created_at: u64,
    status: &str,
    output: Vec<Value>,
    usage: Option<&Usage>,
    req: &ResponsesRequest,
    incomplete_reason: Option<&str>,
) -> Value {
    let mut object = Map::new();
    object.insert("id".to_owned(), json!(id));
    object.insert("object".to_owned(), json!("response"));
    object.insert("created_at".to_owned(), json!(created_at));
    object.insert("status".to_owned(), json!(status));
    object.insert("error".to_owned(), Value::Null);
    object.insert(
        "incomplete_details".to_owned(),
        incomplete_reason.map_or(Value::Null, |reason| json!({"reason": reason})),
    );
    object.insert("instructions".to_owned(), json!(req.instructions));
    object.insert("max_output_tokens".to_owned(), json!(req.max_output_tokens));
    object.insert("model".to_owned(), json!(model));
    object.insert("output".to_owned(), Value::Array(output));
    object.insert(
        "parallel_tool_calls".to_owned(),
        json!(req.parallel_tool_calls.unwrap_or(true)),
    );
    object.insert(
        "previous_response_id".to_owned(),
        json!(req.previous_response_id),
    );
    object.insert(
        "reasoning".to_owned(),
        req.reasoning
            .clone()
            .unwrap_or_else(|| json!({"effort": null, "summary": null})),
    );
    object.insert("store".to_owned(), json!(req.store.unwrap_or(true)));
    object.insert(
        "temperature".to_owned(),
        json!(req.temperature.unwrap_or(1.0)),
    );
    object.insert(
        "text".to_owned(),
        req.text
            .clone()
            .unwrap_or_else(|| json!({"format": {"type": "text"}})),
    );
    object.insert(
        "tool_choice".to_owned(),
        req.tool_choice.clone().unwrap_or_else(|| json!("auto")),
    );
    object.insert(
        "tools".to_owned(),
        json!(req.tools.clone().unwrap_or_default()),
    );
    object.insert("top_p".to_owned(), json!(req.top_p.unwrap_or(1.0)));
    object.insert(
        "truncation".to_owned(),
        json!(
            req.truncation
                .clone()
                .unwrap_or_else(|| "disabled".to_owned())
        ),
    );
    object.insert(
        "usage".to_owned(),
        usage.map_or(Value::Null, usage_to_responses),
    );
    object.insert("user".to_owned(), json!(req.user));
    object.insert(
        "metadata".to_owned(),
        req.metadata.clone().unwrap_or_else(|| json!({})),
    );
    Value::Object(object)
}

/// Chat response -> Responses object (bridge, non-streaming).
pub fn chat_to_responses_response(
    response: &ChatResponse,
    req: &ResponsesRequest,
    model: &str,
    id: &str,
) -> Value {
    let (output, finish) = response
        .choices
        .first()
        .map(|choice| (output_items(&choice.message), choice.finish_reason))
        .unwrap_or_default();
    let incomplete = matches!(finish, Some(FinishReason::Length)).then_some("max_output_tokens");
    response_object(
        id,
        model,
        response.created,
        if incomplete.is_some() {
            "incomplete"
        } else {
            "completed"
        },
        output,
        response.usage.as_ref(),
        req,
        incomplete,
    )
}

/// Replays a complete Responses object as the streaming event sequence (used when post-call
/// guardrails forced a buffered upstream call for a streaming client).
pub fn replay_as_events(response: &Value) -> Vec<bytes::Bytes> {
    let mut sequence: u64 = 0;
    let mut frames = Vec::new();
    let mut event = |kind: &str, mut payload: Value| {
        sequence += 1;
        if let Some(object) = payload.as_object_mut() {
            object.insert("type".to_owned(), json!(kind));
            object.insert("sequence_number".to_owned(), json!(sequence));
        }
        frames.push(bytes::Bytes::from(format!(
            "event: {kind}\ndata: {payload}\n\n"
        )));
    };
    let mut in_progress = response.clone();
    if let Some(object) = in_progress.as_object_mut() {
        object.insert("status".to_owned(), json!("in_progress"));
        object.insert("output".to_owned(), json!([]));
        object.insert("usage".to_owned(), Value::Null);
    }
    event("response.created", json!({"response": in_progress}));
    event("response.in_progress", json!({"response": in_progress}));
    for (index, item) in response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let item_id = item
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        event(
            "response.output_item.added",
            json!({"output_index": index, "item": item}),
        );
        if item.get("type").and_then(Value::as_str) == Some("message") {
            for (content_index, part) in item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
            {
                let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                event(
                    "response.content_part.added",
                    json!({"item_id": item_id, "output_index": index, "content_index": content_index, "part": {"type": "output_text", "text": "", "annotations": []}}),
                );
                event(
                    "response.output_text.delta",
                    json!({"item_id": item_id, "output_index": index, "content_index": content_index, "delta": text}),
                );
                event(
                    "response.output_text.done",
                    json!({"item_id": item_id, "output_index": index, "content_index": content_index, "text": text}),
                );
                event(
                    "response.content_part.done",
                    json!({"item_id": item_id, "output_index": index, "content_index": content_index, "part": part}),
                );
            }
        }
        if item.get("type").and_then(Value::as_str) == Some("function_call") {
            let arguments = item.get("arguments").and_then(Value::as_str).unwrap_or("");
            event(
                "response.function_call_arguments.delta",
                json!({"item_id": item_id, "output_index": index, "delta": arguments}),
            );
            event(
                "response.function_call_arguments.done",
                json!({"item_id": item_id, "output_index": index, "arguments": arguments}),
            );
        }
        event(
            "response.output_item.done",
            json!({"output_index": index, "item": item}),
        );
    }
    let kind = if response.get("status").and_then(Value::as_str) == Some("incomplete") {
        "response.incomplete"
    } else {
        "response.completed"
    };
    event(kind, json!({"response": response}));
    frames
}

#[cfg(test)]
mod tests {
    use rustllmroute_types::{Choice, FinishReason};

    use super::*;

    #[test]
    fn response_ids_round_trip() {
        let encoded = encode_response_id("resp_upstream123", "dep-1");
        assert!(encoded.starts_with("resp_"));
        assert_eq!(
            decode_response_id(&encoded),
            Some(("resp_upstream123".to_owned(), "dep-1".to_owned()))
        );
        assert_eq!(decode_response_id("resp_plain"), None);
    }

    #[test]
    fn responses_request_maps_to_chat() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "m",
            "instructions": "be brief",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "weather?"}, {"type": "input_image", "image_url": "data:image/png;base64,AA"}]},
                {"type": "function_call", "call_id": "call_1", "name": "get_weather", "arguments": "{\"city\":\"Oslo\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "12C"},
                {"type": "message", "role": "user", "content": "thanks"}
            ],
            "tools": [{"type": "function", "name": "get_weather", "parameters": {"type": "object"}}, {"type": "web_search_preview"}],
            "tool_choice": {"type": "function", "name": "get_weather"},
            "text": {"format": {"type": "json_schema", "name": "out", "schema": {"type": "object"}}},
            "reasoning": {"effort": "low"},
            "max_output_tokens": 50,
            "stream": true
        }))
        .unwrap();
        let chat = responses_to_chat_request(&req, "gpt").unwrap();
        let roles: Vec<&str> = chat.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["system", "user", "assistant", "tool", "user"]);
        assert!(matches!(chat.messages[1].content, Some(Content::Parts(_))));
        assert_eq!(
            chat.messages[2].tool_calls.as_ref().unwrap()[0].id,
            "call_1"
        );
        assert_eq!(chat.messages[3].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(chat.tools.as_ref().unwrap().len(), 1);
        assert!(matches!(chat.tool_choice, Some(ToolChoice::Named(_))));
        assert_eq!(
            chat.response_format.as_ref().unwrap()["type"],
            "json_schema"
        );
        assert_eq!(chat.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(chat.max_tokens, Some(50));
        assert!(chat.stream);
    }

    #[test]
    fn chat_response_maps_to_response_object() {
        let response = ChatResponse {
            id: "chatcmpl-1".into(),
            object: "chat.completion".into(),
            created: 7,
            model: "gpt".into(),
            choices: vec![Choice {
                index: 0,
                message: AssistantMessage {
                    content: Some("Checking".into()),
                    tool_calls: Some(vec![ToolCall {
                        id: "call_1".into(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "get_weather".into(),
                            arguments: "{}".into(),
                        },
                        extra: Extra::new(),
                    }]),
                    reasoning_content: Some("hmm".into()),
                    ..AssistantMessage::default()
                },
                finish_reason: Some(FinishReason::ToolCalls),
                logprobs: None,
                extra: Extra::new(),
            }],
            usage: Some(Usage::new(5, 3)),
            system_fingerprint: None,
            extra: Extra::new(),
        };
        let req = ResponsesRequest {
            model: "m".into(),
            ..ResponsesRequest::default()
        };
        let object = chat_to_responses_response(&response, &req, "alias", "resp_x");
        assert_eq!(object["id"], "resp_x");
        assert_eq!(object["object"], "response");
        assert_eq!(object["status"], "completed");
        assert_eq!(object["model"], "alias");
        assert_eq!(object["output"][0]["type"], "reasoning");
        assert_eq!(object["output"][1]["type"], "message");
        assert_eq!(object["output"][1]["content"][0]["text"], "Checking");
        assert_eq!(object["output"][2]["type"], "function_call");
        assert_eq!(object["output"][2]["call_id"], "call_1");
        assert_eq!(object["usage"]["input_tokens"], 5);
        assert_eq!(object["usage"]["total_tokens"], 8);
    }
}
