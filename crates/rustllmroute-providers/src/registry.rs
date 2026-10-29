use std::sync::Arc;

use crate::anthropic::AnthropicProvider;
use crate::bedrock::BedrockProvider;
use crate::gemini::GeminiProvider;
use crate::openai_compat::{CompatEntry, OpenAiCompat, Quirks, table};
use crate::traits::{ChatProvider, EmbeddingProvider, MessagesProvider, ResponsesProvider};
use crate::vertex::VertexProvider;

#[derive(Clone, Copy, Debug)]
pub enum ProviderKind {
    OpenAiCompat(&'static CompatEntry),
    Anthropic,
    /// Google AI Studio (`gemini/`), API-key authenticated.
    Gemini,
    /// Vertex AI (`vertex_ai/`): Gemini and Claude partner models, OAuth authenticated.
    Vertex,
    /// AWS Bedrock (`bedrock/`): Converse for chat, native invoke for Claude Messages.
    Bedrock,
}

impl ProviderKind {
    pub fn name(&self) -> &'static str {
        match self {
            Self::OpenAiCompat(entry) => entry.id,
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
            Self::Vertex => "vertex_ai",
            Self::Bedrock => "bedrock",
        }
    }

    /// Default `api_base`; `None` means the base is derived from other settings (region, location).
    pub fn default_base(&self) -> Option<&'static str> {
        match self {
            Self::OpenAiCompat(entry) => entry.base,
            Self::Anthropic => Some("https://api.anthropic.com"),
            Self::Gemini => Some("https://generativelanguage.googleapis.com"),
            Self::Vertex | Self::Bedrock => None,
        }
    }

    pub fn default_env_key(&self) -> Option<&'static str> {
        match self {
            Self::OpenAiCompat(entry) => entry.env_key,
            Self::Anthropic => Some("ANTHROPIC_API_KEY"),
            Self::Gemini => Some("GEMINI_API_KEY"),
            Self::Vertex | Self::Bedrock => None,
        }
    }
}

/// Maps the `provider/` prefix of `litellm_params.model` to an implementation.
pub fn resolve_provider(name: &str) -> Option<ProviderKind> {
    match name {
        "anthropic" => Some(ProviderKind::Anthropic),
        "gemini" => Some(ProviderKind::Gemini),
        "vertex_ai" | "vertex_ai_beta" => Some(ProviderKind::Vertex),
        "bedrock" | "bedrock_converse" => Some(ProviderKind::Bedrock),
        other => table::lookup(other).map(ProviderKind::OpenAiCompat),
    }
}

pub fn chat_provider(kind: ProviderKind) -> Arc<dyn ChatProvider> {
    match kind {
        ProviderKind::OpenAiCompat(entry) => Arc::new(OpenAiCompat::new(entry)),
        ProviderKind::Anthropic => Arc::new(AnthropicProvider),
        ProviderKind::Gemini => Arc::new(GeminiProvider::ai_studio()),
        ProviderKind::Vertex => Arc::new(VertexProvider),
        ProviderKind::Bedrock => Arc::new(BedrockProvider),
    }
}

pub fn embedding_provider(kind: ProviderKind) -> Option<Arc<dyn EmbeddingProvider>> {
    match kind {
        ProviderKind::OpenAiCompat(entry) => Some(Arc::new(OpenAiCompat::new(entry))),
        ProviderKind::Gemini => Some(Arc::new(GeminiProvider::ai_studio())),
        ProviderKind::Vertex => Some(Arc::new(VertexProvider)),
        ProviderKind::Bedrock => Some(Arc::new(BedrockProvider)),
        ProviderKind::Anthropic => None,
    }
}

/// Providers that speak the Anthropic Messages API natively for the given model, so
/// `/v1/messages` can be relayed instead of translated. Wildcard models fall back to translation.
pub fn messages_provider(kind: ProviderKind, model: &str) -> Option<Arc<dyn MessagesProvider>> {
    match kind {
        ProviderKind::Anthropic => Some(Arc::new(AnthropicProvider)),
        ProviderKind::Vertex if crate::vertex::is_claude(model) => Some(Arc::new(VertexProvider)),
        ProviderKind::Bedrock if crate::bedrock::is_anthropic(model) => {
            Some(Arc::new(BedrockProvider))
        }
        _ => None,
    }
}

/// Providers that serve `/responses` natively: OpenAI-compatible hosts flagged in the table, or
/// any OpenAI-compatible deployment whose `model_info.supports_responses_api` is set.
pub fn responses_provider(
    kind: ProviderKind,
    supports_responses_api: bool,
) -> Option<Arc<dyn ResponsesProvider>> {
    match kind {
        ProviderKind::OpenAiCompat(entry)
            if supports_responses_api || entry.quirks.contains(Quirks::RESPONSES_NATIVE) =>
        {
            Some(Arc::new(OpenAiCompat::new(entry)))
        }
        _ => None,
    }
}
