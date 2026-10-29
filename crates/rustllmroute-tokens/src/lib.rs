//! Token counting with OpenAI's tiktoken encodings. Non-OpenAI models are approximated with
//! `cl100k_base`, as LiteLLM does; the counts drive routing pre-checks and rate-limit
//! reservations, not billing.

use rustllmroute_types::{
    ChatRequest, Content, ContentBlock, ContentPart, MessageContent, MessagesRequest, SystemPrompt,
};
use serde_json::Value;
use tiktoken_rs::CoreBPE;

const TOKENS_PER_MESSAGE: u64 = 3;
const TOKENS_PER_NAME: u64 = 1;
const REPLY_PRIMING: u64 = 3;
const IMAGE_TOKENS: u64 = 250;
const TOOLS_OVERHEAD: u64 = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    O200k,
    Cl100k,
}

/// `o200k_base` for GPT-4o, GPT-4.1, GPT-5 and the o-series; `cl100k_base` for everything else.
pub fn encoding_for(model: &str) -> Encoding {
    let name = model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase();
    let o200k = name.starts_with("gpt-4o")
        || name.starts_with("gpt-4.1")
        || name.starts_with("gpt-5")
        || name.starts_with("chatgpt-4o")
        || name.starts_with("gpt-oss")
        || name.starts_with("o1")
        || name.starts_with("o3")
        || name.starts_with("o4");
    if o200k {
        Encoding::O200k
    } else {
        Encoding::Cl100k
    }
}

fn bpe(encoding: Encoding) -> &'static CoreBPE {
    match encoding {
        Encoding::O200k => tiktoken_rs::o200k_base_singleton(),
        Encoding::Cl100k => tiktoken_rs::cl100k_base_singleton(),
    }
}

/// Builds both encoders (hundreds of milliseconds for `o200k`); call once at startup off the
/// request path.
pub fn warm() {
    let _ = bpe(Encoding::O200k);
    let _ = bpe(Encoding::Cl100k);
}

pub fn count_text(model: &str, text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    bpe(encoding_for(model)).encode_ordinary(text).len() as u64
}

fn count_with(bpe: &CoreBPE, text: &str) -> u64 {
    if text.is_empty() {
        0
    } else {
        bpe.encode_ordinary(text).len() as u64
    }
}

fn count_content(bpe: &CoreBPE, content: &Content) -> u64 {
    match content {
        Content::Text(text) => count_with(bpe, text),
        Content::Parts(parts) => parts
            .iter()
            .map(|part| match part.known() {
                Some(ContentPart::Text { text, .. }) => count_with(bpe, text),
                Some(ContentPart::ImageUrl { .. }) => IMAGE_TOKENS,
                _ => 0,
            })
            .sum(),
    }
}

/// Renders tools the way OpenAI's prompt does (a TypeScript-like namespace) and counts that.
fn count_tools(bpe: &CoreBPE, tools: &[Value]) -> u64 {
    if tools.is_empty() {
        return 0;
    }
    let mut rendered = String::from("namespace functions {\n");
    for tool in tools {
        let function = tool.get("function").unwrap_or(tool);
        if let Some(description) = function.get("description").and_then(Value::as_str) {
            rendered.push_str("// ");
            rendered.push_str(description);
            rendered.push('\n');
        }
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("tool");
        let parameters = function
            .get("parameters")
            .or_else(|| function.get("input_schema"))
            .and_then(|p| p.get("properties"))
            .map(|p| p.to_string())
            .unwrap_or_default();
        rendered.push_str(&format!("type {name} = (_: {parameters}) => any;\n"));
    }
    rendered.push_str("} // namespace functions");
    count_with(bpe, &rendered) + TOOLS_OVERHEAD
}

/// Prompt tokens for an OpenAI chat request, following OpenAI's per-message accounting.
pub fn count_chat(model: &str, req: &ChatRequest) -> u64 {
    let bpe = bpe(encoding_for(model));
    let mut total = REPLY_PRIMING;
    for message in &req.messages {
        total += TOKENS_PER_MESSAGE;
        total += count_with(bpe, &message.role);
        if let Some(content) = &message.content {
            total += count_content(bpe, content);
        }
        if let Some(name) = &message.name {
            total += TOKENS_PER_NAME + count_with(bpe, name);
        }
        for call in message.tool_calls.iter().flatten() {
            total +=
                count_with(bpe, &call.function.name) + count_with(bpe, &call.function.arguments);
        }
        if let Some(reasoning) = &message.reasoning_content {
            total += count_with(bpe, reasoning);
        }
    }
    if let Some(tools) = &req.tools {
        let values: Vec<Value> = tools
            .iter()
            .filter_map(|tool| serde_json::to_value(tool).ok())
            .collect();
        total += count_tools(bpe, &values);
    }
    if let Some(format) = &req.response_format
        && let Some(schema) = format.pointer("/json_schema/schema")
    {
        total += count_with(bpe, &schema.to_string());
    }
    total
}

