use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use rustllmroute_types::GatewayError;

use crate::deployment::RoutedDeployment;
use crate::execute::{RequestCtx, Router, Selected};
use crate::health::HealthStatus;
use crate::registry::Resolved;
use crate::stats::current_minute;
use crate::strategy::Candidate;

const CONTEXT_HEADROOM: f64 = 1.1;

fn tag_allowed(request_tags: &[String], deployment_tags: &[String], any_matches: bool) -> bool {
    if request_tags.is_empty() || !any_matches {
        return deployment_tags.is_empty() || deployment_tags.iter().any(|t| t == "default");
    }
    deployment_tags.iter().any(|t| request_tags.contains(t))
}

impl<D: RoutedDeployment> Router<D> {
    pub(crate) async fn select(
        &self,
        group: &Resolved<D>,
        ctx: &RequestCtx<'_>,
        skip: &HashSet<Arc<str>>,
        min_order: Option<i32>,
        retry_index: u32,
        fallback_depth: u32,
    ) -> Result<Selected<D>, GatewayError> {
        if group.direct {
            let deployment = &group.members[0];
            if deployment.meta().blocked {
                return Err(GatewayError::NoDeployments {
                    message: format!("deployment {} is blocked", deployment.id()),
                    retry_after: None,
                });
            }
            return Ok(self.selected(deployment, group, retry_index, fallback_depth));
        }
        let minute = current_minute();
        let ids: Vec<&Arc<str>> = group.members.iter().map(|d| d.id()).collect();
        let snapshot = self.store.snapshot(&ids, minute).await;
        let any_tag_match = ctx
            .tags
            .iter()
            .any(|tag| group.members.iter().any(|d| d.meta().tags.contains(tag)));
        let input_tokens = u64::from(ctx.input_tokens.unwrap_or(0));
        let mut shortest_cooldown: Option<Duration> = None;
        let mut context_blocked = false;
        let mut candidates: Vec<Candidate<'_, D>> = Vec::with_capacity(group.members.len());
        for (index, deployment) in group.members.iter().enumerate() {
            let meta = deployment.meta();
            if !ctx.endpoint.accepts(deployment.mode())
                || meta.blocked
                || skip.contains(deployment.id())
            {
                continue;
            }
            if let Some(remaining) = snapshot.cooled[index] {
                shortest_cooldown = Some(shortest_cooldown.map_or(remaining, |s| s.min(remaining)));
                continue;
            }
            if let Some(order) = min_order
                && meta.order.unwrap_or(0) < order
            {
                continue;
            }
            let stats = self.stats_for(deployment.id());
            if self.cfg.enable_health_check_routing
                && stats.health.read().status == HealthStatus::Unhealthy
            {
                continue;
            }
            if let Some(max) = meta.max_parallel_requests
                && stats.in_flight() >= max
            {
                continue;
            }
            if self.cfg.enable_pre_call_checks {
                if let (Some(tokens), Some(limit)) =
                    (ctx.input_tokens, deployment.pricing().max_input_tokens)
                    && f64::from(tokens) > f64::from(limit) * CONTEXT_HEADROOM
                {
                    context_blocked = true;
                    continue;
                }
                if let Some(rpm) = meta.rpm
                    && snapshot.rpm[index] >= rpm
                {
                    continue;
                }
            }
            if self.cfg.enable_tag_filtering && !tag_allowed(ctx.tags, &meta.tags, any_tag_match) {
                continue;
            }
            candidates.push(Candidate { index, deployment });
        }
        if candidates.is_empty() {
            if context_blocked {
                return Err(GatewayError::ContextWindowExceeded(format!(
                    "no deployment of {} accepts a prompt of {} tokens",
                    group.group,
                    ctx.input_tokens.unwrap_or(0)
                )));
            }
            return Err(GatewayError::NoDeployments {
                message: match shortest_cooldown {
                    Some(_) => format!("all deployments of {} are cooling down", group.group),
                    None => format!("no available deployment for {}", group.group),
                },
                retry_after: shortest_cooldown.map(|d| d.max(Duration::from_secs(1))),
            });
        }
        let tier = candidates
            .iter()
            .map(|c| c.deployment.meta().order.unwrap_or(0))
            .min()
            .unwrap_or(0);
        candidates.retain(|c| c.deployment.meta().order.unwrap_or(0) == tier);
        let picked = self
            .pick(&candidates, &snapshot, input_tokens, ctx.stream)
            .ok_or_else(|| GatewayError::NoDeployments {
                message: format!(
                    "every deployment of {} is at its per-minute limit",
                    group.group
                ),
                retry_after: Some(Duration::from_secs(60)),
            })?;
        Ok(self.selected(picked, group, retry_index, fallback_depth))
    }

    fn selected(
        &self,
        deployment: &Arc<D>,
        group: &Resolved<D>,
        retry_index: u32,
        fallback_depth: u32,
    ) -> Selected<D> {
        Selected {
            model: deployment.provider_model_for(&group.requested),
            deployment: deployment.clone(),
            group: group.group.clone(),
            retry_index,
            fallback_depth,
        }
    }
}
