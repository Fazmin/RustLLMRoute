use std::io::IsTerminal;

use clap::Parser;
use rustllmroute_config::{Cli, Command, LoadedConfig, LogFormat};
use rustllmroute_server::{app, auth, state};
use secrecy::ExposeSecret;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Some(command) = &cli.command {
        match command {
            Command::Healthcheck { port, host } => {
                std::process::exit(healthcheck(host, *port).await)
            }
            Command::GenerateMasterKey => {
                println!("{}", auth::generate_master_key());
                return;
            }
        }
    }
    let Some(path) = cli.config.clone() else {
        eprintln!("error: --config PATH is required (or set LITELLM_CONFIG)");
        std::process::exit(2);
    };
    let loaded = match LoadedConfig::load(&path) {
        Ok(loaded) => loaded,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(2);
        }
    };
    let telemetry = init_tracing(&cli, &loaded.config.litellm_settings.callbacks);
    if cli.check_config {
        println!(
            "config OK: {} deployment(s) from {}",
            loaded.config.model_list.len(),
            path.display()
        );
        return;
    }
    if cli.migrate_only {
        let Some(url) = state::database_url(&loaded, &cli) else {
            eprintln!(
                "error: --migrate-only needs a database URL (DATABASE_URL or general_settings.database_url)"
            );
            std::process::exit(2);
        };
        match state::open_database(url.expose_secret(), &loaded).await {
            Ok(db) => {
                db.close().await;
                println!("migrations applied");
                return;
            }
            Err(error) => {
                eprintln!("error: {error}");
                std::process::exit(1);
            }
        }
    }
    let state = match state::build(loaded, &cli).await {
        Ok(state) => state,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    };
    let app = app::router(state.clone());
    let address = format!("{}:{}", cli.host, cli.port);
    let listener = match tokio::net::TcpListener::bind(&address).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("error: cannot bind {address}: {error}");
            std::process::exit(1);
        }
    };
    tracing::info!(
        address = %address,
        deployments = state.gateway.router.registry().len(),
        database = state.control.is_some(),
        version = env!("CARGO_PKG_VERSION"),
        "rustllmroute listening"
    );
    if let Ok(url) = std::env::var("LITELLM_MODEL_COST_MAP_URL")
        && !url.is_empty()
    {
        let interval = std::env::var("LITELLM_MODEL_COST_MAP_REFRESH_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(std::time::Duration::from_secs)
            .unwrap_or(std::time::Duration::from_secs(24 * 3600));
        state::spawn_catalog_refresh(state.clone(), url, interval);
    }
    let draining = state.clone();
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            draining
                .draining
                .store(true, std::sync::atomic::Ordering::Relaxed);
        })
        .await;
    if let Some(control) = &state.control {
        control.shutdown().await;
    }
    if let Some(telemetry) = &telemetry {
        telemetry.shutdown();
    }
    if let Err(error) = served {
        eprintln!("error: server failed: {error}");
        std::process::exit(1);
    }
    tracing::info!("shutdown complete");
}

fn init_tracing(cli: &Cli, callbacks: &[String]) -> Option<rustllmroute_observe::Telemetry> {
    let json = match cli.log_format {
        LogFormat::Json => true,
        LogFormat::Pretty => false,
        LogFormat::Auto => !std::io::stdout().is_terminal(),
    };
    let otel = rustllmroute_observe::otel_requested(callbacks).then_some("rustllmroute");
    rustllmroute_observe::init_logging(cli.effective_log_level(), json, otel)
}

async fn healthcheck(host: &str, port: u16) -> i32 {
    let url = format!("http://{host}:{port}/health/liveliness");
    match reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(4))
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => 0,
        Ok(response) => {
            eprintln!("unhealthy: {} returned {}", url, response.status());
            1
        }
        Err(error) => {
            eprintln!("unhealthy: {error}");
            1
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    tracing::info!("shutdown signal received; draining in-flight requests");
}
