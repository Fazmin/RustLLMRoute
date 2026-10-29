//! Prometheus recorder, `/metrics` rendering, log subscriber and optional OpenTelemetry traces.

use std::sync::OnceLock;
use std::time::Duration;

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use metrics_util::MetricKindMask;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// LiteLLM's latency buckets, so existing Grafana panels keep their shape.
pub const LATENCY_BUCKETS: &[f64] = &[
    0.005, 0.00625, 0.0125, 0.025, 0.05, 0.1, 0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 3.5, 4.0, 4.5, 5.0,
    5.5, 6.0, 6.5, 7.0, 7.5, 8.0, 8.5, 9.0, 9.5, 10.0, 15.0, 20.0, 25.0, 30.0, 60.0, 120.0, 180.0,
    240.0, 300.0, 600.0,
];
const SECONDS_BUCKETS: &[f64] = &[
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];
const IDLE_TIMEOUT: Duration = Duration::from_secs(3600);

/// The process-wide Prometheus recorder; installed on first use, shared afterwards.
pub fn prometheus() -> &'static PrometheusHandle {
    static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
    HANDLE.get_or_init(|| {
        let builder = PrometheusBuilder::new()
            .set_buckets_for_metric(
                Matcher::Suffix("_latency_metric".to_owned()),
                LATENCY_BUCKETS,
            )
            .and_then(|b| {
                b.set_buckets_for_metric(
                    Matcher::Suffix("_time_to_first_token_metric".to_owned()),
                    LATENCY_BUCKETS,
                )
            })
            .and_then(|b| {
                b.set_buckets_for_metric(Matcher::Suffix("_seconds".to_owned()), SECONDS_BUCKETS)
            })
            .unwrap_or_else(|_| PrometheusBuilder::new())
            .idle_timeout(MetricKindMask::ALL, Some(IDLE_TIMEOUT));
        match builder.install_recorder() {
            Ok(handle) => handle,
            // Another recorder owns the global slot (tests, embedding); keep a detached one so
            // rendering still works.
            Err(_) => PrometheusBuilder::new().build_recorder().handle(),
        }
    })
}

/// Text exposition for `/metrics`.
pub fn render() -> String {
    prometheus().render()
}

pub fn record_build_info(version: &str) {
    let _ = prometheus();
    metrics::gauge!(
        rustllmroute_types::metrics::BUILD_INFO,
        "version" => version.to_owned()
    )
    .set(1.0);
}

/// Keeps the OpenTelemetry provider alive and flushes it on shutdown.
pub struct Telemetry {
    provider: SdkTracerProvider,
}

impl Telemetry {
    pub fn shutdown(&self) {
        if let Err(error) = self.provider.shutdown() {
            tracing::warn!(error = %error, "opentelemetry shutdown failed");
        }
    }
}

/// Traces are exported when an OTLP endpoint is configured or `otel` is a configured callback.
pub fn otel_requested(callbacks: &[String]) -> bool {
    std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok_and(|v| !v.is_empty())
        || std::env::var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").is_ok_and(|v| !v.is_empty())
        || callbacks
            .iter()
            .any(|c| c == "otel" || c == "opentelemetry")
}

fn build_tracer(
    service_name: &str,
) -> Result<(SdkTracerProvider, opentelemetry_sdk::trace::Tracer), String> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .build()
        .map_err(|error| error.to_string())?;
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            Resource::builder()
                .with_service_name(service_name.to_owned())
                .build(),
        )
        .build();
    let tracer = provider.tracer("rustllmroute");
    opentelemetry::global::set_tracer_provider(provider.clone());
    Ok((provider, tracer))
}

/// Installs the global subscriber: env-filtered, JSON or pretty, plus an OTel layer on request.
pub fn init_logging(level: &str, json: bool, otel_service: Option<&str>) -> Option<Telemetry> {
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));
    let json_layer = json.then(|| {
        tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(false)
    });
    let pretty_layer = (!json).then(tracing_subscriber::fmt::layer);
    let otel = otel_service.and_then(|name| match build_tracer(name) {
        Ok(built) => Some(built),
        Err(error) => {
            eprintln!("warning: opentelemetry disabled: {error}");
            None
        }
    });
    let (provider, tracer) = match otel {
        Some((provider, tracer)) => (Some(provider), Some(tracer)),
        None => (None, None),
    };
    let otel_layer = tracer.map(|tracer| tracing_opentelemetry::layer().with_tracer(tracer));
    tracing_subscriber::registry()
        .with(filter)
        .with(json_layer)
        .with(pretty_layer)
        .with(otel_layer)
        .init();
    provider.map(|provider| Telemetry { provider })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_registered_metrics() {
        let _ = prometheus();
        record_build_info("test");
        metrics::counter!("rustllmroute_test_counter", "k" => "v").increment(1);
        let text = render();
        assert!(text.contains("rustllmroute_build_info"), "{text}");
        assert!(
            text.contains("rustllmroute_test_counter{k=\"v\"} 1"),
            "{text}"
        );
        assert!(
            !otel_requested(&["prometheus".to_owned()])
                || std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok()
        );
    }
}
