use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use rustllmroute_types::{
    ChatChunk, ChatRequest, ChatResponse, EmbeddingRequest, EmbeddingResponse, GatewayError,
    MessagesRequest, ResponsesRequest,
};
use secrecy::SecretString;
use serde_json::{Map, Value};
use url::Url;

use crate::registry::ProviderKind;

/// Google Cloud project/location for Vertex AI deployments.
#[derive(Clone, Debug)]
pub struct VertexSettings {
    pub project: String,
    pub location: String,
}

/// AWS region for Bedrock deployments.
#[derive(Clone, Debug)]
pub struct AwsSettings {
    pub region: Arc<str>,
}

/// Static per-deployment data every provider transform reads.
#[derive(Clone)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    /// Short provider name for errors, metrics and spend logs (`openai`, `groq`, `azure`, ...).
    pub provider_name: &'static str,
    /// Provider model as written in `litellm_params.model` after the prefix; may contain `*`.
    pub model: String,
    pub base_url: Url,
    pub api_key: Option<SecretString>,
    pub api_version: Option<String>,
    pub extra_headers: Vec<(HeaderName, HeaderValue)>,
    pub extra_body: Map<String, Value>,
    pub drop_params: bool,
    pub connect_timeout: Duration,
    pub timeout: Duration,
    pub stream_idle_timeout: Duration,
    /// Fallback `max_tokens` for providers that require one (Anthropic); from the pricing table.
    pub default_max_tokens: Option<u32>,
    pub vertex: Option<VertexSettings>,
    pub aws: Option<AwsSettings>,
    /// Mints bearer tokens for providers authenticated with OAuth (Vertex AI).
    pub token_source: Option<Arc<dyn TokenSource>>,
    /// Resolves AWS credentials for SigV4-signed providers (Bedrock).
    pub aws_credentials: Option<Arc<dyn AwsCredentialSource>>,
}

impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("kind", &self.kind)
            .field("provider_name", &self.provider_name)
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .field("api_version", &self.api_version)
            .field("extra_headers", &self.extra_headers)
            .field("extra_body", &self.extra_body)
            .field("drop_params", &self.drop_params)
            .field("timeout", &self.timeout)
            .field("stream_idle_timeout", &self.stream_idle_timeout)
            .field("default_max_tokens", &self.default_max_tokens)
            .field("vertex", &self.vertex)
            .field("aws", &self.aws)
            .field("token_source", &self.token_source.as_ref().map(|_| ".."))
            .field(
                "aws_credentials",
                &self.aws_credentials.as_ref().map(|_| ".."),
            )
            .finish()
    }
}

impl ProviderConfig {
    /// A config with only the fields every provider needs; tests and builders fill the rest.
    pub fn new(kind: ProviderKind, model: impl Into<String>, base_url: Url) -> Self {
        Self {
            kind,
            provider_name: kind.name(),
            model: model.into(),
            base_url,
            api_key: None,
            api_version: None,
            extra_headers: Vec::new(),
            extra_body: Map::new(),
            drop_params: true,
            connect_timeout: Duration::from_secs(5),
            timeout: Duration::from_secs(600),
            stream_idle_timeout: Duration::from_secs(120),
            default_max_tokens: None,
            vertex: None,
            aws: None,
            token_source: None,
            aws_credentials: None,
        }
    }
}

/// Per-call state shared between the executor and the provider transform.
pub struct CallContext<'a> {
    pub config: &'a ProviderConfig,
    /// Provider model for this call (wildcard already substituted).
    pub model: &'a str,
    pub stream: bool,
    pub request_id: &'a str,
    /// Provider tool name -> client tool name, filled by transforms that must sanitise names.
    pub tool_name_map: HashMap<String, String>,
    /// Set when the transform injected `stream_options.include_usage`; the client did not ask for it.
    pub usage_injected: bool,
    /// The transform emulated `response_format` with a forced tool; unwrap it on the way back.
    pub json_tool_call: bool,
    /// Client headers the pipeline allows through to the provider (for example `anthropic-beta`).
    pub forwarded_headers: HeaderMap,
}

impl<'a> CallContext<'a> {
    pub fn new(
        config: &'a ProviderConfig,
        model: &'a str,
        stream: bool,
        request_id: &'a str,
    ) -> Self {
        Self {
            config,
            model,
            stream,
            request_id,
            tool_name_map: HashMap::new(),
            usage_injected: false,
            json_tool_call: false,
            forwarded_headers: HeaderMap::new(),
        }
    }
}

pub type TokenFuture<'a> = Pin<Box<dyn Future<Output = Result<String, GatewayError>> + Send + 'a>>;

/// Mints or caches a bearer token (GCP service accounts, Azure Entra).
pub trait TokenSource: Send + Sync {
    fn token(&self) -> TokenFuture<'_>;
}

#[derive(Clone, Debug)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: SecretString,
    pub session_token: Option<SecretString>,
    pub expires_at: Option<SystemTime>,
}

