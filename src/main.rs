//! Binary entry point: read config from the environment, serve, shut down gracefully.

use std::process::ExitCode;

use gpt_live_proxy::app::{router, AppState};
use gpt_live_proxy::config::Config;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("GPT_LIVE_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = match Config::from_env() {
        Ok(config) => config,
        Err(err) => {
            // `err` never contains a credential: ConfigError carries key names and
            // parse reasons only.
            tracing::error!("configuration error: {err}");
            return ExitCode::FAILURE;
        }
    };

    let bind = config.bind;
    let requires_auth = config.requires_admission_auth();
    let state = match AppState::new(config) {
        Ok(state) => state,
        Err(err) => {
            tracing::error!("failed to build the upstream HTTP client: {err}");
            return ExitCode::FAILURE;
        }
    };

    let listener = match tokio::net::TcpListener::bind(bind).await {
        Ok(listener) => listener,
        Err(err) => {
            tracing::error!("failed to bind {bind}: {err}");
            return ExitCode::FAILURE;
        }
    };

    tracing::info!(%bind, admission_auth = requires_auth, "gpt-live-proxy listening");

    if let Err(err) = axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        tracing::error!("server error: {err}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(err) => {
                tracing::warn!("cannot listen for SIGTERM: {err}");
                // Stay pending forever. Completing here would make the select below
                // fire immediately and shut the server down at startup.
                std::future::pending::<()>().await
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }

    tracing::info!("shutdown signal received; draining");
}
