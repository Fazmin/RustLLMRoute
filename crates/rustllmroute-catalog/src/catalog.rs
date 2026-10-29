use std::collections::HashMap;

use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::{Error, ModelPricing};

#[derive(Clone, Debug)]
pub struct ModelEntry {
    provider: Option<String>,
    pricing: ModelPricing,
    fields: Map<String, Value>,
}

impl ModelEntry {
    pub fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }

    pub fn pricing(&self) -> &ModelPricing {
        &self.pricing
    }

    pub fn field(&self, name: &str) -> Option<&Value> {
        self.fields.get(name)
    }

    pub fn fields(&self) -> &Map<String, Value> {
        &self.fields
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ModelMatch<'a> {
    /// The key the caller asked for (or an alias of it) as spelled in the table.
    pub matched_key: &'a str,
    /// The canonical entry key after alias resolution.
    pub canonical_key: &'a str,
    pub entry: &'a ModelEntry,
}

#[derive(Debug)]
pub struct Catalog {
    entries: IndexMap<String, ModelEntry>,
    aliases: IndexMap<String, String>,
    lowercase_keys: HashMap<String, String>,
    issues: Vec<String>,
}

impl Catalog {
    pub fn parse(body: &[u8]) -> Result<Self, Error> {
        let root: IndexMap<String, Value> = serde_json::from_slice(body)?;
        if root.is_empty() {
            return Err(Error::Empty);
        }
        let mut entries = IndexMap::with_capacity(root.len());
        let mut alias_lists = Vec::new();
        let mut issues = Vec::new();
        for (name, value) in root {
            if name == "sample_spec" || name == "fallback_generalizations" {
                continue;
            }
            let Value::Object(mut fields) = value else {
                issues.push(format!("model {name:?} is not an object"));
                continue;
            };
            let pricing = match ModelPricing::from_fields(&fields) {
                Ok(pricing) => pricing,
                Err(error) => {
                    issues.push(format!("model {name:?}: {error}"));
                    ModelPricing::default()
                }
            };
            if let Some(Value::Array(names)) = fields.remove("aliases") {
                alias_lists.push((name.clone(), names));
            }
            let provider = fields
                .get("litellm_provider")
                .and_then(Value::as_str)
                .map(str::to_owned);
            entries.insert(
                name,
                ModelEntry {
                    provider,
                    pricing,
                    fields,
                },
            );
        }
        let mut aliases = IndexMap::new();
        for (model, names) in alias_lists {
            for name in names {
                let Value::String(alias) = name else { continue };
                if !entries.contains_key(&alias) && !aliases.contains_key(&alias) {
                    aliases.insert(alias, model.clone());
                }
            }
        }
        let lowercase_keys = entries
            .keys()
            .chain(aliases.keys())
            .map(|key| (key.to_lowercase(), key.clone()))
            .collect();
        Ok(Self {
            entries,
            aliases,
            lowercase_keys,
            issues,
        })
    }

    pub fn require_at_least(&self, minimum: usize) -> Result<(), Error> {
        let actual = self.entries.len();
        if actual < minimum {
            return Err(Error::TooSmall { actual, minimum });
        }
        Ok(())
    }

    /// Exact key, then alias, then case-insensitive match.
    pub fn lookup(&self, key: &str) -> Option<ModelMatch<'_>> {
        let matched_key = if self.entries.contains_key(key) || self.aliases.contains_key(key) {
            key
        } else {
            self.lowercase_keys.get(&key.to_lowercase())?.as_str()
        };
        let canonical_key = self
            .aliases
            .get(matched_key)
            .map(String::as_str)
            .unwrap_or(matched_key);
        let (canonical_key, entry) = self.entries.get_key_value(canonical_key)?;
        let matched_key = self
            .entries
            .get_key_value(matched_key)
            .map(|(key, _)| key.as_str())
            .or_else(|| {
                self.aliases
                    .get_key_value(matched_key)
                    .map(|(key, _)| key.as_str())
            })?;
        Some(ModelMatch {
            matched_key,
            canonical_key,
            entry,
        })
    }

    pub fn model_count(&self) -> usize {
        self.entries.len()
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, &ModelEntry)> {
        self.entries
            .iter()
            .map(|(key, entry)| (key.as_str(), entry))
    }

    pub fn issues(&self) -> &[String] {
        &self.issues
    }
}
