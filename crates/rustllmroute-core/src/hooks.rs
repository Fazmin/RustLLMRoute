use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use http::HeaderMap;
use rustllmroute_types::{GatewayError, Usage};
use serde_json::{Map, Value};

/// Who is calling, as established by the control plane.
#[derive(Clone, Debug, Default)]
pub struct AuthContext {
    pub key_hash: Option<Arc<str>>,
    pub key_alias: Option<String>,
    pub user_id: Option<String>,
    pub team_id: Option<String>,
    pub is_admin: bool,
    pub model_aliases: Arc<HashMap<String, String>>,
    pub metadata: Arc<Map<String, Value>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum RequestKind {
    ChatCompletion,
    Messages,
    Responses,
    Embedding,
}

#[derive(Debug)]
pub enum CallStatus {
    Ok,
    Error(GatewayError),
    ClientDisconnected,
}

impl CallStatus {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// Everything the control plane needs to account for one call; emitted exactly once per call.
#[derive(Debug)]
pub struct CallRecord {
    pub request_id: String,
    pub kind: RequestKind,
    pub requested_model: String,
    pub model_group: Option<String>,
    pub deployment_id: Option<Arc<str>>,
    pub provider: Option<&'static str>,
    pub provider_model: Option<String>,
    pub api_base: Option<String>,
    pub status: CallStatus,
    pub http_status: u16,
    pub usage: Usage,
    pub cost_usd: f64,
    pub pricing_known: bool,
    pub stream: bool,
    pub started_at: SystemTime,
    pub started_instant: Instant,
    pub first_byte_at: Option<Instant>,
    pub ended_at: SystemTime,
    pub attempted_retries: u32,
    pub attempted_fallbacks: u32,
}

impl CallRecord {
    pub fn start(request_id: &str, kind: RequestKind, requested_model: &str, stream: bool) -> Self {
        Self {
            request_id: request_id.to_owned(),
            kind,
            requested_model: requested_model.to_owned(),
            model_group: None,
            deployment_id: None,
            provider: None,
            provider_model: None,
            api_base: None,
            status: CallStatus::Ok,
            http_status: 200,
            usage: Usage::default(),
            cost_usd: 0.0,
            pricing_known: false,
            stream,
            started_at: SystemTime::now(),
            started_instant: Instant::now(),
            first_byte_at: None,
            ended_at: SystemTime::now(),
            attempted_retries: 0,
            attempted_fallbacks: 0,
        }
    }

    pub fn duration_ms(&self) -> u64 {
        self.ended_at
            .duration_since(self.started_at)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

pub struct PreCallRequest<'a> {
    pub requested_model: &'a str,
    pub kind: RequestKind,
    pub stream: bool,
    pub estimated_input_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
}

/// Opaque per-call state the control plane hands out in `pre_call` and takes back in `on_complete`.
pub trait CallTicket: std::any::Any + Send + Sync {
    fn response_headers(&self, _out: &mut HeaderMap) {}
}

pub struct NoTicket;

impl CallTicket for NoTicket {}

#[async_trait::async_trait]
pub trait Hooks: Send + Sync + 'static {
    async fn authenticate(
        &self,
        headers: &HeaderMap,
        path: &str,
    ) -> Result<AuthContext, GatewayError>;
    async fn pre_call(
        &self,
        auth: &AuthContext,
        req: &PreCallRequest<'_>,
    ) -> Result<Box<dyn CallTicket>, GatewayError>;
    /// Must never block: enqueue and return.
    fn on_complete(&self, ticket: Box<dyn CallTicket>, auth: &AuthContext, record: CallRecord);
}
