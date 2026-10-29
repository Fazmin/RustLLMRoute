use rustllmroute_types::{ErrorKind, GatewayError};

use crate::config::RouterConfig;
use crate::deployment::RoutedDeployment;
use crate::registry::Resolved;

/// Where to go after a group's retries are exhausted, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// The same group restricted to deployments with this `order` value (next tier).
    Tier(i32),
    Group(String),
}

pub fn targets<D: RoutedDeployment>(
    cfg: &RouterConfig,
    group: &Resolved<D>,
    error: &GatewayError,
    current_tier: Option<i32>,
    body_fallbacks: Option<&[String]>,
) -> Vec<Target> {
    let mut out = Vec::new();
    let kind = error.kind();
    if !matches!(kind, ErrorKind::ContextWindow | ErrorKind::ContentPolicy) && !group.direct {
        let current = current_tier.unwrap_or_else(|| {
            group
                .members
                .iter()
                .map(|d| d.meta().order.unwrap_or(0))
                .min()
                .unwrap_or(0)
        });
        let mut tiers: Vec<i32> = group
            .members
            .iter()
            .map(|d| d.meta().order.unwrap_or(0))
            .filter(|order| *order > current)
            .collect();
        tiers.sort_unstable();
        tiers.dedup();
        out.extend(tiers.into_iter().map(Target::Tier));
    }
    let specific: Option<&[String]> = match kind {
        ErrorKind::ContextWindow => cfg.context_fallbacks(&group.group),
        ErrorKind::ContentPolicy => cfg.content_fallbacks(&group.group),
        _ => None,
    };
    if let Some(names) = specific {
        out.extend(names.iter().cloned().map(Target::Group));
    }
    let general = body_fallbacks.unwrap_or_else(|| cfg.general_fallbacks(&group.group));
    for name in general {
        let target = Target::Group(name.clone());
        if name.as_str() != group.group.as_ref() && !out.contains(&target) {
            out.push(target);
        }
    }
    out
}
