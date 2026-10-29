use std::time::{Duration, SystemTime};

use serde::Serialize;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    #[default]
    Unknown,
    Healthy,
    Unhealthy,
}

#[derive(Clone, Debug, Default)]
pub struct HealthState {
    pub status: HealthStatus,
    pub checked_at: Option<SystemTime>,
    pub latency: Option<Duration>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct EndpointHealth {
    pub model_name: String,
    pub model_id: String,
    pub status: HealthStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at_epoch: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_remaining_s: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_reason: Option<String>,
    pub in_flight: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct HealthReport {
    pub status: &'static str,
    pub healthy_count: usize,
    pub unhealthy_count: usize,
    pub healthy_endpoints: Vec<EndpointHealth>,
    pub unhealthy_endpoints: Vec<EndpointHealth>,
    pub unknown_endpoints: Vec<EndpointHealth>,
}
