//! Regex and keyword filter: BLOCK rules reject the request, MASK rules redact in place.

use std::sync::LazyLock;

use fancy_regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use crate::{
    Common, Guardrail, GuardrailError, GuardrailInput, GuardrailSpec, Mode, OnError, Outcome,
};

const DEFAULT_REDACTION: &str = "[{pattern_name}_REDACTED]";
const DEFAULT_KEYWORD_TAG: &str = "[KEYWORD_REDACTED]";

#[derive(Clone, Debug, Deserialize)]
struct BundledPattern {
    name: String,
    pattern: String,
    #[serde(default)]
    keyword_pattern: Option<String>,
}

#[derive(Deserialize)]
struct Bundle {
    patterns: Vec<BundledPattern>,
}

static BUNDLED: LazyLock<Vec<BundledPattern>> = LazyLock::new(|| {
    serde_json::from_str::<Bundle>(include_str!("../../patterns.json"))
        .map(|bundle| bundle.patterns)
        .unwrap_or_default()
});

pub fn bundled_pattern_names() -> Vec<&'static str> {
    BUNDLED.iter().map(|p| p.name.as_str()).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Block,
    Mask,
}

impl Action {
    fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_uppercase().as_str() {
            "BLOCK" => Some(Self::Block),
            "MASK" | "REDACT" => Some(Self::Mask),
            _ => None,
        }
    }
}

struct Rule {
    name: String,
    regex: Regex,
    keyword: Option<Regex>,
    action: Action,
}

pub struct ContentFilter {
    common: Common,
    patterns: Vec<Rule>,
    words: Vec<Rule>,
    redaction_format: String,
    keyword_tag: String,
}

fn compile(pattern: &str, case_insensitive: bool) -> Result<Regex, String> {
    let source = if case_insensitive && !pattern.starts_with("(?i)") {
        format!("(?i){pattern}")
    } else {
        pattern.to_owned()
    };
    Regex::new(&source).map_err(|error| format!("invalid pattern {pattern:?}: {error}"))
}

impl ContentFilter {
    pub fn from_spec(spec: &GuardrailSpec) -> Result<Self, String> {
        let common = Common::from_spec(spec, &[Mode::PreCall])?;
        let params = &spec.litellm_params;
        let prefix = format!("guardrail {}", spec.guardrail_name);
        let case_insensitive = params
            .get("case_insensitive")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let default_action = match params.get("default_action").and_then(Value::as_str) {
            Some(action) => Action::parse(action)
                .ok_or_else(|| format!("{prefix}: default_action must be BLOCK or MASK"))?,
            None => Action::Block,
        };
        let mut patterns = Vec::new();
        for item in params
            .get("patterns")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let (name, source, action, keyword) = match item {
                Value::String(name) => (name.clone(), None, None, None),
                Value::Object(object) => (
                    object
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| format!("{prefix}: every pattern needs a name"))?
                        .to_owned(),
                    object
                        .get("pattern")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    object
                        .get("action")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    object
                        .get("keyword_pattern")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                ),
                other => return Err(format!("{prefix}: invalid pattern entry {other}")),
            };
            let bundled = BUNDLED.iter().find(|p| p.name == name);
            let source = match (source, bundled) {
                (Some(source), _) => source,
                (None, Some(bundled)) => bundled.pattern.clone(),
                (None, None) => {
                    return Err(format!(
                        "{prefix}: pattern {name:?} is not a bundled pattern and has no `pattern`"
                    ));
                }
            };
            let keyword = keyword
                .or_else(|| bundled.and_then(|b| b.keyword_pattern.clone()))
                .map(|k| compile(&k, true))
                .transpose()?;
            let action = match action {
                Some(action) => Action::parse(&action).ok_or_else(|| {
                    format!("{prefix}: pattern {name:?} action must be BLOCK or MASK")
                })?,
                None => default_action,
            };
            patterns.push(Rule {
                regex: compile(&source, case_insensitive)?,
                name,
                keyword,
                action,
            });
        }
        let mut words = Vec::new();
        for item in params
            .get("blocked_words")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let (keyword, action) = match item {
                Value::String(word) => (word.clone(), default_action),
                Value::Object(object) => (
                    object
                        .get("keyword")
                        .or_else(|| object.get("word"))
                        .and_then(Value::as_str)
                        .ok_or_else(|| format!("{prefix}: every blocked word needs a keyword"))?
                        .to_owned(),
                    match object.get("action").and_then(Value::as_str) {
                        Some(action) => Action::parse(action).ok_or_else(|| {
                            format!("{prefix}: blocked word action must be BLOCK or MASK")
                        })?,
                        None => default_action,
                    },
                ),
                other => return Err(format!("{prefix}: invalid blocked word {other}")),
            };
            words.push(Rule {
                regex: compile(
                    &format!(r"\b{}\b", fancy_regex::escape(&keyword)),
                    case_insensitive,
                )?,
                name: keyword,
                keyword: None,
                action,
            });
        }
        if patterns.is_empty() && words.is_empty() {
            return Err(format!(
                "{prefix}: content_filter needs patterns or blocked_words"
            ));
        }
        Ok(Self {
            common,
            patterns,
            words,
            redaction_format: params
                .get("pattern_redaction_format")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_REDACTION)
                .to_owned(),
            keyword_tag: params
                .get("keyword_redaction_tag")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_KEYWORD_TAG)
                .to_owned(),
        })
    }

    fn matches(rule: &Rule, text: &str) -> bool {
        rule.regex.is_match(text).unwrap_or(false)
            && rule
                .keyword
                .as_ref()
                .is_none_or(|keyword| keyword.is_match(text).unwrap_or(false))
    }

    fn redaction(&self, rule: &Rule, is_word: bool) -> String {
        if is_word {
            self.keyword_tag.clone()
        } else {
            self.redaction_format
                .replace("{pattern_name}", &rule.name.to_ascii_uppercase())
        }
    }

    /// Returns the masked text, or the name of the rule that blocks it.
    fn filter(&self, text: &str) -> Result<String, String> {
        for (rules, is_word) in [(&self.patterns, false), (&self.words, true)] {
            for rule in rules.iter().filter(|r| r.action == Action::Block) {
                if Self::matches(rule, text) {
                    return Err(if is_word {
                        format!("blocked keyword {:?}", rule.name)
                    } else {
                        format!("content matched the {} pattern", rule.name)
                    });
                }
            }
        }
        let mut masked = text.to_owned();
        for (rules, is_word) in [(&self.patterns, false), (&self.words, true)] {
            for rule in rules.iter().filter(|r| r.action == Action::Mask) {
                if Self::matches(rule, &masked) {
                    let replacement = self.redaction(rule, is_word);
                    masked = rule
                        .regex
                        .replace_all(&masked, replacement.as_str())
                        .into_owned();
                }
            }
        }
        Ok(masked)
    }
}

