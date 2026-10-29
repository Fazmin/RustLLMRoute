use std::collections::HashMap;
use std::time::Duration;

use rustllmroute_config::{FallbackRules, LitellmSettings, RouterSettings, fallback_targets};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RoutingStrategy {
    #[default]
    SimpleShuffle,
    LeastBusy,
    LatencyBased,
    UsageBased,
    CostBased,
}

impl RoutingStrategy {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "simple-shuffle" => Self::SimpleShuffle,
            "least-busy" => Self::LeastBusy,
            "latency-based-routing" => Self::LatencyBased,
            "usage-based-routing" | "usage-based-routing-v2" => Self::UsageBased,
            "cost-based-routing" => Self::CostBased,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct RouterConfig {
    pub strategy: RoutingStrategy,
    pub lowest_latency_buffer: f64,
    pub latency_samples: usize,
    pub num_retries: u32,
    pub retry_after_min: Duration,
    pub cooldown_time: Duration,
    pub allowed_fails: Option<u32>,
    pub disable_cooldowns: bool,
    pub max_fallbacks: u32,
    pub fallbacks: FallbackRules,
    pub context_window_fallbacks: FallbackRules,
    pub content_policy_fallbacks: FallbackRules,
    pub default_fallbacks: Vec<String>,
    pub model_group_alias: HashMap<String, String>,
    pub enable_pre_call_checks: bool,
    pub enable_tag_filtering: bool,
    pub enable_health_check_routing: bool,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            strategy: RoutingStrategy::SimpleShuffle,
            lowest_latency_buffer: 0.0,
            latency_samples: 10,
            num_retries: 2,
            retry_after_min: Duration::ZERO,
            cooldown_time: Duration::from_secs(30),
            allowed_fails: None,
            disable_cooldowns: false,
            max_fallbacks: 5,
            fallbacks: Vec::new(),
            context_window_fallbacks: Vec::new(),
            content_policy_fallbacks: Vec::new(),
            default_fallbacks: Vec::new(),
            model_group_alias: HashMap::new(),
            enable_pre_call_checks: false,
            enable_tag_filtering: false,
            enable_health_check_routing: false,
        }
    }
}

impl RouterConfig {
    pub fn from_settings(router: &RouterSettings, litellm: &LitellmSettings) -> Self {
        let defaults = Self::default();
        Self {
            strategy: router
                .routing_strategy
                .as_deref()
                .and_then(RoutingStrategy::parse)
                .unwrap_or_default(),
            lowest_latency_buffer: router.lowest_latency_buffer.unwrap_or(0.0).max(0.0),
            latency_samples: defaults.latency_samples,
            num_retries: router
                .num_retries
                .or(litellm.num_retries)
                .unwrap_or(defaults.num_retries),
            retry_after_min: router
                .retry_after
                .map(Duration::from_secs_f64)
                .unwrap_or(Duration::ZERO),
            cooldown_time: router
                .cooldown_time
                .map(Duration::from_secs_f64)
                .unwrap_or(defaults.cooldown_time),
            allowed_fails: router.allowed_fails,
            disable_cooldowns: router.disable_cooldowns,
            max_fallbacks: router.max_fallbacks.unwrap_or(defaults.max_fallbacks),
            fallbacks: router.fallbacks.clone(),
            context_window_fallbacks: router.context_window_fallbacks.clone(),
            content_policy_fallbacks: router.content_policy_fallbacks.clone(),
            default_fallbacks: router.default_fallbacks.clone(),
            model_group_alias: router
                .model_group_alias
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            enable_pre_call_checks: router.enable_pre_call_checks,
            enable_tag_filtering: router.enable_tag_filtering,
            enable_health_check_routing: router.enable_health_check_routing,
        }
    }

    /// General fallback targets for a group: the group's own entry, else the `*` entry, else `default_fallbacks`.
    pub fn general_fallbacks(&self, group: &str) -> &[String] {
        fallback_targets(&self.fallbacks, group)
            .or_else(|| fallback_targets(&self.fallbacks, "*"))
            .unwrap_or(self.default_fallbacks.as_slice())
    }

    pub fn context_fallbacks(&self, group: &str) -> Option<&[String]> {
        fallback_targets(&self.context_window_fallbacks, group)
            .or_else(|| fallback_targets(&self.context_window_fallbacks, "*"))
    }

    pub fn content_fallbacks(&self, group: &str) -> Option<&[String]> {
        fallback_targets(&self.content_policy_fallbacks, group)
            .or_else(|| fallback_targets(&self.content_policy_fallbacks, "*"))
    }

    pub fn has_any_fallback(&self, group: &str) -> bool {
        !self.general_fallbacks(group).is_empty()
            || self.context_fallbacks(group).is_some()
            || self.content_fallbacks(group).is_some()
    }
}
