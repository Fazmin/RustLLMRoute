use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use clap::Parser;
use rustllmroute_config::{Cli, LoadedConfig};
use rustllmroute_server::app;
use rustllmroute_server::state::{self, AppState, refresh_catalog_once};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MASTER: &str = "sk-master-test";
const BUNDLED: &str = include_str!("../../../model_prices_and_context_window.json");

async fn boot(upstream: &MockServer) -> (SocketAddr, Arc<AppState>) {
    let yaml = format!(
        "model_list:\n  - model_name: gpt\n    litellm_params: {{ model: openai/gpt-4o-mini, api_key: sk-a, api_base: {u} }}\n    model_info: {{ id: dep-gpt }}\ngeneral_settings:\n  master_key: {MASTER}\n",
        u = upstream.uri()
    );
    let loaded = LoadedConfig::parse(&yaml, Path::new("test.yaml")).unwrap();
    let state = state::build(loaded, &Cli::parse_from(["rustllmroute"]))
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = app::router(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (address, state)
}

#[tokio::test]
async fn readiness_reports_draining_after_the_shutdown_signal() {
    let upstream = MockServer::start().await;
    let (address, state) = boot(&upstream).await;
    let client = reqwest::Client::new();
    let before: Value = client
        .get(format!("http://{address}/health/readiness"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(before["status"], "healthy");
    state.draining.store(true, Ordering::Relaxed);
    let after = client
        .get(format!("http://{address}/health/readiness"))
        .send()
        .await
        .unwrap();
    assert_eq!(after.status(), 503);
    let body: Value = after.json().await.unwrap();
    assert_eq!(body["status"], "draining");
    let still_serving = client
        .get(format!("http://{address}/v1/models"))
        .bearer_auth(MASTER)
        .send()
        .await
        .unwrap();
    assert_eq!(
        still_serving.status(),
        200,
        "in-flight traffic is still served while draining"
    );
}

#[tokio::test]
async fn pricing_refresh_applies_new_rates_and_rejects_truncated_tables() {
    let upstream = MockServer::start().await;
    let (address, state) = boot(&upstream).await;
    let client = reqwest::Client::new();
    let price_of = |info: &Value| {
        info["data"][0]["model_info"]["input_cost_per_token"]
            .as_f64()
            .unwrap()
    };
    let original: Value = client
        .get(format!("http://{address}/model/info"))
        .bearer_auth(MASTER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(price_of(&original) < 0.01);

    let mut table: Value = serde_json::from_str(BUNDLED).unwrap();
    table["gpt-4o-mini"]["input_cost_per_token"] = json!(0.5);
    let pricing = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/prices.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(table))
        .mount(&pricing)
        .await;
    let count = refresh_catalog_once(&state, &format!("{}/prices.json", pricing.uri()))
        .await
        .unwrap();
    assert!(count > 1000, "{count}");
    let updated: Value = client
        .get(format!("http://{address}/model/info"))
        .bearer_auth(MASTER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(price_of(&updated), 0.5);

    let tiny = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"gpt-4o-mini": {"input_cost_per_token": 1.0, "output_cost_per_token": 1.0, "litellm_provider": "openai", "mode": "chat"}})))
        .mount(&tiny)
        .await;
    let error = refresh_catalog_once(&state, &format!("{}/prices.json", tiny.uri()))
        .await
        .unwrap_err();
    assert!(!error.is_empty());
    let unchanged: Value = client
        .get(format!("http://{address}/model/info"))
        .bearer_auth(MASTER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        price_of(&unchanged),
        0.5,
        "a rejected table leaves the previous one live"
    );
    let broken = refresh_catalog_once(&state, "http://127.0.0.1:9/prices.json").await;
    assert!(broken.is_err());
}
