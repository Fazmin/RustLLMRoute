//! Adapter for any HTTP guardrail that speaks LiteLLM's generic guardrail API.

use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value, json};

use super::{http_client, timeout_from};
use crate::{
    Common, Guardrail, GuardrailError, GuardrailInput, GuardrailSpec, Mode, OnError, Outcome,
};

pub struct GenericApi {
    common: Common,
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    extra_params: Map<String, Value>,
}

impl GenericApi {
    pub fn from_spec(spec: &GuardrailSpec) -> Result<Self, String> {
        let common = Common::from_spec(spec, &[Mode::PreCall])?;
        let params = &spec.litellm_params;
        let prefix = format!("guardrail {}", spec.guardrail_name);
        let url = params
            .get("api_base")
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())
            .ok_or_else(|| format!("{prefix}: generic_guardrail_api needs api_base"))?
            .to_owned();
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        if let Some(key) = params
            .get("api_key")
            .and_then(Value::as_str)
            .filter(|k| !k.is_empty())
        {
            headers.insert(
                "x-api-key",
                HeaderValue::from_str(key)
                    .map_err(|_| format!("{prefix}: api_key is not a valid header value"))?,
            );
        }
        for (name, value) in params
            .get("headers")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            let value = value
                .as_str()
                .ok_or_else(|| format!("{prefix}: header {name} must be a string"))?;
            headers.insert(
                HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| format!("{prefix}: invalid header name {name:?}"))?,
                HeaderValue::from_str(value)
                    .map_err(|_| format!("{prefix}: invalid header value for {name}"))?,
            );
        }
        Ok(Self {
            common,
            client: http_client(timeout_from(params, 5.0)),
            url,
            headers,
            extra_params: params
                .get("additional_provider_specific_params")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
        })
    }
}

#[async_trait::async_trait]
impl Guardrail for GenericApi {
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
        let payload = json!({
            "texts": input.texts,
            "input_type": input.input_type.as_str(),
            "model": input.meta.model,
            "tool_calls": input.tool_calls.iter().map(|c| json!({"name": c.name, "arguments": c.arguments})).collect::<Vec<_>>(),
            "metadata": {
                "guardrail_name": self.common.name,
                "request_id": input.meta.request_id,
                "user_api_key_hash": input.meta.key_hash,
                "user_id": input.meta.user_id,
                "team_id": input.meta.team_id,
            },
            "additional_provider_specific_params": self.extra_params,
            "litellm_version": env!("CARGO_PKG_VERSION"),
        });
        let response = self
            .client
            .post(&self.url)
            .headers(self.headers.clone())
            .json(&payload)
            .send()
            .await
            .map_err(|error| GuardrailError(format!("guardrail API unreachable: {error}")))?;
        let status = response.status();
        let body: Value = response.json().await.map_err(|error| {
            GuardrailError(format!("guardrail API response is not JSON: {error}"))
        })?;
        if !status.is_success() {
            return Err(GuardrailError(format!(
                "guardrail API returned {status}: {body}"
            )));
        }
        let action = body
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("NONE")
            .to_ascii_uppercase();
        let reason = || {
            body.get("blocked_reason")
                .or_else(|| body.get("reason"))
                .or_else(|| body.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("content violates policy")
                .to_owned()
        };
        Ok(match action.as_str() {
            "BLOCKED" | "BLOCK" => Outcome::Block { reason: reason() },
            "GUARDRAIL_INTERVENED" | "MODIFY" | "MODIFIED" => {
                let texts: Vec<String> = match body.get("texts") {
                    Some(Value::Array(texts)) => texts
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect(),
                    _ => match body.get("text").and_then(Value::as_str) {
                        Some(text) if input.texts.len() == 1 => vec![text.to_owned()],
                        _ => Vec::new(),
                    },
                };
                if texts.len() == input.texts.len() {
                    Outcome::Modify { texts }
                } else {
                    Outcome::Allow
                }
            }
            _ => Outcome::Allow,
        })
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::{InputType, RequestMeta, ToolCallRef};

    fn spec(uri: &str, extra: Value) -> GuardrailSpec {
        let mut params = json!({"guardrail": "generic_guardrail_api", "api_base": uri, "api_key": "k", "headers": {"x-team": "t"}});
        for (k, v) in extra.as_object().unwrap() {
            params[k] = v.clone();
        }
        serde_json::from_value(json!({"guardrail_name": "ext", "litellm_params": params})).unwrap()
    }

    #[tokio::test]
    async fn sends_litellm_shaped_payloads_and_maps_actions() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("x-api-key", "k"))
            .and(header("x-team", "t"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"action": "GUARDRAIL_INTERVENED", "texts": ["masked"]})),
            )
            .mount(&server)
            .await;
        let guard = GenericApi::from_spec(&spec(&server.uri(), json!({}))).unwrap();
        let meta = RequestMeta {
            request_id: "r1".into(),
            model: "gpt".into(),
            key_hash: Some("h".into()),
            user_id: None,
            team_id: Some("team".into()),
        };
        let texts = vec!["raw".to_owned()];
        let calls = vec![ToolCallRef {
            name: "f".into(),
            arguments: "{}".into(),
        }];
        let outcome = guard
            .apply(GuardrailInput {
                input_type: InputType::Request,
                texts: &texts,
                tool_calls: &calls,
                meta: &meta,
            })
            .await
            .unwrap();
        assert_eq!(
            outcome,
            Outcome::Modify {
                texts: vec!["masked".into()]
            }
        );
        let sent: Value =
            serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(sent["texts"][0], "raw");
        assert_eq!(sent["input_type"], "request");
        assert_eq!(sent["metadata"]["guardrail_name"], "ext");
        assert_eq!(sent["metadata"]["team_id"], "team");
        assert_eq!(sent["tool_calls"][0]["name"], "f");

        let blocker = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"action": "BLOCKED", "blocked_reason": "nope"})),
            )
            .mount(&blocker)
            .await;
        let guard =
            GenericApi::from_spec(&spec(&blocker.uri(), json!({"on_error": "fail_open"}))).unwrap();
        assert_eq!(guard.on_error(), OnError::FailOpen);
        let outcome = guard
            .apply(GuardrailInput {
                input_type: InputType::Response,
                texts: &texts,
                tool_calls: &[],
                meta: &meta,
            })
            .await
            .unwrap();
        assert_eq!(
            outcome,
            Outcome::Block {
                reason: "nope".into()
            }
        );

        let down = GenericApi::from_spec(&spec("http://127.0.0.1:9", json!({}))).unwrap();
        assert!(
            down.apply(GuardrailInput {
                input_type: InputType::Request,
                texts: &texts,
                tool_calls: &[],
                meta: &meta
            })
            .await
            .is_err()
        );
    }
}
