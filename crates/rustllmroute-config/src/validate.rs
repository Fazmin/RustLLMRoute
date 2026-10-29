use std::collections::HashSet;

use sha2::{Digest, Sha256};

use crate::{FallbackRules, ProxyConfig, fallback_targets};

pub const KNOWN_PROVIDERS: &[&str] = &[
    "openai",
    "azure",
    "anthropic",
    "gemini",
    "vertex_ai",
    "bedrock",
    "groq",
    "mistral",
    "together_ai",
    "openrouter",
    "deepseek",
    "xai",
    "fireworks_ai",
    "perplexity",
    "cerebras",
    "sambanova",
    "nvidia_nim",
    "ollama",
    "lm_studio",
    "hosted_vllm",
    "openai_like",
    "custom_openai",
    "deepinfra",
    "cohere",
    "voyage",
    "jina_ai",
    "hyperbolic",
    "nebius",
    "novita",
    "moonshot",
    "dashscope",
    "volcengine",
    "github",
    "github_copilot",
    "lambda_ai",
    "featherless_ai",
    "llamafile",
    "vllm",
    "litellm_proxy",
    "ai21",
    "meta_llama",
    "zai",
    "minimax",
];

/// Collects every problem instead of stopping at the first one, and fills in generated deployment
/// ids so the rest of the system can rely on `model_info.id`.
pub fn validate(config: &mut ProxyConfig) -> Vec<String> {
    let mut errors = Vec::new();
    if config.model_list.is_empty() {
        errors.push("model_list must contain at least one deployment".to_owned());
    }
    let mut ids = HashSet::new();
    for (index, entry) in config.model_list.iter_mut().enumerate() {
        let at = format!("model_list[{index}] ({})", entry.model_name);
        if entry.model_name.trim().is_empty() {
            errors.push(format!("model_list[{index}]: model_name is empty"));
        }
        let (provider, model) = entry.litellm_params.provider_and_model();
        if model.is_empty() {
            errors.push(format!(
                "{at}: litellm_params.model has no model after the provider prefix"
            ));
        }
        if !KNOWN_PROVIDERS.contains(&provider) {
            errors.push(format!(
                "{at}: unknown provider {provider:?} in litellm_params.model"
            ));
        }
        if entry.model_name.contains('*') && !entry.litellm_params.model.contains('*') {
            errors.push(format!(
                "{at}: a wildcard model_name needs a wildcard litellm_params.model"
            ));
        }
        for (name, value) in [
            ("rpm", entry.litellm_params.rpm),
            ("tpm", entry.litellm_params.tpm),
            ("weight", entry.litellm_params.weight),
        ] {
            if value == Some(0) {
                errors.push(format!(
                    "{at}: litellm_params.{name} must be greater than zero"
                ));
            }
        }
        if entry.model_info.id.is_none() {
            entry.model_info.id = Some(generated_id(
                &entry.model_name,
                &entry.litellm_params.model,
                &entry.litellm_params.api_base,
            ));
        }
        if let Some(id) = &entry.model_info.id
            && !ids.insert(id.clone())
        {
            errors.push(format!("{at}: duplicate model_info.id {id:?}"));
        }
    }
    let groups: HashSet<&str> = config
        .model_list
        .iter()
        .map(|entry| entry.model_name.as_str())
        .collect();
    let router = &config.router_settings;
    for (name, rules) in [
        ("fallbacks", &router.fallbacks),
        ("context_window_fallbacks", &router.context_window_fallbacks),
        ("content_policy_fallbacks", &router.content_policy_fallbacks),
    ] {
        check_fallbacks(name, rules, &groups, &mut errors);
    }
    for target in &router.default_fallbacks {
        if !group_exists(&groups, target) {
            errors.push(format!(
                "router_settings.default_fallbacks: unknown model {target:?}"
            ));
        }
    }
    for (alias, target) in &router.model_group_alias {
        if !group_exists(&groups, target) {
            errors.push(format!(
                "router_settings.model_group_alias.{alias}: unknown model {target:?}"
            ));
        }
    }
    if let Some(strategy) = router.routing_strategy.as_deref() {
        const STRATEGIES: &[&str] = &[
            "simple-shuffle",
            "least-busy",
            "latency-based-routing",
            "usage-based-routing",
            "usage-based-routing-v2",
            "cost-based-routing",
        ];
        if !STRATEGIES.contains(&strategy) {
            errors.push(format!(
                "router_settings.routing_strategy: unsupported strategy {strategy:?}"
            ));
        }
    }
    let general = &config.general_settings;
    if general.store_model_in_db {
        errors
            .push("general_settings.store_model_in_db is not supported in this release".to_owned());
    }
    if general.health_check_interval < 30 {
        errors
            .push("general_settings.health_check_interval must be at least 30 seconds".to_owned());
    }
    if let Some(url) = general
        .database_url
        .as_ref()
        .map(|url| secrecy::ExposeSecret::expose_secret(url).to_owned())
    {
        let ok = url.starts_with("sqlite:")
            || url.starts_with("postgres://")
            || url.starts_with("postgresql://");
        if !ok {
            errors.push("general_settings.database_url must start with sqlite:, postgres:// or postgresql://".to_owned());
        }
    }
    errors
}

