use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use http::{HeaderMap, HeaderValue};
use rustllmroute_catalog::cost_usd;
use rustllmroute_framer::sse::SseBlock;
use rustllmroute_guardrails::extract::{
    chat_request_texts, chat_response_texts, chat_response_tool_calls, messages_request_texts,
    messages_response_texts, messages_response_tool_calls, responses_output_texts,
    responses_output_tool_calls, responses_request_texts,
};
use rustllmroute_guardrails::{GuardrailSet, InputType};
use rustllmroute_providers::CallContext;
use rustllmroute_router::{
    Attempt, AttemptGuard, AttemptUsage, Endpoint, Outcome, RequestCtx, Router,
};
use rustllmroute_types::{
    AnthropicUsage, ChatChunk, ChatRequest, ChatResponse, ChunkChoice, Delta, EmbeddingInput,
    EmbeddingRequest, EmbeddingUsage, FunctionCallDelta, GatewayError, MessagesRequest,
    MessagesResponse, MessagesStreamEvent, RESPONSE_LIFECYCLE_EVENTS, ResponsesRequest,
    ToolCallDelta, Usage, responses_usage_to_openai,
};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::chat::{ChatOutcome, execute_chat};
use crate::deployment::Deployment;
use crate::embeddings::{EmbeddingOutcome, estimate_tokens, execute_embeddings};
use crate::guard::{self, Selection};
use crate::hooks::{
    AuthContext, CallRecord, CallStatus, CallTicket, Hooks, NoTicket, PreCallRequest, RequestKind,
};
use crate::http::{ChatStream, Completed, HttpClients, RawStream, UpstreamRaw};
use crate::messages::{chat_to_messages_response, estimate_input_tokens, messages_to_chat_request};
use crate::responses::{
    chat_to_responses_response, decode_response_id, encode_response_id, replay_as_events,
    responses_to_chat_request,
};
use crate::stream::{
    AnthropicSseEncoder, ChatStreamState, ClientEncoder, OpenAiSseEncoder, ResponsesSseEncoder,
};

/// Request parameters consumed by the gateway and never forwarded to a provider.
const GATEWAY_ONLY_PARAMS: &[&str] = &[
    "fallbacks",
    "num_retries",
    "guardrails",
    "litellm_metadata",
    "disable_fallbacks",
];

/// Prompts above this size are counted on a blocking thread.
const INLINE_COUNT_LIMIT: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct GatewaySettings {
    pub keepalive: Duration,
    pub stream_channel_capacity: usize,
}

impl Default for GatewaySettings {
    fn default() -> Self {
        Self {
            keepalive: Duration::from_secs(15),
            stream_channel_capacity: 16,
        }
    }
}

pub struct Gateway {
    pub router: Arc<Router<Deployment>>,
    pub http: HttpClients,
    pub hooks: Arc<dyn Hooks>,
    pub settings: GatewaySettings,
    pub guardrails: GuardrailSet,
}

pub enum ChatReply {
    Json {
        response: Box<ChatResponse>,
        /// Upstream bytes to forward verbatim when the response was not modified.
        raw: Option<Bytes>,
        headers: HeaderMap,
    },
    Stream {
        body: BoxStream<'static, Bytes>,
        headers: HeaderMap,
    },
}

/// A JSON body relayed or rendered for the client, with the gateway headers.
pub enum MessagesReply {
    Json {
        body: Bytes,
        headers: HeaderMap,
    },
    Stream {
        body: BoxStream<'static, Bytes>,
        headers: HeaderMap,
    },
}

pub type ResponsesReply = MessagesReply;

pub struct JsonReply {
    pub body: Bytes,
    pub headers: HeaderMap,
}

struct GatewayParams {
    body_fallbacks: Option<Vec<String>>,
    num_retries: Option<u32>,
    disable_fallbacks: bool,
    guardrails: Vec<String>,
}

fn take_gateway_params(extra: &mut serde_json::Map<String, Value>) -> GatewayParams {
    let params = GatewayParams {
        body_fallbacks: extra
            .get("fallbacks")
            .and_then(|value| serde_json::from_value(value.clone()).ok()),
        num_retries: extra
            .get("num_retries")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        disable_fallbacks: extra
            .get("disable_fallbacks")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        guardrails: extra
            .get("guardrails")
            .and_then(Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
    };
    for key in GATEWAY_ONLY_PARAMS {
        extra.remove(*key);
    }
    params
}

fn header(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

fn base_headers(
    request_id: &str,
    deployment: &Deployment,
    group: &str,
    retries: u32,
    fallbacks: u32,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    header(&mut headers, "x-litellm-call-id", request_id);
    header(&mut headers, "x-litellm-model-id", &deployment.id);
    header(&mut headers, "x-litellm-model-group", group);
    header(
        &mut headers,
        "x-litellm-model-api-base",
        deployment.api_base(),
    );
    header(&mut headers, "x-litellm-version", env!("CARGO_PKG_VERSION"));
    header(
        &mut headers,
        "x-litellm-attempted-retries",
        &retries.to_string(),
    );
    header(
        &mut headers,
        "x-litellm-attempted-fallbacks",
        &fallbacks.to_string(),
    );
    headers
}

fn fill_record(
    record: &mut CallRecord,
    deployment: &Deployment,
    group: &str,
    model: &str,
    retries: u32,
    fallbacks: u32,
) {
    record.model_group = Some(group.to_owned());
    record.deployment_id = Some(deployment.id.clone());
    record.provider = Some(deployment.provider.provider_name);
    record.provider_model = Some(model.to_owned());
    record.api_base = Some(deployment.api_base().to_owned());
    record.attempted_retries = retries;
    record.attempted_fallbacks = fallbacks;
}

fn account(
    record: &mut CallRecord,
    deployment: &Deployment,
    usage: Usage,
    headers: &mut HeaderMap,
) {
    let cost = cost_usd(&deployment.pricing, &usage);
    record.usage = usage;
    record.cost_usd = cost.total;
    record.pricing_known = cost.known;
    record.first_byte_at = Some(Instant::now());
    record.ended_at = SystemTime::now();
    header(
        headers,
        "x-litellm-response-cost",
        &format!("{}", cost.total),
    );
    header(
        headers,
        "x-litellm-overhead-duration-ms",
        &record.started_instant.elapsed().as_millis().to_string(),
    );
}

/// Request tags from the body `metadata.tags` plus the key's metadata, for tag-based routing.
fn request_tags(auth: &AuthContext, metadata: Option<&Value>) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    let body_tags = metadata.and_then(|m| m.get("tags"));
    let key_tags = auth.metadata.get("tags");
    for source in [body_tags, key_tags] {
        if let Some(Value::Array(values)) = source {
            tags.extend(values.iter().filter_map(Value::as_str).map(str::to_owned));
        }
    }
    tags.sort();
    tags.dedup();
    tags
}

/// Counts prompt tokens only when routing needs them; big prompts are counted off-thread.
async fn count_input_tokens(
    router: &Router<Deployment>,
    model: &str,
    aliases: &HashMap<String, String>,
    request: &ChatRequest,
) -> Option<u32> {
    if !router.needs_input_tokens(model, aliases) {
        return None;
    }
    let count = if rustllmroute_tokens::chat_text_bytes(request) > INLINE_COUNT_LIMIT {
        let owned = request.clone();
        let model = model.to_owned();
        tokio::task::spawn_blocking(move || rustllmroute_tokens::count_chat(&model, &owned))
            .await
            .unwrap_or(0)
    } else {
        rustllmroute_tokens::count_chat(model, request)
    };
    u32::try_from(count).ok()
}

fn snapshot(slots: Vec<&mut String>) -> Vec<String> {
    slots.into_iter().map(|s| s.clone()).collect()
}

fn attempt_usage(usage: &Usage) -> AttemptUsage {
    AttemptUsage {
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
    }
}

/// One attempt's result for an endpoint that may relay natively or translate through chat.
enum RelayOutcome {
    NativeComplete(Completed),
    NativeStream(RawStream),
    Translated(ChatOutcome),
}

fn translated_attempt(outcome: ChatOutcome, started: Instant) -> Attempt<RelayOutcome> {
    match outcome {
        ChatOutcome::Complete {
            response,
            raw,
            passthrough,
        } => {
            let usage = response.usage.as_ref().map(attempt_usage);
            Attempt {
                value: RelayOutcome::Translated(ChatOutcome::Complete {
                    response,
                    raw,
                    passthrough,
                }),
                usage,
                ttft: None,
            }
        }
        ChatOutcome::Stream(stream) => Attempt {
            value: RelayOutcome::Translated(ChatOutcome::Stream(stream)),
            usage: None,
            ttft: Some(started.elapsed()),
        },
    }
}

/// Per-format bookkeeping for natively relayed streams: usage, in-band errors, rewrites.
trait RelayBookkeeper: Send + 'static {
    /// Bytes to forward for this block (the block itself unless the format needs a rewrite).
    fn observe(&mut self, block: SseBlock) -> Bytes;
    fn usage(&self) -> Usage;
    fn take_error(&mut self) -> Option<GatewayError>;
    fn error_frames(&mut self, error: &GatewayError) -> Vec<Bytes>;
    fn keepalive(&self) -> Bytes;
}