#[async_trait::async_trait]
impl Guardrail for ContentFilter {
    fn name(&self) -> &str {
        &self.common.name
    }

    fn modes(&self) -> &[Mode] {
        &self.common.modes
    }

    fn default_on(&self) -> bool {
        self.common.default_on
    }

    fn on_error(&self) -> OnError {
        self.common.on_error
    }

    async fn apply(&self, input: GuardrailInput<'_>) -> Result<Outcome, GuardrailError> {
        let mut changed = false;
        let mut texts = Vec::with_capacity(input.texts.len());
        for text in input.texts {
            match self.filter(text) {
                Ok(masked) => {
                    changed |= masked != *text;
                    texts.push(masked);
                }
                Err(reason) => return Ok(Outcome::Block { reason }),
            }
        }
        for call in input.tool_calls {
            if let Err(reason) = self.filter(&call.arguments) {
                return Ok(Outcome::Block { reason });
            }
        }
        Ok(if changed {
            Outcome::Modify { texts }
        } else {
            Outcome::Allow
        })
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::{InputType, RequestMeta};

    fn filter(params: Value) -> ContentFilter {
        let spec: GuardrailSpec = serde_json::from_value(
            serde_json::json!({"guardrail_name": "cf", "litellm_params": params}),
        )
        .unwrap();
        ContentFilter::from_spec(&spec).unwrap()
    }

    async fn run(filter: &ContentFilter, texts: &[&str]) -> Outcome {
        let texts: Vec<String> = texts.iter().map(|t| (*t).to_owned()).collect();
        let meta = RequestMeta::default();
        filter
            .apply(GuardrailInput {
                input_type: InputType::Request,
                texts: &texts,
                tool_calls: &[],
                meta: &meta,
            })
            .await
            .unwrap()
    }

    #[rstest]
    #[case::email_masked("my mail is a.b@example.com ok", "my mail is [EMAIL_REDACTED] ok")]
    #[case::ssn_masked("ssn 123-45-6789", "ssn [US_SSN_REDACTED]")]
    #[case::clean("nothing here", "nothing here")]
    #[tokio::test]
    async fn masks_bundled_patterns(#[case] input: &str, #[case] expected: &str) {
        let filter = filter(
            serde_json::json!({"guardrail": "content_filter", "patterns": [{"name": "email", "action": "MASK"}, {"name": "us_ssn", "action": "MASK"}]}),
        );
        let outcome = run(&filter, &[input]).await;
        if input == expected {
            assert_eq!(outcome, Outcome::Allow);
        } else {
            assert_eq!(
                outcome,
                Outcome::Modify {
                    texts: vec![expected.to_owned()]
                }
            );
        }
    }

    #[tokio::test]
    async fn blocks_before_masking_and_handles_keywords_and_custom_patterns() {
        let filter = filter(serde_json::json!({
            "guardrail": "content_filter",
            "patterns": [{"name": "email", "action": "MASK"}, {"name": "ticket", "pattern": "TCK-\\d+", "action": "BLOCK"}],
            "blocked_words": ["forbidden", {"keyword": "hush", "action": "MASK"}]
        }));
        assert!(
            matches!(run(&filter, &["ref TCK-42 a@b.co"]).await, Outcome::Block { reason } if reason.contains("ticket"))
        );
        assert!(
            matches!(run(&filter, &["this is FORBIDDEN"]).await, Outcome::Block { reason } if reason.contains("forbidden"))
        );
        assert_eq!(
            run(&filter, &["hush now a@b.co", "fine"]).await,
            Outcome::Modify {
                texts: vec![
                    "[KEYWORD_REDACTED] now [EMAIL_REDACTED]".into(),
                    "fine".into()
                ]
            }
        );
        assert!(bundled_pattern_names().contains(&"credit_card"));
        let lookaround = filter_with_lookaround();
        assert!(matches!(
            run(&lookaround, &["my ssn is 123456789"]).await,
            Outcome::Modify { .. }
        ));
    }

    fn filter_with_lookaround() -> ContentFilter {
        filter(
            serde_json::json!({"guardrail": "content_filter", "patterns": [{"name": "us_ssn_no_dash", "action": "MASK"}]}),
        )
    }
}
