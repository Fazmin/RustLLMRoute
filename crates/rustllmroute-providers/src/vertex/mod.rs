//! Vertex AI: Gemini models through `publishers/google`, Claude partner models through
//! `publishers/anthropic` (`rawPredict` / `streamRawPredict`, Anthropic wire format).

use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use rustllmroute_types::{
    ChatRequest, ChatResponse, EmbeddingRequest, EmbeddingResponse, GatewayError, MessagesRequest,
    MessagesResponse,
};
use serde_json::{Value, json};
use url::Url;

use crate::anthropic::{self, AnthropicSseDecoder};
use crate::errors::upstream_error;
use crate::gemini::{Flavor, GeminiProvider};
use crate::traits::{
    CallContext, ChatProvider, ChunkDecoder, EmbeddingProvider, MessagesProvider, ProviderCall,
    StreamWire,
};

pub const PROVIDER: &str = "vertex_ai";
pub const ANTHROPIC_VERSION: &str = "vertex-2023-10-16";

pub fn is_claude(model: &str) -> bool {
    model.contains("claude")
}

/// Default regional endpoint; `global` has no region prefix.
pub fn default_base(location: &str) -> String {
    if location == "global" {
        "https://aiplatform.googleapis.com".to_owned()
    } else {
        format!("https://{location}-aiplatform.googleapis.com")
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct VertexProvider;

impl VertexProvider {
    fn anthropic_url(cx: &CallContext<'_>, model: &str, action: &str) -> Result<Url, GatewayError> {
        let base = cx.config.base_url.as_str().trim_end_matches('/');
        let settings = cx.config.vertex.as_ref().ok_or_else(|| {
            GatewayError::internal("vertex_ai deployment is missing project/location")
        })?;
        Url::parse(&format!(
            "{base}/v1/projects/{}/locations/{}/publishers/anthropic/models/{model}:{action}",
            settings.project, settings.location
        ))
        .map_err(|error| GatewayError::internal(format!("vertex url: {error}")))
    }

    /// Anthropic Messages body for Vertex: no `model`, `anthropic_version` instead.
    fn vertex_messages_body(mut body: Value, cx: &CallContext<'_>) -> Value {
        if let Some(object) = body.as_object_mut() {
            object.remove("model");
            object.insert(
                "anthropic_version".to_owned(),
                Value::String(ANTHROPIC_VERSION.to_owned()),
            );
            for (key, value) in &cx.config.extra_body {
                object.insert(key.clone(), value.clone());
            }
        }
        body
    }

    fn anthropic_call(
        body: Value,
        stream: bool,
        cx: &CallContext<'_>,
    ) -> Result<ProviderCall, GatewayError> {
        let (mut headers, auth) = GeminiProvider::headers_and_auth(Flavor::Vertex, cx)?;
        for (name, value) in &cx.forwarded_headers {
            if name == "anthropic-beta" {
                headers.insert(name.clone(), value.clone());
            }
        }
        let action = if stream {
            "streamRawPredict"
        } else {
            "rawPredict"
        };
        Ok(ProviderCall {
            method: Method::POST,
            url: Self::anthropic_url(cx, cx.model, action)?,
            headers,
            body: Bytes::from(body.to_string()),
            auth,
            wire: StreamWire::AnthropicSse,
            stream,
        })
    }
}

impl ChatProvider for VertexProvider {
    fn build(
        &self,
        req: &ChatRequest,
        cx: &mut CallContext<'_>,
    ) -> Result<ProviderCall, GatewayError> {
        if !is_claude(cx.model) {
            return ChatProvider::build(&GeminiProvider::vertex(), req, cx);
        }
        let messages = anthropic::request::chat_to_messages(req, cx)?;
        let body = serde_json::to_value(&messages).map_err(|error| {
            GatewayError::internal(format!("serialising messages request: {error}"))
        })?;
        Self::anthropic_call(Self::vertex_messages_body(body, cx), req.stream, cx)
    }

    fn parse_response(
        &self,
        body: &[u8],
        cx: &CallContext<'_>,
    ) -> Result<ChatResponse, GatewayError> {
        if !is_claude(cx.model) {
            return ChatProvider::parse_response(&GeminiProvider::vertex(), body, cx);
        }
        let response: MessagesResponse = serde_json::from_slice(body).map_err(|error| {
            GatewayError::InvalidUpstreamResponse(format!(
                "vertex_ai: response is not a Messages object: {error}"
            ))
        })?;
        Ok(anthropic::response::messages_to_chat(response, cx))
    }

    fn decoder(&self, cx: &CallContext<'_>) -> Box<dyn ChunkDecoder> {
        if !is_claude(cx.model) {
            return ChatProvider::decoder(&GeminiProvider::vertex(), cx);
        }
        Box::new(AnthropicSseDecoder::new(
            PROVIDER,
            cx.json_tool_call,
            cx.tool_name_map.clone(),
        ))
    }

    fn parse_error(&self, status: StatusCode, headers: &HeaderMap, body: &[u8]) -> GatewayError {
        upstream_error(PROVIDER, status, headers, body)
    }
}

impl EmbeddingProvider for VertexProvider {
    fn build(
        &self,
        req: &EmbeddingRequest,
        cx: &mut CallContext<'_>,
    ) -> Result<ProviderCall, GatewayError> {
        EmbeddingProvider::build(&GeminiProvider::vertex(), req, cx)
    }

    fn parse_response(
        &self,
        body: &[u8],
        req: &EmbeddingRequest,
        cx: &CallContext<'_>,
    ) -> Result<EmbeddingResponse, GatewayError> {
        EmbeddingProvider::parse_response(&GeminiProvider::vertex(), body, req, cx)
    }

    fn parse_error(&self, status: StatusCode, headers: &HeaderMap, body: &[u8]) -> GatewayError {
        upstream_error(PROVIDER, status, headers, body)
    }

    fn max_batch(&self, cx: &CallContext<'_>) -> usize {
        EmbeddingProvider::max_batch(&GeminiProvider::vertex(), cx)
    }
}

impl MessagesProvider for VertexProvider {
    fn build(
        &self,
        req: &MessagesRequest,
        cx: &mut CallContext<'_>,
    ) -> Result<ProviderCall, GatewayError> {
        let mut request = req.clone();
        if request.max_tokens.is_none() {
            request.max_tokens = Some(anthropic::AnthropicProvider::default_max_tokens(cx));
        }
        let body = serde_json::to_value(&request).map_err(|error| {
            GatewayError::internal(format!("serialising messages request: {error}"))
        })?;
        Self::anthropic_call(Self::vertex_messages_body(body, cx), req.stream, cx)
    }

    fn build_count_tokens(
        &self,
        req: &MessagesRequest,
        cx: &mut CallContext<'_>,
    ) -> Option<ProviderCall> {
        let (headers, auth) = GeminiProvider::headers_and_auth(Flavor::Vertex, cx).ok()?;
        let body = json!({
            "model": cx.model,
            "messages": req.messages,
            "system": req.system,
            "tools": req.tools,
            "thinking": req.thinking,
            "tool_choice": req.tool_choice,
        });
        Some(ProviderCall {
            method: Method::POST,
            url: Self::anthropic_url(cx, "count-tokens", "rawPredict").ok()?,
            headers,
            body: Bytes::from(body.to_string()),
            auth,
            wire: StreamWire::AnthropicSse,
            stream: false,
        })
    }

    fn relay_wire(&self) -> StreamWire {
        StreamWire::AnthropicSse
    }

    fn parse_error(&self, status: StatusCode, headers: &HeaderMap, body: &[u8]) -> GatewayError {
        upstream_error(PROVIDER, status, headers, body)
    }
}

#[cfg(test)]
mod tests {
    use rustllmroute_types::{ChatMessage, role};
    use secrecy::SecretString;

    use super::*;
    use crate::registry::ProviderKind;
    use crate::traits::{AuthPlan, ProviderConfig, VertexSettings};

    fn config(model: &str) -> ProviderConfig {
        let mut config = ProviderConfig::new(
            ProviderKind::Vertex,
            model,
            Url::parse(&default_base("us-east5")).unwrap(),
        );
        config.vertex = Some(VertexSettings {
            project: "proj".into(),
            location: "us-east5".into(),
        });
        config.api_key = Some(SecretString::from("ya29.token"));
        config
    }

    #[test]
    fn claude_models_use_raw_predict_with_the_vertex_version() {
        let config = config("claude-sonnet-4-5@20250929");
        let mut cx = CallContext::new(&config, "claude-sonnet-4-5@20250929", true, "r");
        let request = ChatRequest {
            model: "x".into(),
            messages: vec![ChatMessage::text(role::USER, "hi")],
            stream: true,
            ..ChatRequest::default()
        };
        let call = ChatProvider::build(&VertexProvider, &request, &mut cx).unwrap();
        assert_eq!(
            call.url.as_str(),
            "https://us-east5-aiplatform.googleapis.com/v1/projects/proj/locations/us-east5/publishers/anthropic/models/claude-sonnet-4-5@20250929:streamRawPredict"
        );
        assert_eq!(call.headers["authorization"], "Bearer ya29.token");
        assert!(matches!(call.auth, AuthPlan::Ready));
        let body: Value = serde_json::from_slice(&call.body).unwrap();
        assert!(body.get("model").is_none());
        assert_eq!(body["anthropic_version"], "vertex-2023-10-16");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hi");
        assert!(body["max_tokens"].as_u64().unwrap() > 0);
    }

    #[test]
    fn gemini_models_use_the_google_publisher() {
        let config = config("gemini-2.5-pro");
        let mut cx = CallContext::new(&config, "gemini-2.5-pro", false, "r");
        let request = ChatRequest {
            model: "x".into(),
            messages: vec![ChatMessage::text(role::USER, "hi")],
            ..ChatRequest::default()
        };
        let call = ChatProvider::build(&VertexProvider, &request, &mut cx).unwrap();
        assert_eq!(
            call.url.as_str(),
            "https://us-east5-aiplatform.googleapis.com/v1/projects/proj/locations/us-east5/publishers/google/models/gemini-2.5-pro:generateContent"
        );
        assert_eq!(call.wire, StreamWire::GeminiSse);
        assert_eq!(default_base("global"), "https://aiplatform.googleapis.com");
    }
}
