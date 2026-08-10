// SPDX-License-Identifier: AGPL-3.0-only
#![forbid(unsafe_code)]
//! Relay entry point. Loads `yapstack-relay.toml`, connects Postgres, runs
//! migrations, and serves. Zero outbound calls.

use std::net::SocketAddr;
use std::process::ExitCode;

use tracing_subscriber::EnvFilter;
use yapstack_server::{build_router, config::Config, db, state::AppState};

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config_path = std::env::var("YAPSTACK_RELAY_CONFIG")
        .unwrap_or_else(|_| "yapstack-relay.toml".to_string());

    let config = match Config::from_path(&config_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(path = %config_path, error = %e, "failed to load config");
            return ExitCode::FAILURE;
        }
    };
    if !config.limits_enabled() {
        tracing::info!("[limits] absent — entitlements source = AllowAll (self-host, unlimited)");
    }

    let bind_addr = config.bind_addr.clone();

    let pool = match db::connect(&config.database_url).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "failed to connect to Postgres");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = db::migrate(&pool).await {
        tracing::error!(error = %e, "migrations failed");
        return ExitCode::FAILURE;
    }

    // Migrate-as-owner one-shot: the compose `migrate` service connects as the DB owner,
    // applies migrations, and exits, so the long-running server can serve as the
    // non-owner `yapstack_app` role (which never runs DDL — `db::migrate` then skips).
    if std::env::var("YAPSTACK_MIGRATE_ONLY").is_ok_and(|v| matches!(v.as_str(), "1" | "true")) {
        tracing::info!("migrations applied; exiting (YAPSTACK_MIGRATE_ONLY)");
        return ExitCode::SUCCESS;
    }

    let state = AppState::new(pool, config);
    let app = build_router(state);

    let listener = match tokio::net::TcpListener::bind(&bind_addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(addr = %bind_addr, error = %e, "failed to bind");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(addr = %bind_addr, "yapstack-server listening");

    // `into_make_service_with_connect_info` surfaces the wire peer as
    // `ConnectInfo<SocketAddr>`, the ground truth for `ClientIp` rate-limit keys.
    if let Err(e) = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    {
        tracing::error!(error = %e, "server error");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
