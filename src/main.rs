//! Binary entry point: read config from the environment, serve, shut down gracefully.

use std::process::ExitCode;

use gpt_live_proxy::app::{router, AppState};
use gpt_live_proxy::config::Config;

#[tokio::main]
async fn main() -> ExitCode {
    gpt_live_proxy::observability::init_tracing();

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

    // The drain flag is set the moment a signal arrives, so requests that land
    // during the graceful-shutdown window get 503 rather than a dropped socket.
    let drain = state.drain.clone();

    let mut frame_log = state.frame_log.clone();

    let serve_result = axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            drain.begin();
        })
        .await;

    // Flush forensics on BOTH paths: an error exit is exactly when the tail of
    // the log is most worth having.
    if !frame_log.drain() {
        tracing::warn!("frame log did not flush within the shutdown budget");
    }

    if let Err(err) = serve_result {
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
