use std::sync::Arc;
use std::time::Duration;

use http::{HeaderName, HeaderValue};
use rustllmroute_catalog::{Catalog, Mode, ModelPricing};
use rustllmroute_config::{LitellmSettings, ModelEntry, RouterSettings};
use rustllmroute_providers::{
    AwsCredentials, AwsSettings, ChatProvider, EmbeddingProvider, MessagesProvider, ProviderConfig,
    ProviderKind, ResponsesProvider, VertexSettings, chat_provider,
    registry::{embedding_provider, messages_provider, responses_provider},
    resolve_provider,
};
use rustllmroute_router::RoutedDeployment;
pub use rustllmroute_router::RouterMeta;
use rustllmroute_types::GatewayError;
use secrecy::SecretString;
use serde_json::Map;
use url::Url;

use crate::auth::{AwsAuth, GcpAuth};

pub struct Deployment {
    pub id: Arc<str>,
    pub model_group: Arc<str>,
    pub provider: Arc<ProviderConfig>,
    pub chat: Arc<dyn ChatProvider>,
    pub embed: Option<Arc<dyn EmbeddingProvider>>,
    pub messages: Option<Arc<dyn MessagesProvider>>,
    pub responses: Option<Arc<dyn ResponsesProvider>>,
    pub pricing: ModelPricing,
    pub pricing_key: Option<String>,
    pub mode: Mode,
    pub base_model: Option<String>,
    pub meta: RouterMeta,
}

impl std::fmt::Debug for Deployment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deployment")
            .field("id", &self.id)
            .field("model_group", &self.model_group)
            .field("provider", &self.provider.provider_name)
            .field("model", &self.provider.model)
            .finish()
    }
}

const PROVIDER_CONFIG_KEYS: &[&str] = &[
    "custom_llm_provider",
    "litellm_credential_name",
    "use_in_pass_through",
    "merge_reasoning_content_in_choices",
    "max_retries",
    "stream_timeout",
    "mock_response",
    "model_info",
];

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

