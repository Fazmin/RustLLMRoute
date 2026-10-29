//! Guardrails: a trait, per-request selection, text extraction and the built-in implementations.
//! Orchestration (when each mode runs, what happens on block) lives in the core pipeline.

pub mod builtin;
pub mod extract;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub use rustllmroute_config::GuardrailSpec;
use rustllmroute_types::GatewayError;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    PreCall,
    DuringCall,
    PostCall,
    LoggingOnly,
}

impl Mode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pre_call" => Some(Self::PreCall),
            "during_call" => Some(Self::DuringCall),
            "post_call" => Some(Self::PostCall),
            "logging_only" => Some(Self::LoggingOnly),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreCall => "pre_call",
            Self::DuringCall => "during_call",
            Self::PostCall => "post_call",
            Self::LoggingOnly => "logging_only",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputType {
    Request,
    Response,
}

impl InputType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Response => "response",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnError {
    FailClosed,
    FailOpen,
}

/// Who and what is being checked; forwarded to external guardrails as metadata.
#[derive(Clone, Debug, Default)]
pub struct RequestMeta {
    pub request_id: String,
    pub model: String,
    pub key_hash: Option<String>,
    pub user_id: Option<String>,
    pub team_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCallRef {
    pub name: String,
    pub arguments: String,
}

pub struct GuardrailInput<'a> {
    pub input_type: InputType,
    pub texts: &'a [String],
    pub tool_calls: &'a [ToolCallRef],
    pub meta: &'a RequestMeta,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Allow,
    /// Replacement texts, one per input text and in the same order.
    Modify {
        texts: Vec<String>,
    },
    Block {
        reason: String,
    },
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct GuardrailError(pub String);

#[async_trait::async_trait]
pub trait Guardrail: Send + Sync {
    fn name(&self) -> &str;
    fn modes(&self) -> &[Mode];
    fn default_on(&self) -> bool;
    fn on_error(&self) -> OnError {
        OnError::FailClosed
    }
    async fn apply(&self, input: GuardrailInput<'_>) -> Result<Outcome, GuardrailError>;
}

/// Completes a prompt through the gateway's own router, for `llm_judge`.
pub trait JudgeClient: Send + Sync {
    fn complete<'a>(
        &'a self,
        model: &'a str,
        prompt: &'a str,
        max_tokens: u32,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;
}

/// Settings every guardrail reads from `litellm_params`.
#[derive(Clone, Debug)]
pub struct Common {
    pub name: String,
    pub modes: Vec<Mode>,
    pub default_on: bool,
    pub on_error: OnError,
}

impl Common {
    pub fn from_spec(spec: &GuardrailSpec, default_modes: &[Mode]) -> Result<Self, String> {
        let params = &spec.litellm_params;
        let modes = match params.get("mode") {
            None => default_modes.to_vec(),
            Some(Value::String(mode)) => vec![Mode::parse(mode).ok_or_else(|| {
                format!("guardrail {}: unknown mode {mode:?}", spec.guardrail_name)
            })?],
            Some(Value::Array(modes)) => modes
                .iter()
                .map(|mode| {
                    mode.as_str().and_then(Mode::parse).ok_or_else(|| {
                        format!("guardrail {}: unknown mode {mode}", spec.guardrail_name)
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(other) => {
                return Err(format!(
                    "guardrail {}: mode must be a string or a list, got {other}",
                    spec.guardrail_name
                ));
            }
        };
        let on_error = match params
            .get("on_error")
            .or_else(|| params.get("unreachable_fallback"))
            .and_then(Value::as_str)
        {
            Some("fail_open") => OnError::FailOpen,
            Some("fail_closed") | None => OnError::FailClosed,
            Some(other) => {
                return Err(format!(
                    "guardrail {}: on_error must be fail_open or fail_closed, got {other:?}",
                    spec.guardrail_name
                ));
            }
        };
        Ok(Self {
            name: spec.guardrail_name.clone(),
            modes,
            default_on: params
                .get("default_on")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            on_error,
        })
    }
}

/// The configured guardrails and the per-request selection rules.
#[derive(Clone, Default)]
pub struct GuardrailSet {
    entries: Vec<Arc<dyn Guardrail>>,
}

impl GuardrailSet {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn from_specs(
        specs: &[GuardrailSpec],
        judge: Option<Arc<dyn JudgeClient>>,
    ) -> Result<Self, String> {
        let mut entries: Vec<Arc<dyn Guardrail>> = Vec::with_capacity(specs.len());
        for spec in specs {
            if entries.iter().any(|g| g.name() == spec.guardrail_name) {
                return Err(format!(
                    "guardrail {} is defined twice",
                    spec.guardrail_name
                ));
            }
            entries.push(builtin::build(spec, judge.clone())?);
        }
        Ok(Self { entries })
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|g| g.name()).collect()
    }

    pub fn has_mode(&self, mode: Mode) -> bool {
        self.entries.iter().any(|g| g.modes().contains(&mode))
    }

    /// Rejects names that are not configured (a client typo must not silently skip a guardrail).
    pub fn validate_names(&self, requested: &[String]) -> Result<(), GatewayError> {
        for name in requested {
            if !self.entries.iter().any(|g| g.name() == name) {
                return Err(GatewayError::BadRequest {
                    message: format!("guardrail {name:?} is not configured"),
                    param: Some("guardrails".to_owned()),
                    code: None,
                });
            }
        }
        Ok(())
    }

    /// Guardrails that run for this request in `mode`: those `default_on` (unless the key opted
    /// out), plus any named in the request body or the key's metadata.
    pub fn select(
        &self,
        mode: Mode,
        requested: &[String],
        key_requested: &[String],
        key_disabled: &[String],
    ) -> Vec<Arc<dyn Guardrail>> {
        self.entries
            .iter()
            .filter(|g| g.modes().contains(&mode))
            .filter(|g| {
                let name = g.name();
                let named =
                    requested.iter().any(|r| r == name) || key_requested.iter().any(|r| r == name);
                let disabled = key_disabled.iter().any(|d| d == name);
                named || (g.default_on() && !disabled)
            })
            .cloned()
            .collect()
    }
}

/// Runs guardrails one after another over mutable text slots: `Modify` writes back so the next
/// guardrail sees the change, `Block` stops with a 400, errors follow each guardrail's `on_error`.
pub async fn run_sequential(
    guards: &[Arc<dyn Guardrail>],
    mode: Mode,
    input_type: InputType,
    mut slots: Vec<&mut String>,
    tool_calls: &[ToolCallRef],
    meta: &RequestMeta,
) -> Result<(), GatewayError> {
    for guard in guards {
        let texts: Vec<String> = slots.iter().map(|s| (**s).clone()).collect();
        let started = std::time::Instant::now();
        let outcome = guard
            .apply(GuardrailInput {
                input_type,
                texts: &texts,
                tool_calls,
                meta,
            })
            .await;
        let status = match &outcome {
            Ok(Outcome::Allow) => "allow",
            Ok(Outcome::Modify { .. }) => "modify",
            Ok(Outcome::Block { .. }) => "block",
            Err(_) => "error",
        };
        metrics::counter!(
            rustllmroute_types::metrics::GUARDRAIL_REQUESTS,
            "guardrail_name" => guard.name().to_owned(),
            "guardrail_mode" => mode.as_str(),
            "status" => status
        )
        .increment(1);
        metrics::histogram!(
            rustllmroute_types::metrics::GUARDRAIL_LATENCY,
            "guardrail_name" => guard.name().to_owned(),
            "guardrail_mode" => mode.as_str()
        )
        .record(started.elapsed().as_secs_f64());
        match outcome {
            Ok(Outcome::Allow) => {}
            Ok(Outcome::Modify { texts: replaced }) => {
                if replaced.len() == slots.len() {
                    for (slot, text) in slots.iter_mut().zip(replaced) {
                        **slot = text;
                    }
                } else {
                    tracing::warn!(
                        guardrail = guard.name(),
                        "guardrail returned {} texts for {} inputs; ignoring the modification",
                        replaced.len(),
                        slots.len()
                    );
                }
            }
            Ok(Outcome::Block { reason }) => {
                return Err(GatewayError::GuardrailBlocked {
                    guardrail: guard.name().to_owned(),
                    reason,
                });
            }
            Err(error) => match guard.on_error() {
                OnError::FailOpen => {
                    tracing::warn!(guardrail = guard.name(), error = %error, "guardrail failed; fail_open lets the request through");
                }
                OnError::FailClosed => {
                    return Err(GatewayError::GuardrailError(format!(
                        "guardrail {} failed: {error}",
                        guard.name()
                    )));
                }
            },
        }
    }
    Ok(())
}

/// Runs guardrails on a snapshot without writing back; `Block` and fail-closed errors still fail.
pub async fn run_readonly(
    guards: &[Arc<dyn Guardrail>],
    mode: Mode,
    input_type: InputType,
    texts: &[String],
    tool_calls: &[ToolCallRef],
    meta: &RequestMeta,
) -> Result<(), GatewayError> {
    let mut owned: Vec<String> = texts.to_vec();
    let slots: Vec<&mut String> = owned.iter_mut().collect();
    run_sequential(guards, mode, input_type, slots, tool_calls, meta).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, params: Value) -> GuardrailSpec {
        serde_json::from_value(
            serde_json::json!({"guardrail_name": name, "litellm_params": params}),
        )
        .unwrap()
    }

    #[test]
    fn selection_honours_default_on_requests_and_opt_outs() {
        let set = GuardrailSet::from_specs(
            &[
                spec("pii", serde_json::json!({"guardrail": "content_filter", "mode": "pre_call", "default_on": true, "patterns": ["email"]})),
                spec("strict", serde_json::json!({"guardrail": "content_filter", "mode": ["pre_call", "post_call"], "blocked_words": ["secret"]})),
            ],
            None,
        )
        .unwrap();
        let names =
            |g: Vec<Arc<dyn Guardrail>>| g.iter().map(|g| g.name().to_owned()).collect::<Vec<_>>();
        assert_eq!(names(set.select(Mode::PreCall, &[], &[], &[])), vec!["pii"]);
        assert_eq!(
            names(set.select(Mode::PreCall, &["strict".into()], &[], &[])),
            vec!["pii", "strict"]
        );
        assert_eq!(
            names(set.select(Mode::PreCall, &[], &[], &["pii".into()])),
            Vec::<String>::new()
        );
        assert_eq!(
            names(set.select(Mode::PostCall, &[], &["strict".into()], &[])),
            vec!["strict"]
        );
        assert!(set.validate_names(&["nope".into()]).is_err());
        assert!(set.has_mode(Mode::PostCall) && !set.has_mode(Mode::DuringCall));
        let bad = spec(
            "x",
            serde_json::json!({"guardrail": "content_filter", "mode": "sometimes"}),
        );
        assert!(GuardrailSet::from_specs(&[bad], None).is_err());
    }
}
