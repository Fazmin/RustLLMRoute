use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rustllmroute_core::AuthContext;
use rustllmroute_db::{KeyRow, TeamRow, UserRow};
use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    ProxyAdmin,
    InternalUser,
    InternalUserViewer,
}

impl Role {
    pub fn parse(name: &str) -> Self {
        match name {
            "proxy_admin" => Self::ProxyAdmin,
            "internal_user_viewer" | "proxy_admin_viewer" => Self::InternalUserViewer,
            _ => Self::InternalUser,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProxyAdmin => "proxy_admin",
            Self::InternalUser => "internal_user",
            Self::InternalUserViewer => "internal_user_viewer",
        }
    }
}

/// Precompiled `models` list: empty, `*` and `all-proxy-models` allow everything; entries may be
/// exact names or globs such as `openai/*`; `all-team-models` defers to the team's list.
#[derive(Clone, Debug, Default)]
pub struct ModelMatcher {
    any: bool,
    exact: HashSet<String>,
    globs: Vec<(String, String)>,
    pub inherit_team: bool,
}

impl ModelMatcher {
    pub fn from_list(models: &[String]) -> Self {
        let mut matcher = Self::default();
        if models.is_empty() {
            matcher.any = true;
            return matcher;
        }
        for model in models {
            match model.as_str() {
                "*" | "all-proxy-models" => matcher.any = true,
                "all-team-models" => matcher.inherit_team = true,
                pattern if pattern.contains('*') => {
                    if let Some((prefix, suffix)) = pattern.split_once('*') {
                        matcher.globs.push((prefix.to_owned(), suffix.to_owned()));
                    }
                }
                exact => {
                    matcher.exact.insert(exact.to_owned());
                }
            }
        }
        matcher
    }

    pub fn allows(&self, model: &str) -> bool {
        self.any
            || self.exact.contains(model)
            || self.globs.iter().any(|(prefix, suffix)| {
                model.len() >= prefix.len() + suffix.len()
                    && model.starts_with(prefix)
                    && model.ends_with(suffix)
            })
    }

    pub fn is_unrestricted(&self) -> bool {
        self.any
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Limits {
    pub rpm: Option<u32>,
    pub tpm: Option<u32>,
    pub max_parallel: Option<u32>,
}

fn limits(rpm: Option<i64>, tpm: Option<i64>, parallel: Option<i64>) -> Limits {
    let to_u32 = |value: Option<i64>| value.and_then(|v| u32::try_from(v).ok());
    Limits {
        rpm: to_u32(rpm),
        tpm: to_u32(tpm),
        max_parallel: to_u32(parallel),
    }
}

/// A resolved virtual key with its team and user, cached for the auth TTL.
#[derive(Clone, Debug)]
pub struct Principal {
    pub token_hash: Arc<str>,
    pub key: KeyRow,
    pub team: Option<TeamRow>,
    pub user: Option<UserRow>,
    pub role: Role,
    pub models: ModelMatcher,
    pub team_models: Option<ModelMatcher>,
    pub aliases: Arc<HashMap<String, String>>,
    pub metadata: Arc<Map<String, Value>>,
}

impl Principal {
    pub fn from_rows(key: KeyRow, team: Option<TeamRow>, user: Option<UserRow>) -> Self {
        let role = user
            .as_ref()
            .map(|u| Role::parse(&u.user_role))
            .unwrap_or(Role::InternalUser);
        let models = ModelMatcher::from_list(&key.models_list());
        let team_models = team
            .as_ref()
            .map(|t| ModelMatcher::from_list(&t.models_list()));
        let aliases = Arc::new(key.aliases_map());
        let metadata = Arc::new(match key.metadata_value() {
            Value::Object(map) => map,
            _ => Map::new(),
        });
        Self {
            token_hash: Arc::from(key.token.as_str()),
            key,
            team,
            user,
            role,
            models,
            team_models,
            aliases,
            metadata,
        }
    }

    /// Key list first; an empty key list or `all-team-models` defers to the team; a team with an
    /// explicit list also bounds what its keys may use.
    pub fn allows_model(&self, model: &str) -> bool {
        let key_list_empty = self.key.models_list().is_empty();
        if self.models.inherit_team || key_list_empty {
            return self
                .team_models
                .as_ref()
                .is_none_or(|team| team.allows(model));
        }
        if !self.models.allows(model) {
            return false;
        }
        self.team_models
            .as_ref()
            .is_none_or(|team| team.is_unrestricted() || team.allows(model))
    }

    pub fn key_limits(&self) -> Limits {
        limits(
            self.key.rpm_limit,
            self.key.tpm_limit,
            self.key.max_parallel_requests,
        )
    }

    pub fn team_limits(&self) -> Option<Limits> {
        self.team
            .as_ref()
            .map(|t| limits(t.rpm_limit, t.tpm_limit, t.max_parallel_requests))
    }

    pub fn user_limits(&self) -> Option<Limits> {
        self.user
            .as_ref()
            .map(|u| limits(u.rpm_limit, u.tpm_limit, u.max_parallel_requests))
    }

    pub fn is_admin(&self) -> bool {
        self.role == Role::ProxyAdmin
    }

    pub fn auth_context(&self) -> AuthContext {
        AuthContext {
            key_hash: Some(self.token_hash.clone()),
            key_alias: self.key.key_alias.clone(),
            user_id: self.key.user_id.clone(),
            team_id: self.key.team_id.clone(),
            is_admin: self.is_admin(),
            model_aliases: self.aliases.clone(),
            metadata: self.metadata.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn principal(key_models: &str, team_models: Option<&str>) -> Principal {
        let mut key = KeyRow::new("h".to_owned(), 0);
        key.models = key_models.to_owned();
        let team = team_models.map(|models| {
            let mut team = TeamRow::new("t".to_owned(), 0);
            team.models = models.to_owned();
            team
        });
        Principal::from_rows(key, team, None)
    }

    #[rstest]
    #[case::empty_allows_all("[]", None, "anything", true)]
    #[case::star("[\"*\"]", None, "anything", true)]
    #[case::exact("[\"gpt-4o\"]", None, "gpt-4o", true)]
    #[case::exact_miss("[\"gpt-4o\"]", None, "gpt-4o-mini", false)]
    #[case::glob("[\"openai/*\"]", None, "openai/gpt-4o", true)]
    #[case::glob_miss("[\"openai/*\"]", None, "anthropic/claude", false)]
    #[case::inherit_team("[\"all-team-models\"]", Some("[\"claude\"]"), "claude", true)]
    #[case::inherit_team_miss("[\"all-team-models\"]", Some("[\"claude\"]"), "gpt-4o", false)]
    #[case::empty_key_uses_team("[]", Some("[\"claude\"]"), "gpt-4o", false)]
    #[case::team_bounds_key("[\"gpt-4o\"]", Some("[\"claude\"]"), "gpt-4o", false)]
    #[case::team_unrestricted("[\"gpt-4o\"]", Some("[]"), "gpt-4o", true)]
    fn model_access(
        #[case] key_models: &str,
        #[case] team_models: Option<&str>,
        #[case] model: &str,
        #[case] expected: bool,
    ) {
        assert_eq!(
            principal(key_models, team_models).allows_model(model),
            expected
        );
    }
}
