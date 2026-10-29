use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use rustllmroute_catalog::Catalog;
use rustllmroute_config::{Cli, ConfigHandle, LoadedConfig, ProxyConfig};
use rustllmroute_control::{ControlPlane, ControlSettings};
use rustllmroute_core::guard::JudgeExecutor;
use rustllmroute_core::guardrails::GuardrailSet;
use rustllmroute_core::{Deployment, Gateway, GatewaySettings, Hooks, HttpClients};
use rustllmroute_db::{Db, DbOptions};
use rustllmroute_router::{Registry, Router, RouterConfig};
use rustllmroute_types::GatewayError;
use secrecy::{ExposeSecret, SecretString};

use crate::auth::MasterKeyHooks;

pub struct AppState {
    pub gateway: Arc<Gateway>,
    pub config: ConfigHandle,
    pub control: Option<Arc<ControlPlane>>,
    pub started: Instant,
    pub started_epoch: u64,
    pub max_body_bytes: usize,
    pub metrics_auth: bool,
    pub master_key: SecretString,
    pub ui_secure_cookie: bool,
    pub login_throttle: std::sync::Mutex<crate::session::LoginThrottle>,
    /// Set when a shutdown signal arrives; readiness answers 503 so load balancers stop sending.
    pub draining: AtomicBool,
    /// The live pricing table: bundled at startup, optionally refreshed from `LITELLM_MODEL_COST_MAP_URL`.
    pub catalog: arc_swap::ArcSwap<Catalog>,
}

/// Fetches a pricing table, accepts it only when it parses and is not suspiciously small, and
/// re-resolves every deployment against it. Returns the number of models in the new table.
pub async fn refresh_catalog_once(state: &AppState, url: &str) -> Result<usize, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| error.to_string())?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("{url} returned {}", response.status()));
    }
    let body = response.bytes().await.map_err(|error| error.to_string())?;
    let catalog = Catalog::parse(&body).map_err(|error| error.to_string())?;
    let bundled = rustllmroute_catalog::bundled()
        .map(|c| c.model_count())
        .unwrap_or(0);
    catalog
        .require_at_least(bundled * 9 / 10)
        .map_err(|error| error.to_string())?;
    let count = catalog.model_count();
    let snapshot = state.config.snapshot();
    let deployments =
        build_deployments(&snapshot.config, &catalog).map_err(|error| error.to_string())?;
    let aliases = snapshot
        .config
        .router_settings
        .model_group_alias
        .iter()
        .map(|(alias, target)| (alias.clone(), target.clone()))
        .collect();
    state.catalog.store(Arc::new(catalog));
    state
        .gateway
        .router
        .replace_registry(Registry::new(deployments, aliases));
    Ok(count)
}

