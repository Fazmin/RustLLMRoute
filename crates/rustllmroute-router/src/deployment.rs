use std::sync::Arc;
use std::time::Duration;

use rustllmroute_catalog::{Mode, ModelPricing};

/// Routing inputs read from `litellm_params` / `model_info`.
#[derive(Clone, Debug, Default)]
pub struct RouterMeta {
    pub rpm: Option<u32>,
    pub tpm: Option<u32>,
    pub weight: Option<u32>,
    pub order: Option<i32>,
    pub tags: Vec<String>,
    pub cooldown_time: Option<Duration>,
    pub num_retries: Option<u32>,
    pub max_parallel_requests: Option<u32>,
    pub blocked: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Endpoint {
    ChatCompletions,
    Messages,
    Responses,
    Embeddings,
}

impl Endpoint {
    pub fn accepts(self, mode: Mode) -> bool {
        match self {
            Self::Embeddings => mode == Mode::Embedding,
            Self::ChatCompletions | Self::Messages | Self::Responses => mode != Mode::Embedding,
        }
    }
}

/// What the router needs to know about a deployment; the data plane's type implements it.
pub trait RoutedDeployment: Send + Sync + 'static {
    fn id(&self) -> &Arc<str>;
    fn model_group(&self) -> &Arc<str>;
    fn meta(&self) -> &RouterMeta;
    fn mode(&self) -> Mode;
    fn pricing(&self) -> &ModelPricing;
    /// The provider model for a request name (wildcard groups substitute the suffix).
    fn provider_model_for(&self, requested: &str) -> String;
}
