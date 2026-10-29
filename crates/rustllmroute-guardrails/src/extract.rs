//! Mutable text slots for each payload shape, so guardrails can read and rewrite in place.

use rustllmroute_types::{
    ChatRequest, ChatResponse, Content, ContentBlock, ContentPart, MessageContent, MessagesRequest,
    Recognized, ResponsesRequest, SystemPrompt,
};
use serde_json::Value;

use crate::ToolCallRef;

fn content_slots<'a>(content: &'a mut Content, out: &mut Vec<&'a mut String>) {
    match content {
        Content::Text(text) => out.push(text),
        Content::Parts(parts) => {
            for part in parts {
                if let Recognized::Known(ContentPart::Text { text, .. }) = part {
                    out.push(text);
                }
            }
        }
    }
}

/// Every text segment of every message (system, user, assistant, tool).
pub fn chat_request_texts(req: &mut ChatRequest) -> Vec<&mut String> {
    let mut out = Vec::new();
    for message in &mut req.messages {
        if let Some(content) = message.content.as_mut() {
            content_slots(content, &mut out);
        }
    }
    out
}

pub fn chat_response_texts(response: &mut ChatResponse) -> Vec<&mut String> {
    response
        .choices
        .iter_mut()
        .filter_map(|choice| choice.message.content.as_mut())
        .collect()
}

pub fn chat_response_tool_calls(response: &ChatResponse) -> Vec<ToolCallRef> {
    response
        .choices
        .iter()
        .flat_map(|choice| choice.message.tool_calls.iter().flatten())
        .map(|call| ToolCallRef {
            name: call.function.name.clone(),
            arguments: call.function.arguments.clone(),
        })
        .collect()
}

fn block_slots<'a>(blocks: &'a mut [Recognized<ContentBlock>], out: &mut Vec<&'a mut String>) {
    for block in blocks {
        match block {
            Recognized::Known(ContentBlock::Text { text, .. }) => out.push(text),
            Recognized::Known(ContentBlock::ToolResult {
                content: Some(content),
                ..
            }) => value_text_slots(content, out),
            _ => {}
        }
    }
}

/// Strings inside tool-result content: a bare string or `[{type: text, text}]` blocks.
fn value_text_slots<'a>(value: &'a mut Value, out: &mut Vec<&'a mut String>) {
    match value {
        Value::String(text) => out.push(text),
        Value::Array(items) => {
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(Value::String(text)) = item.get_mut("text")
                {
                    out.push(text);
                }
            }
        }
        _ => {}
    }
}

pub fn messages_request_texts(req: &mut MessagesRequest) -> Vec<&mut String> {
    let mut out = Vec::new();
    match req.system.as_mut() {
        Some(SystemPrompt::Text(text)) => out.push(text),
        Some(SystemPrompt::Blocks(blocks)) => block_slots(blocks, &mut out),
        None => {}
    }
    for message in &mut req.messages {
        match &mut message.content {
            MessageContent::Text(text) => out.push(text),
            MessageContent::Blocks(blocks) => block_slots(blocks, &mut out),
        }
    }
    out
}

/// Text blocks of an Anthropic Messages response (as JSON, since native replies are relayed).
pub fn messages_response_texts(value: &mut Value) -> Vec<&mut String> {
    let mut out = Vec::new();
    if let Some(Value::Array(blocks)) = value.get_mut("content") {
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("text")
                && let Some(Value::String(text)) = block.get_mut("text")
            {
                out.push(text);
            }
        }
    }
    out
}

