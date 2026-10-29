use std::collections::HashMap;

use bytes::{BufMut, Bytes, BytesMut};
use rustllmroute_types::{
    AnthropicUsage, ChatChunk, FinishReason, GatewayError, Usage, finish_to_stop_reason,
};
use serde_json::{Value, json};

use super::encode::ClientEncoder;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Open {
    Text,
    Thinking,
    Tool(u32),
}

/// OpenAI chat chunks -> Anthropic Messages SSE (`event: {type}\ndata: {json}\n\n`).
pub struct AnthropicSseEncoder {
    id: String,
    model: String,
    started: bool,
    next_index: u32,
    open: Option<(u32, Open)>,
    tool_blocks: HashMap<u32, u32>,
    finish: Option<FinishReason>,
}

fn event(kind: &str, data: &Value) -> Bytes {
    let mut out = BytesMut::with_capacity(128);
    out.put_slice(b"event: ");
    out.put_slice(kind.as_bytes());
    out.put_slice(b"\ndata: ");
    out.put_slice(data.to_string().as_bytes());
    out.put_slice(b"\n\n");
    out.freeze()
}

impl AnthropicSseEncoder {
    pub fn new(model: &str) -> Self {
        Self {
            id: format!("msg_{}", uuid::Uuid::now_v7().simple()),
            model: model.to_owned(),
            started: false,
            next_index: 0,
            open: None,
            tool_blocks: HashMap::new(),
            finish: None,
        }
    }

    fn start(&mut self, out: &mut Vec<Bytes>) {
        if self.started {
            return;
        }
        self.started = true;
        out.push(event(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": self.id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": 0, "output_tokens": 0},
                }
            }),
        ));
    }

    fn close_open(&mut self, out: &mut Vec<Bytes>) {
        if let Some((index, _)) = self.open.take() {
            out.push(event(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": index}),
            ));
        }
    }

    fn ensure(&mut self, kind: Open, out: &mut Vec<Bytes>) -> u32 {
        if let Some((index, open)) = self.open
            && open == kind
        {
            return index;
        }
        self.close_open(out);
        let index = self.next_index;
        self.next_index += 1;
        let block = match kind {
            Open::Text => json!({"type": "text", "text": ""}),
            Open::Thinking => json!({"type": "thinking", "thinking": ""}),
            Open::Tool(_) => json!({"type": "tool_use", "id": "", "name": "", "input": {}}),
        };
        out.push(event(
            "content_block_start",
            &json!({"type": "content_block_start", "index": index, "content_block": block}),
        ));
        self.open = Some((index, kind));
        index
    }

    fn delta(index: u32, delta: Value) -> Bytes {
        event(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": index, "delta": delta}),
        )
    }
}

impl ClientEncoder for AnthropicSseEncoder {
    fn chunk(&mut self, chunk: &ChatChunk) -> Vec<Bytes> {
        let mut out = Vec::new();
        self.start(&mut out);
        for choice in chunk.choices.iter().filter(|c| c.index == 0) {
            let delta = &choice.delta;
            if let Some(reasoning) = delta.reasoning_content.as_deref().filter(|r| !r.is_empty()) {
                let index = self.ensure(Open::Thinking, &mut out);
                out.push(Self::delta(
                    index,
                    json!({"type": "thinking_delta", "thinking": reasoning}),
                ));
            }
            if let Some(blocks) = delta.extra.get("thinking_blocks").and_then(Value::as_array)
                && let Some(signature) = blocks
                    .iter()
                    .find_map(|b| b.get("signature").and_then(Value::as_str))
                && let Some((index, Open::Thinking)) = self.open
            {
                out.push(Self::delta(
                    index,
                    json!({"type": "signature_delta", "signature": signature}),
                ));
            }
            if let Some(text) = delta.content.as_deref().filter(|t| !t.is_empty()) {
                let index = self.ensure(Open::Text, &mut out);
                out.push(Self::delta(
                    index,
                    json!({"type": "text_delta", "text": text}),
                ));
            }
            if let Some(calls) = &delta.tool_calls {
                for call in calls {
                    let index = match self.tool_blocks.get(&call.index) {
                        Some(index) => *index,
                        None => {
                            self.close_open(&mut out);
                            let index = self.next_index;
                            self.next_index += 1;
                            self.tool_blocks.insert(call.index, index);
                            self.open = Some((index, Open::Tool(call.index)));
                            let id =
                                call.id
                                    .clone()
                                    .filter(|id| !id.is_empty())
                                    .unwrap_or_else(|| {
                                        format!("toolu_{}", uuid::Uuid::now_v7().simple())
                                    });
                            out.push(event(
                                "content_block_start",
                                &json!({
                                    "type": "content_block_start",
                                    "index": index,
                                    "content_block": {"type": "tool_use", "id": id, "name": call.function.name.clone().unwrap_or_default(), "input": {}},
                                }),
                            ));
                            index
                        }
                    };
                    if !call.function.arguments.is_empty() {
                        out.push(Self::delta(
                            index,
                            json!({"type": "input_json_delta", "partial_json": call.function.arguments}),
                        ));
                    }
                }
            }
            if let Some(finish) = choice.finish_reason {
                self.finish = Some(finish);
            }
        }
        out
    }

