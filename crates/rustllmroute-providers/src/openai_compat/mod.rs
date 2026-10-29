pub mod decoder;
pub mod table;

use std::sync::LazyLock;

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use regex::Regex;
use rustllmroute_types::{
    ChatRequest, ChatResponse, EmbeddingRequest, EmbeddingResponse, GatewayError, ResponsesRequest,
    StreamOptions, role,
};
use secrecy::ExposeSecret;
use serde_json::Value;
use url::Url;

use crate::errors::upstream_error;
use crate::traits::{
    AuthPlan, CallContext, ChatProvider, ChunkDecoder, EmbeddingProvider, ProviderCall,
    ResponsesProvider, StreamWire,
};
pub use decoder::OpenAiSseDecoder;
pub use table::{AuthStyle, CompatEntry, Quirks, TABLE, UrlStyle};

const DEFAULT_AZURE_API_VERSION: &str = "2024-10-21";
static REASONING_MODEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(o[1-9]|gpt-5)").expect("valid regex"));

/// Every provider that speaks the OpenAI wire format: one implementation, a table of base URLs,
/// auth styles and small quirks.
#[derive(Clone, Copy, Debug)]
pub struct OpenAiCompat {
    pub entry: &'static CompatEntry,
}

impl OpenAiCompat {
    pub fn new(entry: &'static CompatEntry) -> Self {
        Self { entry }
    }

    fn provider_name(&self) -> &'static str {
        self.entry.id
    }

    fn url(&self, cx: &CallContext<'_>, path: &str) -> Result<Url, GatewayError> {
        let base = &cx.config.base_url;
        let mut url = base.clone();
        let base_path = base.path().trim_end_matches('/');
        match self.entry.url_style {
            UrlStyle::OpenAi => {
                url.set_path(&format!("{base_path}{path}"));
            }
            UrlStyle::AzureDeployments => {
                let version = cx
                    .config
                    .api_version
                    .as_deref()
                    .unwrap_or(DEFAULT_AZURE_API_VERSION);
                url.set_path(&format!(
                    "{base_path}/openai/deployments/{}{path}",
                    cx.model
                ));
                url.set_query(Some(&format!("api-version={version}")));
            }
        }
        Ok(url)
    }

    fn headers(&self, cx: &CallContext<'_>) -> Result<HeaderMap, GatewayError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
        let key = cx
            .config
            .api_key
            .as_ref()
            .map(|key| key.expose_secret().to_owned());
        match (self.entry.auth, key) {
            (AuthStyle::Bearer, Some(key)) => {
                headers.insert(
                    header::AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| {
                        GatewayError::internal(
                            "api_key contains characters not allowed in a header",
                        )
                    })?,
                );
            }
            (AuthStyle::ApiKeyHeader(name), Some(key)) => {
                headers.insert(
                    http::HeaderName::from_static(name),
                    HeaderValue::from_str(&key).map_err(|_| {
                        GatewayError::internal(
                            "api_key contains characters not allowed in a header",
                        )
                    })?,
                );
            }
            (AuthStyle::None, _) | (_, None) => {}
        }
        for (name, value) in self.entry.extra_headers {
            headers.insert(
                http::HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        for (name, value) in &cx.config.extra_headers {
            headers.insert(name.clone(), value.clone());
        }
        Ok(headers)
    }

    fn apply_quirks(&self, body: &mut ChatRequest, cx: &mut CallContext<'_>) {
        let quirks = self.entry.quirks;
        if quirks.contains(Quirks::DROP_MESSAGE_NAME) {
            for message in &mut body.messages {
                message.name = None;
            }
        }
        if quirks.contains(Quirks::NO_DEVELOPER_ROLE) {
            for message in &mut body.messages {
                if message.role == role::DEVELOPER {
                    message.role = role::SYSTEM.to_owned();
                }
            }
        }
        if quirks.contains(Quirks::REASONING_MAX_COMPLETION)
            && REASONING_MODEL.is_match(cx.model)
            && let Some(max) = body.max_tokens.take()
        {
            body.max_completion_tokens.get_or_insert(max);
        }
        if body.stream && !quirks.contains(Quirks::NO_STREAM_OPTIONS) {
            let options = body
                .stream_options
                .get_or_insert_with(StreamOptions::default);
            if options.include_usage.is_none() {
                options.include_usage = Some(true);
                cx.usage_injected = true;
            }
        }
        if quirks.contains(Quirks::NO_STREAM_OPTIONS) {
            body.stream_options = None;
        }
        for (key, value) in &cx.config.extra_body {
            body.extra.insert(key.clone(), value.clone());
        }
    }
}

