use std::collections::HashMap;

use rustllmroute_types::{
    AnthropicUsage, ChatChunk, ChunkChoice, ContentBlock, ContentDelta, Delta, FinishReason,
    FunctionCallDelta, GatewayError, MessagesStreamEvent, ToolCallDelta, stop_reason_to_finish,
};
use serde_json::{Value, json};

use super::request::JSON_TOOL_NAME;
use super::status_for_error_type;
use crate::errors::classify;
use crate::traits::{ChunkDecoder, Frame};

#[derive(Clone, Copy, Debug)]
enum BlockKind {
    Text,
    Tool(u32),
    JsonTool,
    Thinking,
}

/// Anthropic Messages SSE -> OpenAI chat chunks.
pub struct AnthropicSseDecoder {
    provider: &'static str,
    id: String,
    model: String,
    created: u64,
    blocks: HashMap<u32, BlockKind>,
    next_tool_index: u32,
    json_tool: bool,
    tool_name_map: HashMap<String, String>,
    start_usage: Option<AnthropicUsage>,
    done: bool,
}

impl AnthropicSseDecoder {
    pub fn new(
        provider: &'static str,
        json_tool: bool,
        tool_name_map: HashMap<String, String>,
    ) -> Self {
        Self {
            provider,
            id: String::new(),
            model: String::new(),
            created: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            blocks: HashMap::new(),
            next_tool_index: 0,
            json_tool,
            tool_name_map,
            start_usage: None,
            done: false,
        }
    }

    fn chunk(&self, delta: Delta, finish: Option<FinishReason>) -> ChatChunk {
        let mut chunk = ChatChunk::new(self.id.clone(), self.created, self.model.clone());
        chunk.choices.push(ChunkChoice {
            index: 0,
            delta,
            finish_reason: finish,
            ..ChunkChoice::default()
        });
        chunk
    }

    fn text(&self, text: String) -> ChatChunk {
        self.chunk(
            Delta {
                content: Some(text),
                ..Delta::default()
            },
            None,
        )
    }

    fn tool_delta(
        &self,
        index: u32,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    ) -> ChatChunk {
        self.chunk(
            Delta {
                tool_calls: Some(vec![ToolCallDelta {
                    index,
                    id,
                    kind: name.as_ref().map(|_| "function".to_owned()),
                    function: FunctionCallDelta { name, arguments },
                    extra: Default::default(),
                }]),
                ..Delta::default()
            },
            None,
        )
    }

    fn error(&self, error: &Value) -> GatewayError {
        let kind = error
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("api_error");
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("upstream error inside stream")
            .to_owned();
        let status = status_for_error_type(kind);
        GatewayError::Upstream {
            provider: self.provider,
            status,
            kind: classify(status, &message),
            code: Some(kind.to_owned()),
            message,
            retry_after: None,
            body: None,
        }
    }
}

