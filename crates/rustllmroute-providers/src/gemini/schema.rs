//! JSON Schema -> the OpenAPI subset Gemini accepts for function parameters and response schemas.

use serde_json::{Map, Value, json};

const VALID_FIELDS: &[&str] = &[
    "type",
    "format",
    "title",
    "description",
    "nullable",
    "enum",
    "properties",
    "required",
    "items",
    "anyOf",
    "minimum",
    "maximum",
    "minItems",
    "maxItems",
    "minLength",
    "maxLength",
    "pattern",
    "propertyOrdering",
    "default",
    "example",
    "minProperties",
    "maxProperties",
];
const MAX_DEPTH: usize = 32;

pub fn clean_schema(schema: &Value) -> Value {
    let mut root = schema.clone();
    let defs = extract_defs(&mut root);
    clean(&mut root, &defs, 0);
    root
}

fn extract_defs(root: &mut Value) -> Map<String, Value> {
    let mut defs = Map::new();
    if let Some(object) = root.as_object_mut() {
        for key in ["$defs", "definitions"] {
            if let Some(Value::Object(map)) = object.remove(key) {
                defs.extend(map);
            }
        }
    }
    defs
}

fn resolve_ref(value: &mut Value, defs: &Map<String, Value>) {
    let Some(reference) = value.get("$ref").and_then(Value::as_str) else {
        return;
    };
    let name = reference.rsplit('/').next().unwrap_or(reference).to_owned();
    let siblings = value.as_object().cloned().unwrap_or_default();
    match defs.get(&name) {
        Some(definition) => {
            let mut merged = definition.clone();
            if let Some(target) = merged.as_object_mut() {
                for (key, sibling) in siblings {
                    if key != "$ref" {
                        target.entry(key).or_insert(sibling);
                    }
                }
            }
            *value = merged;
        }
        None => {
            if let Some(object) = value.as_object_mut() {
                object.remove("$ref");
            }
        }
    }
}

fn is_null_type(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("null")
}