/// Refreshes the pricing table now and then every `interval`; failures keep the last good table.
pub fn spawn_catalog_refresh(state: Arc<AppState>, url: String, interval: Duration) {
    tokio::spawn(async move {
        loop {
            match refresh_catalog_once(&state, &url).await {
                Ok(models) => tracing::info!(models, url = %url, "pricing table refreshed"),
                Err(error) => {
                    metrics::counter!(rustllmroute_types::metrics::PRICING_REFRESH_FAILURES)
                        .increment(1);
                    tracing::warn!(error = %error, url = %url, "pricing table refresh failed; keeping the current table");
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
}

/// Builds every deployment of a config; shared by startup and `/config/reload`.
pub fn build_deployments(
    config: &ProxyConfig,
    catalog: &Catalog,
) -> Result<Vec<Arc<Deployment>>, GatewayError> {
    let mut deployments = Vec::with_capacity(config.model_list.len());
    for entry in &config.model_list {
        let deployment = Deployment::from_entry(
            entry,
            &config.litellm_settings,
            &config.router_settings,
            catalog,
        )?;
        if !deployment.pricing.has_rates() && !deployment.model_group.contains('*') {
            tracing::warn!(
                model = %deployment.model_group,
                provider_model = %deployment.provider.model,
                "no pricing found; spend for this deployment will be recorded as 0"
            );
        }
        deployments.push(Arc::new(deployment));
    }
    Ok(deployments)
}

fn sqlite_path(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))?;
    let rest = rest.split('?').next().unwrap_or(rest);
    (!rest.is_empty() && rest != ":memory:").then_some(rest)
}

pub async fn open_database(url: &str, loaded: &LoadedConfig) -> Result<Db, GatewayError> {
    if !url.starts_with("sqlite") {
        return Err(GatewayError::internal(
            "only sqlite: database URLs are supported by this build; PostgreSQL support arrives in a later release",
        ));
    }
    if let Some(path) = sqlite_path(url)
        && let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            GatewayError::internal(format!(
                "cannot create database directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    let settings = &loaded.config.general_settings.database;
    let db = Db::connect(
        url,
        DbOptions {
            reader_connections: settings.reader_connections,
            synchronous_full: settings.sqlite_synchronous.eq_ignore_ascii_case("full"),
            query_timeout: Duration::from_secs(settings.query_timeout_s.max(1)),
            busy_timeout: Duration::from_secs(10),
        },
    )
    .await
    .map_err(|error| GatewayError::internal(format!("cannot open database {url}: {error}")))?;
    db.migrate()
        .await
        .map_err(|error| GatewayError::internal(format!("database migration failed: {error}")))?;
    Ok(db)
}

pub fn database_url(loaded: &LoadedConfig, cli: &Cli) -> Option<SecretString> {
    cli.database_url
        .clone()
        .filter(|url| !url.is_empty())
        .map(SecretString::from)
        .or_else(|| loaded.database_url.clone())
}

pub async fn build(loaded: LoadedConfig, cli: &Cli) -> Result<Arc<AppState>, GatewayError> {
    let master_key = loaded
        .master_key
        .clone()
        .ok_or_else(|| GatewayError::internal("a master key is required"))?;
    let catalog = rustllmroute_catalog::bundled().map_err(|error| {
        GatewayError::internal(format!("bundled pricing table is unreadable: {error}"))
    })?;
    tracing::info!(
        models = catalog.model_count(),
        issues = catalog.issues().len(),
        "pricing catalog loaded"
    );
    let config = &loaded.config;
    let deployments = build_deployments(config, &catalog)?;
    let aliases: HashMap<String, String> = config
        .router_settings
        .model_group_alias
        .iter()
        .map(|(alias, target)| (alias.clone(), target.clone()))
        .collect();
    let router = Arc::new(Router::new(
        Registry::new(deployments, aliases),
        RouterConfig::from_settings(&config.router_settings, &config.litellm_settings),
    ));
    let http = HttpClients::new()?;
    let judge = Arc::new(JudgeExecutor {
        router: router.clone(),
        http: http.clone(),
    });
    let guardrails = GuardrailSet::from_specs(&config.guardrails, Some(judge))
        .map_err(|error| GatewayError::internal(format!("guardrails: {error}")))?;
    if !guardrails.is_empty() {
        tracing::info!(guardrails = ?guardrails.names(), "guardrails configured");
    }
    let general = &config.general_settings;
    let (hooks, control): (Arc<dyn Hooks>, Option<Arc<ControlPlane>>) = match database_url(
        &loaded, cli,
    ) {
        Some(url) => {
            let db = open_database(url.expose_secret(), &loaded).await?;
            let retention = general
                .maximum_spend_logs_retention_period
                .as_deref()
                .and_then(rustllmroute_control::budget::to_duration)
                .map(|keep| {
                    let interval = general
                        .maximum_spend_logs_retention_interval
                        .as_deref()
                        .and_then(rustllmroute_control::budget::to_duration)
                        .unwrap_or(Duration::from_secs(86_400));
                    (keep, interval)
                });
            let settings = ControlSettings {
                spend: rustllmroute_control::spend::SpendSettings {
                    flush_interval: Duration::from_secs(general.spend.flush_interval_s.max(1)),
                    queue_capacity: general.spend.queue_capacity,
                    batch_rows: general.spend.batch_rows.max(1),
                    disable_spend_logs: general.disable_spend_logs,
                    ..Default::default()
                },
                retention,
                ..ControlSettings::default()
            };
            let plane = ControlPlane::start(db, master_key.clone(), settings);
            tracing::info!("database connected; virtual keys and spend tracking enabled");
            (plane.clone(), Some(plane))
        }
        None => {
            tracing::warn!(
                "no DATABASE_URL: running with the master key only; virtual keys and spend logs are disabled"
            );
            (Arc::new(MasterKeyHooks::new(master_key.clone())), None)
        }
    };
    let settings = GatewaySettings {
        keepalive: Duration::from_secs(general.keepalive_interval.max(1)),
        ..GatewaySettings::default()
    };
    let max_body_bytes =
        usize::try_from(general.max_body_mb.max(1) * 1024 * 1024).unwrap_or(32 * 1024 * 1024);
    let gateway = Arc::new(Gateway::new(router, http, hooks, settings, guardrails));
    rustllmroute_observe::record_build_info(env!("CARGO_PKG_VERSION"));
    let metrics_auth = general.metrics_auth;
    let ui_secure_cookie = general.ui_secure_cookie.unwrap_or(false);
    tokio::task::spawn_blocking(rustllmroute_tokens::warm);
    if general.background_health_checks {
        let interval = Duration::from_secs(general.health_check_interval.max(30));
        rustllmroute_core::health::spawn_health_loop(gateway.clone(), interval);
        tracing::info!(
            interval_s = interval.as_secs(),
            "background health checks enabled"
        );
    }
    Ok(Arc::new(AppState {
        gateway,
        config: ConfigHandle::new(loaded),
        control,
        started: Instant::now(),
        started_epoch: rustllmroute_core::now_epoch_secs(),
        max_body_bytes,
        metrics_auth,
        master_key,
        ui_secure_cookie,
        login_throttle: std::sync::Mutex::new(crate::session::LoginThrottle::default()),
        draining: AtomicBool::new(false),
        catalog: arc_swap::ArcSwap::from_pointee(catalog),
    }))
}
