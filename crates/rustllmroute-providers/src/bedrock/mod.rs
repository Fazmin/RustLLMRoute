//! AWS Bedrock: Converse for chat completions, native `invoke` for Claude Messages, SigV4 signed.

pub mod decoder;
pub mod embed;
pub mod errors;
pub mod request;
pub mod response;

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use rustllmroute_types::{ChatRequest, ChatResponse, GatewayError, MessagesRequest};
use serde_json::Value;
use url::Url;

pub use decoder::ConverseStreamDecoder;

use crate::anthropic::AnthropicProvider;
use crate::traits::{
    AuthPlan, CallContext, ChatProvider, ChunkDecoder, MessagesProvider, ProviderCall, StreamWire,
};

pub const PROVIDER: &str = "bedrock";
pub const SERVICE: &str = "bedrock";
pub const ANTHROPIC_VERSION: &str = "bedrock-2023-05-31";

pub fn is_anthropic(model: &str) -> bool {
    request::is_anthropic_model(model)
}

pub fn default_base(region: &str) -> String {
    format!("https://bedrock-runtime.{region}.amazonaws.com")
}

/// `bedrock/converse/...` and `bedrock/invoke/...` spell the API explicitly in LiteLLM configs.
pub fn model_id(model: &str) -> &str {
    model
        .strip_prefix("converse/")
        .or_else(|| model.strip_prefix("invoke/"))
        .unwrap_or(model)
}

/// Percent-encodes a model id or ARN for the path (`:` and `/` included).
pub fn encode_model_id(id: &str) -> String {
    let mut out = String::with_capacity(id.len() + 8);
    for byte in id.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BedrockProvider;

impl BedrockProvider {
    fn url(cx: &CallContext<'_>, action: &str) -> Result<Url, GatewayError> {
        let base = cx.config.base_url.as_str().trim_end_matches('/');
        let id = encode_model_id(model_id(cx.model));
        Url::parse(&format!("{base}/model/{id}/{action}"))
            .map_err(|error| GatewayError::internal(format!("bedrock url: {error}")))
    }

    pub(crate) fn headers(cx: &CallContext<'_>, stream: bool) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static(if stream {
                "application/vnd.amazon.eventstream"
            } else {
                "application/json"
            }),
        );
        for (name, value) in &cx.config.extra_headers {
            headers.insert(name.clone(), value.clone());
        }
        headers
    }

    pub(crate) fn auth(cx: &CallContext<'_>) -> Result<AuthPlan, GatewayError> {
        let settings = cx.config.aws.as_ref().ok_or_else(|| {
            GatewayError::internal("bedrock deployment is missing aws_region_name")
        })?;
        let credentials = cx.config.aws_credentials.clone().ok_or_else(|| {
            GatewayError::internal("bedrock deployment has no AWS credential source")
        })?;
        Ok(AuthPlan::SigV4 {
            region: settings.region.clone(),
            service: SERVICE,
            credentials,
        })
    }
}

impl ChatProvider for BedrockProvider {
    fn build(
        &self,
        req: &ChatRequest,
        cx: &mut CallContext<'_>,
    ) -> Result<ProviderCall, GatewayError> {
        let body = request::chat_to_converse(req, cx)?;
        let action = if req.stream {
            "converse-stream"
        } else {
            "converse"
        };
        Ok(ProviderCall {
            method: Method::POST,
            url: Self::url(cx, action)?,
            headers: Self::headers(cx, req.stream),
            body: Bytes::from(body.to_string()),
            auth: Self::auth(cx)?,
            wire: StreamWire::AwsEventStream,
            stream: req.stream,
        })
    }

    fn parse_response(
        &self,
        body: &[u8],
        cx: &CallContext<'_>,
    ) -> Result<ChatResponse, GatewayError> {
        let value: Value = serde_json::from_slice(body).map_err(|error| {
            GatewayError::InvalidUpstreamResponse(format!("bedrock: response is not JSON: {error}"))
        })?;
        response::converse_to_chat(&value, cx, crate::gemini::now_epoch_secs())
    }

    fn decoder(&self, cx: &CallContext<'_>) -> Box<dyn ChunkDecoder> {
        Box::new(ConverseStreamDecoder::new(
            PROVIDER,
            cx.json_tool_call,
            cx.tool_name_map.clone(),
            crate::gemini::now_epoch_secs(),
        ))
    }

    fn parse_error(&self, status: StatusCode, headers: &HeaderMap, body: &[u8]) -> GatewayError {
        errors::parse_error(status, headers, body)
    }
}

