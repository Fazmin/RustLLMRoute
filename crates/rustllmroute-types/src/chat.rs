use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Extra, Recognized, is_false};

pub mod role {
    pub const SYSTEM: &str = "system";
    pub const DEVELOPER: &str = "developer";
    pub const USER: &str = "user";
    pub const ASSISTANT: &str = "assistant";
    pub const TOOL: &str = "tool";
    pub const FUNCTION: &str = "function";
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<Stop>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl ChatRequest {
    /// The output token cap the client asked for, whichever field it used.
    pub fn output_token_cap(&self) -> Option<u32> {
        self.max_completion_tokens.or(self.max_tokens)
    }

    pub fn wants_stream_usage(&self) -> bool {
        self.stream_options
            .as_ref()
            .and_then(|options| options.include_usage)
            .unwrap_or(false)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StreamOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_usage: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl ChatMessage {
    pub fn text(role: &str, text: impl Into<String>) -> Self {
        Self {
            role: role.to_owned(),
            content: Some(Content::Text(text.into())),
            ..Self::default()
        }
    }

    /// Every text segment of the message content, in order.
    pub fn text_segments(&self) -> impl Iterator<Item = &str> {
        self.content.iter().flat_map(Content::text_segments)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Parts(Vec<Recognized<ContentPart>>),
}

impl Content {
    pub fn text_segments(&self) -> Box<dyn Iterator<Item = &str> + '_> {
        match self {
            Self::Text(text) => Box::new(std::iter::once(text.as_str())),
            Self::Parts(parts) => Box::new(parts.iter().filter_map(|part| match part.known()? {
                ContentPart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })),
        }
    }

    pub fn joined_text(&self) -> String {
        self.text_segments().collect::<Vec<_>>().join("")
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
        #[serde(flatten)]
        extra: Extra,
    },
    ImageUrl {
        image_url: ImageUrl,
        #[serde(flatten)]
        extra: Extra,
    },
    InputAudio {
        input_audio: Value,
        #[serde(flatten)]
        extra: Extra,
    },
    File {
        file: Value,
        #[serde(flatten)]
        extra: Extra,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<FunctionDef>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FunctionDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolChoice {
    Mode(String),
    Named(Value),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Stop {
    One(String),
    Many(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function_type")]
    pub kind: String,
    pub function: FunctionCall,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    #[serde(default)]
    pub arguments: String,
}

fn function_type() -> String {
    "function".to_owned()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    pub id: String,
    #[serde(default = "chat_completion_object")]
    pub object: String,
    #[serde(default)]
    pub created: u64,
    pub model: String,
    pub choices: Vec<Choice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_fingerprint: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

fn chat_completion_object() -> String {
    "chat.completion".to_owned()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    #[serde(default)]
    pub index: u32,
    pub message: AssistantMessage,
    #[serde(default)]
    pub finish_reason: Option<FinishReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    #[serde(default = "assistant_role")]
    pub role: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_blocks: Option<Vec<Value>>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for AssistantMessage {
    fn default() -> Self {
        Self {
            role: assistant_role(),
            content: None,
            tool_calls: None,
            reasoning_content: None,
            refusal: None,
            thinking_blocks: None,
            extra: Extra::new(),
        }
    }
}

fn assistant_role() -> String {
    role::ASSISTANT.to_owned()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    FunctionCall,
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
    /// LiteLLM top-level extension carrying Anthropic cache writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    /// Provider-reported cost in USD (OpenRouter).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    /// True when the gateway counted tokens locally because the provider sent no usage.
    #[serde(skip)]
    pub estimated: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Usage {
    /// Convention shared with LiteLLM: `prompt_tokens` already includes cached and cache-creation tokens.
    pub fn new(prompt_tokens: u64, completion_tokens: u64) -> Self {
        Self {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
            ..Self::default()
        }
    }

    pub fn cached_tokens(&self) -> u64 {
        self.prompt_tokens_details
            .as_ref()
            .and_then(|details| details.cached_tokens)
            .unwrap_or(0)
    }

    pub fn cache_creation_tokens(&self) -> u64 {
        self.cache_creation_input_tokens.unwrap_or(0)
    }

    pub fn reasoning_tokens(&self) -> u64 {
        self.completion_tokens_details
            .as_ref()
            .and_then(|details| details.reasoning_tokens)
            .unwrap_or(0)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PromptTokensDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_tokens: Option<u64>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CompletionTokensDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_prediction_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected_prediction_tokens: Option<u64>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatChunk {
    pub id: String,
    #[serde(default = "chat_chunk_object")]
    pub object: String,
    #[serde(default)]
    pub created: u64,
    pub model: String,
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(flatten)]
    pub extra: Extra,
}

fn chat_chunk_object() -> String {
    "chat.completion.chunk".to_owned()
}

impl ChatChunk {
    pub fn new(id: impl Into<String>, created: u64, model: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            object: chat_chunk_object(),
            created,
            model: model.into(),
            choices: Vec::new(),
            usage: None,
            extra: Extra::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChunkChoice {
    #[serde(default)]
    pub index: u32,
    #[serde(default)]
    pub delta: Delta,
    #[serde(default)]
    pub finish_reason: Option<FinishReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Delta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Delta {
    pub fn is_empty(&self) -> bool {
        self.role.is_none()
            && self.content.as_deref().is_none_or(str::is_empty)
            && self.tool_calls.as_ref().is_none_or(Vec::is_empty)
            && self.reasoning_content.as_deref().is_none_or(str::is_empty)
            && self.refusal.is_none()
            && self.extra.is_empty()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolCallDelta {
    #[serde(default)]
    pub index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default)]
    pub function: FunctionCallDelta,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FunctionCallDelta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    fn unknown_fields_round_trip_on_requests() {
        let wire = r#"{"model":"m","messages":[{"role":"user","content":"hi","cache_control":{"type":"ephemeral"}}],"top_k":5,"stream":true}"#;
        let parsed: ChatRequest = serde_json::from_str(wire).unwrap();
        assert_eq!(parsed.extra["top_k"], 5);
        assert_eq!(
            parsed.messages[0].extra["cache_control"]["type"],
            "ephemeral"
        );
        let back: Value = serde_json::to_value(&parsed).unwrap();
        assert_eq!(back["top_k"], 5);
        assert_eq!(back["stream"], true);
        assert_eq!(back["messages"][0]["cache_control"]["type"], "ephemeral");
    }

    #[rstest]
    fn unknown_content_parts_are_preserved() {
        let wire = r#"{"role":"user","content":[{"type":"text","text":"a"},{"type":"video_url","video_url":{"url":"x"}}]}"#;
        let message: ChatMessage = serde_json::from_str(wire).unwrap();
        assert_eq!(message.text_segments().collect::<Vec<_>>(), vec!["a"]);
        let back = serde_json::to_string(&message).unwrap();
        assert!(back.contains("video_url"));
    }

    #[rstest]
    fn usage_helpers_read_nested_details() {
        let usage: Usage = serde_json::from_str(
            r#"{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,"prompt_tokens_details":{"cached_tokens":4},"completion_tokens_details":{"reasoning_tokens":2}}"#,
        )
        .unwrap();
        assert_eq!(usage.cached_tokens(), 4);
        assert_eq!(usage.reasoning_tokens(), 2);
        assert!(!usage.estimated);
    }

    #[rstest]
    #[case::stop("stop", FinishReason::Stop)]
    #[case::tool_calls("tool_calls", FinishReason::ToolCalls)]
    #[case::unknown("weird", FinishReason::Other)]
    fn finish_reason_parses(#[case] wire: &str, #[case] expected: FinishReason) {
        let parsed: FinishReason = serde_json::from_value(Value::String(wire.to_owned())).unwrap();
        assert_eq!(parsed, expected);
    }
}
