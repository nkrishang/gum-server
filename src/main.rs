use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use gum_server::config::Config;
use gum_server::state::AppState;
use gum_server::{MIGRATOR, app, outbox, reconciler, telemetry};
use sqlx::postgres::PgPoolOptions;
use tokio::signal;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    telemetry::init_tracing();
    let config_dir = std::env::var("GUM_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("config"));
    let config = Config::load(&config_dir)?;
    for warning in config.production_warnings() {
        tracing::warn!(warning, "configuration");
    }
    let metrics = telemetry::init_metrics()?;

    let pool = PgPoolOptions::new()
        .max_connections(config.database.max_connections)
        .min_connections(config.database.min_connections)
        .acquire_timeout(Duration::from_millis(config.database.acquire_timeout_ms))
        .connect(&config.database.url)
        .await
        .context("connecting to postgres")?;
    if config.database.auto_migrate {
        MIGRATOR.run(&pool).await.context("running migrations")?;
        tracing::info!("migrations applied");
    }

    let ip: IpAddr = config.server.bind.parse().context("server.bind is not an IP address")?;
    let bind = SocketAddr::new(ip, config.server.port);
    let grace = Duration::from_secs(config.server.shutdown_grace_secs);
    let state = AppState::new(config, pool, metrics)?;
    tracing::info!(chains = ?state.registry.chain_ids(), factory = %state.factory, recovery = %state.recovery, "gum-server configured");

    let shutdown = CancellationToken::new();
    let workers = tokio::spawn(outbox::run(state.clone(), shutdown.clone()));
    let reconciler = tokio::spawn(reconciler::run(state.clone(), shutdown.clone()));

    let listener = tokio::net::TcpListener::bind(bind).await.with_context(|| format!("binding {bind}"))?;
    tracing::info!(%bind, "listening");
    let server_shutdown = shutdown.clone();
    axum::serve(listener, app::router(state.clone()).into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("shutdown signal received; draining");
            server_shutdown.cancel();
        })
        .await?;

    // HTTP has drained; give background work the rest of the grace period.
    let _ = tokio::time::timeout(grace, async {
        let _ = workers.await;
        let _ = reconciler.await;
    })
    .await;
    state.pool.close().await;
    tracing::info!("bye");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("ctrl-c handler");
    };
    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate()).expect("sigterm handler").recv().await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
