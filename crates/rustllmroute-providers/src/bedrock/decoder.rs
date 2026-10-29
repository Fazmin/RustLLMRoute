use std::collections::HashMap;

use rustllmroute_types::{
    ChatChunk, ChunkChoice, Delta, FinishReason, FunctionCallDelta, GatewayError, ToolCallDelta,
};
use serde_json::{Value, json};

use super::errors::stream_exception;
use super::response::{map_stop_reason, usage_from_converse};
use crate::anthropic::request::JSON_TOOL_NAME;
use crate::traits::{ChunkDecoder, Frame};

/// Decodes `converse-stream` event-stream messages (`frame.event` is the `:event-type` header, or
/// the `:exception-type` for exceptions, which always end in `Exception`).
pub struct ConverseStreamDecoder {
    provider: &'static str,
    id: String,
    created: u64,
    json_tool: bool,
    tool_name_map: HashMap<String, String>,
    /// Bedrock content block index -> OpenAI tool call index.
    block_tools: HashMap<u64, u32>,
    json_tool_block: Option<u64>,
    next_tool: u32,
    saw_tools: bool,
    finish: Option<FinishReason>,
    done: bool,
}

impl ConverseStreamDecoder {
    pub fn new(
        provider: &'static str,
        json_tool: bool,
        tool_name_map: HashMap<String, String>,
        created: u64,
    ) -> Self {
        Self {
            provider,
            id: format!("chatcmpl-{}", uuid::Uuid::now_v7().simple()),
            created,
            json_tool,
            tool_name_map,
            block_tools: HashMap::new(),
            json_tool_block: None,
            next_tool: 0,
            saw_tools: false,
            finish: None,
            done: false,
        }
    }

    fn chunk(&self, delta: Delta, finish: Option<FinishReason>) -> ChatChunk {
        let mut chunk = ChatChunk::new(self.id.clone(), self.created, "");
        chunk.choices.push(ChunkChoice {
            index: 0,
            delta,
            finish_reason: finish,
            logprobs: None,
            extra: Default::default(),
        });
        chunk
    }

    fn block_index(payload: &Value) -> u64 {
        payload
            .get("contentBlockIndex")
            .and_then(Value::as_u64)
            .unwrap_or(0)
    }
}

