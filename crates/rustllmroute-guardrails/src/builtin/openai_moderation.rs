//! OpenAI `/moderations`: any flagged input blocks the request.

use serde_json::{Value, json};

use super::{http_client, secret_param, timeout_from};
use crate::{
    Common, Guardrail, GuardrailError, GuardrailInput, GuardrailSpec, Mode, OnError, Outcome,
};

pub struct OpenAiModeration {
    common: Common,
    client: reqwest::Client,
    url: String,
    api_key: String,
    model: String,
}

impl OpenAiModeration {
    pub fn from_spec(spec: &GuardrailSpec) -> Result<Self, String> {
        let common = Common::from_spec(spec, &[Mode::PreCall])?;
        let params = &spec.litellm_params;
        let api_key = secret_param(params, "api_key", "OPENAI_API_KEY").ok_or_else(|| {
            format!(
                "guardrail {}: openai_moderation needs api_key or OPENAI_API_KEY",
                spec.guardrail_name
            )
        })?;
        let base = params
            .get("api_base")
            .and_then(Value::as_str)
            .unwrap_or("https://api.openai.com/v1")
            .trim_end_matches('/')
            .to_owned();
        Ok(Self {
            common,
            client: http_client(timeout_from(params, 5.0)),
            url: format!("{base}/moderations"),
            api_key,
            model: params
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("omni-moderation-latest")
                .to_owned(),
        })
    }
}

#[async_trait::async_trait]
impl Guardrail for OpenAiModeration {
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
        let texts: Vec<&str> = input
            .texts
            .iter()
            .map(String::as_str)
            .filter(|t| !t.trim().is_empty())
            .collect();
        if texts.is_empty() {
            return Ok(Outcome::Allow);
        }
        let response = self
            .client
            .post(&self.url)
            .bearer_auth(&self.api_key)
            .json(&json!({"input": texts, "model": self.model}))
            .send()
            .await
            .map_err(|error| GuardrailError(format!("moderation request failed: {error}")))?;
        let status = response.status();
        let body: Value = response
            .json()
            .await
            .map_err(|error| GuardrailError(format!("moderation response: {error}")))?;
        if !status.is_success() {
            return Err(GuardrailError(format!(
                "moderation endpoint returned {status}: {}",
                body.pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            )));
        }
        let mut flagged: Vec<String> = Vec::new();
        for result in body
            .get("results")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if result.get("flagged").and_then(Value::as_bool) == Some(true) {
                let categories = result
                    .get("categories")
                    .and_then(Value::as_object)
                    .map(|c| {
                        c.iter()
                            .filter(|(_, v)| v.as_bool() == Some(true))
                            .map(|(k, _)| k.clone())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                flagged.extend(categories);
            }
        }
        if flagged.is_empty() {
            return Ok(Outcome::Allow);
        }
        flagged.sort();
        flagged.dedup();
        Ok(Outcome::Block {
            reason: format!("flagged by moderation: {}", flagged.join(", ")),
        })
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::{InputType, RequestMeta};

    #[tokio::test]
    async fn flagged_results_block_with_categories() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/moderations"))
            .and(header("authorization", "Bearer mod-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [{"flagged": false, "categories": {}}, {"flagged": true, "categories": {"violence": true, "hate": false}}]
            })))
            .mount(&server)
            .await;
        let spec: GuardrailSpec = serde_json::from_value(json!({
            "guardrail_name": "mod",
            "litellm_params": {"guardrail": "openai_moderation", "api_key": "mod-key", "api_base": server.uri()}
        }))
        .unwrap();
        let guard = OpenAiModeration::from_spec(&spec).unwrap();
        let texts = vec!["hello".to_owned(), "bad".to_owned()];
        let meta = RequestMeta::default();
        let outcome = guard
            .apply(GuardrailInput {
                input_type: InputType::Request,
                texts: &texts,
                tool_calls: &[],
                meta: &meta,
            })
            .await
            .unwrap();
        assert_eq!(
            outcome,
            Outcome::Block {
                reason: "flagged by moderation: violence".into()
            }
        );
    }
}
