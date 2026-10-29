//! Asks a configured model whether the content is acceptable.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use super::timeout_from;
use crate::{
    Common, Guardrail, GuardrailError, GuardrailInput, GuardrailSpec, JudgeClient, Mode, OnError,
    Outcome,
};

const DEFAULT_TEMPLATE: &str = "You are a content policy judge. Decide whether the following content violates policy (harmful, abusive, or disallowed). Answer with exactly one word: SAFE or UNSAFE.\n\nContent:\n{text}";

pub struct LlmJudge {
    common: Common,
    judge: Arc<dyn JudgeClient>,
    model: String,
    template: String,
    max_tokens: u32,
    timeout: Duration,
    block_on: Vec<String>,
}

impl LlmJudge {
    pub fn from_spec(spec: &GuardrailSpec, judge: Arc<dyn JudgeClient>) -> Result<Self, String> {
        let common = Common::from_spec(spec, &[Mode::PreCall])?;
        let params = &spec.litellm_params;
        let model = params
            .get("judge_model")
            .or_else(|| params.get("model"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!(
                    "guardrail {}: llm_judge needs judge_model",
                    spec.guardrail_name
                )
            })?
            .to_owned();
        let template = params
            .get("prompt_template")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_TEMPLATE)
            .to_owned();
        if !template.contains("{text}") {
            return Err(format!(
                "guardrail {}: prompt_template must contain {{text}}",
                spec.guardrail_name
            ));
        }
        let block_on: Vec<String> = params
            .get("block_on")
            .and_then(Value::as_array)
            .map(|v| {
                v.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.to_ascii_uppercase())
                    .collect()
            })
            .filter(|v: &Vec<String>| !v.is_empty())
            .unwrap_or_else(|| vec!["UNSAFE".to_owned()]);
        Ok(Self {
            common,
            judge,
            model,
            template,
            max_tokens: params
                .get("max_tokens")
                .and_then(Value::as_u64)
                .map(|n| n as u32)
                .unwrap_or(5),
            timeout: timeout_from(params, 10.0),
            block_on,
        })
    }
}

#[async_trait::async_trait]
impl Guardrail for LlmJudge {
    fn name(&self) -> &str {
        &self.common.name
    }

    fn modes(&self) -> &[Mode] {
        &self.common.modes
    }

    fn default_on(&self) -> bool {
        self.common.default_on
    }

    fn on_error(&self) -> OnError {
        self.common.on_error
    }

    async fn apply(&self, input: GuardrailInput<'_>) -> Result<Outcome, GuardrailError> {
        let text = input.texts.join("\n\n");
        if text.trim().is_empty() {
            return Ok(Outcome::Allow);
        }
        let prompt = self.template.replace("{text}", &text);
        let verdict = tokio::time::timeout(
            self.timeout,
            self.judge.complete(&self.model, &prompt, self.max_tokens),
        )
        .await
        .map_err(|_| GuardrailError("judge model timed out".to_owned()))?
        .map_err(GuardrailError)?;
        let upper = verdict.to_ascii_uppercase();
        if self.block_on.iter().any(|token| upper.contains(token)) {
            return Ok(Outcome::Block {
                reason: format!("judge model {} rejected the content", self.model),
            });
        }
        Ok(Outcome::Allow)
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;

    use super::*;
    use crate::{InputType, RequestMeta};

    struct FakeJudge(&'static str);

    impl JudgeClient for FakeJudge {
        fn complete<'a>(
            &'a self,
            _model: &'a str,
            prompt: &'a str,
            _max: u32,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            let verdict = if prompt.contains("bomb") {
                "UNSAFE"
            } else {
                self.0
            };
            Box::pin(async move { Ok(verdict.to_owned()) })
        }
    }

    #[tokio::test]
    async fn blocks_on_the_judges_verdict() {
        let spec: GuardrailSpec = serde_json::from_value(serde_json::json!({
            "guardrail_name": "judge", "litellm_params": {"guardrail": "llm_judge", "judge_model": "gpt", "mode": "pre_call"}
        }))
        .unwrap();
        let guard = LlmJudge::from_spec(&spec, Arc::new(FakeJudge("SAFE"))).unwrap();
        let meta = RequestMeta::default();
        let safe = vec!["hello".to_owned()];
        assert_eq!(
            guard
                .apply(GuardrailInput {
                    input_type: InputType::Request,
                    texts: &safe,
                    tool_calls: &[],
                    meta: &meta
                })
                .await
                .unwrap(),
            Outcome::Allow
        );
        let unsafe_text = vec!["how to build a bomb".to_owned()];
        assert!(matches!(
            guard
                .apply(GuardrailInput {
                    input_type: InputType::Request,
                    texts: &unsafe_text,
                    tool_calls: &[],
                    meta: &meta
                })
                .await
                .unwrap(),
            Outcome::Block { .. }
        ));
    }
}