impl MessagesProvider for BedrockProvider {
    fn build(
        &self,
        req: &MessagesRequest,
        cx: &mut CallContext<'_>,
    ) -> Result<ProviderCall, GatewayError> {
        let mut request = req.clone();
        if request.max_tokens.is_none() {
            request.max_tokens = Some(AnthropicProvider::default_max_tokens(cx));
        }
        let mut body = serde_json::to_value(&request).map_err(|error| {
            GatewayError::internal(format!("serialising messages request: {error}"))
        })?;
        if let Some(object) = body.as_object_mut() {
            object.remove("model");
            object.remove("stream");
            object.insert(
                "anthropic_version".to_owned(),
                Value::String(ANTHROPIC_VERSION.to_owned()),
            );
            let betas: Vec<Value> = cx
                .forwarded_headers
                .get_all("anthropic-beta")
                .iter()
                .filter_map(|value| value.to_str().ok())
                .flat_map(|value| value.split(','))
                .map(|beta| Value::String(beta.trim().to_owned()))
                .filter(|beta| beta.as_str().is_some_and(|b| !b.is_empty()))
                .collect();
            if !betas.is_empty() {
                object.insert("anthropic_beta".to_owned(), Value::Array(betas));
            }
            for (key, value) in &cx.config.extra_body {
                object.insert(key.clone(), value.clone());
            }
        }
        let action = if req.stream {
            "invoke-with-response-stream"
        } else {
            "invoke"
        };
        Ok(ProviderCall {
            method: Method::POST,
            url: Self::url(cx, action)?,
            headers: Self::headers(cx, req.stream),
            body: Bytes::from(body.to_string()),
            auth: Self::auth(cx)?,
            wire: StreamWire::AwsEventStream,
            stream: req.stream,
        })
    }

    fn build_count_tokens(
        &self,
        _req: &MessagesRequest,
        _cx: &mut CallContext<'_>,
    ) -> Option<ProviderCall> {
        None
    }

    fn relay_wire(&self) -> StreamWire {
        StreamWire::AwsEventStream
    }

    fn parse_error(&self, status: StatusCode, headers: &HeaderMap, body: &[u8]) -> GatewayError {
        errors::parse_error(status, headers, body)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustllmroute_types::{ChatMessage, role};
    use secrecy::SecretString;

    use super::*;
    use crate::registry::ProviderKind;
    use crate::traits::{
        AwsCredentialSource, AwsCredentials, AwsSettings, CredentialsFuture, ProviderConfig,
    };

    struct StaticCreds;

    impl AwsCredentialSource for StaticCreds {
        fn credentials(&self) -> CredentialsFuture<'_> {
            Box::pin(async {
                Ok(AwsCredentials {
                    access_key_id: "AKIA".into(),
                    secret_access_key: SecretString::from("secret"),
                    session_token: None,
                    expires_at: None,
                })
            })
        }
    }

    fn config(model: &str) -> ProviderConfig {
        let mut config = ProviderConfig::new(
            ProviderKind::Bedrock,
            model,
            Url::parse(&default_base("us-east-1")).unwrap(),
        );
        config.aws = Some(AwsSettings {
            region: Arc::from("us-east-1"),
        });
        config.aws_credentials = Some(Arc::new(StaticCreds));
        config
    }

    #[test]
    fn converse_urls_encode_model_ids() {
        let config = config("anthropic.claude-sonnet-4-5-20250929-v1:0");
        let mut cx = CallContext::new(
            &config,
            "anthropic.claude-sonnet-4-5-20250929-v1:0",
            true,
            "r",
        );
        let request = ChatRequest {
            model: "x".into(),
            messages: vec![ChatMessage::text(role::USER, "hi")],
            stream: true,
            ..ChatRequest::default()
        };
        let call = ChatProvider::build(&BedrockProvider, &request, &mut cx).unwrap();
        assert_eq!(
            call.url.as_str(),
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-sonnet-4-5-20250929-v1%3A0/converse-stream"
        );
        assert!(matches!(
            call.auth,
            AuthPlan::SigV4 {
                service: "bedrock",
                ..
            }
        ));
        assert_eq!(call.wire, StreamWire::AwsEventStream);
        assert_eq!(
            encode_model_id("arn:aws:bedrock:us-east-1:123:inference-profile/x"),
            "arn%3Aaws%3Abedrock%3Aus-east-1%3A123%3Ainference-profile%2Fx"
        );
        assert_eq!(
            model_id("converse/amazon.nova-pro-v1:0"),
            "amazon.nova-pro-v1:0"
        );
    }

    #[test]
    fn messages_relay_uses_invoke_with_the_bedrock_version() {
        let config = config("us.anthropic.claude-sonnet-4-5-20250929-v1:0");
        let mut cx = CallContext::new(
            &config,
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            true,
            "r",
        );
        cx.forwarded_headers
            .insert("anthropic-beta", "a-beta, b-beta".parse().unwrap());
        let request: MessagesRequest = serde_json::from_str(
            r#"{"model":"claude","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .unwrap();
        let call = MessagesProvider::build(&BedrockProvider, &request, &mut cx).unwrap();
        assert!(call.url.as_str().ends_with("/invoke-with-response-stream"));
        let body: Value = serde_json::from_slice(&call.body).unwrap();
        assert!(body.get("model").is_none());
        assert!(body.get("stream").is_none());
        assert_eq!(body["anthropic_version"], "bedrock-2023-05-31");
        assert_eq!(
            body["anthropic_beta"],
            serde_json::json!(["a-beta", "b-beta"])
        );
        assert!(body["max_tokens"].as_u64().unwrap() > 0);
        assert!(is_anthropic("us.anthropic.claude-sonnet-4-5-20250929-v1:0"));
        assert!(!is_anthropic("amazon.nova-pro-v1:0"));
    }
}