impl ChatProvider for OpenAiCompat {
    fn build(
        &self,
        req: &ChatRequest,
        cx: &mut CallContext<'_>,
    ) -> Result<ProviderCall, GatewayError> {
        let mut body = req.clone();
        body.model = cx.model.to_owned();
        self.apply_quirks(&mut body, cx);
        let bytes = serde_json::to_vec(&body).map_err(|error| {
            GatewayError::internal(format!("serialising chat request: {error}"))
        })?;
        Ok(ProviderCall {
            method: Method::POST,
            url: self.url(cx, "/chat/completions")?,
            headers: self.headers(cx)?,
            body: Bytes::from(bytes),
            auth: AuthPlan::Ready,
            wire: StreamWire::OpenAiSse,
            stream: body.stream,
        })
    }

    fn parse_response(
        &self,
        body: &[u8],
        _cx: &CallContext<'_>,
    ) -> Result<ChatResponse, GatewayError> {
        serde_json::from_slice(body).map_err(|error| {
            GatewayError::InvalidUpstreamResponse(format!(
                "{}: response is not an OpenAI chat completion: {error}",
                self.provider_name()
            ))
        })
    }

    fn decoder(&self, _cx: &CallContext<'_>) -> Box<dyn ChunkDecoder> {
        Box::new(OpenAiSseDecoder::new(self.provider_name()))
    }

    fn parse_error(&self, status: StatusCode, headers: &HeaderMap, body: &[u8]) -> GatewayError {
        upstream_error(self.provider_name(), status, headers, body)
    }

    fn passthrough_response(&self) -> bool {
        !self.entry.quirks.contains(Quirks::HONOUR_USAGE_COST)
    }
}

impl EmbeddingProvider for OpenAiCompat {
    fn build(
        &self,
        req: &EmbeddingRequest,
        cx: &mut CallContext<'_>,
    ) -> Result<ProviderCall, GatewayError> {
        let mut body = req.clone();
        body.model = cx.model.to_owned();
        for (key, value) in &cx.config.extra_body {
            body.extra.insert(key.clone(), value.clone());
        }
        let bytes = serde_json::to_vec(&body).map_err(|error| {
            GatewayError::internal(format!("serialising embedding request: {error}"))
        })?;
        Ok(ProviderCall {
            method: Method::POST,
            url: self.url(cx, "/embeddings")?,
            headers: self.headers(cx)?,
            body: Bytes::from(bytes),
            auth: AuthPlan::Ready,
            wire: StreamWire::OpenAiSse,
            stream: false,
        })
    }

    fn parse_response(
        &self,
        body: &[u8],
        _req: &EmbeddingRequest,
        _cx: &CallContext<'_>,
    ) -> Result<EmbeddingResponse, GatewayError> {
        serde_json::from_slice(body).map_err(|error| {
            GatewayError::InvalidUpstreamResponse(format!(
                "{}: response is not an OpenAI embedding list: {error}",
                self.provider_name()
            ))
        })
    }

    fn parse_error(&self, status: StatusCode, headers: &HeaderMap, body: &[u8]) -> GatewayError {
        upstream_error(self.provider_name(), status, headers, body)
    }

    fn passthrough_response(&self) -> bool {
        true
    }
}

impl ResponsesProvider for OpenAiCompat {
    fn build(
        &self,
        req: &ResponsesRequest,
        cx: &mut CallContext<'_>,
    ) -> Result<ProviderCall, GatewayError> {
        let mut body = req.clone();
        body.model = cx.model.to_owned();
        for (key, value) in &cx.config.extra_body {
            body.extra.insert(key.clone(), value.clone());
        }
        let bytes = serde_json::to_vec(&body).map_err(|error| {
            GatewayError::internal(format!("serialising responses request: {error}"))
        })?;
        let url = match self.entry.url_style {
            UrlStyle::OpenAi => self.url(cx, "/responses")?,
            UrlStyle::AzureDeployments => {
                let mut url = cx.config.base_url.clone();
                let base_path = cx.config.base_url.path().trim_end_matches('/');
                url.set_path(&format!("{base_path}/openai/v1/responses"));
                url.set_query(Some("api-version=preview"));
                url
            }
        };
        Ok(ProviderCall {
            method: Method::POST,
            url,
            headers: self.headers(cx)?,
            body: Bytes::from(bytes),
            auth: AuthPlan::Ready,
            wire: StreamWire::OpenAiSse,
            stream: req.stream,
        })
    }