/// Prompt tokens for an Anthropic Messages request (same accounting, Anthropic block types).
pub fn count_messages(model: &str, req: &MessagesRequest) -> u64 {
    let bpe = bpe(encoding_for(model));
    let mut total = REPLY_PRIMING;
    match &req.system {
        Some(SystemPrompt::Text(text)) => total += TOKENS_PER_MESSAGE + count_with(bpe, text),
        Some(SystemPrompt::Blocks(blocks)) => {
            total += TOKENS_PER_MESSAGE;
            for block in blocks {
                if let Some(ContentBlock::Text { text, .. }) = block.known() {
                    total += count_with(bpe, text);
                }
            }
        }
        None => {}
    }
    for message in &req.messages {
        total += TOKENS_PER_MESSAGE + count_with(bpe, &message.role);
        match &message.content {
            MessageContent::Text(text) => total += count_with(bpe, text),
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    total += match block.known() {
                        Some(ContentBlock::Text { text, .. }) => count_with(bpe, text),
                        Some(ContentBlock::Image { .. }) => IMAGE_TOKENS,
                        Some(ContentBlock::ToolUse { name, input, .. }) => {
                            count_with(bpe, name) + count_with(bpe, &input.to_string())
                        }
                        Some(ContentBlock::ToolResult { content, .. }) => content
                            .as_ref()
                            .map(|c| match c {
                                Value::String(text) => count_with(bpe, text),
                                other => count_with(bpe, &other.to_string()),
                            })
                            .unwrap_or(0),
                        Some(ContentBlock::Thinking { thinking, .. }) => count_with(bpe, thinking),
                        _ => 0,
                    };
                }
            }
        }
    }
    if let Some(tools) = &req.tools {
        total += count_tools(bpe, tools);
    }
    total
}

/// Rough byte size of a chat request's text, used to decide whether to count off-thread.
pub fn chat_text_bytes(req: &ChatRequest) -> usize {
    req.messages
        .iter()
        .map(|m| m.text_segments().map(str::len).sum::<usize>())
        .sum::<usize>()
        + req
            .tools
            .as_ref()
            .map(|tools| tools.len() * 256)
            .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use rustllmroute_types::{ChatMessage, FunctionDef, Tool, role};

    use super::*;

    #[test]
    fn encodings_follow_model_families() {
        assert_eq!(encoding_for("gpt-4o-mini"), Encoding::O200k);
        assert_eq!(encoding_for("openai/o3-mini"), Encoding::O200k);
        assert_eq!(encoding_for("gpt-5"), Encoding::O200k);
        assert_eq!(encoding_for("claude-sonnet-4-5"), Encoding::Cl100k);
        assert_eq!(encoding_for("gpt-4-turbo"), Encoding::Cl100k);
    }

    #[test]
    fn counts_text_messages_and_tools() {
        assert_eq!(count_text("gpt-4o", "hello world"), 2);
        assert_eq!(count_text("claude", ""), 0);
        let plain = ChatRequest {
            model: "gpt-4o".into(),
            messages: vec![ChatMessage::text(role::USER, "hello world")],
            ..ChatRequest::default()
        };
        let plain_count = count_chat("gpt-4o", &plain);
        assert_eq!(plain_count, REPLY_PRIMING + TOKENS_PER_MESSAGE + 1 + 2);
        let with_tools = ChatRequest {
            tools: Some(vec![Tool {
                kind: "function".into(),
                function: Some(FunctionDef {
                    name: "get_weather".into(),
                    description: Some("Weather lookup".into()),
                    parameters: Some(
                        serde_json::json!({"type": "object", "properties": {"city": {"type": "string"}}}),
                    ),
                    strict: None,
                    extra: Default::default(),
                }),
                extra: Default::default(),
            }]),
            ..plain.clone()
        };
        assert!(count_chat("gpt-4o", &with_tools) > plain_count + TOOLS_OVERHEAD);
        let messages: MessagesRequest = serde_json::from_str(
            r#"{"model":"claude","system":"be brief","messages":[{"role":"user","content":[{"type":"text","text":"hello world"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA"}}]}],"max_tokens":5}"#,
        )
        .unwrap();
        let count = count_messages("claude", &messages);
        assert!(count > IMAGE_TOKENS + 2, "{count}");
        assert!(chat_text_bytes(&with_tools) > 11);
    }
}
