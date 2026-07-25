//! Router construction and shared state.

use std::sync::Arc;

use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::config::Config;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub http: reqwest::Client,
}

impl AppState {
    /// Fallible because `ClientBuilder::build` can fail on TLS backend
    /// initialization; startup reports that rather than panicking.
    pub fn new(config: Config) -> Result<Self, reqwest::Error> {
        Ok(Self {
            config: Arc::new(config),
            // The relay owns its own timeouts per request, so the client carries none.
            http: reqwest::Client::builder().build()?,
        })
    }
}

pub fn router(state: AppState) -> Router {
    // Call-create and sideband routes are registered in later phases, behind the
    // trust boundary from docs/015.
    Router::new()
        .route("/healthz", get(healthz))
        .with_state(state)
}

async fn healthz() -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "service": "gpt-live-proxy",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_state() -> AppState {
        let config = Config::from_source(|k| match k {
            "GPT_LIVE_TOKEN" => Some("test-token".to_string()),
            _ => None,
        })
        .expect("test config");
        AppState::new(config).expect("test client")
    }

    #[tokio::test]
    async fn healthz_reports_service_identity() {
        let app = router(test_state());
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["service"], "gpt-live-proxy");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    }
}
