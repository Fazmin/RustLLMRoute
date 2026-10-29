use std::collections::HashMap;
use std::sync::Arc;

use crate::deployment::RoutedDeployment;

/// Model groups, aliases, wildcard patterns and direct deployment ids.
pub struct Registry<D> {
    by_id: HashMap<Arc<str>, Arc<D>>,
    groups: HashMap<Arc<str>, Vec<Arc<D>>>,
    wildcards: Vec<(Arc<str>, Vec<Arc<D>>)>,
    aliases: HashMap<String, String>,
}

pub struct Resolved<D> {
    pub group: Arc<str>,
    /// The name used for provider-model substitution (the client's model, a fallback target, ...).
    pub requested: String,
    pub members: Vec<Arc<D>>,
    /// The request named a deployment id; filters are bypassed.
    pub direct: bool,
}

impl<D> Default for Registry<D> {
    fn default() -> Self {
        Self {
            by_id: HashMap::new(),
            groups: HashMap::new(),
            wildcards: Vec::new(),
            aliases: HashMap::new(),
        }
    }
}

impl<D: RoutedDeployment> Registry<D> {
    pub fn new(deployments: Vec<Arc<D>>, aliases: HashMap<String, String>) -> Self {
        let mut by_id = HashMap::new();
        let mut groups: HashMap<Arc<str>, Vec<Arc<D>>> = HashMap::new();
        let mut wildcard_groups: HashMap<Arc<str>, Vec<Arc<D>>> = HashMap::new();
        for deployment in deployments {
            by_id.insert(deployment.id().clone(), deployment.clone());
            let name = deployment.model_group().clone();
            if name.contains('*') {
                wildcard_groups.entry(name).or_default().push(deployment);
            } else {
                groups.entry(name).or_default().push(deployment);
            }
        }
        let mut wildcards: Vec<_> = wildcard_groups.into_iter().collect();
        wildcards.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        Self {
            by_id,
            groups,
            wildcards,
            aliases,
        }
    }

    /// Key alias, then `model_group_alias`, then exact group, then deployment id, then the most
    /// specific wildcard pattern.
    pub fn resolve(
        &self,
        model: &str,
        key_aliases: &HashMap<String, String>,
    ) -> Option<Resolved<D>> {
        let name = key_aliases
            .get(model)
            .or_else(|| self.aliases.get(model))
            .map(String::as_str)
            .unwrap_or(model);
        if let Some((group, members)) = self.groups.get_key_value(name) {
            return Some(Resolved {
                group: group.clone(),
                requested: name.to_owned(),
                members: members.clone(),
                direct: false,
            });
        }
        if let Some(deployment) = self.by_id.get(name) {
            return Some(Resolved {
                group: deployment.model_group().clone(),
                requested: name.to_owned(),
                members: vec![deployment.clone()],
                direct: true,
            });
        }
        self.wildcards
            .iter()
            .find(|(pattern, _)| wildcard_matches(pattern, name))
            .map(|(pattern, members)| Resolved {
                group: pattern.clone(),
                requested: name.to_owned(),
                members: members.clone(),
                direct: false,
            })
    }

    pub fn get(&self, id: &str) -> Option<&Arc<D>> {
        self.by_id.get(id)
    }

    pub fn model_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.groups.keys().map(|k| k.to_string()).collect();
        names.extend(self.aliases.keys().cloned());
        names.sort();
        names.dedup();
        names
    }

    pub fn group_names(&self) -> impl Iterator<Item = &Arc<str>> {
        self.groups
            .keys()
            .chain(self.wildcards.iter().map(|(p, _)| p))
    }

    pub fn deployments(&self) -> impl Iterator<Item = &Arc<D>> {
        self.by_id.values()
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

pub fn wildcard_matches(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        Some((prefix, suffix)) => {
            name.len() >= prefix.len() + suffix.len()
                && name.starts_with(prefix)
                && name.ends_with(suffix)
        }
        None => pattern == name,
    }
}

impl<D> Clone for Resolved<D> {
    fn clone(&self) -> Self {
        Self {
            group: self.group.clone(),
            requested: self.requested.clone(),
            members: self.members.clone(),
            direct: self.direct,
        }
    }
}
