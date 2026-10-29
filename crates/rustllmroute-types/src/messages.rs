//! Anthropic Messages API wire types. Fat on purpose: unknown fields and block types round-trip
//! so the gateway can relay bodies to Anthropic-compatible backends untouched.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Extra, FinishReason, PromptTokensDetails, Recognized, Usage, is_false};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    pub messages: Vec<AnthropicMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemPrompt>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SystemPrompt {
    Text(String),
    Blocks(Vec<Recognized<ContentBlock>>),
}

impl SystemPrompt {
    pub fn text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b.known()? {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: MessageContent,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<Recognized<ContentBlock>>),
}

impl MessageContent {
    pub fn into_blocks(self) -> Vec<Recognized<ContentBlock>> {
        match self {
            Self::Text(text) => vec![Recognized::Known(ContentBlock::text(text))],
            Self::Blocks(blocks) => blocks,
        }
    }

    pub fn text_segments(&self) -> Vec<&str> {
        match self {
            Self::Text(text) => vec![text.as_str()],
            Self::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b.known()? {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(flatten)]
        extra: Extra,
    },
    Image {
        source: Value,
        #[serde(flatten)]
        extra: Extra,
    },
    Document {
        source: Value,
        #[serde(flatten)]
        extra: Extra,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(flatten)]
        extra: Extra,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(flatten)]
        extra: Extra,
    },
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        #[serde(flatten)]
        extra: Extra,
    },
    RedactedThinking {
        data: String,
        #[serde(flatten)]
        extra: Extra,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            extra: Extra::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AnthropicUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl AnthropicUsage {
    /// Anthropic's `input_tokens` excludes cached tokens; the OpenAI convention includes them.
    pub fn to_openai(&self) -> Usage {
        let cache_read = self.cache_read_input_tokens.unwrap_or(0);
        let cache_creation = self.cache_creation_input_tokens.unwrap_or(0);
        let prompt = self.input_tokens + cache_read + cache_creation;
        let mut usage = Usage::new(prompt, self.output_tokens);
        if cache_read > 0 {
            usage.prompt_tokens_details = Some(PromptTokensDetails {
                cached_tokens: Some(cache_read),
                ..PromptTokensDetails::default()
            });
        }
        if cache_creation > 0 {
            usage.cache_creation_input_tokens = Some(cache_creation);
        }
        usage
    }

    pub fn from_openai(usage: &Usage) -> Self {
        let cache_read = usage.cached_tokens();
        let cache_creation = usage.cache_creation_tokens();
        Self {
            input_tokens: usage
                .prompt_tokens
                .saturating_sub(cache_read + cache_creation),
            output_tokens: usage.completion_tokens,
            cache_creation_input_tokens: Some(cache_creation),
            cache_read_input_tokens: Some(cache_read),
            extra: Extra::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessagesResponse {
    pub id: String,
    #[serde(rename = "type", default = "message_type")]
    pub kind: String,
    #[serde(default = "assistant")]
    pub role: String,
    #[serde(default)]
    pub content: Vec<Recognized<ContentBlock>>,
    pub model: String,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub stop_sequence: Option<String>,
    #[serde(default)]
    pub usage: AnthropicUsage,
    #[serde(flatten)]
    pub extra: Extra,
}

fn message_type() -> String {
    "message".to_owned()
}

fn assistant() -> String {
    "assistant".to_owned()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentDelta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
    ThinkingDelta { thinking: String },
    SignatureDelta { signature: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessagesStreamEvent {
    MessageStart {
        message: MessagesResponse,
    },
    ContentBlockStart {
        index: u32,
        content_block: Recognized<ContentBlock>,
    },
    ContentBlockDelta {
        index: u32,
        delta: Recognized<ContentDelta>,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        delta: Value,
        #[serde(default)]
        usage: Option<AnthropicUsage>,
    },
    MessageStop,
    Ping,
    Error {
        error: Value,
    },
}

pub fn stop_reason_to_finish(stop_reason: &str) -> FinishReason {
    match stop_reason {
        "end_turn" | "stop_sequence" | "pause_turn" => FinishReason::Stop,
        "max_tokens" => FinishReason::Length,
        "tool_use" => FinishReason::ToolCalls,
        "refusal" => FinishReason::ContentFilter,
        _ => FinishReason::Other,
    }
}

pub fn finish_to_stop_reason(finish: FinishReason) -> &'static str {
    match finish {
        FinishReason::Stop | FinishReason::Other => "end_turn",
        FinishReason::Length => "max_tokens",
        FinishReason::ToolCalls | FinishReason::FunctionCall => "tool_use",
        FinishReason::ContentFilter => "refusal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_blocks_and_events_round_trip() {
        let wire = r#"{"model":"claude","max_tokens":10,"messages":[{"role":"user","content":[{"type":"text","text":"hi","cache_control":{"type":"ephemeral"}},{"type":"search_result","source":"x"}]}],"betas":["x"]}"#;
        let parsed: MessagesRequest = serde_json::from_str(wire).unwrap();
        let back: Value = serde_json::to_value(&parsed).unwrap();
        assert_eq!(back["messages"][0]["content"][1]["type"], "search_result");
        assert_eq!(
            back["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert_eq!(back["betas"][0], "x");
        let event: MessagesStreamEvent = serde_json::from_str(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"a"}}"#,
        )
        .unwrap();
        assert!(matches!(
            event,
            MessagesStreamEvent::ContentBlockDelta { index: 0, .. }
        ));
    }

    #[test]
    fn usage_conventions_convert_both_ways() {
        let anthropic = AnthropicUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_input_tokens: Some(3),
            cache_read_input_tokens: Some(7),
            extra: Extra::new(),
        };
        let openai = anthropic.to_openai();
        assert_eq!(openai.prompt_tokens, 20);
        assert_eq!(openai.cached_tokens(), 7);
        assert_eq!(openai.cache_creation_tokens(), 3);
        let back = AnthropicUsage::from_openai(&openai);
        assert_eq!(back.input_tokens, 10);
        assert_eq!(back.output_tokens, 5);
    }
}
