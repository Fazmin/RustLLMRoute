use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Unknown,
    Chat,
    Completion,
    Embedding,
    Responses,
    ImageGeneration,
    AudioTranscription,
    AudioSpeech,
    Rerank,
    Moderation,
    #[serde(other)]
    Other,
}

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    pub struct Caps: u16 {
        const FUNCTION_CALLING = 1 << 0;
        const VISION = 1 << 1;
        const REASONING = 1 << 2;
        const RESPONSE_SCHEMA = 1 << 3;
        const PROMPT_CACHING = 1 << 4;
        const SYSTEM_MESSAGES = 1 << 5;
        const PARALLEL_FUNCTION_CALLING = 1 << 6;
        const TOOL_CHOICE = 1 << 7;
        const WEB_SEARCH = 1 << 8;
        const PDF_INPUT = 1 << 9;
        const NATIVE_STREAMING = 1 << 10;
        const AUDIO_INPUT = 1 << 11;
        const AUDIO_OUTPUT = 1 << 12;
    }
}

/// The slice of a catalog entry the gateway needs on the hot path. All rates are USD per token.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ModelPricing {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_creation: Option<f64>,
    pub output_reasoning: Option<f64>,
    pub input_above_200k: Option<f64>,
    pub output_above_200k: Option<f64>,
    pub cache_read_above_200k: Option<f64>,
    pub cache_creation_above_200k: Option<f64>,
    pub max_input_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub mode: Mode,
    pub caps: Caps,
}

impl ModelPricing {
    pub fn has_rates(&self) -> bool {
        self.input.is_some() && self.output.is_some()
    }

    /// Deployment-level overrides from `model_info` / `litellm_params` take precedence over the table.
    pub fn with_overrides(
        mut self,
        input: Option<f64>,
        output: Option<f64>,
        max_input_tokens: Option<u32>,
        max_output_tokens: Option<u32>,
    ) -> Self {
        if input.is_some() {
            self.input = input;
        }
        if output.is_some() {
            self.output = output;
        }
        if max_input_tokens.is_some() {
            self.max_input_tokens = max_input_tokens;
        }
        if max_output_tokens.is_some() {
            self.max_output_tokens = max_output_tokens;
        }
        self
    }
}

#[derive(Default, Deserialize)]
struct RawEntry {
    input_cost_per_token: Option<f64>,
    output_cost_per_token: Option<f64>,
    cache_read_input_token_cost: Option<f64>,
    cache_creation_input_token_cost: Option<f64>,
    output_cost_per_reasoning_token: Option<f64>,
    input_cost_per_token_above_200k_tokens: Option<f64>,
    output_cost_per_token_above_200k_tokens: Option<f64>,
    cache_read_input_token_cost_above_200k_tokens: Option<f64>,
    cache_creation_input_token_cost_above_200k_tokens: Option<f64>,
    max_input_tokens: Option<f64>,
    max_output_tokens: Option<f64>,
    max_tokens: Option<f64>,
    #[serde(default)]
    mode: Mode,
    #[serde(default)]
    supports_function_calling: bool,
    #[serde(default)]
    supports_vision: bool,
    #[serde(default)]
    supports_reasoning: bool,
    #[serde(default)]
    supports_response_schema: bool,
    #[serde(default)]
    supports_prompt_caching: bool,
    #[serde(default)]
    supports_system_messages: bool,
    #[serde(default)]
    supports_parallel_function_calling: bool,
    #[serde(default)]
    supports_tool_choice: bool,
    #[serde(default)]
    supports_web_search: bool,
    #[serde(default)]
    supports_pdf_input: bool,
    #[serde(default)]
    supports_native_streaming: bool,
    #[serde(default)]
    supports_audio_input: bool,
    #[serde(default)]
    supports_audio_output: bool,
}

fn to_tokens(value: Option<f64>) -> Option<u32> {
    value
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(|value| value.min(u32::MAX as f64) as u32)
}

fn flag(caps: &mut Caps, on: bool, bit: Caps) {
    if on {
        caps.insert(bit);
    }
}

impl ModelPricing {
    /// Lenient per-entry parse: a malformed field never rejects the whole catalog.
    pub(crate) fn from_fields(fields: &Map<String, Value>) -> Result<Self, serde_json::Error> {
        let raw: RawEntry = RawEntry::deserialize(Value::Object(fields.clone()))?;
        let mut caps = Caps::empty();
        flag(
            &mut caps,
            raw.supports_function_calling,
            Caps::FUNCTION_CALLING,
        );
        flag(&mut caps, raw.supports_vision, Caps::VISION);
        flag(&mut caps, raw.supports_reasoning, Caps::REASONING);
        flag(
            &mut caps,
            raw.supports_response_schema,
            Caps::RESPONSE_SCHEMA,
        );
        flag(&mut caps, raw.supports_prompt_caching, Caps::PROMPT_CACHING);
        flag(
            &mut caps,
            raw.supports_system_messages,
            Caps::SYSTEM_MESSAGES,
        );
        flag(
            &mut caps,
            raw.supports_parallel_function_calling,
            Caps::PARALLEL_FUNCTION_CALLING,
        );
        flag(&mut caps, raw.supports_tool_choice, Caps::TOOL_CHOICE);
        flag(&mut caps, raw.supports_web_search, Caps::WEB_SEARCH);
        flag(&mut caps, raw.supports_pdf_input, Caps::PDF_INPUT);
        flag(
            &mut caps,
            raw.supports_native_streaming,
            Caps::NATIVE_STREAMING,
        );
        flag(&mut caps, raw.supports_audio_input, Caps::AUDIO_INPUT);
        flag(&mut caps, raw.supports_audio_output, Caps::AUDIO_OUTPUT);
        Ok(Self {
            input: raw.input_cost_per_token,
            output: raw.output_cost_per_token,
            cache_read: raw.cache_read_input_token_cost,
            cache_creation: raw.cache_creation_input_token_cost,
            output_reasoning: raw.output_cost_per_reasoning_token,
            input_above_200k: raw.input_cost_per_token_above_200k_tokens,
            output_above_200k: raw.output_cost_per_token_above_200k_tokens,
            cache_read_above_200k: raw.cache_read_input_token_cost_above_200k_tokens,
            cache_creation_above_200k: raw.cache_creation_input_token_cost_above_200k_tokens,
            max_input_tokens: to_tokens(raw.max_input_tokens).or(to_tokens(raw.max_tokens)),
            max_output_tokens: to_tokens(raw.max_output_tokens).or(to_tokens(raw.max_tokens)),
            mode: raw.mode,
            caps,
        })
    }
}