    fn finish(&mut self, tail: Vec<ChatChunk>, usage: Option<&Usage>) -> Vec<Bytes> {
        let mut out = Vec::new();
        self.start(&mut out);
        for chunk in &tail {
            for choice in &chunk.choices {
                if let Some(finish) = choice.finish_reason {
                    self.finish = Some(finish);
                }
            }
        }
        self.close_open(&mut out);
        let stop_reason = finish_to_stop_reason(self.finish.unwrap_or(FinishReason::Stop));
        let usage = usage.map(AnthropicUsage::from_openai).unwrap_or_default();
        out.push(event(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": {
                    "input_tokens": usage.input_tokens,
                    "output_tokens": usage.output_tokens,
                    "cache_read_input_tokens": usage.cache_read_input_tokens.unwrap_or(0),
                    "cache_creation_input_tokens": usage.cache_creation_input_tokens.unwrap_or(0),
                },
            }),
        ));
        out.push(event("message_stop", &json!({"type": "message_stop"})));
        out
    }

    fn error(&mut self, error: &GatewayError) -> Vec<Bytes> {
        vec![event("error", &error.to_anthropic_body())]
    }

    fn keepalive(&self) -> Bytes {
        Bytes::from_static(b"event: ping\ndata: {\"type\":\"ping\"}\n\n")
    }
}

#[cfg(test)]
mod tests {
    use rustllmroute_types::{ChunkChoice, Delta, FunctionCallDelta, ToolCallDelta};

    use super::*;

    fn events(frames: &[Bytes]) -> Vec<(String, Value)> {
        frames
            .iter()
            .map(|frame| {
                let text = std::str::from_utf8(frame).unwrap();
                let mut lines = text.lines();
                let event = lines
                    .next()
                    .unwrap()
                    .strip_prefix("event: ")
                    .unwrap()
                    .to_owned();
                let data = lines.next().unwrap().strip_prefix("data: ").unwrap();
                (event, serde_json::from_str(data).unwrap())
            })
            .collect()
    }

    fn chunk(delta: Delta, finish: Option<FinishReason>) -> ChatChunk {
        let mut chunk = ChatChunk::new("c", 1, "m");
        chunk.choices.push(ChunkChoice {
            index: 0,
            delta,
            finish_reason: finish,
            ..ChunkChoice::default()
        });
        chunk
    }

    #[test]
    fn text_then_tool_call_produce_the_anthropic_event_sequence() {
        let mut encoder = AnthropicSseEncoder::new("claude");
        let mut frames = Vec::new();
        frames.extend(encoder.chunk(&chunk(
            Delta {
                role: Some("assistant".into()),
                content: Some("Hel".into()),
                ..Delta::default()
            },
            None,
        )));
        frames.extend(encoder.chunk(&chunk(
            Delta {
                content: Some("lo".into()),
                ..Delta::default()
            },
            None,
        )));
        frames.extend(encoder.chunk(&chunk(
            Delta {
                tool_calls: Some(vec![ToolCallDelta {
                    index: 0,
                    id: Some("call_1".into()),
                    kind: Some("function".into()),
                    function: FunctionCallDelta {
                        name: Some("get_weather".into()),
                        arguments: "{\"c\":".into(),
                    },
                    ..Default::default()
                }]),
                ..Delta::default()
            },
            None,
        )));
        frames.extend(encoder.chunk(&chunk(
            Delta {
                tool_calls: Some(vec![ToolCallDelta {
                    index: 0,
                    function: FunctionCallDelta {
                        name: None,
                        arguments: "1}".into(),
                    },
                    ..Default::default()
                }]),
                ..Delta::default()
            },
            None,
        )));
        frames.extend(encoder.finish(
            vec![chunk(Delta::default(), Some(FinishReason::ToolCalls))],
            Some(&Usage::new(7, 3)),
        ));
        let seq = events(&frames);
        let kinds: Vec<&str> = seq.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(seq[0].1["message"]["model"], "claude");
        assert_eq!(seq[1].1["content_block"]["type"], "text");
        assert_eq!(seq[2].1["delta"]["text"], "Hel");
        assert_eq!(seq[5].1["content_block"]["type"], "tool_use");
        assert_eq!(seq[5].1["content_block"]["id"], "call_1");
        assert_eq!(seq[5].1["index"], 1);
        assert_eq!(seq[6].1["delta"]["partial_json"], "{\"c\":");
        assert_eq!(seq[9].1["delta"]["stop_reason"], "tool_use");
        assert_eq!(seq[9].1["usage"]["output_tokens"], 3);
        assert_eq!(seq[9].1["usage"]["input_tokens"], 7);
    }

    #[test]
    fn errors_and_empty_streams_are_well_formed() {
        let mut encoder = AnthropicSseEncoder::new("claude");
        let frames = encoder.finish(Vec::new(), None);
        let seq = events(&frames);
        let kinds: Vec<&str> = seq.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["message_start", "message_delta", "message_stop"]
        );
        let error = encoder.error(&GatewayError::Unauthorized("no".into()));
        let seq = events(&error);
        assert_eq!(seq[0].0, "error");
        assert_eq!(seq[0].1["error"]["type"], "authentication_error");
    }
}
