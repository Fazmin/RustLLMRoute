use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use arc_swap::ArcSwap;
use dashmap::DashMap;
use rustllmroute_types::GatewayError;
use tokio::time::Instant;

use crate::config::RouterConfig;
use crate::cooldown;
use crate::deployment::{Endpoint, RoutedDeployment};
use crate::fallback::{self, Target};
use crate::health::{EndpointHealth, HealthReport, HealthState, HealthStatus};
use crate::registry::{Registry, Resolved};
use crate::retry::{self, RetryContext};
use crate::state::{MemoryStateStore, RouterStateStore, StatsMap};
use crate::stats::{DeploymentStats, current_minute};

pub struct RequestCtx<'a> {
    pub endpoint: Endpoint,
    pub stream: bool,
    /// Counted by the caller only when [`Router::needs_input_tokens`] says so.
    pub input_tokens: Option<u32>,
    pub tags: &'a [String],
    pub key_model_aliases: &'a HashMap<String, String>,
    pub body_fallbacks: Option<&'a [String]>,
    pub num_retries: Option<u32>,
    pub disable_fallbacks: bool,
}

impl<'a> RequestCtx<'a> {
    pub fn new(
        endpoint: Endpoint,
        stream: bool,
        key_model_aliases: &'a HashMap<String, String>,
    ) -> Self {
        Self {
            endpoint,
            stream,
            input_tokens: None,
            tags: &[],
            key_model_aliases,
            body_fallbacks: None,
            num_retries: None,
            disable_fallbacks: false,
        }
    }
}

pub struct Selected<D> {
    pub deployment: Arc<D>,
    /// Provider model for this call (wildcard substituted).
    pub model: String,
    pub group: Arc<str>,
    pub retry_index: u32,
    pub fallback_depth: u32,
}

