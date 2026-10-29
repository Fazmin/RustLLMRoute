use std::collections::BTreeMap;

use rustllmroute_types::{
    AssistantMessage, ChatChunk, ChatResponse, Choice, ChunkChoice, Delta, FinishReason,
    FunctionCall, ToolCall, Usage, role,
};

#[derive(Debug, Default)]
struct ToolCallAcc {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

#[derive(Debug, Default)]
struct ChoiceAcc {
    text: String,
    reasoning: String,
    refusal: Option<String>,
    tool_calls: BTreeMap<u32, ToolCallAcc>,
    finish: Option<FinishReason>,
}

/// Normalises provider chunks into what OpenAI clients expect and accumulates the full response
/// for accounting: primes `role`, withholds `finish_reason` until the end, records usage and emits
/// it only when the client asked, and re-stamps the model name only when it differs.
#[derive(Debug)]
pub struct ChatStreamState {
    id: Option<String>,
    model: Option<String>,
    created: u64,
    role_sent: bool,
    choices: BTreeMap<u32, ChoiceAcc>,
    usage: Option<Usage>,
    rewrite_model: Option<String>,
    include_usage_to_client: bool,
    finished: bool,
}

impl ChatStreamState {
    pub fn new(requested_model: &str, include_usage_to_client: bool) -> Self {
        Self {
            id: None,
            model: None,
            created: crate::now_epoch_secs(),
            role_sent: false,
            choices: BTreeMap::new(),
            usage: None,
            rewrite_model: Some(requested_model.to_owned()),
            include_usage_to_client,
            finished: false,
        }
    }

    pub fn usage(&self) -> Option<&Usage> {
        self.usage.as_ref()
    }

    /// Processes one batch of provider chunks and returns the chunks to send to the client.
    pub fn step(&mut self, chunks: Vec<ChatChunk>) -> Vec<ChatChunk> {
        let mut out = Vec::with_capacity(chunks.len());
        for mut chunk in chunks {
            if self.id.is_none() && !chunk.id.is_empty() {
                self.id = Some(chunk.id.clone());
                self.model = Some(chunk.model.clone());
                if chunk.created > 0 {
                    self.created = chunk.created;
                }
            }
            if let Some(model) = &self.rewrite_model
                && chunk.model != *model
            {
                chunk.model = model.clone();
            }
            if let Some(usage) = chunk.usage.take() {
                self.record_usage(usage);
            }
            let mut keep = Vec::with_capacity(chunk.choices.len());
            for mut choice in chunk.choices.drain(..) {
                let acc = self.choices.entry(choice.index).or_default();
                if let Some(finish) = choice.finish_reason.take() {
                    acc.finish = Some(finish);
                }
                accumulate(acc, &choice.delta);
                if choice.delta.is_empty() {
                    continue;
                }
                if !self.role_sent && choice.index == 0 {
                    choice
                        .delta
                        .role
                        .get_or_insert_with(|| role::ASSISTANT.to_owned());
                    self.role_sent = true;
                }
                keep.push(choice);
            }
            if keep.is_empty() && chunk.extra.is_empty() {
                continue;
            }
            chunk.choices = keep;
            out.push(chunk);
        }
        out
    }

    fn record_usage(&mut self, usage: Usage) {
        let empty = usage.prompt_tokens == 0 && usage.completion_tokens == 0;
        if !(empty && self.usage.is_some()) {
            self.usage = Some(usage);
        }
    }

    /// Emits the held finish chunks and, if requested, the usage chunk. Idempotent.
    pub fn finish(&mut self) -> Vec<ChatChunk> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let id = self
            .id
            .clone()
            .unwrap_or_else(|| format!("chatcmpl-{}", crate::new_request_id()));
        let model = self
            .rewrite_model
            .clone()
            .or_else(|| self.model.clone())
            .unwrap_or_default();
        let mut out = Vec::new();
        for (index, acc) in &self.choices {
            let finish = acc.finish.unwrap_or(if acc.tool_calls.is_empty() {
                FinishReason::Stop
            } else {
                FinishReason::ToolCalls
            });
            let mut chunk = ChatChunk::new(id.clone(), self.created, model.clone());
            chunk.choices.push(ChunkChoice {
                index: *index,
                delta: Delta::default(),
                finish_reason: Some(finish),
                ..ChunkChoice::default()
            });
            out.push(chunk);
        }
        if self.include_usage_to_client
            && let Some(usage) = &self.usage
        {
            let mut chunk = ChatChunk::new(id, self.created, model);
            chunk.usage = Some(usage.clone());
            out.push(chunk);
        }
        out
    }

    /// The complete response as if the call had not streamed, for logging and guardrails.
    pub fn assemble(&self) -> ChatResponse {
        let choices = self
            .choices
            .iter()
            .map(|(index, acc)| Choice {
                index: *index,
                message: AssistantMessage {
                    content: (!acc.text.is_empty()).then(|| acc.text.clone()),
                    reasoning_content: (!acc.reasoning.is_empty()).then(|| acc.reasoning.clone()),
                    refusal: acc.refusal.clone(),
                    tool_calls: (!acc.tool_calls.is_empty()).then(|| {
                        acc.tool_calls
                            .values()
                            .map(|call| ToolCall {
                                id: call.id.clone().unwrap_or_default(),
                                kind: "function".to_owned(),
                                function: FunctionCall {
                                    name: call.name.clone().unwrap_or_default(),
                                    arguments: call.arguments.clone(),
                                },
                                extra: Default::default(),
                            })
                            .collect()
                    }),
                    ..AssistantMessage::default()
                },
                finish_reason: acc.finish,
                logprobs: None,
                extra: Default::default(),
            })
            .collect();
        ChatResponse {
            id: self.id.clone().unwrap_or_default(),
            object: "chat.completion".to_owned(),
            created: self.created,
            model: self
                .rewrite_model
                .clone()
                .or_else(|| self.model.clone())
                .unwrap_or_default(),
            choices,
            usage: self.usage.clone(),
            system_fingerprint: None,
            extra: Default::default(),
        }
    }

