use std::collections::HashMap;

use bytes::Bytes;
use rustllmroute_types::{ChatChunk, FinishReason, GatewayError, ResponsesRequest, Usage};
use serde_json::{Value, json};

use super::encode::ClientEncoder;
use crate::responses::{function_call_item, message_item, new_item_id, response_object};

struct MessageState {
    item_id: String,
    output_index: u32,
    text: String,
}

struct CallState {
    item_id: String,
    call_id: String,
    name: String,
    output_index: u32,
    arguments: String,
}

/// OpenAI chat chunks -> Responses API SSE (`event: {type}\ndata: {json}\n\n`), one output item
/// per message / function call, `response.completed` carrying the full object.
pub struct ResponsesSseEncoder {
    sequence: u64,
    id: String,
    model: String,
    created_at: u64,
    request: ResponsesRequest,
    started: bool,
    message: Option<MessageState>,
    calls: HashMap<u32, CallState>,
    call_order: Vec<u32>,
    reasoning: String,
    next_output_index: u32,
    finish: Option<FinishReason>,
}

impl ResponsesSseEncoder {
    pub fn new(id: String, model: &str, request: ResponsesRequest) -> Self {
        Self {
            sequence: 0,
            id,
            model: model.to_owned(),
            created_at: crate::now_epoch_secs(),
            request,
            started: false,
            message: None,
            calls: HashMap::new(),
            call_order: Vec::new(),
            reasoning: String::new(),
            next_output_index: 0,
            finish: None,
        }
    }

    fn event(&mut self, kind: &str, mut payload: Value) -> Bytes {
        self.sequence += 1;
        if let Some(object) = payload.as_object_mut() {
            object.insert("type".to_owned(), json!(kind));
            object.insert("sequence_number".to_owned(), json!(self.sequence));
        }
        Bytes::from(format!("event: {kind}\ndata: {payload}\n\n"))
    }

    fn partial_response(&self, status: &str) -> Value {
        response_object(
            &self.id,
            &self.model,
            self.created_at,
            status,
            Vec::new(),
            None,
            &self.request,
            None,
        )
    }

    fn start(&mut self, out: &mut Vec<Bytes>) {
        if self.started {
            return;
        }
        self.started = true;
        let created = self.partial_response("in_progress");
        out.push(self.event("response.created", json!({"response": created})));
        let in_progress = self.partial_response("in_progress");
        out.push(self.event("response.in_progress", json!({"response": in_progress})));
    }

    fn next_index(&mut self) -> u32 {
        let index = self.next_output_index;
        self.next_output_index += 1;
        index
    }

    fn ensure_message(&mut self, out: &mut Vec<Bytes>) -> (String, u32) {
        if let Some(message) = &self.message {
            return (message.item_id.clone(), message.output_index);
        }
        let item_id = new_item_id("msg");
        let output_index = self.next_index();
        let item = message_item(&item_id, "", "in_progress");
        out.push(self.event(
            "response.output_item.added",
            json!({"output_index": output_index, "item": item}),
        ));
        out.push(self.event(
            "response.content_part.added",
            json!({
                "item_id": item_id,
                "output_index": output_index,
                "content_index": 0,
                "part": {"type": "output_text", "text": "", "annotations": []},
            }),
        ));
        self.message = Some(MessageState {
            item_id: item_id.clone(),
            output_index,
            text: String::new(),
        });
        (item_id, output_index)
    }

    fn close_message(&mut self, out: &mut Vec<Bytes>) -> Option<Value> {
        let message = self.message.take()?;
        out.push(self.event(
            "response.output_text.done",
            json!({
                "item_id": message.item_id,
                "output_index": message.output_index,
                "content_index": 0,
                "text": message.text,
            }),
        ));
        out.push(self.event(
            "response.content_part.done",
            json!({
                "item_id": message.item_id,
                "output_index": message.output_index,
                "content_index": 0,
                "part": {"type": "output_text", "text": message.text, "annotations": []},
            }),
        ));
        let item = message_item(&message.item_id, &message.text, "completed");
        out.push(self.event(
            "response.output_item.done",
            json!({"output_index": message.output_index, "item": item}),
        ));
        Some(item)
    }
}