impl ChunkDecoder for AnthropicSseDecoder {
    fn decode(&mut self, frame: Frame<'_>, out: &mut Vec<ChatChunk>) -> Result<(), GatewayError> {
        if self.done || frame.data.trim_ascii().is_empty() {
            return Ok(());
        }
        let event: MessagesStreamEvent = match serde_json::from_slice(frame.data) {
            Ok(event) => event,
            Err(error) => {
                // Unknown event types (e.g. new block kinds) are skipped rather than fatal.
                if serde_json::from_slice::<Value>(frame.data).is_ok() {
                    tracing::debug!(error = %error, event = ?frame.event, "skipping unknown anthropic stream event");
                    return Ok(());
                }
                return Err(GatewayError::InvalidUpstreamResponse(format!(
                    "{}: undecodable stream event: {error}",
                    self.provider
                )));
            }
        };
        match event {
            MessagesStreamEvent::MessageStart { message } => {
                self.id = message.id;
                self.model = message.model;
                self.start_usage = Some(message.usage);
                out.push(self.chunk(
                    Delta {
                        role: Some("assistant".to_owned()),
                        content: Some(String::new()),
                        ..Delta::default()
                    },
                    None,
                ));
            }
            MessagesStreamEvent::ContentBlockStart {
                index,
                content_block,
            } => match content_block.known() {
                Some(ContentBlock::Text { text, .. }) => {
                    self.blocks.insert(index, BlockKind::Text);
                    if !text.is_empty() {
                        out.push(self.text(text.clone()));
                    }
                }
                Some(ContentBlock::ToolUse { id, name, .. }) => {
                    if self.json_tool && name == JSON_TOOL_NAME {
                        self.blocks.insert(index, BlockKind::JsonTool);
                    } else {
                        let tool_index = self.next_tool_index;
                        self.next_tool_index += 1;
                        self.blocks.insert(index, BlockKind::Tool(tool_index));
                        let original = self
                            .tool_name_map
                            .get(name)
                            .cloned()
                            .unwrap_or_else(|| name.clone());
                        out.push(self.tool_delta(
                            tool_index,
                            Some(id.clone()),
                            Some(original),
                            String::new(),
                        ));
                    }
                }
                Some(ContentBlock::Thinking { thinking, .. }) => {
                    self.blocks.insert(index, BlockKind::Thinking);
                    if !thinking.is_empty() {
                        out.push(self.chunk(
                            Delta {
                                reasoning_content: Some(thinking.clone()),
                                ..Delta::default()
                            },
                            None,
                        ));
                    }
                }
                Some(ContentBlock::RedactedThinking { data, .. }) => {
                    let mut delta = Delta::default();
                    delta.extra.insert(
                        "thinking_blocks".to_owned(),
                        json!([{"type": "redacted_thinking", "data": data}]),
                    );
                    out.push(self.chunk(delta, None));
                }
                _ => {}
            },
            MessagesStreamEvent::ContentBlockDelta { index, delta } => {
                match (delta.known(), self.blocks.get(&index).copied()) {
                    (Some(ContentDelta::TextDelta { text }), _) => {
                        out.push(self.text(text.clone()))
                    }
                    (
                        Some(ContentDelta::InputJsonDelta { partial_json }),
                        Some(BlockKind::Tool(tool_index)),
                    ) => {
                        out.push(self.tool_delta(tool_index, None, None, partial_json.clone()));
                    }
                    (
                        Some(ContentDelta::InputJsonDelta { partial_json }),
                        Some(BlockKind::JsonTool),
                    ) => {
                        out.push(self.text(partial_json.clone()));
                    }
                    (Some(ContentDelta::ThinkingDelta { thinking }), _) => out.push(self.chunk(
                        Delta {
                            reasoning_content: Some(thinking.clone()),
                            ..Delta::default()
                        },
                        None,
                    )),
                    (Some(ContentDelta::SignatureDelta { signature }), _) => {
                        let mut delta = Delta::default();
                        delta.extra.insert(
                            "thinking_blocks".to_owned(),
                            json!([{"type": "thinking", "signature": signature}]),
                        );
                        out.push(self.chunk(delta, None));
                    }
                    _ => {}
                }
            }
            MessagesStreamEvent::ContentBlockStop { .. } | MessagesStreamEvent::Ping => {}
            MessagesStreamEvent::MessageDelta { delta, usage } => {
                let mut finish = delta
                    .get("stop_reason")
                    .and_then(Value::as_str)
                    .map(stop_reason_to_finish);
                if self.json_tool && finish == Some(FinishReason::ToolCalls) {
                    finish = Some(FinishReason::Stop);
                }
                if let Some(finish) = finish {
                    out.push(self.chunk(Delta::default(), Some(finish)));
                }
                let mut total = self.start_usage.clone().unwrap_or_default();
                if let Some(usage) = usage {
                    total.output_tokens = usage.output_tokens;
                    if usage.input_tokens > 0 {
                        total.input_tokens = usage.input_tokens;
                    }
                    if usage.cache_read_input_tokens.is_some() {
                        total.cache_read_input_tokens = usage.cache_read_input_tokens;
                    }
                    if usage.cache_creation_input_tokens.is_some() {
                        total.cache_creation_input_tokens = usage.cache_creation_input_tokens;
                    }
                }
                let mut usage_chunk =
                    ChatChunk::new(self.id.clone(), self.created, self.model.clone());
                usage_chunk.usage = Some(total.to_openai());
                out.push(usage_chunk);
            }
            MessagesStreamEvent::MessageStop => self.done = true,
            MessagesStreamEvent::Error { error } => return Err(self.error(&error)),
        }
        Ok(())
    }

    fn is_done(&self) -> bool {
        self.done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(decoder: &mut AnthropicSseDecoder, events: &[&str]) -> Vec<ChatChunk> {
        let mut out = Vec::new();
        for data in events {
            decoder
                .decode(
                    Frame {
                        event: None,
                        data: data.as_bytes(),
                    },
                    &mut out,
                )
                .unwrap();
        }
        out
    }

    #[test]
    fn text_tool_and_usage_events_become_openai_chunks() {
        let mut decoder = AnthropicSseDecoder::new("anthropic", false, HashMap::new());
        let chunks = feed(
            &mut decoder,
            &[
                r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude","content":[],"usage":{"input_tokens":10,"output_tokens":1}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{}}}"#,
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}"#,
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"Oslo\"}"}}"#,
                r#"{"type":"content_block_stop","index":1}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":9}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        assert!(decoder.is_done());
        assert_eq!(
            chunks[0].choices[0].delta.role.as_deref(),
            Some("assistant")
        );
        assert_eq!(chunks[1].choices[0].delta.content.as_deref(), Some("Hi"));
        let start = &chunks[2].choices[0].delta.tool_calls.as_ref().unwrap()[0];
        assert_eq!(
            (
                start.index,
                start.id.as_deref(),
                start.function.name.as_deref()
            ),
            (0, Some("toolu_1"), Some("get_weather"))
        );
        let args: String = chunks[3..5]
            .iter()
            .map(|c| {
                c.choices[0].delta.tool_calls.as_ref().unwrap()[0]
                    .function
                    .arguments
                    .clone()
            })
            .collect();
        assert_eq!(args, "{\"city\":\"Oslo\"}");
        assert_eq!(
            chunks[5].choices[0].finish_reason,
            Some(FinishReason::ToolCalls)
        );
        let usage = chunks[6].usage.as_ref().unwrap();
        assert_eq!((usage.prompt_tokens, usage.completion_tokens), (10, 9));
    }

    #[test]
    fn error_events_fail_the_stream_with_mapped_status() {
        let mut decoder = AnthropicSseDecoder::new("anthropic", false, HashMap::new());
        let mut out = Vec::new();
        let error = decoder
            .decode(
                Frame { event: Some("error"), data: br#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"# },
                &mut out,
            )
            .unwrap_err();
        assert!(matches!(error, GatewayError::Upstream { status: 529, .. }));
        assert!(error.retryable());
    }
}