    fn parse_error(&self, status: StatusCode, headers: &HeaderMap, body: &[u8]) -> GatewayError {
        upstream_error(self.provider_name(), status, headers, body)
    }
}

/// Reads the upstream usage block without a full typed parse (used for passthrough accounting).
pub fn usage_from_passthrough(body: &[u8]) -> Option<Value> {
    serde_json::from_slice::<Value>(body)
        .ok()?
        .get("usage")
        .cloned()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rstest::rstest;
    use rustllmroute_types::ChatMessage;
    use secrecy::SecretString;

    use super::*;
    use crate::registry::ProviderKind;
    use crate::traits::ProviderConfig;

    fn config(id: &str, base: &str, api_version: Option<&str>) -> ProviderConfig {
        let entry = table::lookup(id).unwrap();
        ProviderConfig {
            kind: ProviderKind::OpenAiCompat(entry),
            provider_name: entry.id,
            model: "x".to_owned(),
            base_url: Url::parse(base).unwrap(),
            api_key: Some(SecretString::from("sk-test")),
            api_version: api_version.map(str::to_owned),
            extra_headers: Vec::new(),
            extra_body: serde_json::Map::new(),
            drop_params: true,
            connect_timeout: Duration::from_secs(5),
            timeout: Duration::from_secs(60),
            stream_idle_timeout: Duration::from_secs(60),
            default_max_tokens: None,
            vertex: None,
            aws: None,
            token_source: None,
            aws_credentials: None,
        }
    }

    fn request(model: &str, stream: bool) -> ChatRequest {
        ChatRequest {
            model: model.to_owned(),
            messages: vec![ChatMessage::text(role::USER, "hi")],
            stream,
            max_tokens: Some(10),
            ..ChatRequest::default()
        }
    }

    #[rstest]
    fn openai_build_injects_usage_and_bearer() {
        let config = config("openai", "https://api.openai.com/v1", None);
        let mut cx = CallContext::new(&config, "gpt-4o", true, "req");
        let call = ChatProvider::build(
            &OpenAiCompat::new(table::lookup("openai").unwrap()),
            &request("public-name", true),
            &mut cx,
        )
        .unwrap();
        assert_eq!(
            call.url.as_str(),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(call.headers[header::AUTHORIZATION], "Bearer sk-test");
        let body: Value = serde_json::from_slice(&call.body).unwrap();
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert!(cx.usage_injected);
    }

    #[rstest]
    fn reasoning_models_get_max_completion_tokens() {
        let config = config("openai", "https://api.openai.com/v1", None);
        let mut cx = CallContext::new(&config, "o3-mini", false, "req");
        let call = ChatProvider::build(
            &OpenAiCompat::new(table::lookup("openai").unwrap()),
            &request("o3", false),
            &mut cx,
        )
        .unwrap();
        let body: Value = serde_json::from_slice(&call.body).unwrap();
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["max_completion_tokens"], 10);
    }

    #[rstest]
    fn azure_uses_deployment_url_and_api_key_header() {
        let config = config(
            "azure",
            "https://acme.openai.azure.com",
            Some("2025-01-01-preview"),
        );
        let mut cx = CallContext::new(&config, "my-deployment", false, "req");
        let call = ChatProvider::build(
            &OpenAiCompat::new(table::lookup("azure").unwrap()),
            &request("gpt-4o", false),
            &mut cx,
        )
        .unwrap();
        assert_eq!(
            call.url.as_str(),
            "https://acme.openai.azure.com/openai/deployments/my-deployment/chat/completions?api-version=2025-01-01-preview"
        );
        assert_eq!(call.headers["api-key"], "sk-test");
        assert!(call.headers.get(header::AUTHORIZATION).is_none());
    }

    #[rstest]
    fn ollama_never_sends_stream_options_or_auth() {
        let mut config = config("ollama", "http://localhost:11434/v1", None);
        config.api_key = None;
        let mut cx = CallContext::new(&config, "llama3", true, "req");
        let call = ChatProvider::build(
            &OpenAiCompat::new(table::lookup("ollama").unwrap()),
            &request("llama3", true),
            &mut cx,
        )
        .unwrap();
        let body: Value = serde_json::from_slice(&call.body).unwrap();
        assert!(body.get("stream_options").is_none());
        assert!(!cx.usage_injected);
        assert!(call.headers.get(header::AUTHORIZATION).is_none());
    }
}
