//! Deployment health probes: one tiny request per deployment, results stored in the router so
//! `/health` and health-aware routing can read them.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use futures_util::StreamExt;
use rustllmroute_catalog::Mode;
use rustllmroute_router::{HealthReport, HealthState, HealthStatus};
use rustllmroute_types::{
    ChatMessage, ChatRequest, EmbeddingInput, EmbeddingRequest, GatewayError, role,
};

use crate::chat::execute_chat;
use crate::deployment::Deployment;
use crate::embeddings::execute_embeddings;
use crate::gateway::Gateway;
use crate::http::HttpClients;

const PROBE_CONCURRENCY: usize = 4;
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const FIRST_RUN_DELAY: Duration = Duration::from_secs(10);

/// Sends the cheapest request the deployment accepts and returns its latency.
pub async fn probe_deployment(
    http: &HttpClients,
    deployment: &Deployment,
) -> Result<Duration, GatewayError> {
    if deployment.provider.model.contains('*') {
        return Err(GatewayError::internal(
            "wildcard deployments are not probed",
        ));
    }
    let started = Instant::now();
    let request_id = format!("health-{}", uuid::Uuid::now_v7().simple());
    let model = deployment.provider.model.clone();
    match deployment.mode {
        Mode::Embedding => {
            let request = EmbeddingRequest {
                model: model.clone(),
                input: EmbeddingInput::Texts(vec!["test".to_owned()]),
                dimensions: None,
                encoding_format: None,
                user: None,
                extra: Default::default(),
            };
            execute_embeddings(http, deployment, &request, &model, &request_id).await?;
        }
        _ => {
            let request = ChatRequest {
                model: model.clone(),
                messages: vec![ChatMessage::text(role::USER, "test")],
                max_tokens: Some(1),
                ..ChatRequest::default()
            };
            execute_chat(http, deployment, &request, &model, &request_id).await?;
        }
    }
    Ok(started.elapsed())
}

async fn probe_and_record(gateway: &Gateway, deployment: Arc<Deployment>, timeout: Duration) {
    let result = tokio::time::timeout(timeout, probe_deployment(&gateway.http, &deployment)).await;
    let state = match result {
        Ok(Ok(latency)) => HealthState {
            status: HealthStatus::Healthy,
            checked_at: Some(SystemTime::now()),
            latency: Some(latency),
            error: None,
        },
        Ok(Err(error)) if deployment.provider.model.contains('*') => HealthState {
            status: HealthStatus::Unknown,
            checked_at: Some(SystemTime::now()),
            latency: None,
            error: Some(error.to_string()),
        },
        Ok(Err(error)) => HealthState {
            status: HealthStatus::Unhealthy,
            checked_at: Some(SystemTime::now()),
            latency: None,
            error: Some(error.to_string()),
        },
        Err(_) => HealthState {
            status: HealthStatus::Unhealthy,
            checked_at: Some(SystemTime::now()),
            latency: None,
            error: Some(format!(
                "health probe timed out after {}s",
                timeout.as_secs()
            )),
        },
    };
    let status = state.status;
    let error = state.error.clone();
    let previous = gateway.router.set_health(&deployment.id, state);
    if previous != status {
        tracing::info!(
            deployment = %deployment.id,
            model = %deployment.model_group,
            from = ?previous,
            to = ?status,
            error = error.as_deref().unwrap_or(""),
            "deployment health changed"
        );
    }
}

/// Probes every deployment (bounded concurrency, per-probe timeout) and returns the report.
pub async fn run_health_checks(gateway: &Gateway, timeout: Duration) -> HealthReport {
    let deployments: Vec<Arc<Deployment>> =
        gateway.router.registry().deployments().cloned().collect();
    futures_util::stream::iter(deployments)
        .for_each_concurrent(PROBE_CONCURRENCY, |deployment| {
            probe_and_record(gateway, deployment, timeout)
        })
        .await;
    gateway.router.health_report()
}

/// Background loop: first run after a short delay, then every `interval`.
pub fn spawn_health_loop(gateway: Arc<Gateway>, interval: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_RUN_DELAY).await;
        loop {
            let report = run_health_checks(&gateway, PROBE_TIMEOUT).await;
            tracing::debug!(
                healthy = report.healthy_count,
                unhealthy = report.unhealthy_count,
                "background health check complete"
            );
            tokio::time::sleep(interval).await;
        }
    })
}