impl Deployment {
    pub fn from_entry(
        entry: &ModelEntry,
        settings: &LitellmSettings,
        router: &RouterSettings,
        catalog: &Catalog,
    ) -> Result<Self, GatewayError> {
        let params = &entry.litellm_params;
        let name = &entry.model_name;
        let (provider_name, model) = params.provider_and_model();
        let kind = resolve_provider(provider_name).ok_or_else(|| {
            GatewayError::internal(format!(
                "deployment {name}: provider {provider_name:?} is not supported by this build"
            ))
        })?;
        let vertex = match kind {
            ProviderKind::Vertex => Some(VertexSettings {
                project: params
                    .extra_str("vertex_project")
                    .map(str::to_owned)
                    .or_else(|| env_non_empty("VERTEXAI_PROJECT"))
                    .or_else(|| env_non_empty("GOOGLE_CLOUD_PROJECT"))
                    .ok_or_else(|| {
                        GatewayError::internal(format!(
                            "deployment {name}: vertex_ai needs litellm_params.vertex_project (or VERTEXAI_PROJECT)"
                        ))
                    })?,
                location: params
                    .extra_str("vertex_location")
                    .map(str::to_owned)
                    .or_else(|| env_non_empty("VERTEXAI_LOCATION"))
                    .unwrap_or_else(|| "us-central1".to_owned()),
            }),
            _ => None,
        };
        let aws = match kind {
            ProviderKind::Bedrock => Some(AwsSettings {
                region: Arc::from(
                    params
                        .extra_str("aws_region_name")
                        .map(str::to_owned)
                        .or_else(|| env_non_empty("AWS_REGION"))
                        .or_else(|| env_non_empty("AWS_DEFAULT_REGION"))
                        .unwrap_or_else(|| "us-east-1".to_owned()),
                ),
            }),
            _ => None,
        };
        let base = params
            .api_base
            .clone()
            .or_else(|| {
                params
                    .extra_str("aws_bedrock_runtime_endpoint")
                    .map(str::to_owned)
            })
            .or_else(|| {
                vertex
                    .as_ref()
                    .map(|v| rustllmroute_providers::vertex::default_base(&v.location))
            })
            .or_else(|| {
                aws.as_ref()
                    .map(|a| rustllmroute_providers::bedrock::default_base(&a.region))
            })
            .or_else(|| kind.default_base().map(str::to_owned))
            .ok_or_else(|| {
                GatewayError::internal(format!(
                    "deployment {name}: provider {provider_name} needs litellm_params.api_base"
                ))
            })?;
        let base_url = Url::parse(&base).map_err(|error| {
            GatewayError::internal(format!(
                "deployment {name}: invalid api_base {base:?}: {error}"
            ))
        })?;
        let api_key = params.api_key.clone().or_else(|| {
            let mut names: Vec<&str> = kind.default_env_key().into_iter().collect();
            if matches!(kind, ProviderKind::Gemini) {
                names.push("GOOGLE_API_KEY");
            }
            names
                .into_iter()
                .find_map(env_non_empty)
                .map(SecretString::from)
        });
        let token_source = match kind {
            ProviderKind::Vertex if api_key.is_none() => Some(Arc::new(
                GcpAuth::new(params.extra_str("vertex_credentials")).map_err(|error| {
                    GatewayError::internal(format!("deployment {name}: {error}"))
                })?,
            )
                as Arc<dyn rustllmroute_providers::TokenSource>),
            _ => None,
        };
        let aws_credentials = aws.as_ref().map(|settings| {
            let explicit = match (
                params.extra_str("aws_access_key_id"),
                params.extra_str("aws_secret_access_key"),
            ) {
                (Some(access_key_id), Some(secret)) => Some(AwsCredentials {
                    access_key_id: access_key_id.to_owned(),
                    secret_access_key: SecretString::from(secret.to_owned()),
                    session_token: params
                        .extra_str("aws_session_token")
                        .map(|token| SecretString::from(token.to_owned())),
                    expires_at: None,
                }),
                _ => None,
            };
            Arc::new(AwsAuth::new(
                explicit,
                params.extra_str("aws_profile_name").map(str::to_owned),
                settings.region.clone(),
            )) as Arc<dyn rustllmroute_providers::AwsCredentialSource>
        });
        let mut extra_headers = Vec::new();
        for (header_name, value) in &params.extra_headers {
            let header_name = HeaderName::from_bytes(header_name.as_bytes()).map_err(|_| {
                GatewayError::internal(format!(
                    "deployment {name}: invalid header name {header_name:?}"
                ))
            })?;
            let value = HeaderValue::from_str(value).map_err(|_| {
                GatewayError::internal(format!(
                    "deployment {name}: invalid header value for {header_name}"
                ))
            })?;
            extra_headers.push((header_name, value));
        }
        let mut extra_body = Map::new();
        for (key, value) in &params.extra {
            let is_provider_key = PROVIDER_CONFIG_KEYS.contains(&key.as_str())
                || key.starts_with("aws_")
                || key.starts_with("vertex_")
                || key.starts_with("azure_");
            if !is_provider_key {
                extra_body.insert(key.clone(), value.clone());
            }
        }
        let timeout = params
            .timeout
            .or(router.timeout)
            .or(settings.request_timeout)
            .map(Duration::from_secs_f64)
            .unwrap_or(Duration::from_secs(600));
        let stream_idle_timeout = params
            .stream_timeout
            .or(router.stream_timeout)
            .map(Duration::from_secs_f64)
            .unwrap_or(Duration::from_secs(120));
        let default_max_tokens = entry.model_info.max_output_tokens.or_else(|| {
            let lookup = entry.model_info.base_model.as_deref().unwrap_or(model);
            (!lookup.contains('*'))
                .then(|| catalog.resolve(Some(provider_name), lookup))
                .flatten()
                .and_then(|found| found.entry.pricing().max_output_tokens)
        });
        let provider = Arc::new(ProviderConfig {
            kind,
            provider_name: kind.name(),
            model: model.to_owned(),
            base_url,
            api_key,
            api_version: params.api_version.clone(),
            extra_headers,
            extra_body,
            drop_params: params.drop_params.or(settings.drop_params).unwrap_or(true),
            connect_timeout: Duration::from_secs(5),
            timeout,
            stream_idle_timeout,
            default_max_tokens,
            vertex,
            aws,
            token_source,
            aws_credentials,
        });
        let info = &entry.model_info;
        let pricing_model = info.base_model.as_deref().unwrap_or(model);
        let catalog_match = if pricing_model.contains('*') {
            None
        } else {
            catalog.resolve(Some(provider_name), pricing_model)
        };
        let table = catalog_match
            .map(|found| *found.entry.pricing())
            .unwrap_or_default();
        let pricing = table.with_overrides(
            params.input_cost_per_token,
            params.output_cost_per_token,
            info.max_input_tokens,
            info.max_output_tokens,
        );
        let mode = match info.mode.as_deref() {
            Some("embedding") => Mode::Embedding,
            Some("chat") | None => {
                if table.mode == Mode::Embedding {
                    Mode::Embedding
                } else {
                    Mode::Chat
                }
            }
            Some(_) => Mode::Other,
        };
        Ok(Self {
            id: Arc::from(info.id.clone().unwrap_or_else(|| entry.model_name.clone())),
            model_group: Arc::from(entry.model_name.as_str()),
            chat: chat_provider(kind),
            embed: embedding_provider(kind),
            messages: messages_provider(kind, model),
            responses: responses_provider(kind, info.supports_responses_api.unwrap_or(false)),
            provider,
            pricing,
            pricing_key: catalog_match.map(|found| found.canonical_key.to_owned()),
            mode,
            base_model: info.base_model.clone(),
            meta: RouterMeta {
                rpm: params.rpm,
                tpm: params.tpm,
                weight: params.weight,
                order: params.order,
                tags: params.tags.clone(),
                cooldown_time: params.cooldown_time.map(Duration::from_secs_f64),
                num_retries: params.num_retries,
                max_parallel_requests: params.max_parallel_requests,
                blocked: info.blocked.unwrap_or(false),
            },
        })
    }