    pub fn completion_text_len(&self) -> usize {
        self.choices
            .values()
            .map(|acc| acc.text.len() + acc.reasoning.len())
            .sum()
    }
}

fn accumulate(acc: &mut ChoiceAcc, delta: &Delta) {
    if let Some(text) = &delta.content {
        acc.text.push_str(text);
    }
    if let Some(text) = &delta.reasoning_content {
        acc.reasoning.push_str(text);
    }
    if let Some(refusal) = &delta.refusal {
        acc.refusal
            .get_or_insert_with(String::new)
            .push_str(refusal);
    }
    if let Some(calls) = &delta.tool_calls {
        for call in calls {
            let slot = acc.tool_calls.entry(call.index).or_default();
            if let Some(id) = &call.id {
                slot.id = Some(id.clone());
            }
            if let Some(name) = &call.function.name {
                slot.name = Some(name.clone());
            }
            slot.arguments.push_str(&call.function.arguments);
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use rustllmroute_types::{FunctionCallDelta, ToolCallDelta};

    use super::*;

    fn chunk(content: Option<&str>, finish: Option<FinishReason>) -> ChatChunk {
        let mut chunk = ChatChunk::new("c1", 5, "provider-model");
        chunk.choices.push(ChunkChoice {
            index: 0,
            delta: Delta {
                content: content.map(str::to_owned),
                ..Delta::default()
            },
            finish_reason: finish,
            ..ChunkChoice::default()
        });
        chunk
    }

    #[rstest]
    fn primes_role_holds_finish_and_restamps_model() {
        let mut state = ChatStreamState::new("public", false);
        let out = state.step(vec![chunk(Some("Hel"), None)]);
        assert_eq!(out[0].choices[0].delta.role.as_deref(), Some("assistant"));
        assert_eq!(out[0].model, "public");
        let out = state.step(vec![chunk(Some("lo"), Some(FinishReason::Stop))]);
        assert_eq!(out.len(), 1);
        assert!(
            out[0].choices[0].finish_reason.is_none(),
            "finish is withheld"
        );
        assert!(out[0].choices[0].delta.role.is_none(), "role only once");
        let out = state.step(vec![chunk(None, Some(FinishReason::Stop))]);
        assert!(out.is_empty(), "finish-only chunks are swallowed");
        let tail = state.finish();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].choices[0].finish_reason, Some(FinishReason::Stop));
        assert_eq!(
            state.assemble().choices[0].message.content.as_deref(),
            Some("Hello")
        );
        assert!(state.finish().is_empty(), "finish is idempotent");
    }

    #[rstest]
    fn usage_chunk_is_recorded_and_only_forwarded_on_request() {
        for (wanted, expected_tail) in [(false, 1), (true, 2)] {
            let mut state = ChatStreamState::new("m", wanted);
            state.step(vec![chunk(Some("x"), None)]);
            let mut usage_chunk = ChatChunk::new("c1", 5, "m");
            usage_chunk.usage = Some(Usage::new(3, 1));
            assert!(state.step(vec![usage_chunk]).is_empty());
            assert_eq!(state.usage().map(|u| u.total_tokens), Some(4));
            assert_eq!(state.finish().len(), expected_tail);
        }
    }

    #[rstest]
    fn tool_call_fragments_are_joined_by_index() {
        let mut state = ChatStreamState::new("m", false);
        let mut first = ChatChunk::new("c", 1, "m");
        first.choices.push(ChunkChoice {
            delta: Delta {
                tool_calls: Some(vec![ToolCallDelta {
                    index: 0,
                    id: Some("call_1".into()),
                    kind: Some("function".into()),
                    function: FunctionCallDelta {
                        name: Some("get_weather".into()),
                        arguments: "{\"ci".into(),
                    },
                    ..ToolCallDelta::default()
                }]),
                ..Delta::default()
            },
            ..ChunkChoice::default()
        });
        let mut second = ChatChunk::new("c", 1, "m");
        second.choices.push(ChunkChoice {
            delta: Delta {
                tool_calls: Some(vec![ToolCallDelta {
                    index: 0,
                    function: FunctionCallDelta {
                        name: None,
                        arguments: "ty\":\"Oslo\"}".into(),
                    },
                    ..ToolCallDelta::default()
                }]),
                ..Delta::default()
            },
            ..ChunkChoice::default()
        });
        state.step(vec![first, second]);
        let tail = state.finish();
        assert_eq!(
            tail[0].choices[0].finish_reason,
            Some(FinishReason::ToolCalls)
        );
        let calls = state.assemble().choices[0]
            .message
            .tool_calls
            .clone()
            .unwrap();
        assert_eq!(calls[0].function.arguments, "{\"city\":\"Oslo\"}");
        assert_eq!(calls[0].id, "call_1");
    }
}
