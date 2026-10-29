//! Guardrail orchestration: which guardrails run for a request and when.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use rustllmroute_guardrails::{
    Guardrail, GuardrailSet, InputType, JudgeClient, Mode, RequestMeta, ToolCallRef, run_readonly,
    run_sequential,
};
use rustllmroute_router::{Endpoint, RequestCtx, Router};
use rustllmroute_types::{ChatMessage, ChatRequest, GatewayError, role};
use serde_json::Value;

use crate::chat::{ChatOutcome, execute_chat};
use crate::deployment::Deployment;
use crate::hooks::AuthContext;
use crate::http::HttpClients;

/// The guardrails selected for one request, grouped by when they run.
#[derive(Clone, Default)]
pub struct Selection {
    pub pre: Vec<Arc<dyn Guardrail>>,
    pub during: Vec<Arc<dyn Guardrail>>,
    pub post: Vec<Arc<dyn Guardrail>>,
    pub logging: Vec<Arc<dyn Guardrail>>,
}

impl Selection {
    pub fn is_empty(&self) -> bool {
        self.pre.is_empty()
            && self.during.is_empty()
            && self.post.is_empty()
            && self.logging.is_empty()
    }

    /// Post-call guardrails need the whole response, so streams are buffered upstream.
    pub fn buffers_streams(&self) -> bool {
        !self.post.is_empty()
    }
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Validates the body's `guardrails` names and resolves the active set per mode.
pub fn select(
    set: &GuardrailSet,
    auth: &AuthContext,
    body_names: &[String],
) -> Result<Selection, GatewayError> {
    if set.is_empty() {
        if !body_names.is_empty() {
            return Err(GatewayError::BadRequest {
                message: "no guardrails are configured on this gateway".to_owned(),
                param: Some("guardrails".to_owned()),
                code: None,
            });
        }
        return Ok(Selection::default());
    }
    set.validate_names(body_names)?;
    let key_requested = string_list(auth.metadata.get("guardrails"));
    let key_disabled = string_list(auth.metadata.get("disabled_guardrails"));
    let pick = |mode| set.select(mode, body_names, &key_requested, &key_disabled);
    Ok(Selection {
        pre: pick(Mode::PreCall),
        during: pick(Mode::DuringCall),
        post: pick(Mode::PostCall),
        logging: pick(Mode::LoggingOnly),
    })
}

pub fn meta(auth: &AuthContext, request_id: &str, model: &str) -> RequestMeta {
    RequestMeta {
        request_id: request_id.to_owned(),
        model: model.to_owned(),
        key_hash: auth.key_hash.as_ref().map(|h| h.to_string()),
        user_id: auth.user_id.clone(),
        team_id: auth.team_id.clone(),
    }
}

/// Runs pre-call guardrails in config order over the request's text slots (masks write back).
pub async fn pre_call(
    selection: &Selection,
    slots: Vec<&mut String>,
    meta: &RequestMeta,
) -> Result<(), GatewayError> {
    if selection.pre.is_empty() {
        return Ok(());
    }
    run_sequential(
        &selection.pre,
        Mode::PreCall,
        InputType::Request,
        slots,
        &[],
        meta,
    )
    .await
}

/// Runs during-call guardrails on a snapshot; the caller races this with the provider call.
pub async fn during_call(
    selection: &Selection,
    texts: Vec<String>,
    meta: &RequestMeta,
) -> Result<(), GatewayError> {
    if selection.during.is_empty() {
        return Ok(());
    }
    run_readonly(
        &selection.during,
        Mode::DuringCall,
        InputType::Request,
        &texts,
        &[],
        meta,
    )
    .await
}

/// Runs post-call guardrails over the response's text slots and tool calls.
pub async fn post_call(
    selection: &Selection,
    slots: Vec<&mut String>,
    tool_calls: &[ToolCallRef],
    meta: &RequestMeta,
) -> Result<(), GatewayError> {
    if selection.post.is_empty() {
        return Ok(());
    }
    run_sequential(
        &selection.post,
        Mode::PostCall,
        InputType::Response,
        slots,
        tool_calls,
        meta,
    )
    .await
}

/// Runs logging-only guardrails detached; outcomes are logged, never enforced.
pub fn spawn_logging(
    selection: &Selection,
    input_type: InputType,
    texts: Vec<String>,
    tool_calls: Vec<ToolCallRef>,
    meta: RequestMeta,
) {
    if selection.logging.is_empty() {
        return;
    }
    let guards = selection.logging.clone();
    tokio::spawn(async move {
        for guard in guards {
            let outcome = guard
                .apply(rustllmroute_guardrails::GuardrailInput {
                    input_type,
                    texts: &texts,
                    tool_calls: &tool_calls,
                    meta: &meta,
                })
                .await;
            match outcome {
                Ok(rustllmroute_guardrails::Outcome::Allow) => {}
                Ok(rustllmroute_guardrails::Outcome::Modify { .. }) => {
                    tracing::info!(guardrail = guard.name(), request_id = %meta.request_id, "logging-only guardrail would have modified the content");
                }
                Ok(rustllmroute_guardrails::Outcome::Block { reason }) => {
                    tracing::warn!(guardrail = guard.name(), request_id = %meta.request_id, reason, "logging-only guardrail would have blocked the request");
                }
                Err(error) => {
                    tracing::warn!(guardrail = guard.name(), request_id = %meta.request_id, error = %error, "logging-only guardrail failed");
                }
            }
        }
    });
}

/// `llm_judge` completes prompts through the gateway's own router (no hooks, no spend row).
pub struct JudgeExecutor {
    pub router: Arc<Router<Deployment>>,
    pub http: HttpClients,
}

impl JudgeClient for JudgeExecutor {
    fn complete<'a>(
        &'a self,
        model: &'a str,
        prompt: &'a str,
        max_tokens: u32,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            let aliases: HashMap<String, String> = HashMap::new();
            let ctx = RequestCtx::new(Endpoint::ChatCompletions, false, &aliases);
            let request = ChatRequest {
                model: model.to_owned(),
                messages: vec![ChatMessage::text(role::USER, prompt)],
                max_tokens: Some(max_tokens),
                temperature: Some(0.0),
                ..ChatRequest::default()
            };
            let request_id = format!("judge-{}", uuid::Uuid::now_v7().simple());
            let http = &self.http;
            let req = &request;
            let id = request_id.as_str();
            let outcome = self
                .router
                .execute(model, &ctx, |selected| async move {
                    let outcome =
                        execute_chat(http, &selected.deployment, req, &selected.model, id).await?;
                    match outcome {
                        ChatOutcome::Complete { response, .. } => {
                            let usage = response.usage.as_ref().map(|u| {
                                rustllmroute_router::AttemptUsage {
                                    prompt_tokens: u.prompt_tokens,
                                    completion_tokens: u.completion_tokens,
                                }
                            });
                            Ok(rustllmroute_router::Attempt {
                                value: *response,
                                usage,
                                ttft: None,
                            })
                        }
                        ChatOutcome::Stream(_) => Err(GatewayError::InvalidUpstreamResponse(
                            "judge model streamed a non-streaming request".to_owned(),
                        )),
                    }
                })
                .await
                .map_err(|error| error.to_string())?;
            let text = outcome
                .value
                .choices
                .first()
                .and_then(|choice| choice.message.content.clone())
                .unwrap_or_default();
            Ok(text)
        })
    }
}