fn clean(value: &mut Value, defs: &Map<String, Value>, depth: usize) {
    if depth > MAX_DEPTH {
        *value = json!({"type": "object"});
        return;
    }
    resolve_ref(value, defs);
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if let Some(constant) = object.remove("const") {
        object.insert("enum".to_owned(), Value::Array(vec![constant]));
    }
    if let Some(Value::Array(types)) = object.get("type").cloned() {
        let had_null = types.iter().any(|t| t == "null");
        let rest: Vec<Value> = types.into_iter().filter(|t| t != "null").collect();
        object.remove("type");
        match rest.len() {
            0 => {}
            1 => {
                object.insert(
                    "type".to_owned(),
                    rest.into_iter().next().unwrap_or(Value::Null),
                );
            }
            _ => {
                object.insert(
                    "anyOf".to_owned(),
                    Value::Array(rest.into_iter().map(|t| json!({"type": t})).collect()),
                );
            }
        }
        if had_null {
            object.insert("nullable".to_owned(), Value::Bool(true));
        }
    }
    if let Some(one_of) = object.remove("oneOf") {
        object.entry("anyOf").or_insert(one_of);
    }
    if let Some(all_of) = object.remove("allOf")
        && let Value::Array(parts) = all_of
    {
        for part in parts {
            if let Value::Object(fields) = part {
                for (key, field) in fields {
                    object.entry(key).or_insert(field);
                }
            }
        }
    }
    if let Some(Value::Array(branches)) = object.get_mut("anyOf") {
        let had_null = branches.iter().any(is_null_type);
        branches.retain(|branch| !is_null_type(branch));
        for branch in branches.iter_mut() {
            if had_null && let Some(fields) = branch.as_object_mut() {
                fields.insert("nullable".to_owned(), Value::Bool(true));
            }
            clean(branch, defs, depth + 1);
        }
        let collapse = match branches.len() {
            0 => Some(json!({"type": "object"})),
            1 => Some(branches.remove(0)),
            _ => None,
        };
        if let Some(Value::Object(fields)) = collapse {
            object.remove("anyOf");
            for (key, field) in fields {
                object.entry(key).or_insert(field);
            }
        }
    }
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    if let Some(kind) = &kind {
        object.insert("type".to_owned(), Value::String(kind.clone()));
    }
    if object.contains_key("enum")
        && matches!(kind.as_deref(), Some("integer" | "number" | "boolean"))
    {
        object.remove("enum");
    }
    if let Some(format) = object.get("format").and_then(Value::as_str) {
        let keep = match kind.as_deref() {
            Some("string") => matches!(format, "enum" | "date-time"),
            Some("number") => matches!(format, "float" | "double"),
            Some("integer") => matches!(format, "int32" | "int64"),
            _ => false,
        };
        if !keep {
            object.remove("format");
        }
    }
    if let Some(items) = object.get_mut("items") {
        if items.as_object().is_some_and(Map::is_empty) || !items.is_object() {
            *items = json!({"type": "object"});
        } else {
            clean(items, defs, depth + 1);
        }
    }
    let mut has_properties = false;
    if let Some(Value::Object(properties)) = object.get_mut("properties") {
        for property in properties.values_mut() {
            clean(property, defs, depth + 1);
        }
        has_properties = !properties.is_empty();
        if !has_properties {
            object.remove("properties");
            object.remove("required");
        }
    }
    if has_properties {
        object.insert("type".to_owned(), Value::String("object".to_owned()));
        if let Some(Value::Array(required)) = object.get("required").cloned() {
            let names: Vec<Value> = required
                .into_iter()
                .filter(|name| {
                    name.as_str().is_some_and(|name| {
                        object
                            .get("properties")
                            .and_then(Value::as_object)
                            .is_some_and(|props| props.contains_key(name))
                    })
                })
                .collect();
            if names.is_empty() {
                object.remove("required");
            } else {
                object.insert("required".to_owned(), Value::Array(names));
            }
        }
    } else if !object.contains_key("type") && !object.contains_key("anyOf") {
        object.insert("type".to_owned(), Value::String("object".to_owned()));
    }
    object.retain(|key, _| VALID_FIELDS.contains(&key.as_str()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_unsupported_keywords_and_expands_refs() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "$defs": {"Unit": {"type": "string", "enum": ["c", "f"], "format": "uuid"}},
            "properties": {
                "city": {"type": ["string", "null"], "description": "d", "format": "email"},
                "unit": {"$ref": "#/$defs/Unit", "description": "unit"},
                "count": {"type": "integer", "enum": [1, 2], "format": "int32"},
                "tags": {"type": "array", "items": {}},
                "choice": {"anyOf": [{"type": "number"}, {"type": "null"}]},
                "mode": {"const": "fast"}
            },
            "required": ["city", "missing"],
            "strict": true
        });
        let cleaned = clean_schema(&schema);
        assert!(cleaned.get("$schema").is_none());
        assert!(cleaned.get("additionalProperties").is_none());
        assert!(cleaned.get("strict").is_none());
        assert_eq!(cleaned["properties"]["city"]["type"], "string");
        assert_eq!(cleaned["properties"]["city"]["nullable"], true);
        assert!(cleaned["properties"]["city"].get("format").is_none());
        assert_eq!(cleaned["properties"]["unit"]["enum"], json!(["c", "f"]));
        assert_eq!(cleaned["properties"]["unit"]["description"], "unit");
        assert!(cleaned["properties"]["unit"].get("format").is_none());
        assert!(cleaned["properties"]["count"].get("enum").is_none());
        assert_eq!(cleaned["properties"]["count"]["format"], "int32");
        assert_eq!(cleaned["properties"]["tags"]["items"]["type"], "object");
        assert_eq!(cleaned["properties"]["choice"]["type"], "number");
        assert_eq!(cleaned["properties"]["choice"]["nullable"], true);
        assert_eq!(cleaned["properties"]["mode"]["enum"], json!(["fast"]));
        assert_eq!(cleaned["required"], json!(["city"]));
    }

    #[test]
    fn empty_schemas_become_objects() {
        assert_eq!(clean_schema(&json!({})), json!({"type": "object"}));
        assert_eq!(
            clean_schema(&json!({"type": "object", "properties": {}})),
            json!({"type": "object"})
        );
    }
}