impl<D> Clone for Selected<D> {
    fn clone(&self) -> Self {
        Self {
            deployment: self.deployment.clone(),
            model: self.model.clone(),
            group: self.group.clone(),
            retry_index: self.retry_index,
            fallback_depth: self.fallback_depth,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AttemptUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// What one attempt returns. `usage` is `None` for streams, whose usage is only known at the end.
pub struct Attempt<T> {
    pub value: T,
    pub usage: Option<AttemptUsage>,
    pub ttft: Option<Duration>,
}

/// Tracks one in-flight attempt. Call [`complete`](Self::complete) or [`fail`](Self::fail) when the
/// result is known; dropping it without either only releases the in-flight slot.
pub struct AttemptGuard {
    stats: Arc<DeploymentStats>,
    store: Arc<dyn RouterStateStore>,
    id: Arc<str>,
    started: Instant,
    done: bool,
}

impl AttemptGuard {
    pub fn complete(&mut self, usage: Option<AttemptUsage>, ttft: Option<Duration>) {
        if self.done {
            return;
        }
        self.done = true;
        let tokens = usage.map_or(0, |u| u.prompt_tokens + u.completion_tokens);
        self.store
            .record_result(&self.id, current_minute(), true, tokens);
        metrics::counter!(rustllmroute_types::metrics::DEPLOYMENT_TOTAL_REQUESTS, "model_id" => self.id.to_string()).increment(1);
        metrics::counter!(rustllmroute_types::metrics::DEPLOYMENT_SUCCESS_RESPONSES, "model_id" => self.id.to_string()).increment(1);
        let elapsed = self.started.elapsed();
        let outputs = usage.map_or(1, |u| u.completion_tokens.max(1));
        self.stats
            .record_latency(elapsed / u32::try_from(outputs).unwrap_or(u32::MAX), ttft);
        self.stats.in_flight.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn fail(&mut self, _error: &GatewayError) {
        if self.done {
            return;
        }
        self.done = true;
        self.store
            .record_result(&self.id, current_minute(), false, 0);
        metrics::counter!(rustllmroute_types::metrics::DEPLOYMENT_TOTAL_REQUESTS, "model_id" => self.id.to_string()).increment(1);
        metrics::counter!(rustllmroute_types::metrics::DEPLOYMENT_FAILURE_RESPONSES, "model_id" => self.id.to_string()).increment(1);
        self.stats.in_flight.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

impl Drop for AttemptGuard {
    fn drop(&mut self) {
        if !self.done {
            self.stats.in_flight.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

pub struct Outcome<T, D> {
    pub value: T,
    pub selected: Selected<D>,
    pub attempted_retries: u32,
    pub attempted_fallbacks: u32,
    pub guard: AttemptGuard,
}

pub struct Router<D> {
    registry: ArcSwap<Registry<D>>,
    stats: StatsMap,
    pub(crate) store: Arc<dyn RouterStateStore>,
    pub(crate) cfg: RouterConfig,
}

impl<D: RoutedDeployment> Router<D> {
    pub fn new(registry: Registry<D>, cfg: RouterConfig) -> Self {
        let stats: StatsMap = Arc::new(DashMap::new());
        let store = Arc::new(MemoryStateStore::new(stats.clone()));
        Self::with_store(registry, cfg, stats, store)
    }

    pub fn with_store(
        registry: Registry<D>,
        cfg: RouterConfig,
        stats: StatsMap,
        store: Arc<dyn RouterStateStore>,
    ) -> Self {
        Self {
            registry: ArcSwap::from_pointee(registry),
            stats,
            store,
            cfg,
        }
    }

    pub fn config(&self) -> &RouterConfig {
        &self.cfg
    }

    pub fn registry(&self) -> arc_swap::Guard<Arc<Registry<D>>> {
        self.registry.load()
    }

    pub fn replace_registry(&self, registry: Registry<D>) {
        self.registry.store(Arc::new(registry));
    }

    pub fn stats_for(&self, id: &Arc<str>) -> Arc<DeploymentStats> {
        self.stats
            .entry(id.clone())
            .or_insert_with(|| Arc::new(DeploymentStats::new(self.cfg.latency_samples)))
            .clone()
    }

    pub fn store(&self) -> &Arc<dyn RouterStateStore> {
        &self.store
    }

    /// Records a health probe result; returns the previous status so callers can log transitions.
    pub fn set_health(&self, id: &Arc<str>, state: HealthState) -> HealthStatus {
        let stats = self.stats_for(id);
        let mut health = stats.health.write();
        let previous = health.status;
        *health = state;
        previous
    }

    /// True when a token count would change routing for this model, so callers can skip counting.
    pub fn needs_input_tokens(&self, model: &str, key_aliases: &HashMap<String, String>) -> bool {
        if !self.cfg.enable_pre_call_checks
            && !matches!(
                self.cfg.strategy,
                crate::RoutingStrategy::UsageBased
                    | crate::RoutingStrategy::LatencyBased
                    | crate::RoutingStrategy::CostBased
            )
        {
            return false;
        }
        self.registry
            .load()
            .resolve(model, key_aliases)
            .is_some_and(|group| {
                group
                    .members
                    .iter()
                    .any(|d| d.meta().tpm.is_some() || d.pricing().max_input_tokens.is_some())
            })
    }

    /// Runs `call` against one deployment at a time with retries, order tiers, fallbacks and
    /// cooldowns. For streams the closure must return only once the upstream has committed (2xx
    /// and a first decoded frame); nothing is retried after `execute` returns.
    pub async fn execute<T, F, Fut>(
        &self,
        model: &str,
        ctx: &RequestCtx<'_>,
        mut call: F,
    ) -> Result<Outcome<T, D>, GatewayError>
    where
        F: FnMut(Selected<D>) -> Fut,
        Fut: Future<Output = Result<Attempt<T>, GatewayError>>,
    {
        let registry = self.registry.load();
        let root = registry
            .resolve(model, ctx.key_model_aliases)
            .ok_or_else(|| GatewayError::NotFound(format!("model {model:?} is not configured")))?;
        let mut stack: Vec<(Resolved<D>, u32, Option<i32>)> = vec![(root, 0, None)];
        let mut attempted: HashSet<String> = HashSet::new();
        let mut first_error: Option<GatewayError> = None;
        let mut retries_total = 0u32;
        let mut fallbacks_tried = 0u32;
        while let Some((group, depth, min_order)) = stack.pop() {
            let key = match min_order {
                Some(order) => format!("{}@{order}", group.group),
                None => group.group.to_string(),
            };
            if !attempted.insert(key) {
                continue;
            }
            if depth > 0 {
                fallbacks_tried += 1;
            }
            match self
                .with_retries(&group, ctx, &mut call, min_order, depth, &mut retries_total)
                .await
            {
                Ok(mut outcome) => {
                    outcome.attempted_retries = retries_total;
                    outcome.attempted_fallbacks = fallbacks_tried;
                    if depth > 0 {
                        metrics::counter!(rustllmroute_types::metrics::DEPLOYMENT_SUCCESSFUL_FALLBACKS, "model_group" => group.group.to_string()).increment(1);
                    }
                    return Ok(outcome);
                }
                Err(error) => {
                    tracing::debug!(group = %group.group, depth, error = %error, "group exhausted");
                    if depth > 0 {
                        metrics::counter!(rustllmroute_types::metrics::DEPLOYMENT_FAILED_FALLBACKS, "model_group" => group.group.to_string()).increment(1);
                    }
                    if ctx.disable_fallbacks
                        || depth >= self.cfg.max_fallbacks
                        || error.is_client_bug()
                    {
                        first_error.get_or_insert(error);
                        continue;
                    }
                    let targets =
                        fallback::targets(&self.cfg, &group, &error, min_order, ctx.body_fallbacks);
                    first_error.get_or_insert(error);
                    for target in targets.into_iter().rev() {
                        match target {
                            Target::Tier(order) => {
                                stack.push((group.clone(), depth + 1, Some(order)))
                            }
                            Target::Group(name) => {
                                if let Some(resolved) =
                                    registry.resolve(&name, ctx.key_model_aliases)
                                {
                                    stack.push((resolved, depth + 1, None));
                                }
                            }
                        }
                    }
                }
            }
        }
        Err(first_error.unwrap_or_else(|| GatewayError::NoDeployments {
            message: format!("no available deployment for {model:?}"),
            retry_after: None,
        }))
    }

    async fn with_retries<T, F, Fut>(
        &self,
        group: &Resolved<D>,
        ctx: &RequestCtx<'_>,
        call: &mut F,
        min_order: Option<i32>,
        depth: u32,
        retries_total: &mut u32,
    ) -> Result<Outcome<T, D>, GatewayError>
    where
        F: FnMut(Selected<D>) -> Fut,
        Fut: Future<Output = Result<Attempt<T>, GatewayError>>,
    {
        let mut skip: HashSet<Arc<str>> = HashSet::new();
        let mut attempt = 0u32;
        let mut max_retries = ctx.num_retries;
        loop {
            let selected = self
                .select(group, ctx, &skip, min_order, attempt, depth)
                .await?;
            let deployment = selected.deployment.clone();
            let stats = self.stats_for(deployment.id());
            stats.in_flight.fetch_add(1, Ordering::Relaxed);
            self.store.record_request(deployment.id(), current_minute());
            let mut guard = AttemptGuard {
                stats,
                store: self.store.clone(),
                id: deployment.id().clone(),
                started: Instant::now(),
                done: false,
            };
            match call(selected.clone()).await {
                Ok(result) => {
                    if let Some(usage) = result.usage {
                        guard.complete(Some(usage), result.ttft);
                    }
                    return Ok(Outcome {
                        value: result.value,
                        selected,
                        attempted_retries: attempt,
                        attempted_fallbacks: 0,
                        guard,
                    });
                }
                Err(error) => {
                    guard.fail(&error);
                    drop(guard);
                    let group_size = group.members.len();
                    let has_fallback = self.cfg.has_any_fallback(&group.group)
                        || ctx.body_fallbacks.is_some_and(|f| !f.is_empty());
                    self.maybe_cooldown(&deployment, &error, group_size, has_fallback);
                    let healthy_siblings =
                        self.healthy_siblings(group, &skip, deployment.id()).await;
                    let limit = *max_retries.get_or_insert_with(|| {
                        deployment
                            .meta()
                            .num_retries
                            .unwrap_or(self.cfg.num_retries)
                    });
                    let retry_ctx = RetryContext {
                        cfg: &self.cfg,
                        group: &group.group,
                        group_size,
                        healthy_siblings,
                    };
                    if attempt >= limit || !retry::should_retry(&retry_ctx, &error) {
                        return Err(error);
                    }
                    if !error.retryable_status() {
                        skip.insert(deployment.id().clone());
                    }
                    let wait = retry::backoff(&retry_ctx, &error, attempt);
                    tracing::debug!(group = %group.group, attempt, wait_ms = wait.as_millis() as u64, error = %error, "retrying");
                    if !wait.is_zero() {
                        tokio::time::sleep(wait).await;
                    }
                    attempt += 1;
                    *retries_total += 1;
                    metrics::counter!(rustllmroute_types::metrics::RETRIES, "model_group" => group.group.to_string()).increment(1);
                }
            }
        }
    }

    fn maybe_cooldown(
        &self,
        deployment: &Arc<D>,
        error: &GatewayError,
        group_size: usize,
        has_fallback: bool,
    ) {
        let window = self.stats_for(deployment.id()).minute(current_minute());
        if let Some(ttl) = cooldown::decide(
            &self.cfg,
            error,
            window,
            group_size,
            has_fallback,
            deployment.meta().cooldown_time,
        ) {
            self.store
                .set_cooldown(deployment.id(), cooldown::entry_for(deployment, error, ttl));
            metrics::counter!(rustllmroute_types::metrics::DEPLOYMENT_COOLED_DOWN, "model_id" => deployment.id().to_string(), "model_group" => deployment.model_group().to_string()).increment(1);
        }
    }

    async fn healthy_siblings(
        &self,
        group: &Resolved<D>,
        skip: &HashSet<Arc<str>>,
        failed: &Arc<str>,
    ) -> usize {
        let ids: Vec<&Arc<str>> = group.members.iter().map(|d| d.id()).collect();
        let snapshot = self.store.snapshot(&ids, current_minute()).await;
        group
            .members
            .iter()
            .enumerate()
            .filter(|(index, d)| {
                d.id() != failed
                    && !skip.contains(d.id())
                    && !d.meta().blocked
                    && snapshot.cooled[*index].is_none()
            })
            .count()
    }

    pub fn health_report(&self) -> HealthReport {
        let registry = self.registry.load();
        let cooldowns: HashMap<Arc<str>, _> = self.store.active_cooldowns().into_iter().collect();
        let mut healthy = Vec::new();
        let mut unhealthy = Vec::new();
        let mut unknown = Vec::new();
        for deployment in registry.deployments() {
            let stats = self.stats_for(deployment.id());
            let health = stats.health.read().clone();
            let cooldown = cooldowns.get(deployment.id());
            let endpoint = EndpointHealth {
                model_name: deployment.model_group().to_string(),
                model_id: deployment.id().to_string(),
                status: health.status,
                latency_ms: health.latency.map(|d| d.as_millis() as u64),
                checked_at_epoch: health
                    .checked_at
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs()),
                error: health.error.clone(),
                cooldown_remaining_s: cooldown.and_then(|c| c.remaining()).map(|d| d.as_secs()),
                cooldown_reason: cooldown.map(|c| c.reason.clone()),
                in_flight: stats.in_flight(),
            };
            match (health.status, cooldown.is_some()) {
                (HealthStatus::Unhealthy, _) | (_, true) => unhealthy.push(endpoint),
                (HealthStatus::Healthy, false) => healthy.push(endpoint),
                (HealthStatus::Unknown, false) => unknown.push(endpoint),
            }
        }
        let status = if unhealthy.is_empty() {
            "healthy"
        } else if healthy.is_empty() && unknown.is_empty() {
            "unhealthy"
        } else {
            "degraded"
        };
        HealthReport {
            status,
            healthy_count: healthy.len() + unknown.len(),
            unhealthy_count: unhealthy.len(),
            healthy_endpoints: healthy,
            unhealthy_endpoints: unhealthy,
            unknown_endpoints: unknown,
        }
    }
}
