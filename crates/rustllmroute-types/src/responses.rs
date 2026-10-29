use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Extra, is_false};

/// OpenAI Responses API request. Only the fields the gateway reads are typed; everything else
/// is preserved in `extra` and forwarded to native providers.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    #[serde(default)]
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Lifecycle events that carry the whole response object (and therefore its id and model).
pub const RESPONSE_LIFECYCLE_EVENTS: &[&str] = &[
    "response.created",
    "response.queued",
    "response.in_progress",
    "response.completed",
    "response.incomplete",
    "response.failed",
];

/// Reads OpenAI Responses usage (`input_tokens`/`output_tokens`) into the chat `Usage` shape.
pub fn responses_usage_to_openai(usage: &Value) -> crate::Usage {
    let count = |path: &str| usage.pointer(path).and_then(Value::as_u64).unwrap_or(0);
    let input = count("/input_tokens");
    let output = count("/output_tokens");
    let cached = count("/input_tokens_details/cached_tokens");
    let reasoning = count("/output_tokens_details/reasoning_tokens");
    crate::Usage {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(input + output),
        prompt_tokens_details: (cached > 0).then(|| crate::PromptTokensDetails {
            cached_tokens: Some(cached),
            ..Default::default()
        }),
        completion_tokens_details: (reasoning > 0).then(|| crate::CompletionTokensDetails {
            reasoning_tokens: Some(reasoning),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Chat `Usage` -> Responses usage object.
pub fn usage_to_responses(usage: &crate::Usage) -> Value {
    serde_json::json!({
        "input_tokens": usage.prompt_tokens,
        "input_tokens_details": {"cached_tokens": usage.cached_tokens()},
        "output_tokens": usage.completion_tokens,
        "output_tokens_details": {"reasoning_tokens": usage.reasoning_tokens()},
        "total_tokens": usage.total_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_fields_round_trip() {
        let wire =
            r#"{"model":"m","input":"hi","stream":true,"include":["x"],"max_output_tokens":5}"#;
        let parsed: ResponsesRequest = serde_json::from_str(wire).unwrap();
        assert_eq!(parsed.input, Value::String("hi".into()));
        assert!(parsed.stream);
        assert_eq!(parsed.extra["include"][0], "x");
        let back = serde_json::to_value(&parsed).unwrap();
        assert_eq!(back["include"][0], "x");
        assert_eq!(back["max_output_tokens"], 5);
        let usage = responses_usage_to_openai(
            &serde_json::json!({"input_tokens": 3, "output_tokens": 2, "input_tokens_details": {"cached_tokens": 1}}),
        );
        assert_eq!(usage.total_tokens, 5);
        assert_eq!(usage.cached_tokens(), 1);
    }
}