impl ChunkDecoder for ConverseStreamDecoder {
    fn decode(&mut self, frame: Frame<'_>, out: &mut Vec<ChatChunk>) -> Result<(), GatewayError> {
        if self.done {
            return Ok(());
        }
        let Some(event) = frame.event else {
            return Ok(());
        };
        if event.to_ascii_lowercase().ends_with("exception") {
            return Err(stream_exception(event, frame.data));
        }
        let payload: Value = if frame.data.trim_ascii().is_empty() {
            json!({})
        } else {
            serde_json::from_slice(frame.data).map_err(|error| {
                GatewayError::InvalidUpstreamResponse(format!(
                    "{}: undecodable {event} payload: {error}",
                    self.provider
                ))
            })?
        };
        match event {
            "messageStart" => {
                out.push(self.chunk(
                    Delta {
                        role: Some("assistant".to_owned()),
                        ..Delta::default()
                    },
                    None,
                ));
            }
            "contentBlockStart" => {
                let index = Self::block_index(&payload);
                if let Some(tool) = payload.pointer("/start/toolUse") {
                    let name = tool.get("name").and_then(Value::as_str).unwrap_or_default();
                    if self.json_tool && name == JSON_TOOL_NAME {
                        self.json_tool_block = Some(index);
                        return Ok(());
                    }
                    let tool_index = self.next_tool;
                    self.next_tool += 1;
                    self.block_tools.insert(index, tool_index);
                    self.saw_tools = true;
                    let client_name = self
                        .tool_name_map
                        .get(name)
                        .cloned()
                        .unwrap_or_else(|| name.to_owned());
                    out.push(self.chunk(
                        Delta {
                            tool_calls: Some(vec![ToolCallDelta {
                                    index: tool_index,
                                    id: tool
                                        .get("toolUseId")
                                        .and_then(Value::as_str)
                                        .map(str::to_owned),
                                    kind: Some("function".to_owned()),
                                    function: FunctionCallDelta {
                                        name: Some(client_name),
                                        arguments: String::new(),
                                    },
                                    extra: Default::default(),
                                }]),
                            ..Delta::default()
                        },
                        None,
                    ));
                }
            }
            "contentBlockDelta" => {
                let index = Self::block_index(&payload);
                let Some(delta) = payload.get("delta") else {
                    return Ok(());
                };
                if let Some(text) = delta.get("text").and_then(Value::as_str) {
                    out.push(self.chunk(
                        Delta {
                            content: Some(text.to_owned()),
                            ..Delta::default()
                        },
                        None,
                    ));
                }
                if let Some(input) = delta.pointer("/toolUse/input").and_then(Value::as_str) {
                    if self.json_tool_block == Some(index) {
                        out.push(self.chunk(
                            Delta {
                                content: Some(input.to_owned()),
                                ..Delta::default()
                            },
                            None,
                        ));
                    } else if let Some(tool_index) = self.block_tools.get(&index).copied() {
                        out.push(self.chunk(
                            Delta {
                                tool_calls: Some(vec![ToolCallDelta {
                                    index: tool_index,
                                    function: FunctionCallDelta {
                                        name: None,
                                        arguments: input.to_owned(),
                                    },
                                    ..ToolCallDelta::default()
                                }]),
                                ..Delta::default()
                            },
                            None,
                        ));
                    }
                }
                if let Some(reasoning) = delta.get("reasoningContent") {
                    let mut out_delta = Delta::default();
                    if let Some(text) = reasoning.get("text").and_then(Value::as_str) {
                        out_delta.reasoning_content = Some(text.to_owned());
                    }
                    if let Some(signature) = reasoning.get("signature").and_then(Value::as_str) {
                        out_delta.extra.insert(
                            "thinking_blocks".to_owned(),
                            json!([{"type": "thinking", "thinking": "", "signature": signature}]),
                        );
                    }
                    if let Some(data) = reasoning.get("redactedContent").and_then(Value::as_str) {
                        out_delta.extra.insert(
                            "thinking_blocks".to_owned(),
                            json!([{"type": "redacted_thinking", "data": data}]),
                        );
                    }
                    if !out_delta.is_empty() {
                        out.push(self.chunk(out_delta, None));
                    }
                }
            }
            "messageStop" => {
                let reason = payload.get("stopReason").and_then(Value::as_str);
                let finish = if self.json_tool_block.is_some() {
                    Some(FinishReason::Stop)
                } else if self.saw_tools {
                    Some(FinishReason::ToolCalls)
                } else {
                    map_stop_reason(reason)
                };
                self.finish = finish;
                out.push(self.chunk(Delta::default(), finish));
            }
            "metadata" => {
                if let Some(usage) = payload.get("usage") {
                    let mut chunk = ChatChunk::new(self.id.clone(), self.created, "");
                    chunk.usage = Some(usage_from_converse(usage));
                    out.push(chunk);
                }
                self.done = true;
            }
            _ => {}
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

    fn frame<'a>(event: &'a str, data: &'a str) -> Frame<'a> {
        Frame {
            event: Some(event),
            data: data.as_bytes(),
        }
    }

    #[test]
    fn decodes_text_tool_use_and_usage() {
        let mut decoder = ConverseStreamDecoder::new("bedrock", false, HashMap::new(), 1);
        let mut out = Vec::new();
        for (event, data) in [
            ("messageStart", r#"{"role":"assistant"}"#),
            (
                "contentBlockDelta",
                r#"{"contentBlockIndex":0,"delta":{"text":"Hi"}}"#,
            ),
            ("contentBlockStop", r#"{"contentBlockIndex":0}"#),
            (
                "contentBlockStart",
                r#"{"contentBlockIndex":1,"start":{"toolUse":{"toolUseId":"t1","name":"get_weather"}}}"#,
            ),
            (
                "contentBlockDelta",
                r#"{"contentBlockIndex":1,"delta":{"toolUse":{"input":"{\"city\":"}}}"#,
            ),
            (
                "contentBlockDelta",
                r#"{"contentBlockIndex":1,"delta":{"toolUse":{"input":"\"Oslo\"}"}}}"#,
            ),
            ("contentBlockStop", r#"{"contentBlockIndex":1}"#),
            ("messageStop", r#"{"stopReason":"tool_use"}"#),
            (
                "metadata",
                r#"{"usage":{"inputTokens":9,"outputTokens":4,"totalTokens":13},"metrics":{"latencyMs":5}}"#,
            ),
        ] {
            decoder.decode(frame(event, data), &mut out).unwrap();
        }
        assert!(decoder.is_done());
        assert_eq!(out[0].choices[0].delta.role.as_deref(), Some("assistant"));
        assert_eq!(out[1].choices[0].delta.content.as_deref(), Some("Hi"));
        let start = &out[2].choices[0].delta.tool_calls.as_ref().unwrap()[0];
        assert_eq!(start.id.as_deref(), Some("t1"));
        assert_eq!(start.function.name.as_deref(), Some("get_weather"));
        let args: String = out[3..5]
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
            out[5].choices[0].finish_reason,
            Some(FinishReason::ToolCalls)
        );
        assert_eq!(out[6].usage.as_ref().unwrap().prompt_tokens, 9);
    }

    #[test]
    fn exceptions_end_the_stream() {
        let mut decoder = ConverseStreamDecoder::new("bedrock", false, HashMap::new(), 1);
        let mut out = Vec::new();
        let error = decoder
            .decode(
                frame("throttlingException", r#"{"message":"slow down"}"#),
                &mut out,
            )
            .unwrap_err();
        assert!(matches!(error, GatewayError::Upstream { status: 429, .. }));
    }
}