impl ClientEncoder for ResponsesSseEncoder {
    fn chunk(&mut self, chunk: &ChatChunk) -> Vec<Bytes> {
        let mut out = Vec::new();
        self.start(&mut out);
        for choice in chunk.choices.iter().filter(|c| c.index == 0) {
            let delta = &choice.delta;
            if let Some(reasoning) = delta.reasoning_content.as_deref() {
                self.reasoning.push_str(reasoning);
            }
            if let Some(text) = delta.content.as_deref().filter(|t| !t.is_empty()) {
                let (item_id, output_index) = self.ensure_message(&mut out);
                if let Some(message) = self.message.as_mut() {
                    message.text.push_str(text);
                }
                out.push(self.event(
                    "response.output_text.delta",
                    json!({"item_id": item_id, "output_index": output_index, "content_index": 0, "delta": text}),
                ));
            }
            for call in delta.tool_calls.iter().flatten() {
                if !self.calls.contains_key(&call.index) {
                    let item_id = new_item_id("fc");
                    let call_id = call
                        .id
                        .clone()
                        .filter(|id| !id.is_empty())
                        .unwrap_or_else(|| new_item_id("call"));
                    let name = call.function.name.clone().unwrap_or_default();
                    let output_index = self.next_index();
                    let item = function_call_item(&item_id, &call_id, &name, "", "in_progress");
                    out.push(self.event(
                        "response.output_item.added",
                        json!({"output_index": output_index, "item": item}),
                    ));
                    self.calls.insert(
                        call.index,
                        CallState {
                            item_id,
                            call_id,
                            name,
                            output_index,
                            arguments: String::new(),
                        },
                    );
                    self.call_order.push(call.index);
                }
                if !call.function.arguments.is_empty()
                    && let Some(state) = self.calls.get_mut(&call.index)
                {
                    state.arguments.push_str(&call.function.arguments);
                    let (item_id, output_index) = (state.item_id.clone(), state.output_index);
                    out.push(self.event(
                        "response.function_call_arguments.delta",
                        json!({"item_id": item_id, "output_index": output_index, "delta": call.function.arguments}),
                    ));
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
        let mut output: Vec<(u32, Value)> = Vec::new();
        if let Some(message) = self.message.as_ref().map(|m| m.output_index)
            && let Some(item) = self.close_message(&mut out)
        {
            output.push((message, item));
        }
        for index in std::mem::take(&mut self.call_order) {
            let Some(state) = self.calls.remove(&index) else {
                continue;
            };
            out.push(self.event(
                "response.function_call_arguments.done",
                json!({"item_id": state.item_id, "output_index": state.output_index, "arguments": state.arguments}),
            ));
            let item = function_call_item(
                &state.item_id,
                &state.call_id,
                &state.name,
                &state.arguments,
                "completed",
            );
            out.push(self.event(
                "response.output_item.done",
                json!({"output_index": state.output_index, "item": item}),
            ));
            output.push((state.output_index, item));
        }
        if !self.reasoning.is_empty() {
            let output_index = self.next_index();
            let item = json!({
                "type": "reasoning",
                "id": new_item_id("rs"),
                "summary": [{"type": "summary_text", "text": self.reasoning}],
            });
            out.push(self.event(
                "response.output_item.added",
                json!({"output_index": output_index, "item": item}),
            ));
            out.push(self.event(
                "response.output_item.done",
                json!({"output_index": output_index, "item": item}),
            ));
            output.push((output_index, item));
        }
        output.sort_by_key(|(index, _)| *index);
        let incomplete =
            matches!(self.finish, Some(FinishReason::Length)).then_some("max_output_tokens");
        let status = if incomplete.is_some() {
            "incomplete"
        } else {
            "completed"
        };
        let response = response_object(
            &self.id,
            &self.model,
            self.created_at,
            status,
            output.into_iter().map(|(_, item)| item).collect(),
            usage,
            &self.request,
            incomplete,
        );
        let kind = if incomplete.is_some() {
            "response.incomplete"
        } else {
            "response.completed"
        };
        out.push(self.event(kind, json!({"response": response})));
        out
    }

    fn error(&mut self, error: &GatewayError) -> Vec<Bytes> {
        let body = error.to_openai_body();
        let message = body
            .pointer("/error/message")
            .cloned()
            .unwrap_or_else(|| json!(error.to_string()));
        let code = body.pointer("/error/code").cloned().unwrap_or(Value::Null);
        vec![self.event(
            "error",
            json!({"code": code, "message": message, "param": null}),
        )]
    }

    fn keepalive(&self) -> Bytes {
        Bytes::from_static(b": ping\n\n")
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
    fn text_and_function_call_produce_the_responses_event_sequence() {
        let request = ResponsesRequest {
            model: "alias".into(),
            ..ResponsesRequest::default()
        };
        let mut encoder = ResponsesSseEncoder::new("resp_1".into(), "alias", request);
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
                        arguments: "{\"c\":1}".into(),
                    },
                    ..Default::default()
                }]),
                ..Delta::default()
            },
            None,
        )));
        frames.extend(encoder.finish(
            vec![chunk(Delta::default(), Some(FinishReason::ToolCalls))],
            Some(&Usage::new(4, 2)),
        ));
        let seq = events(&frames);
        let kinds: Vec<&str> = seq.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        let sequence: Vec<u64> = seq
            .iter()
            .map(|(_, v)| v["sequence_number"].as_u64().unwrap())
            .collect();
        assert_eq!(sequence, (1..=14).collect::<Vec<_>>());
        assert_eq!(seq[0].1["response"]["id"], "resp_1");
        assert_eq!(seq[0].1["response"]["status"], "in_progress");
        assert_eq!(seq[4].1["delta"], "Hel");
        assert_eq!(seq[6].1["item"]["type"], "function_call");
        assert_eq!(seq[6].1["output_index"], 1);
        let completed = &seq[13].1["response"];
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["output"][0]["content"][0]["text"], "Hello");
        assert_eq!(completed["output"][1]["arguments"], "{\"c\":1}");
        assert_eq!(completed["usage"]["output_tokens"], 2);
    }

    #[test]
    fn errors_are_responses_error_events() {
        let request = ResponsesRequest::default();
        let mut encoder = ResponsesSseEncoder::new("resp_1".into(), "m", request);
        let frames = encoder.error(&GatewayError::RateLimited {
            message: "slow".into(),
            retry_after: None,
            scope: rustllmroute_types::RateLimitScope::Upstream,
        });
        let seq = events(&frames);
        assert_eq!(seq[0].0, "error");
        assert_eq!(seq[0].1["type"], "error");
        assert_eq!(seq[0].1["message"], "slow");
    }
}
