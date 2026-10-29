//! Allow/deny rules over the tool calls a model produced (post_call).

use fancy_regex::Regex;
use serde_json::Value;

use crate::{
    Common, Guardrail, GuardrailError, GuardrailInput, GuardrailSpec, Mode, OnError, Outcome,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Allow,
    Deny,
}

impl Action {
    fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "allow" => Some(Self::Allow),
            "deny" | "block" => Some(Self::Deny),
            _ => None,
        }
    }
}

struct Rule {
    id: String,
    tool_name: Regex,
    action: Action,
}

pub struct ToolPermission {
    common: Common,
    rules: Vec<Rule>,
    default_action: Action,
}

impl ToolPermission {
    pub fn from_spec(spec: &GuardrailSpec) -> Result<Self, String> {
        let common = Common::from_spec(spec, &[Mode::PostCall])?;
        let params = &spec.litellm_params;
        let prefix = format!("guardrail {}", spec.guardrail_name);
        let mut rules = Vec::new();
        for (index, item) in params
            .get("rules")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let pattern = item
                .get("tool_name")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("{prefix}: rule {index} needs a tool_name regex"))?;
            let action = item
                .get("action")
                .and_then(Value::as_str)
                .and_then(Action::parse)
                .ok_or_else(|| format!("{prefix}: rule {index} action must be allow or deny"))?;
            rules.push(Rule {
                id: item
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("rule-{index}")),
                tool_name: Regex::new(&format!("^(?:{pattern})$"))
                    .map_err(|error| format!("{prefix}: rule {index} tool_name: {error}"))?,
                action,
            });
        }
        let default_action = match params.get("default_action").and_then(Value::as_str) {
            Some(action) => Action::parse(action)
                .ok_or_else(|| format!("{prefix}: default_action must be allow or deny"))?,
            None => Action::Deny,
        };
        Ok(Self {
            common,
            rules,
            default_action,
        })
    }

    fn decide(&self, tool: &str) -> (Action, Option<&str>) {
        for rule in &self.rules {
            if rule.tool_name.is_match(tool).unwrap_or(false) {
                return (rule.action, Some(rule.id.as_str()));
            }
        }
        (self.default_action, None)
    }
}

#[async_trait::async_trait]
impl Guardrail for ToolPermission {
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
        for call in input.tool_calls {
            if let (Action::Deny, rule) = self.decide(&call.name) {
                return Ok(Outcome::Block {
                    reason: match rule {
                        Some(id) => format!("tool {:?} is not permitted (rule {id})", call.name),
                        None => format!("tool {:?} is not permitted (default deny)", call.name),
                    },
                });
            }
        }
        Ok(Outcome::Allow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InputType, RequestMeta, ToolCallRef};

    #[tokio::test]
    async fn first_matching_rule_wins_then_the_default() {
        let spec: GuardrailSpec = serde_json::from_value(serde_json::json!({
            "guardrail_name": "tools",
            "litellm_params": {"guardrail": "tool_permission", "rules": [
                {"id": "safe-reads", "tool_name": "read_.*", "action": "allow"},
                {"id": "no-shell", "tool_name": "shell|exec", "action": "deny"}
            ], "default_action": "deny"}
        }))
        .unwrap();
        let guard = ToolPermission::from_spec(&spec).unwrap();
        let meta = RequestMeta::default();
        let calls = |names: &[&str]| {
            names
                .iter()
                .map(|n| ToolCallRef {
                    name: (*n).into(),
                    arguments: "{}".into(),
                })
                .collect::<Vec<_>>()
        };
        let allowed = calls(&["read_file"]);
        assert_eq!(
            guard
                .apply(GuardrailInput {
                    input_type: InputType::Response,
                    texts: &[],
                    tool_calls: &allowed,
                    meta: &meta
                })
                .await
                .unwrap(),
            Outcome::Allow
        );
        let denied = calls(&["read_file", "shell"]);
        assert!(
            matches!(guard.apply(GuardrailInput { input_type: InputType::Response, texts: &[], tool_calls: &denied, meta: &meta }).await.unwrap(), Outcome::Block { reason } if reason.contains("no-shell"))
        );
        let unknown = calls(&["write_file"]);
        assert!(
            matches!(guard.apply(GuardrailInput { input_type: InputType::Response, texts: &[], tool_calls: &unknown, meta: &meta }).await.unwrap(), Outcome::Block { reason } if reason.contains("default deny"))
        );
        assert_eq!(guard.modes(), &[Mode::PostCall]);
    }
}
