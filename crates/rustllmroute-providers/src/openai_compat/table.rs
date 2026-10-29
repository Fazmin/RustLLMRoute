bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Quirks: u16 {
        /// The host rejects `messages[].name`.
        const DROP_MESSAGE_NAME = 1 << 0;
        /// Reasoning models (`o*`, `gpt-5*`) take `max_completion_tokens`, not `max_tokens`.
        const REASONING_MAX_COMPLETION = 1 << 1;
        /// The usage chunk carries a `cost` field the gateway should trust.
        const HONOUR_USAGE_COST = 1 << 2;
        /// The host 400s on `stream_options`; never inject `include_usage`.
        const NO_STREAM_OPTIONS = 1 << 3;
        /// The host rejects the `developer` role; rewrite it to `system`.
        const NO_DEVELOPER_ROLE = 1 << 4;
        /// The host serves `/responses` natively.
        const RESPONSES_NATIVE = 1 << 5;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthStyle {
    Bearer,
    /// Azure-style `api-key: <key>` header.
    ApiKeyHeader(&'static str),
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UrlStyle {
    /// `{base}/chat/completions`
    OpenAi,
    /// `{base}/openai/deployments/{deployment}/chat/completions?api-version=...`
    AzureDeployments,
}

#[derive(Clone, Copy, Debug)]
pub struct CompatEntry {
    pub id: &'static str,
    pub base: Option<&'static str>,
    pub env_key: Option<&'static str>,
    pub auth: AuthStyle,
    pub url_style: UrlStyle,
    pub quirks: Quirks,
    pub extra_headers: &'static [(&'static str, &'static str)],
}

const fn entry(id: &'static str, base: &'static str, env_key: &'static str) -> CompatEntry {
    CompatEntry {
        id,
        base: Some(base),
        env_key: Some(env_key),
        auth: AuthStyle::Bearer,
        url_style: UrlStyle::OpenAi,
        quirks: Quirks::empty(),
        extra_headers: &[],
    }
}

const fn with_quirks(mut entry: CompatEntry, quirks: Quirks) -> CompatEntry {
    entry.quirks = quirks;
    entry
}

pub static TABLE: &[CompatEntry] = &[
    with_quirks(
        entry("openai", "https://api.openai.com/v1", "OPENAI_API_KEY"),
        Quirks::REASONING_MAX_COMPLETION.union(Quirks::RESPONSES_NATIVE),
    ),
    CompatEntry {
        id: "azure",
        base: None,
        env_key: Some("AZURE_API_KEY"),
        auth: AuthStyle::ApiKeyHeader("api-key"),
        url_style: UrlStyle::AzureDeployments,
        quirks: Quirks::REASONING_MAX_COMPLETION.union(Quirks::RESPONSES_NATIVE),
        extra_headers: &[],
    },
    entry("groq", "https://api.groq.com/openai/v1", "GROQ_API_KEY"),
    with_quirks(
        entry("mistral", "https://api.mistral.ai/v1", "MISTRAL_API_KEY"),
        Quirks::DROP_MESSAGE_NAME,
    ),
    entry(
        "together_ai",
        "https://api.together.xyz/v1",
        "TOGETHERAI_API_KEY",
    ),
    CompatEntry {
        id: "openrouter",
        base: Some("https://openrouter.ai/api/v1"),
        env_key: Some("OPENROUTER_API_KEY"),
        auth: AuthStyle::Bearer,
        url_style: UrlStyle::OpenAi,
        quirks: Quirks::HONOUR_USAGE_COST.union(Quirks::RESPONSES_NATIVE),
        extra_headers: &[
            ("HTTP-Referer", "https://github.com/fazmin/RustLLMRoute"),
            ("X-Title", "RustLLMroute"),
        ],
    },
    entry("deepseek", "https://api.deepseek.com", "DEEPSEEK_API_KEY"),
    with_quirks(
        entry("xai", "https://api.x.ai/v1", "XAI_API_KEY"),
        Quirks::RESPONSES_NATIVE,
    ),
    entry(
        "fireworks_ai",
        "https://api.fireworks.ai/inference/v1",
        "FIREWORKS_AI_API_KEY",
    ),
    entry(
        "perplexity",
        "https://api.perplexity.ai",
        "PERPLEXITYAI_API_KEY",
    ),
    entry("cerebras", "https://api.cerebras.ai/v1", "CEREBRAS_API_KEY"),
    entry(
        "sambanova",
        "https://api.sambanova.ai/v1",
        "SAMBANOVA_API_KEY",
    ),
    entry(
        "nvidia_nim",
        "https://integrate.api.nvidia.com/v1",
        "NVIDIA_NIM_API_KEY",
    ),
    entry(
        "deepinfra",
        "https://api.deepinfra.com/v1/openai",
        "DEEPINFRA_API_KEY",
    ),
    entry(
        "hyperbolic",
        "https://api.hyperbolic.xyz/v1",
        "HYPERBOLIC_API_KEY",
    ),
    entry(
        "nebius",
        "https://api.studio.nebius.ai/v1",
        "NEBIUS_API_KEY",
    ),
    entry(
        "novita",
        "https://api.novita.ai/v3/openai",
        "NOVITA_API_KEY",
    ),
    entry("moonshot", "https://api.moonshot.ai/v1", "MOONSHOT_API_KEY"),
    entry(
        "dashscope",
        "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        "DASHSCOPE_API_KEY",
    ),
    entry(
        "volcengine",
        "https://ark.cn-beijing.volces.com/api/v3",
        "VOLCENGINE_API_KEY",
    ),
    entry(
        "github",
        "https://models.inference.ai.azure.com",
        "GITHUB_API_KEY",
    ),
    entry("lambda_ai", "https://api.lambda.ai/v1", "LAMBDA_API_KEY"),
    entry(
        "featherless_ai",
        "https://api.featherless.ai/v1",
        "FEATHERLESS_AI_API_KEY",
    ),
    entry(
        "meta_llama",
        "https://api.llama.com/compat/v1",
        "LLAMA_API_KEY",
    ),
    entry("zai", "https://api.z.ai/api/paas/v4", "ZAI_API_KEY"),
    entry("minimax", "https://api.minimax.io/v1", "MINIMAX_API_KEY"),
    entry("ai21", "https://api.ai21.com/studio/v1", "AI21_API_KEY"),
    entry(
        "cohere",
        "https://api.cohere.ai/compatibility/v1",
        "COHERE_API_KEY",
    ),
    entry(
        "litellm_proxy",
        "http://localhost:4000",
        "LITELLM_PROXY_API_KEY",
    ),
    CompatEntry {
        id: "ollama",
        base: Some("http://localhost:11434/v1"),
        env_key: Some("OLLAMA_API_KEY"),
        auth: AuthStyle::None,
        url_style: UrlStyle::OpenAi,
        quirks: Quirks::NO_STREAM_OPTIONS,
        extra_headers: &[],
    },
    with_quirks(
        entry("lm_studio", "http://localhost:1234/v1", "LM_STUDIO_API_KEY"),
        Quirks::NO_STREAM_OPTIONS,
    ),
    CompatEntry {
        id: "hosted_vllm",
        base: None,
        env_key: Some("HOSTED_VLLM_API_KEY"),
        auth: AuthStyle::Bearer,
        url_style: UrlStyle::OpenAi,
        quirks: Quirks::NO_STREAM_OPTIONS,
        extra_headers: &[],
    },
    CompatEntry {
        id: "vllm",
        base: None,
        env_key: Some("HOSTED_VLLM_API_KEY"),
        auth: AuthStyle::Bearer,
        url_style: UrlStyle::OpenAi,
        quirks: Quirks::NO_STREAM_OPTIONS,
        extra_headers: &[],
    },
    CompatEntry {
        id: "llamafile",
        base: Some("http://127.0.0.1:8080/v1"),
        env_key: None,
        auth: AuthStyle::None,
        url_style: UrlStyle::OpenAi,
        quirks: Quirks::NO_STREAM_OPTIONS,
        extra_headers: &[],
    },
    CompatEntry {
        id: "openai_like",
        base: None,
        env_key: Some("OPENAI_LIKE_API_KEY"),
        auth: AuthStyle::Bearer,
        url_style: UrlStyle::OpenAi,
        quirks: Quirks::empty(),
        extra_headers: &[],
    },
    CompatEntry {
        id: "custom_openai",
        base: None,
        env_key: Some("OPENAI_API_KEY"),
        auth: AuthStyle::Bearer,
        url_style: UrlStyle::OpenAi,
        quirks: Quirks::empty(),
        extra_headers: &[],
    },
];

pub fn lookup(id: &str) -> Option<&'static CompatEntry> {
    TABLE.iter().find(|entry| entry.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_bases_parse() {
        let mut seen = std::collections::HashSet::new();
        for entry in TABLE {
            assert!(seen.insert(entry.id), "duplicate id {}", entry.id);
            if let Some(base) = entry.base {
                url::Url::parse(base).unwrap_or_else(|_| panic!("bad base for {}", entry.id));
            }
        }
        assert!(lookup("groq").is_some());
        assert!(lookup("nope").is_none());
    }
}