pub fn messages_response_tool_calls(value: &Value) -> Vec<ToolCallRef> {
    value
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                .map(|b| ToolCallRef {
                    name: b
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    arguments: b.get("input").map(Value::to_string).unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn responses_content_slots<'a>(content: &'a mut Value, out: &mut Vec<&'a mut String>) {
    match content {
        Value::String(text) => out.push(text),
        Value::Array(parts) => {
            for part in parts {
                if matches!(
                    part.get("type").and_then(Value::as_str),
                    Some("input_text" | "output_text" | "text")
                ) && let Some(Value::String(text)) = part.get_mut("text")
                {
                    out.push(text);
                }
            }
        }
        _ => {}
    }
}

pub fn responses_request_texts(req: &mut ResponsesRequest) -> Vec<&mut String> {
    let mut out = Vec::new();
    if let Some(instructions) = req.instructions.as_mut() {
        out.push(instructions);
    }
    match &mut req.input {
        Value::String(text) => out.push(text),
        Value::Array(items) => {
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("function_call_output") => {
                        if let Some(Value::String(output)) = item.get_mut("output") {
                            out.push(output);
                        }
                    }
                    Some("message") | None => {
                        if let Some(content) = item.get_mut("content") {
                            responses_content_slots(content, &mut out);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

/// `output_text` parts of a Responses object's message items.
pub fn responses_output_texts(value: &mut Value) -> Vec<&mut String> {
    let mut out = Vec::new();
    if let Some(Value::Array(items)) = value.get_mut("output") {
        for item in items {
            if item.get("type").and_then(Value::as_str) == Some("message")
                && let Some(content) = item.get_mut("content")
            {
                responses_content_slots(content, &mut out);
            }
        }
    }
    out
}

pub fn responses_output_tool_calls(value: &Value) -> Vec<ToolCallRef> {
    value
        .get("output")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|i| i.get("type").and_then(Value::as_str) == Some("function_call"))
                .map(|i| ToolCallRef {
                    name: i
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    arguments: i
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use rustllmroute_types::{ChatMessage, role};
    use serde_json::json;

    use super::*;

    #[test]
    fn slots_write_back_into_every_shape() {
        let mut chat = ChatRequest {
            model: "m".into(),
            messages: vec![
                ChatMessage::text(role::SYSTEM, "sys"),
                ChatMessage {
                    role: role::USER.into(),
                    content: Some(Content::Parts(vec![
                        Recognized::Known(ContentPart::Text {
                            text: "a".into(),
                            extra: Default::default(),
                        }),
                        Recognized::Known(ContentPart::ImageUrl {
                            image_url: rustllmroute_types::ImageUrl {
                                url: "x".into(),
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
        let slots = chat_request_texts(&mut chat);
        assert_eq!(slots.len(), 2);
        for slot in slots {
            slot.push('!');
        }
        assert_eq!(
            chat.messages[0].text_segments().collect::<Vec<_>>(),
            vec!["sys!"]
        );
        assert_eq!(
            chat.messages[1].text_segments().collect::<Vec<_>>(),
            vec!["a!"]
        );

        let mut messages: MessagesRequest = serde_json::from_value(json!({
            "model": "m", "system": [{"type": "text", "text": "s"}],
            "messages": [
                {"role": "user", "content": "u"},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": [{"type": "text", "text": "r"}]}, {"type": "text", "text": "v"}]}
            ]
        }))
        .unwrap();
        let slots = messages_request_texts(&mut messages);
        assert_eq!(
            slots.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["s", "u", "r", "v"]
        );

        let mut responses: ResponsesRequest = serde_json::from_value(json!({
            "model": "m", "instructions": "i",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "q"}]}, {"type": "function_call_output", "call_id": "c", "output": "o"}]
        }))
        .unwrap();
        let slots = responses_request_texts(&mut responses);
        assert_eq!(
            slots.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["i", "q", "o"]
        );

        let mut output = json!({"output": [{"type": "message", "content": [{"type": "output_text", "text": "hello"}]}, {"type": "function_call", "name": "f", "arguments": "{}"}]});
        let slots = responses_output_texts(&mut output);
        assert_eq!(slots.len(), 1);
        assert_eq!(responses_output_tool_calls(&output)[0].name, "f");
        let mut anthropic = json!({"content": [{"type": "text", "text": "t"}, {"type": "tool_use", "name": "g", "input": {"a": 1}}]});
        assert_eq!(messages_response_texts(&mut anthropic).len(), 1);
        assert_eq!(
            messages_response_tool_calls(&anthropic)[0].arguments,
            "{\"a\":1}"
        );
    }
}
