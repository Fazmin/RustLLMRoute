pub mod decoder;
pub mod request;
pub mod response;

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use rustllmroute_types::{
    ChatRequest, ChatResponse, GatewayError, MessagesRequest, MessagesResponse,
};
use secrecy::ExposeSecret;
use serde_json::json;
use url::Url;

use crate::errors::upstream_error;
use crate::traits::{
    AuthPlan, CallContext, ChatProvider, ChunkDecoder, MessagesProvider, ProviderCall, StreamWire,
};
pub use decoder::AnthropicSseDecoder;

pub const PROVIDER: &str = "anthropic";
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Anthropic Messages API: real translation for `/v1/chat/completions`, native relay for `/v1/messages`.
#[derive(Clone, Copy, Debug, Default)]
pub struct AnthropicProvider;

impl AnthropicProvider {
    fn url(&self, cx: &CallContext<'_>, path: &str) -> Url {
        let mut url = cx.config.base_url.clone();
        let base = cx.config.base_url.path().trim_end_matches('/');
        let path = if base.ends_with("/v1") {
            path.trim_start_matches("/v1").to_owned()
        } else {
            path.to_owned()
        };
        url.set_path(&format!("{base}{path}"));
        url
    }

    fn headers(&self, cx: &CallContext<'_>) -> Result<HeaderMap, GatewayError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(
            "anthropic-version",
            HeaderValue::from_static(ANTHROPIC_VERSION),
        );
        if let Some(key) = &cx.config.api_key {
            headers.insert(
                "x-api-key",
                HeaderValue::from_str(key.expose_secret()).map_err(|_| {
                    GatewayError::internal("api_key contains characters not allowed in a header")
                })?,
            );
        }
        for (name, value) in &cx.config.extra_headers {
            headers.insert(name.clone(), value.clone());
        }
        for (name, value) in &cx.forwarded_headers {
            if name == "anthropic-beta" || name == "anthropic-version" {
                headers.insert(name.clone(), value.clone());
            }
        }
        Ok(headers)
    }

    pub fn default_max_tokens(cx: &CallContext<'_>) -> u32 {
        cx.config.default_max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)
    }
}

impl ChatProvider for AnthropicProvider {
    fn build(
        &self,
        req: &ChatRequest,
        cx: &mut CallContext<'_>,
    ) -> Result<ProviderCall, GatewayError> {
        let body = request::chat_to_messages(req, cx)?;
        let bytes = serde_json::to_vec(&body).map_err(|error| {
            GatewayError::internal(format!("serialising messages request: {error}"))
        })?;
        Ok(ProviderCall {
            method: Method::POST,
            url: self.url(cx, "/v1/messages"),
            headers: self.headers(cx)?,
            body: Bytes::from(bytes),
            auth: AuthPlan::Ready,
            wire: StreamWire::AnthropicSse,
            stream: req.stream,
        })
    }

    fn parse_response(
        &self,
        body: &[u8],
        cx: &CallContext<'_>,
    ) -> Result<ChatResponse, GatewayError> {
        let response: MessagesResponse = serde_json::from_slice(body).map_err(|error| {
            GatewayError::InvalidUpstreamResponse(format!(
                "anthropic: response is not a Messages object: {error}"
            ))
        })?;
        Ok(response::messages_to_chat(response, cx))
    }

    fn decoder(&self, cx: &CallContext<'_>) -> Box<dyn ChunkDecoder> {
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

impl MessagesProvider for AnthropicProvider {
    fn build(
        &self,
        req: &MessagesRequest,
        cx: &mut CallContext<'_>,
    ) -> Result<ProviderCall, GatewayError> {
        let mut body = req.clone();
        body.model = cx.model.to_owned();
        if body.max_tokens.is_none() {
            body.max_tokens = Some(Self::default_max_tokens(cx));
        }
        for (key, value) in &cx.config.extra_body {
            body.extra.insert(key.clone(), value.clone());
        }
        let bytes = serde_json::to_vec(&body).map_err(|error| {
            GatewayError::internal(format!("serialising messages request: {error}"))
        })?;
        Ok(ProviderCall {
            method: Method::POST,
            url: self.url(cx, "/v1/messages"),
            headers: self.headers(cx)?,
            body: Bytes::from(bytes),
            auth: AuthPlan::Ready,
            wire: StreamWire::AnthropicSse,
            stream: body.stream,
        })
    }

    fn build_count_tokens(
        &self,
        req: &MessagesRequest,
        cx: &mut CallContext<'_>,
    ) -> Option<ProviderCall> {
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
            url: self.url(cx, "/v1/messages/count_tokens"),
            headers: self.headers(cx).ok()?,
            body: Bytes::from(body.to_string()),
            auth: AuthPlan::Ready,
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

/// Maps an Anthropic error `type` to the HTTP status it is normally served with.
pub fn status_for_error_type(kind: &str) -> u16 {
    match kind {
        "invalid_request_error" => 400,
        "authentication_error" => 401,
        "permission_error" => 403,
        "not_found_error" => 404,
        "request_too_large" => 413,
        "rate_limit_error" => 429,
        "overloaded_error" => 529,
        _ => 500,
    }
}