    /// The provider model for a request: a wildcard deployment substitutes the captured suffix.
    pub fn resolve_provider_model(&self, requested: &str) -> String {
        let pattern = self.model_group.as_ref();
        let template = self.provider.model.as_str();
        match (pattern.split_once('*'), template.contains('*')) {
            (Some((prefix, suffix)), true) => {
                let captured = requested
                    .strip_prefix(prefix)
                    .and_then(|rest| rest.strip_suffix(suffix))
                    .unwrap_or(requested);
                template.replacen('*', captured, 1)
            }
            _ => template.to_owned(),
        }
    }

    pub fn api_base(&self) -> &str {
        self.provider.base_url.as_str()
    }
}

impl RoutedDeployment for Deployment {
    fn id(&self) -> &Arc<str> {
        &self.id
    }

    fn model_group(&self) -> &Arc<str> {
        &self.model_group
    }

    fn meta(&self) -> &RouterMeta {
        &self.meta
    }

    fn mode(&self) -> Mode {
        self.mode
    }

    fn pricing(&self) -> &ModelPricing {
        &self.pricing
    }

    fn provider_model_for(&self, requested: &str) -> String {
        self.resolve_provider_model(requested)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn entry(yaml: &str) -> ModelEntry {
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    fn build(yaml: &str) -> Result<Deployment, GatewayError> {
        let catalog = rustllmroute_catalog::bundled().unwrap();
        Deployment::from_entry(
            &entry(yaml),
            &LitellmSettings::default(),
            &RouterSettings::default(),
            &catalog,
        )
    }

    #[rstest]
    fn builds_openai_deployment_with_catalog_pricing() {
        let dep = build(
            "{model_name: gpt-4o, litellm_params: {model: openai/gpt-4o, api_key: sk-x, rpm: 10}, model_info: {id: dep-1}}",
        )
        .unwrap();
        assert_eq!(&*dep.id, "dep-1");
        assert!(dep.pricing.has_rates());
        assert_eq!(dep.pricing_key.as_deref(), Some("gpt-4o"));
        assert_eq!(dep.meta.rpm, Some(10));
        assert_eq!(dep.resolve_provider_model("gpt-4o"), "gpt-4o");
    }

    #[rstest]
    fn wildcard_substitutes_requested_suffix() {
        let dep =
            build("{model_name: 'groq/*', litellm_params: {model: 'groq/*', api_key: k}}").unwrap();
        assert_eq!(
            dep.provider_model_for("groq/llama-3.3-70b"),
            "llama-3.3-70b"
        );
    }

    #[rstest]
    fn unknown_provider_is_rejected_at_build() {
        let error =
            build("{model_name: x, litellm_params: {model: cohere_chat/command}}").unwrap_err();
        assert!(error.to_string().contains("not supported"));
    }

    #[rstest]
    fn vertex_and_bedrock_derive_their_endpoints() {
        let vertex = build(
            "{model_name: claude, litellm_params: {model: vertex_ai/claude-sonnet-4-5@20250929, vertex_project: proj, vertex_location: us-east5, api_key: ya29.x}}",
        )
        .unwrap();
        assert_eq!(
            vertex.api_base(),
            "https://us-east5-aiplatform.googleapis.com/"
        );
        assert!(
            vertex.messages.is_some(),
            "Claude on Vertex relays /v1/messages natively"
        );
        assert!(vertex.pricing.has_rates());
        let gemini = build(
            "{model_name: g, litellm_params: {model: vertex_ai/gemini-2.5-pro, vertex_project: proj, api_key: ya29.x}}",
        )
        .unwrap();
        assert!(gemini.messages.is_none(), "Gemini on Vertex is translated");
        assert_eq!(
            gemini.provider.vertex.as_ref().unwrap().location,
            "us-central1"
        );
        let bedrock = build(
            "{model_name: b, litellm_params: {model: bedrock/us.anthropic.claude-sonnet-4-5-20250929-v1:0, aws_region_name: eu-west-1, aws_access_key_id: AKIA, aws_secret_access_key: s}}",
        )
        .unwrap();
        assert_eq!(
            bedrock.api_base(),
            "https://bedrock-runtime.eu-west-1.amazonaws.com/"
        );
        assert!(bedrock.messages.is_some());
        assert!(bedrock.provider.aws_credentials.is_some());
        assert!(
            bedrock.pricing.has_rates(),
            "cross-region ids resolve in the catalog"
        );
        let missing =
            build("{model_name: v, litellm_params: {model: vertex_ai/gemini-2.5-pro, api_key: x}}");
        if std::env::var("VERTEXAI_PROJECT").is_err()
            && std::env::var("GOOGLE_CLOUD_PROJECT").is_err()
        {
            assert!(missing.unwrap_err().to_string().contains("vertex_project"));
        }
    }
}