fn check_fallbacks(
    name: &str,
    rules: &FallbackRules,
    groups: &HashSet<&str>,
    errors: &mut Vec<String>,
) {
    for rule in rules {
        for (from, targets) in rule {
            if from != "*" && !group_exists(groups, from) {
                errors.push(format!(
                    "router_settings.{name}: unknown source model {from:?}"
                ));
            }
            for target in targets {
                if !group_exists(groups, target) {
                    errors.push(format!(
                        "router_settings.{name}.{from}: unknown fallback model {target:?}"
                    ));
                }
            }
        }
    }
    let _ = fallback_targets(rules, "*");
}

fn group_exists(groups: &HashSet<&str>, name: &str) -> bool {
    groups.contains(name)
        || groups.iter().any(|group| {
            group
                .strip_suffix('*')
                .is_some_and(|prefix| name.starts_with(prefix))
        })
}

fn generated_id(model_name: &str, model: &str, api_base: &Option<String>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(model_name.as_bytes());
    hasher.update(b"\0");
    hasher.update(model.as_bytes());
    hasher.update(b"\0");
    hasher.update(api_base.as_deref().unwrap_or("").as_bytes());
    let digest = hasher.finalize();
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn config(yaml: &str) -> ProxyConfig {
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    #[rstest]
    fn valid_config_gets_stable_ids_and_no_errors() {
        let yaml = r#"
model_list:
  - model_name: gpt-4o
    litellm_params: { model: openai/gpt-4o, api_key: sk-x }
  - model_name: gpt-4o
    litellm_params: { model: azure/gpt-4o, api_base: https://a.openai.azure.com }
router_settings:
  fallbacks: [{"gpt-4o": ["gpt-4o"]}]
"#;
        let mut first = config(yaml);
        let mut second = config(yaml);
        assert!(validate(&mut first).is_empty());
        assert!(validate(&mut second).is_empty());
        assert_eq!(
            first.model_list[0].model_info.id,
            second.model_list[0].model_info.id
        );
        assert_ne!(
            first.model_list[0].model_info.id,
            first.model_list[1].model_info.id
        );
    }

    #[rstest]
    fn every_problem_is_reported_with_its_location() {
        let mut bad = config(
            r#"
model_list:
  - model_name: a
    litellm_params: { model: nope/x, rpm: 0 }
router_settings:
  fallbacks: [{"a": ["missing"]}]
  routing_strategy: magic
general_settings:
  store_model_in_db: true
  health_check_interval: 5
"#,
        );
        let errors = validate(&mut bad);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("unknown provider \"nope\"")),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("rpm must be greater than zero"))
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("unknown fallback model \"missing\""))
        );
        assert!(errors.iter().any(|e| e.contains("unsupported strategy")));
        assert!(errors.iter().any(|e| e.contains("store_model_in_db")));
        assert!(errors.iter().any(|e| e.contains("health_check_interval")));
    }

    #[rstest]
    fn wildcard_groups_satisfy_fallback_targets() {
        let mut cfg = config(
            r#"
model_list:
  - model_name: "anthropic/*"
    litellm_params: { model: "anthropic/*" }
router_settings:
  default_fallbacks: ["anthropic/claude-sonnet-4-5"]
"#,
        );
        assert!(validate(&mut cfg).is_empty());
    }
}