pub type CredentialsFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AwsCredentials, GatewayError>> + Send + 'a>>;

/// Resolves (and caches) AWS credentials for SigV4 signing.
pub trait AwsCredentialSource: Send + Sync {
    fn credentials(&self) -> CredentialsFuture<'_>;
}

#[derive(Clone)]
pub enum AuthPlan {
    /// Headers already carry the credentials.
    Ready,
    /// Fetch a bearer token per attempt and send `Authorization: Bearer`.
    Bearer(Arc<dyn TokenSource>),
    /// Sign the request with AWS SigV4 per attempt.
    SigV4 {
        region: Arc<str>,
        service: &'static str,
        credentials: Arc<dyn AwsCredentialSource>,
    },
}

impl std::fmt::Debug for AuthPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready => f.write_str("Ready"),
            Self::Bearer(_) => f.write_str("Bearer(..)"),
            Self::SigV4 {
                region, service, ..
            } => write!(f, "SigV4({region}, {service})"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamWire {
    OpenAiSse,
    AnthropicSse,
    GeminiSse,
    AwsEventStream,
    Ndjson,
}

/// A fully built upstream request. The executor owns I/O; the transform never touches sockets.
#[derive(Debug)]
pub struct ProviderCall {
    pub method: Method,
    pub url: Url,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub auth: AuthPlan,
    pub wire: StreamWire,
    pub stream: bool,
}

/// One framed unit from the upstream stream: an SSE event, an event-stream message or an NDJSON line.
#[derive(Clone, Copy, Debug)]
pub struct Frame<'a> {
    pub event: Option<&'a str>,
    pub data: &'a [u8],
}

pub trait ChunkDecoder: Send {
    fn decode(&mut self, frame: Frame<'_>, out: &mut Vec<ChatChunk>) -> Result<(), GatewayError>;
    fn finish(&mut self, _out: &mut Vec<ChatChunk>) -> Result<(), GatewayError> {
        Ok(())
    }
    /// True once the provider signalled the end of the stream (`[DONE]`, `message_stop`, ...).
    fn is_done(&self) -> bool {
        false
    }
}

pub trait ChatProvider: Send + Sync + 'static {
    fn build(
        &self,
        req: &ChatRequest,
        cx: &mut CallContext<'_>,
    ) -> Result<ProviderCall, GatewayError>;
    fn parse_response(
        &self,
        body: &[u8],
        cx: &CallContext<'_>,
    ) -> Result<ChatResponse, GatewayError>;
    fn decoder(&self, cx: &CallContext<'_>) -> Box<dyn ChunkDecoder>;
    fn parse_error(&self, status: StatusCode, headers: &HeaderMap, body: &[u8]) -> GatewayError;
    /// True when a non-streaming upstream body is already in OpenAI chat shape and may be forwarded
    /// verbatim once parsed for accounting.
    fn passthrough_response(&self) -> bool {
        false
    }
}

pub trait EmbeddingProvider: Send + Sync + 'static {
    fn build(
        &self,
        req: &EmbeddingRequest,
        cx: &mut CallContext<'_>,
    ) -> Result<ProviderCall, GatewayError>;
    fn parse_response(
        &self,
        body: &[u8],
        req: &EmbeddingRequest,
        cx: &CallContext<'_>,
    ) -> Result<EmbeddingResponse, GatewayError>;
    fn parse_error(&self, status: StatusCode, headers: &HeaderMap, body: &[u8]) -> GatewayError;
    /// Largest number of inputs one upstream call accepts; the executor splits bigger requests.
    fn max_batch(&self, _cx: &CallContext<'_>) -> usize {
        usize::MAX
    }
    /// True when the upstream body is already an OpenAI embedding list.
    fn passthrough_response(&self) -> bool {
        false
    }
}

/// Providers that speak the Anthropic Messages API natively; `/v1/messages` bodies are relayed.
pub trait MessagesProvider: Send + Sync + 'static {
    fn build(
        &self,
        req: &MessagesRequest,
        cx: &mut CallContext<'_>,
    ) -> Result<ProviderCall, GatewayError>;
    fn build_count_tokens(
        &self,
        req: &MessagesRequest,
        cx: &mut CallContext<'_>,
    ) -> Option<ProviderCall>;
    fn relay_wire(&self) -> StreamWire;
    fn parse_error(&self, status: StatusCode, headers: &HeaderMap, body: &[u8]) -> GatewayError;
}

/// Providers that serve the OpenAI Responses API natively; bodies are relayed with the model
/// swapped and `previous_response_id` already decoded by the pipeline.
pub trait ResponsesProvider: Send + Sync + 'static {
    fn build(
        &self,
        req: &ResponsesRequest,
        cx: &mut CallContext<'_>,
    ) -> Result<ProviderCall, GatewayError>;
    fn parse_error(&self, status: StatusCode, headers: &HeaderMap, body: &[u8]) -> GatewayError;
}