#[derive(Default)]
struct MessagesRelay {
    usage: AnthropicUsage,
    error: Option<GatewayError>,
}

impl RelayBookkeeper for MessagesRelay {
    fn observe(&mut self, block: SseBlock) -> Bytes {
        match block.event.event.as_deref() {
            Some("message_start") | Some("message_delta") => {
                if let Ok(event) = serde_json::from_str::<MessagesStreamEvent>(&block.event.data) {
                    match event {
                        MessagesStreamEvent::MessageStart { message } => {
                            self.usage = message.usage;
                        }
                        MessagesStreamEvent::MessageDelta {
                            usage: Some(delta), ..
                        } => {
                            self.usage.output_tokens = delta.output_tokens;
                            if delta.input_tokens > 0 {
                                self.usage.input_tokens = delta.input_tokens;
                            }
                            if delta.cache_read_input_tokens.is_some() {
                                self.usage.cache_read_input_tokens = delta.cache_read_input_tokens;
                            }
                            if delta.cache_creation_input_tokens.is_some() {
                                self.usage.cache_creation_input_tokens =
                                    delta.cache_creation_input_tokens;
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some("error") => {
                tracing::warn!(data = %block.event.data, "upstream sent an error event mid-stream");
                self.error = Some(GatewayError::StreamAborted(block.event.data.clone()));
            }
            _ => {}
        }
        block.bytes
    }

    fn usage(&self) -> Usage {
        self.usage.to_openai()
    }

    fn take_error(&mut self) -> Option<GatewayError> {
        self.error.take()
    }

    fn error_frames(&mut self, error: &GatewayError) -> Vec<Bytes> {
        AnthropicSseEncoder::new("").error(error)
    }

    fn keepalive(&self) -> Bytes {
        AnthropicSseEncoder::new("").keepalive()
    }
}

struct ResponsesRelay {
    usage: Usage,
    deployment_id: Arc<str>,
    model: String,
    error: Option<GatewayError>,
}

impl RelayBookkeeper for ResponsesRelay {
    fn observe(&mut self, block: SseBlock) -> Bytes {
        let data = block.event.data.as_str();
        let kind = block.event.event.clone().or_else(|| {
            data.starts_with("{\"type\":\"response.")
                .then(|| serde_json::from_str::<Value>(data).ok())
                .flatten()
                .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_owned))
        });
        match kind.as_deref() {
            Some("error") => {
                tracing::warn!(data = %data, "upstream sent an error event mid-stream");
                self.error = Some(GatewayError::StreamAborted(data.to_owned()));
                block.bytes
            }
            Some(kind) if RESPONSE_LIFECYCLE_EVENTS.contains(&kind) => {
                let Ok(mut value) = serde_json::from_str::<Value>(data) else {
                    return block.bytes;
                };
                if let Some(response) = value.get_mut("response").and_then(Value::as_object_mut) {
                    if let Some(id) = response.get("id").and_then(Value::as_str) {
                        let encoded = encode_response_id(id, &self.deployment_id);
                        response.insert("id".to_owned(), Value::String(encoded));
                    }
                    response.insert("model".to_owned(), Value::String(self.model.clone()));
                    if matches!(kind, "response.completed" | "response.incomplete")
                        && let Some(usage) = response.get("usage")
                    {
                        self.usage = responses_usage_to_openai(usage);
                    }
                }
                Bytes::from(format!("event: {kind}\ndata: {value}\n\n"))
            }
            _ => block.bytes,
        }
    }

    fn usage(&self) -> Usage {
        self.usage.clone()
    }

    fn take_error(&mut self) -> Option<GatewayError> {
        self.error.take()
    }

    fn error_frames(&mut self, error: &GatewayError) -> Vec<Bytes> {
        let body = error.to_openai_body();
        let payload = serde_json::json!({
            "type": "error",
            "sequence_number": 0,
            "code": body.pointer("/error/code").cloned().unwrap_or(Value::Null),
            "message": body.pointer("/error/message").cloned().unwrap_or_else(|| Value::String(error.to_string())),
            "param": null,
        });
        vec![Bytes::from(format!("event: error\ndata: {payload}\n\n"))]
    }

    fn keepalive(&self) -> Bytes {
        Bytes::from_static(b": ping\n\n")
    }
}

impl Gateway {
    pub fn new(
        router: Arc<Router<Deployment>>,
        http: HttpClients,
        hooks: Arc<dyn Hooks>,
        settings: GatewaySettings,
        guardrails: GuardrailSet,
    ) -> Self {
        Self {
            router,
            http,
            hooks,
            settings,
            guardrails,
        }
    }

    async fn pre_call(
        &self,
        auth: &AuthContext,
        record: CallRecord,
        req: &PreCallRequest<'_>,
    ) -> Result<(Box<dyn CallTicket>, CallRecord), GatewayError> {
        match self.hooks.pre_call(auth, req).await {
            Ok(ticket) => Ok((ticket, record)),
            Err(error) => {
                self.finish_failed(Box::new(NoTicket), auth, record, &error);
                Err(error)
            }
        }
    }

    /// Guardrail selection and pre-call run, before any provider is chosen.
    async fn guard_request(
        &self,
        auth: &AuthContext,
        record: &CallRecord,
        body_names: &[String],
        slots: Vec<&mut String>,
        meta: &rustllmroute_guardrails::RequestMeta,
    ) -> Result<Selection, GatewayError> {
        let selection = guard::select(&self.guardrails, auth, body_names)?;
        guard::pre_call(&selection, slots, meta).await?;
        let _ = record;
        Ok(selection)
    }

    pub async fn chat(
        &self,
        auth: &AuthContext,
        request_id: &str,
        mut request: ChatRequest,
    ) -> Result<ChatReply, GatewayError> {
        let requested_model = request.model.clone();
        let record = CallRecord::start(
            request_id,
            RequestKind::ChatCompletion,
            &requested_model,
            request.stream,
        );
        let params = take_gateway_params(&mut request.extra);
        let meta = guard::meta(auth, request_id, &requested_model);
        let selection = match self
            .guard_request(
                auth,
                &record,
                &params.guardrails,
                chat_request_texts(&mut request),
                &meta,
            )
            .await
        {
            Ok(selection) => selection,
            Err(error) => {
                self.finish_failed(Box::new(NoTicket), auth, record, &error);
                return Err(error);
            }
        };
        if !selection.logging.is_empty() {
            guard::spawn_logging(
                &selection,
                InputType::Request,
                snapshot(chat_request_texts(&mut request)),
                Vec::new(),
                meta.clone(),
            );
        }
        let aliases: &HashMap<String, String> = &auth.model_aliases;
        let tags = request_tags(auth, request.metadata.as_ref());
        let input_tokens =
            count_input_tokens(&self.router, &requested_model, aliases, &request).await;
        let (ticket, mut record) = self
            .pre_call(
                auth,
                record,
                &PreCallRequest {
                    requested_model: &requested_model,
                    kind: RequestKind::ChatCompletion,
                    stream: request.stream,
                    estimated_input_tokens: input_tokens,
                    max_output_tokens: request.output_token_cap(),
                },
            )
            .await?;
        let mut ctx = RequestCtx::new(Endpoint::ChatCompletions, request.stream, aliases);
        ctx.body_fallbacks = params.body_fallbacks.as_deref();
        ctx.num_retries = params.num_retries;
        ctx.disable_fallbacks = params.disable_fallbacks;
        ctx.tags = &tags;
        ctx.input_tokens = input_tokens;
        let during_texts = if selection.during.is_empty() {
            Vec::new()
        } else {
            snapshot(chat_request_texts(&mut request))
        };
        let buffered = request.stream && selection.buffers_streams();
        let upstream_owned: ChatRequest;
        let req: &ChatRequest = if buffered {
            let mut owned = request.clone();
            owned.stream = false;
            upstream_owned = owned;
            &upstream_owned
        } else {
            &request
        };
        let http = &self.http;
        let id: &str = request_id;
        let routed = self
            .router
            .execute(&requested_model, &ctx, |selected| async move {
                let started = Instant::now();
                let outcome =
                    execute_chat(http, &selected.deployment, req, &selected.model, id).await?;
                Ok(match outcome {
                    ChatOutcome::Complete {
                        response,
                        raw,
                        passthrough,
                    } => {
                        let usage = response.usage.as_ref().map(attempt_usage);
                        Attempt {
                            value: ChatOutcome::Complete {
                                response,
                                raw,
                                passthrough,
                            },
                            usage,
                            ttft: None,
                        }
                    }
                    ChatOutcome::Stream(stream) => Attempt {
                        value: ChatOutcome::Stream(stream),
                        usage: None,
                        ttft: Some(started.elapsed()),
                    },
                })
            });
        let (result, during) =
            tokio::join!(routed, guard::during_call(&selection, during_texts, &meta));
        if let Err(error) = during {
            drop(result);
            self.finish_failed(ticket, auth, record, &error);
            return Err(error);
        }
        let Outcome {
            value,
            selected,
            attempted_retries,
            attempted_fallbacks,
            guard,
        } = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                self.finish_failed(ticket, auth, record, &error);
                return Err(error);
            }
        };
        let deployment = selected.deployment.clone();
        fill_record(
            &mut record,
            &deployment,
            &selected.group,
            &selected.model,
            attempted_retries,
            attempted_fallbacks,
        );
        let mut headers = base_headers(
            request_id,
            &deployment,
            &selected.group,
            attempted_retries,
            attempted_fallbacks,
        );
        ticket.response_headers(&mut headers);
        match value {
            ChatOutcome::Complete {
                mut response,
                raw,
                passthrough,
            } => {
                drop(guard);
                let usage = response.usage.clone().unwrap_or_default();
                if !selection.post.is_empty() || !selection.logging.is_empty() {
                    let calls = chat_response_tool_calls(&response);
                    if let Err(error) = guard::post_call(
                        &selection,
                        chat_response_texts(&mut response),
                        &calls,
                        &meta,
                    )
                    .await
                    {
                        record.usage = usage;
                        self.finish_failed(ticket, auth, record, &error);
                        return Err(error);
                    }
                    guard::spawn_logging(
                        &selection,
                        InputType::Response,
                        snapshot(chat_response_texts(&mut response)),
                        calls,
                        meta.clone(),
                    );
                }
                account(&mut record, &deployment, usage, &mut headers);
                let unchanged =
                    passthrough && selection.post.is_empty() && response.model == requested_model;
                response.model = requested_model.clone();
                crate::observe::complete(&self.hooks, ticket, auth, record);
                if request.stream {
                    return Ok(self.synthesised_stream(*response, headers));
                }
                Ok(ChatReply::Json {
                    response,
                    raw: unchanged.then_some(raw),
                    headers,
                })
            }
            ChatOutcome::Stream(stream) => {
                let include_usage = request.wants_stream_usage();
                let state = ChatStreamState::new(&requested_model, include_usage);
                record.first_byte_at = Some(Instant::now());
                let body = self.pump(
                    stream,
                    state,
                    OpenAiSseEncoder,
                    ticket,
                    auth.clone(),
                    record,
                    deployment,
                    guard,
                );
                Ok(ChatReply::Stream { body, headers })
            }
        }
    }

    /// `/v1/messages`: relayed natively to Anthropic-speaking deployments, translated through
    /// chat completions for everything else.
    pub async fn messages(
        &self,
        auth: &AuthContext,
        request_id: &str,
        mut request: MessagesRequest,
        forwarded: HeaderMap,
    ) -> Result<MessagesReply, GatewayError> {
        let requested_model = request.model.clone();
        let record = CallRecord::start(
            request_id,
            RequestKind::Messages,
            &requested_model,
            request.stream,
        );
        let params = take_gateway_params(&mut request.extra);
        let meta = guard::meta(auth, request_id, &requested_model);
        let selection = match self
            .guard_request(
                auth,
                &record,
                &params.guardrails,
                messages_request_texts(&mut request),
                &meta,
            )
            .await
        {
            Ok(selection) => selection,
            Err(error) => {
                self.finish_failed(Box::new(NoTicket), auth, record, &error);
                return Err(error);
            }
        };
        if !selection.logging.is_empty() {
            guard::spawn_logging(
                &selection,
                InputType::Request,
                snapshot(messages_request_texts(&mut request)),
                Vec::new(),
                meta.clone(),
            );
        }
        let aliases: &HashMap<String, String> = &auth.model_aliases;
        let tags = request_tags(auth, request.metadata.as_ref());
        let translated = messages_to_chat_request(&request, &requested_model);
        let input_tokens = match &translated {
            Ok(chat) => count_input_tokens(&self.router, &requested_model, aliases, chat).await,
            Err(_) => None,
        };
        let (ticket, mut record) = self
            .pre_call(
                auth,
                record,
                &PreCallRequest {
                    requested_model: &requested_model,
                    kind: RequestKind::Messages,
                    stream: request.stream,
                    estimated_input_tokens: input_tokens,
                    max_output_tokens: request.max_tokens,
                },
            )
            .await?;
        let mut ctx = RequestCtx::new(Endpoint::Messages, request.stream, aliases);
        ctx.body_fallbacks = params.body_fallbacks.as_deref();
        ctx.num_retries = params.num_retries;
        ctx.disable_fallbacks = params.disable_fallbacks;
        ctx.tags = &tags;
        ctx.input_tokens = input_tokens;
        let during_texts = if selection.during.is_empty() {
            Vec::new()
        } else {
            snapshot(messages_request_texts(&mut request))
        };
        let buffered = request.stream && selection.buffers_streams();
        let upstream_owned: MessagesRequest;
        let translated_owned: Result<ChatRequest, GatewayError>;
        let (req, translated): (&MessagesRequest, &Result<ChatRequest, GatewayError>) = if buffered
        {
            let mut owned = request.clone();
            owned.stream = false;
            upstream_owned = owned;
            translated_owned = translated.as_ref().map_err(Clone::clone).map(|chat| {
                let mut chat = chat.clone();
                chat.stream = false;
                chat
            });
            (&upstream_owned, &translated_owned)
        } else {
            (&request, &translated)
        };
        let http = &self.http;
        let forwarded = &forwarded;
        let id: &str = request_id;
        let routed = self
            .router
            .execute(&requested_model, &ctx, |selected| async move {
                let started = Instant::now();
                let deployment = &selected.deployment;
                if let Some(native) = &deployment.messages {
                    let mut cx =
                        CallContext::new(&deployment.provider, &selected.model, req.stream, id);
                    cx.forwarded_headers = forwarded.clone();
                    let call = native.build(req, &mut cx)?;
                    let relayed = http
                        .execute_relay(
                            &|status, headers, body| native.parse_error(status, headers, body),
                            call,
                            &cx,
                        )
                        .await?;
                    return Ok(match relayed {
                        UpstreamRaw::Completed(completed) => {
                            let usage = serde_json::from_slice::<MessagesResponse>(&completed.body)
                                .ok()
                                .map(|r| attempt_usage(&r.usage.to_openai()));
                            Attempt {
                                value: RelayOutcome::NativeComplete(completed),
                                usage,
                                ttft: None,
                            }
                        }
                        UpstreamRaw::Stream(stream) => Attempt {
                            value: RelayOutcome::NativeStream(stream),
                            usage: None,
                            ttft: Some(started.elapsed()),
                        },
                    });
                }
                let chat_request = match translated {
                    Ok(chat_request) => chat_request,
                    Err(error) => return Err(error.clone()),
                };
                let outcome =
                    execute_chat(http, deployment, chat_request, &selected.model, id).await?;
                Ok(translated_attempt(outcome, started))
            });
        let (result, during) =
            tokio::join!(routed, guard::during_call(&selection, during_texts, &meta));
        if let Err(error) = during {
            drop(result);
            self.finish_failed(ticket, auth, record, &error);
            return Err(error);
        }
        let Outcome {
            value,
            selected,
            attempted_retries,
            attempted_fallbacks,
            guard,
        } = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                self.finish_failed(ticket, auth, record, &error);
                return Err(error);
            }
        };
        let deployment = selected.deployment.clone();
        fill_record(
            &mut record,
            &deployment,
            &selected.group,
            &selected.model,
            attempted_retries,
            attempted_fallbacks,
        );
        let mut headers = base_headers(
            request_id,
            &deployment,
            &selected.group,
            attempted_retries,
            attempted_fallbacks,
        );
        ticket.response_headers(&mut headers);
        match value {
            RelayOutcome::NativeComplete(completed) => {
                drop(guard);
                let mut body: Value = serde_json::from_slice(&completed.body).map_err(|error| {
                    GatewayError::InvalidUpstreamResponse(format!(
                        "messages body is not JSON: {error}"
                    ))
                })?;
                let usage = serde_json::from_value::<MessagesResponse>(body.clone())
                    .map(|r| r.usage.to_openai())
                    .unwrap_or_default();
                if !selection.post.is_empty() || !selection.logging.is_empty() {
                    let calls = messages_response_tool_calls(&body);
                    if let Err(error) = guard::post_call(
                        &selection,
                        messages_response_texts(&mut body),
                        &calls,
                        &meta,
                    )
                    .await
                    {
                        record.usage = usage;
                        self.finish_failed(ticket, auth, record, &error);
                        return Err(error);
                    }
                    guard::spawn_logging(
                        &selection,
                        InputType::Response,
                        snapshot(messages_response_texts(&mut body)),
                        calls,
                        meta.clone(),
                    );
                }
                account(&mut record, &deployment, usage, &mut headers);
                crate::observe::complete(&self.hooks, ticket, auth, record);
                if request.stream {
                    let parsed: MessagesResponse =
                        serde_json::from_value(body).map_err(|error| {
                            GatewayError::InvalidUpstreamResponse(format!(
                                "messages body is not a Messages object: {error}"
                            ))
                        })?;
                    let cx =
                        CallContext::new(&deployment.provider, &selected.model, false, request_id);
                    let chat =
                        rustllmroute_providers::anthropic::response::messages_to_chat(parsed, &cx);
                    let mut encoder = AnthropicSseEncoder::new(&requested_model);
                    let state = ChatStreamState::new(&requested_model, false);
                    let frames = self.replay_complete(&chat, &mut encoder, state);
                    return Ok(MessagesReply::Stream {
                        body: futures_util::stream::iter(frames).boxed(),
                        headers,
                    });
                }
                let bytes = if selection.post.is_empty() {
                    completed.body
                } else {
                    Bytes::from(body.to_string())
                };
                Ok(MessagesReply::Json {
                    body: bytes,
                    headers,
                })
            }
            RelayOutcome::NativeStream(stream) => {
                record.first_byte_at = Some(Instant::now());
                let body = self.pump_relay(
                    stream,
                    MessagesRelay::default(),
                    ticket,
                    auth.clone(),
                    record,
                    deployment,
                    guard,
                );
                Ok(MessagesReply::Stream { body, headers })
            }
            RelayOutcome::Translated(ChatOutcome::Complete { mut response, .. }) => {
                drop(guard);
                let usage = response.usage.clone().unwrap_or_default();
                if !selection.post.is_empty() || !selection.logging.is_empty() {
                    let calls = chat_response_tool_calls(&response);
                    if let Err(error) = guard::post_call(
                        &selection,
                        chat_response_texts(&mut response),
                        &calls,
                        &meta,
                    )
                    .await
                    {
                        record.usage = usage;
                        self.finish_failed(ticket, auth, record, &error);
                        return Err(error);
                    }
                    guard::spawn_logging(
                        &selection,
                        InputType::Response,
                        snapshot(chat_response_texts(&mut response)),
                        calls,
                        meta.clone(),
                    );
                }
                account(&mut record, &deployment, usage, &mut headers);
                let converted = chat_to_messages_response(&response, &requested_model);
                crate::observe::complete(&self.hooks, ticket, auth, record);
                let body = serde_json::to_vec(&converted).map_err(|error| {
                    GatewayError::internal(format!("serialising messages response: {error}"))
                })?;
                if request.stream {
                    let mut encoder = AnthropicSseEncoder::new(&requested_model);
                    let state = ChatStreamState::new(&requested_model, false);
                    let frames = self.replay_complete(&response, &mut encoder, state);
                    return Ok(MessagesReply::Stream {
                        body: futures_util::stream::iter(frames).boxed(),
                        headers,
                    });
                }
                Ok(MessagesReply::Json {
                    body: Bytes::from(body),
                    headers,
                })
            }
            RelayOutcome::Translated(ChatOutcome::Stream(stream)) => {
                let state = ChatStreamState::new(&requested_model, false);
                record.first_byte_at = Some(Instant::now());
                let encoder = AnthropicSseEncoder::new(&requested_model);
                let body = self.pump(
                    stream,
                    state,
                    encoder,
                    ticket,
                    auth.clone(),
                    record,
                    deployment,
                    guard,
                );
                Ok(MessagesReply::Stream { body, headers })
            }
        }
    }

    /// `/v1/messages/count_tokens`: forwarded to native deployments, estimated for the rest.
    pub async fn count_tokens(
        &self,
        auth: &AuthContext,
        request_id: &str,
        request: MessagesRequest,
        forwarded: HeaderMap,
    ) -> Result<Bytes, GatewayError> {
        let requested_model = request.model.clone();
        let aliases: &HashMap<String, String> = &auth.model_aliases;
        let ctx = RequestCtx::new(Endpoint::Messages, false, aliases);
        let http = &self.http;
        let req = &request;
        let forwarded = &forwarded;
        let id: &str = request_id;
        let outcome = self
            .router
            .execute(&requested_model, &ctx, |selected| async move {
                let deployment = &selected.deployment;
                if let Some(native) = &deployment.messages {
                    let mut cx = CallContext::new(&deployment.provider, &selected.model, false, id);
                    cx.forwarded_headers = forwarded.clone();
                    if let Some(call) = native.build_count_tokens(req, &mut cx) {
                        let relayed = http
                            .execute_relay(
                                &|status, headers, body| native.parse_error(status, headers, body),
                                call,
                                &cx,
                            )
                            .await?;
                        return match relayed {
                            UpstreamRaw::Completed(completed) => Ok(Attempt {
                                value: completed.body,
                                usage: Some(AttemptUsage::default()),
                                ttft: None,
                            }),
                            UpstreamRaw::Stream(_) => Err(GatewayError::InvalidUpstreamResponse(
                                "count_tokens returned a stream".to_owned(),
                            )),
                        };
                    }
                }
                let counted = rustllmroute_tokens::count_messages(&selected.model, req);
                let estimate = serde_json::json!({ "input_tokens": counted.max(estimate_input_tokens(req).min(counted.max(1))) });
                Ok(Attempt {
                    value: Bytes::from(estimate.to_string()),
                    usage: Some(AttemptUsage::default()),
                    ttft: None,
                })
            })
            .await?;
        Ok(outcome.value)
    }

    /// `/v1/embeddings`: one or more upstream calls per attempt, merged in input order.
    pub async fn embeddings(
        &self,
        auth: &AuthContext,
        request_id: &str,
        mut request: EmbeddingRequest,
    ) -> Result<JsonReply, GatewayError> {
        let requested_model = request.model.clone();
        let record = CallRecord::start(request_id, RequestKind::Embedding, &requested_model, false);
        let params = take_gateway_params(&mut request.extra);
        let meta = guard::meta(auth, request_id, &requested_model);
        let input_slots: Vec<&mut String> = match &mut request.input {
            EmbeddingInput::Text(text) => vec![text],
            EmbeddingInput::Texts(texts) => texts.iter_mut().collect(),
            _ => Vec::new(),
        };
        let selection = match self
            .guard_request(auth, &record, &params.guardrails, input_slots, &meta)
            .await
        {
            Ok(selection) => selection,
            Err(error) => {
                self.finish_failed(Box::new(NoTicket), auth, record, &error);
                return Err(error);
            }
        };
        let estimated = estimate_tokens(&request.input);
        let tags = request_tags(auth, request.extra.get("metadata"));
        let (ticket, mut record) = self
            .pre_call(
                auth,
                record,
                &PreCallRequest {
                    requested_model: &requested_model,
                    kind: RequestKind::Embedding,
                    stream: false,
                    estimated_input_tokens: u32::try_from(estimated).ok(),
                    max_output_tokens: None,
                },
            )
            .await?;
        let aliases: &HashMap<String, String> = &auth.model_aliases;
        let mut ctx = RequestCtx::new(Endpoint::Embeddings, false, aliases);
        ctx.body_fallbacks = params.body_fallbacks.as_deref();
        ctx.num_retries = params.num_retries;
        ctx.disable_fallbacks = params.disable_fallbacks;
        ctx.tags = &tags;
        ctx.input_tokens = u32::try_from(estimated).ok();
        let _ = &selection;
        let http = &self.http;
        let req = &request;
        let id: &str = request_id;
        let result = self
            .router
            .execute(&requested_model, &ctx, |selected| async move {
                let outcome =
                    execute_embeddings(http, &selected.deployment, req, &selected.model, id)
                        .await?;
                let prompt_tokens = outcome.response.usage.prompt_tokens;
                Ok(Attempt {
                    value: outcome,
                    usage: Some(AttemptUsage {
                        prompt_tokens,
                        completion_tokens: 0,
                    }),
                    ttft: None,
                })
            })
            .await;
        let Outcome {
            value,
            selected,
            attempted_retries,
            attempted_fallbacks,
            guard,
        } = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                self.finish_failed(ticket, auth, record, &error);
                return Err(error);
            }
        };
        drop(guard);
        let deployment = selected.deployment.clone();
        fill_record(
            &mut record,
            &deployment,
            &selected.group,
            &selected.model,
            attempted_retries,
            attempted_fallbacks,
        );
        let mut headers = base_headers(
            request_id,
            &deployment,
            &selected.group,
            attempted_retries,
            attempted_fallbacks,
        );
        ticket.response_headers(&mut headers);
        let EmbeddingOutcome { mut response, raw } = value;
        let mut raw = raw;
        if response.usage.prompt_tokens == 0 {
            response.usage = EmbeddingUsage {
                prompt_tokens: estimated,
                total_tokens: estimated,
                extra: Default::default(),
            };
            raw = None;
        }
        let mut usage = Usage::new(response.usage.prompt_tokens, 0);
        usage.estimated = response.usage.prompt_tokens == estimated && raw.is_none();
        account(&mut record, &deployment, usage, &mut headers);
        let unchanged = raw.is_some() && response.model == requested_model;
        response.model = requested_model.clone();
        crate::observe::complete(&self.hooks, ticket, auth, record);
        let body = match (unchanged, raw) {
            (true, Some(raw)) => raw,
            _ => Bytes::from(serde_json::to_vec(&response).map_err(|error| {
                GatewayError::internal(format!("serialising embedding response: {error}"))
            })?),
        };
        Ok(JsonReply { body, headers })
    }

    /// `/v1/responses`: relayed natively where the provider serves it, bridged through chat
    /// completions elsewhere. Response ids encode the deployment so `previous_response_id`
    /// returns to the same backend.
    pub async fn responses(
        &self,
        auth: &AuthContext,
        request_id: &str,
        mut request: ResponsesRequest,
    ) -> Result<ResponsesReply, GatewayError> {
        let requested_model = request.model.clone();
        let record = CallRecord::start(
            request_id,
            RequestKind::Responses,
            &requested_model,
            request.stream,
        );
        let params = take_gateway_params(&mut request.extra);
        let meta = guard::meta(auth, request_id, &requested_model);
        let selection = match self
            .guard_request(
                auth,
                &record,
                &params.guardrails,
                responses_request_texts(&mut request),
                &meta,
            )
            .await
        {
            Ok(selection) => selection,
            Err(error) => {
                self.finish_failed(Box::new(NoTicket), auth, record, &error);
                return Err(error);
            }
        };
        if !selection.logging.is_empty() {
            guard::spawn_logging(
                &selection,
                InputType::Request,
                snapshot(responses_request_texts(&mut request)),
                Vec::new(),
                meta.clone(),
            );
        }
        let aliases: &HashMap<String, String> = &auth.model_aliases;
        let tags = request_tags(auth, request.metadata.as_ref());
        let translated = responses_to_chat_request(&request, &requested_model);
        let input_tokens = match &translated {
            Ok(chat) => count_input_tokens(&self.router, &requested_model, aliases, chat).await,
            Err(_) => None,
        };
        let (ticket, mut record) = self
            .pre_call(
                auth,
                record,
                &PreCallRequest {
                    requested_model: &requested_model,
                    kind: RequestKind::Responses,
                    stream: request.stream,
                    estimated_input_tokens: input_tokens,
                    max_output_tokens: request.max_output_tokens,
                },
            )
            .await?;
        let (route_model, upstream_previous) = match request
            .previous_response_id
            .as_deref()
            .and_then(decode_response_id)
        {
            Some((upstream, deployment_id)) => (deployment_id, Some(upstream)),
            None => (
                requested_model.clone(),
                request.previous_response_id.clone(),
            ),
        };
        let mut ctx = RequestCtx::new(Endpoint::Responses, request.stream, aliases);
        ctx.body_fallbacks = params.body_fallbacks.as_deref();
        ctx.num_retries = params.num_retries;
        ctx.disable_fallbacks = params.disable_fallbacks;
        ctx.tags = &tags;
        ctx.input_tokens = input_tokens;
        let during_texts = if selection.during.is_empty() {
            Vec::new()
        } else {
            snapshot(responses_request_texts(&mut request))
        };
        let buffered = request.stream && selection.buffers_streams();
        let upstream_owned: ResponsesRequest;
        let translated_owned: Result<ChatRequest, GatewayError>;
        let (req, translated): (&ResponsesRequest, &Result<ChatRequest, GatewayError>) = if buffered
        {
            let mut owned = request.clone();
            owned.stream = false;
            upstream_owned = owned;
            translated_owned = translated.as_ref().map_err(Clone::clone).map(|chat| {
                let mut chat = chat.clone();
                chat.stream = false;
                chat
            });
            (&upstream_owned, &translated_owned)
        } else {
            (&request, &translated)
        };
        let http = &self.http;
        let upstream_previous = &upstream_previous;
        let id: &str = request_id;
        let routed = self
            .router
            .execute(&route_model, &ctx, |selected| async move {
                let started = Instant::now();
                let deployment = &selected.deployment;
                if let Some(native) = &deployment.responses {
                    let mut cx =
                        CallContext::new(&deployment.provider, &selected.model, req.stream, id);
                    let mut native_request = req.clone();
                    native_request.previous_response_id = upstream_previous.clone();
                    let call = native.build(&native_request, &mut cx)?;
                    let relayed = http
                        .execute_relay(
                            &|status, headers, body| native.parse_error(status, headers, body),
                            call,
                            &cx,
                        )
                        .await?;
                    return Ok(match relayed {
                        UpstreamRaw::Completed(completed) => {
                            let usage = serde_json::from_slice::<Value>(&completed.body)
                                .ok()
                                .and_then(|v| v.get("usage").map(responses_usage_to_openai))
                                .map(|u| attempt_usage(&u));
                            Attempt {
                                value: RelayOutcome::NativeComplete(completed),
                                usage,
                                ttft: None,
                            }
                        }
                        UpstreamRaw::Stream(stream) => Attempt {
                            value: RelayOutcome::NativeStream(stream),
                            usage: None,
                            ttft: Some(started.elapsed()),
                        },
                    });
                }
                let chat_request = match translated {
                    Ok(chat_request) => chat_request,
                    Err(error) => return Err(error.clone()),
                };
                let outcome =
                    execute_chat(http, deployment, chat_request, &selected.model, id).await?;
                Ok(translated_attempt(outcome, started))
            });
        let (result, during) =
            tokio::join!(routed, guard::during_call(&selection, during_texts, &meta));
        if let Err(error) = during {
            drop(result);
            self.finish_failed(ticket, auth, record, &error);
            return Err(error);
        }
        let Outcome {
            value,
            selected,
            attempted_retries,
            attempted_fallbacks,
            guard,
        } = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                self.finish_failed(ticket, auth, record, &error);
                return Err(error);
            }
        };
        let deployment = selected.deployment.clone();
        fill_record(
            &mut record,
            &deployment,
            &selected.group,
            &selected.model,
            attempted_retries,
            attempted_fallbacks,
        );
        let mut headers = base_headers(
            request_id,
            &deployment,
            &selected.group,
            attempted_retries,
            attempted_fallbacks,
        );
        ticket.response_headers(&mut headers);
        match value {
            RelayOutcome::NativeComplete(completed) => {
                drop(guard);
                let mut body: Value = serde_json::from_slice(&completed.body).map_err(|error| {
                    GatewayError::InvalidUpstreamResponse(format!(
                        "responses body is not JSON: {error}"
                    ))
                })?;
                let usage = body
                    .get("usage")
                    .map(responses_usage_to_openai)
                    .unwrap_or_default();
                if !selection.post.is_empty() || !selection.logging.is_empty() {
                    let calls = responses_output_tool_calls(&body);
                    if let Err(error) = guard::post_call(
                        &selection,
                        responses_output_texts(&mut body),
                        &calls,
                        &meta,
                    )
                    .await
                    {
                        record.usage = usage;
                        self.finish_failed(ticket, auth, record, &error);
                        return Err(error);
                    }
                    guard::spawn_logging(
                        &selection,
                        InputType::Response,
                        snapshot(responses_output_texts(&mut body)),
                        calls,
                        meta.clone(),
                    );
                }
                account(&mut record, &deployment, usage, &mut headers);
                if let Some(object) = body.as_object_mut() {
                    if let Some(upstream_id) = object.get("id").and_then(Value::as_str) {
                        let encoded = encode_response_id(upstream_id, &deployment.id);
                        object.insert("id".to_owned(), Value::String(encoded));
                    }
                    object.insert("model".to_owned(), Value::String(requested_model.clone()));
                }
                crate::observe::complete(&self.hooks, ticket, auth, record);
                if request.stream {
                    return Ok(MessagesReply::Stream {
                        body: futures_util::stream::iter(replay_as_events(&body)).boxed(),
                        headers,
                    });
                }
                Ok(MessagesReply::Json {
                    body: Bytes::from(body.to_string()),
                    headers,
                })
            }
            RelayOutcome::NativeStream(stream) => {
                record.first_byte_at = Some(Instant::now());
                let keeper = ResponsesRelay {
                    usage: Usage::default(),
                    deployment_id: deployment.id.clone(),
                    model: requested_model.clone(),
                    error: None,
                };
                let body = self.pump_relay(
                    stream,
                    keeper,
                    ticket,
                    auth.clone(),
                    record,
                    deployment,
                    guard,
                );
                Ok(MessagesReply::Stream { body, headers })
            }
            RelayOutcome::Translated(ChatOutcome::Complete { mut response, .. }) => {
                drop(guard);
                let usage = response.usage.clone().unwrap_or_default();
                if !selection.post.is_empty() || !selection.logging.is_empty() {
                    let calls = chat_response_tool_calls(&response);
                    if let Err(error) = guard::post_call(
                        &selection,
                        chat_response_texts(&mut response),
                        &calls,
                        &meta,
                    )
                    .await
                    {
                        record.usage = usage;
                        self.finish_failed(ticket, auth, record, &error);
                        return Err(error);
                    }
                    guard::spawn_logging(
                        &selection,
                        InputType::Response,
                        snapshot(chat_response_texts(&mut response)),
                        calls,
                        meta.clone(),
                    );
                }
                account(&mut record, &deployment, usage, &mut headers);
                let response_id = format!("resp_{}", uuid::Uuid::now_v7().simple());
                crate::observe::complete(&self.hooks, ticket, auth, record);
                if request.stream {
                    let mut encoder =
                        ResponsesSseEncoder::new(response_id, &requested_model, request.clone());
                    let state = ChatStreamState::new(&requested_model, false);
                    let frames = self.replay_complete(&response, &mut encoder, state);
                    return Ok(MessagesReply::Stream {
                        body: futures_util::stream::iter(frames).boxed(),
                        headers,
                    });
                }
                let object =
                    chat_to_responses_response(&response, &request, &requested_model, &response_id);
                Ok(MessagesReply::Json {
                    body: Bytes::from(object.to_string()),
                    headers,
                })
            }
            RelayOutcome::Translated(ChatOutcome::Stream(stream)) => {
                let state = ChatStreamState::new(&requested_model, false);
                record.first_byte_at = Some(Instant::now());
                let response_id = format!("resp_{}", uuid::Uuid::now_v7().simple());
                let encoder =
                    ResponsesSseEncoder::new(response_id, &requested_model, request.clone());
                let body = self.pump(
                    stream,
                    state,
                    encoder,
                    ticket,
                    auth.clone(),
                    record,
                    deployment,
                    guard,
                );
                Ok(MessagesReply::Stream { body, headers })
            }
        }
    }

    fn finish_failed(
        &self,
        ticket: Box<dyn CallTicket>,
        auth: &AuthContext,
        mut record: CallRecord,
        error: &GatewayError,
    ) {
        record.http_status = error.status().as_u16();
        record.ended_at = SystemTime::now();
        record.status = CallStatus::Error(error.clone());
        crate::observe::complete(&self.hooks, ticket, auth, record);
    }

    fn complete_as_chunks(response: &ChatResponse) -> Vec<ChatChunk> {
        let mut chunks = Vec::new();
        for choice in &response.choices {
            let mut chunk = ChatChunk::new(
                response.id.clone(),
                response.created,
                response.model.clone(),
            );
            chunk.choices.push(ChunkChoice {
                index: choice.index,
                delta: Delta {
                    role: Some(choice.message.role.clone()),
                    content: choice.message.content.clone(),
                    reasoning_content: choice.message.reasoning_content.clone(),
                    tool_calls: choice.message.tool_calls.as_ref().map(|calls| {
                        calls
                            .iter()
                            .enumerate()
                            .map(|(index, call)| ToolCallDelta {
                                index: index as u32,
                                id: Some(call.id.clone()),
                                kind: Some(call.kind.clone()),
                                function: FunctionCallDelta {
                                    name: Some(call.function.name.clone()),
                                    arguments: call.function.arguments.clone(),
                                },
                                extra: Default::default(),
                            })
                            .collect()
                    }),
                    ..Default::default()
                },
                finish_reason: choice.finish_reason,
                ..Default::default()
            });
            chunks.push(chunk);
        }
        if let Some(usage) = &response.usage {
            let mut chunk = ChatChunk::new(
                response.id.clone(),
                response.created,
                response.model.clone(),
            );
            chunk.usage = Some(usage.clone());
            chunks.push(chunk);
        }
        chunks
    }

    /// A provider ignored `stream: true` (or guardrails buffered the call); replay the complete
    /// response as a stream.
    fn replay_complete<E: ClientEncoder>(
        &self,
        response: &ChatResponse,
        encoder: &mut E,
        mut state: ChatStreamState,
    ) -> Vec<Bytes> {
        let mut frames = Vec::new();
        for chunk in state.step(Self::complete_as_chunks(response)) {
            frames.extend(encoder.chunk(&chunk));
        }
        let tail = state.finish();
        frames.extend(encoder.finish(tail, state.usage()));
        frames
    }

    fn synthesised_stream(&self, response: ChatResponse, headers: HeaderMap) -> ChatReply {
        let mut encoder = OpenAiSseEncoder;
        let state = ChatStreamState::new(&response.model, response.usage.is_some());
        let frames = self.replay_complete(&response, &mut encoder, state);
        ChatReply::Stream {
            body: futures_util::stream::iter(frames).boxed(),
            headers,
        }
    }

    /// Spawns the pump that turns provider chunks into client frames. The task owns the stream
    /// state and the router guard, so client disconnects (receiver dropped) and upstream errors
    /// both end in exactly one `on_complete` with partial usage and one guard settlement.
    #[allow(clippy::too_many_arguments)]
    fn pump<E: ClientEncoder>(
        &self,
        stream: ChatStream,
        mut state: ChatStreamState,
        mut encoder: E,
        ticket: Box<dyn CallTicket>,
        auth: AuthContext,
        mut record: CallRecord,
        deployment: Arc<Deployment>,
        mut guard: AttemptGuard,
    ) -> BoxStream<'static, Bytes> {
        let (tx, rx) = mpsc::channel::<Bytes>(self.settings.stream_channel_capacity);
        let hooks = self.hooks.clone();
        let keepalive = self.settings.keepalive;
        tokio::spawn(async move {
            let ChatStream {
                first, mut rest, ..
            } = stream;
            let mut pending: Vec<Result<Vec<ChatChunk>, GatewayError>> = vec![Ok(first)];
            let mut status = CallStatus::Ok;
            'outer: loop {
                let item = if let Some(item) = pending.pop() {
                    Some(item)
                } else {
                    tokio::select! {
                        biased;
                        _ = tx.closed() => { status = CallStatus::ClientDisconnected; break 'outer; }
                        next = tokio::time::timeout(keepalive, rest.next()) => match next {
                            Ok(next) => next,
                            Err(_) => {
                                if tx.send(encoder.keepalive()).await.is_err() {
                                    status = CallStatus::ClientDisconnected;
                                    break 'outer;
                                }
                                continue 'outer;
                            }
                        },
                    }
                };
                match item {
                    Some(Ok(chunks)) => {
                        for chunk in state.step(chunks) {
                            for frame in encoder.chunk(&chunk) {
                                if tx.send(frame).await.is_err() {
                                    status = CallStatus::ClientDisconnected;
                                    break 'outer;
                                }
                            }
                        }
                    }
                    Some(Err(error)) => {
                        tracing::warn!(error = %error, "upstream stream failed mid-way");
                        for frame in encoder.error(&error) {
                            let _ = tx.send(frame).await;
                        }
                        status = CallStatus::Error(error);
                        break 'outer;
                    }
                    None => {
                        let tail = state.finish();
                        for frame in encoder.finish(tail, state.usage()) {
                            if tx.send(frame).await.is_err() {
                                status = CallStatus::ClientDisconnected;
                                break 'outer;
                            }
                        }
                        break 'outer;
                    }
                }
            }
            drop(rest);
            let usage: Usage = state.usage().cloned().unwrap_or_default();
            settle(&mut guard, &status, &usage);
            drop(guard);
            finalize(&mut record, &deployment, usage, status);
            crate::observe::complete(&hooks, ticket, &auth, record);
        });
        tokio_stream_receiver(rx)
    }

    /// Native relay: forwards blocks byte-for-byte, with the bookkeeper reading (and, where the
    /// format needs it, rewriting) only the events that matter for accounting.
    #[allow(clippy::too_many_arguments)]
    fn pump_relay<B: RelayBookkeeper>(
        &self,
        stream: RawStream,
        mut keeper: B,
        ticket: Box<dyn CallTicket>,
        auth: AuthContext,
        mut record: CallRecord,
        deployment: Arc<Deployment>,
        mut guard: AttemptGuard,
    ) -> BoxStream<'static, Bytes> {
        let (tx, rx) = mpsc::channel::<Bytes>(self.settings.stream_channel_capacity);
        let hooks = self.hooks.clone();
        let keepalive = self.settings.keepalive;
        tokio::spawn(async move {
            let RawStream {
                first, mut rest, ..
            } = stream;
            let mut pending: Vec<Result<SseBlock, GatewayError>> = vec![Ok(first)];
            let mut status = CallStatus::Ok;
            'outer: loop {
                let item = if let Some(item) = pending.pop() {
                    Some(item)
                } else {
                    tokio::select! {
                        biased;
                        _ = tx.closed() => { status = CallStatus::ClientDisconnected; break 'outer; }
                        next = tokio::time::timeout(keepalive, rest.next()) => match next {
                            Ok(next) => next,
                            Err(_) => {
                                if tx.send(keeper.keepalive()).await.is_err() {
                                    status = CallStatus::ClientDisconnected;
                                    break 'outer;
                                }
                                continue 'outer;
                            }
                        },
                    }
                };
                match item {
                    Some(Ok(block)) => {
                        let bytes = keeper.observe(block);
                        if tx.send(bytes).await.is_err() {
                            status = CallStatus::ClientDisconnected;
                            break 'outer;
                        }
                    }
                    Some(Err(error)) => {
                        tracing::warn!(error = %error, "upstream relay failed mid-way");
                        for frame in keeper.error_frames(&error) {
                            let _ = tx.send(frame).await;
                        }
                        status = CallStatus::Error(error);
                        break 'outer;
                    }
                    None => break 'outer,
                }
            }
            drop(rest);
            if matches!(status, CallStatus::Ok)
                && let Some(error) = keeper.take_error()
            {
                status = CallStatus::Error(error);
            }
            let usage = keeper.usage();
            settle(&mut guard, &status, &usage);
            drop(guard);
            finalize(&mut record, &deployment, usage, status);
            crate::observe::complete(&hooks, ticket, &auth, record);
        });
        tokio_stream_receiver(rx)
    }
}

fn settle(guard: &mut AttemptGuard, status: &CallStatus, usage: &Usage) {
    match status {
        CallStatus::Error(error) => guard.fail(error),
        CallStatus::Ok | CallStatus::ClientDisconnected => {
            guard.complete(Some(attempt_usage(usage)), None)
        }
    }
}

fn finalize(record: &mut CallRecord, deployment: &Deployment, usage: Usage, status: CallStatus) {
    let cost = cost_usd(&deployment.pricing, &usage);
    record.usage = usage;
    record.cost_usd = cost.total;
    record.pricing_known = cost.known;
    record.ended_at = SystemTime::now();
    if let CallStatus::Error(error) = &status {
        record.http_status = error.status().as_u16();
    }
    record.status = status;
}

fn tokio_stream_receiver(rx: mpsc::Receiver<Bytes>) -> BoxStream<'static, Bytes> {
    futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    })
    .boxed()
}
