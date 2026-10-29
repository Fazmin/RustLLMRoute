use std::collections::BTreeMap;

use serde_json::Value;

const PREFIX: &str = "os.environ/";

/// Replaces every `os.environ/NAME` string in the tree with the variable's value, looking at the
/// process environment first and then at the file's own `environment_variables` map. Each missing
/// variable is reported with the YAML path that referenced it.
pub fn resolve_env_references(
    value: &mut Value,
    file_env: &BTreeMap<String, String>,
    errors: &mut Vec<String>,
) {
    walk(value, "", file_env, errors);
}

fn walk(
    value: &mut Value,
    path: &str,
    file_env: &BTreeMap<String, String>,
    errors: &mut Vec<String>,
) {
    match value {
        Value::String(text) => {
            if let Some(name) = text.strip_prefix(PREFIX) {
                let name = name.trim();
                match std::env::var(name)
                    .ok()
                    .or_else(|| file_env.get(name).cloned())
                {
                    Some(resolved) => *text = resolved,
                    None => errors.push(format!("{path}: environment variable {name} is not set")),
                }
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                walk(item, &format!("{path}[{index}]"), file_env, errors);
            }
        }
        Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                let child = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                walk(item, &child, file_env, errors);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use serde_json::json;

    #[rstest]
    fn resolves_from_file_map_and_reports_missing_with_path() {
        let mut value = json!({
            "model_list": [{"litellm_params": {"api_key": "os.environ/RLR_TEST_PRESENT", "api_base": "os.environ/RLR_TEST_MISSING"}}]
        });
        let file_env = BTreeMap::from([("RLR_TEST_PRESENT".to_owned(), "secret".to_owned())]);
        let mut errors = Vec::new();
        resolve_env_references(&mut value, &file_env, &mut errors);
        assert_eq!(
            value["model_list"][0]["litellm_params"]["api_key"],
            "secret"
        );
        assert_eq!(
            errors,
            vec![
                "model_list[0].litellm_params.api_base: environment variable RLR_TEST_MISSING is not set"
            ]
        );
    }
}
