//! Router construction and shared state.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Json, Router};
use http::{HeaderMap, Method, StatusCode, Uri};
use serde_json::json;

use crate::admission::{cors, guard, DrainState};
use crate::config::Config;
use crate::error::{RelayError, RequestKind};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub http: reqwest::Client,
    pub drain: DrainState,
    /// Service-owned so shutdown can flush it and so tests can inject one.
    pub frame_log: crate::observability::FrameLogger,
}

impl AppState {
    /// Fallible because `ClientBuilder::build` can fail on TLS backend
    /// initialization; startup reports that rather than panicking.
    pub fn new(config: Config) -> Result<Self, reqwest::Error> {
        Ok(Self {
            config: Arc::new(config),
            // The relay owns its own timeouts per request, so the client carries none.
            // Redirects are responses to relay, never navigation instructions for
            // the proxy: following one could replay a credential and body to an
            // attacker-controlled location.
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            drain: DrainState::new(),
            frame_log: crate::observability::FrameLogger::from_env_owned(),
        })
    }
}

pub fn router(state: AppState) -> Router {
    // `/healthz` is the ONLY route outside the boundary: a supervisor must be able
    // to probe liveness without a data-plane credential, and the reply carries no
    // configuration or account information.
    Router::new()
        .route("/healthz", get(healthz))
        .merge(protected_routes(state.clone()))
        .fallback(any(protected_fallback))
        .with_state(state)
}

/// The data plane.
///
/// Protection is applied here as a **middleware layer over the whole subrouter**,
/// not by convention inside each handler. A route registered in this function is
/// guarded whether or not its author remembers to do anything, which is the
/// property that makes this safe to extend in later phases.
fn protected_routes(state: AppState) -> Router<AppState> {
    // Registered inside the protected subrouter, so they inherit the trust
    // boundary from the layer below rather than opting into it.
    // `any` rather than `post`: the contract answers a non-POST on these paths
    // with the unknown-endpoint 404, not axum's default 405.
    let routes = Router::<AppState>::new()
        .route("/v1/live", any(call_create_dispatch))
        .route("/v1/realtime/calls", any(call_create_dispatch))
        // Sideband joins. Registered here so they inherit the boundary, which
        // also gives them the upgrade-specific 403 wording automatically.
        // Both slash forms are registered: axum treats them as distinct routes,
        // so the parser's trailing-slash tolerance would otherwise be dead code.
        // `any` rather than `get`, so a non-upgrade method reaches the handler
        // and receives the contract's 404 instead of axum's 405.
        .route(
            "/v1/live/{call_id}",
            any(crate::live::sideband::handle_sideband),
        )
        .route(
            "/v1/live/{call_id}/",
            any(crate::live::sideband::handle_sideband),
        )
        .route(
            "/v1/realtime/calls/{call_id}",
            any(crate::live::sideband::handle_sideband),
        )
        .route(
            "/v1/realtime/calls/{call_id}/",
            any(crate::live::sideband::handle_sideband),
        )
        .route("/v1/realtime", any(crate::live::sideband::handle_sideband))
        .route("/v1/realtime/", any(crate::live::sideband::handle_sideband));

    // The probe exists ONLY under `cfg(test)`: it gives the layer something to
    // wrap before the relay routes exist, without shipping a real endpoint.
    #[cfg(test)]
    let routes = routes.route(BOUNDARY_PROBE_PATH, get(boundary_probe));

    routes.layer(axum::middleware::from_fn_with_state(
        state,
        boundary_middleware,
    ))
}

/// The trust boundary as a `tower` layer: draining, then preflight, then the
/// admission/origin guard, then the inner handler, with CORS on every outcome.
async fn boundary_middleware(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let method = request.method().clone();
    let headers = request.headers().clone();
    let kind = if is_websocket_upgrade(&headers) {
        RequestKind::WebSocketUpgrade
    } else {
        RequestKind::Http
    };

    protect(&state, &method, &headers, kind, || async move {
        next.run(request).await
    })
    .await
}

/// A WebSocket upgrade is judged by its `Upgrade` header, so the sideband routes
/// added in phase 030 automatically get the upgrade-specific 403 message.
fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// Exists only to prove the layer wraps whatever is registered beside it.
#[cfg(test)]
const BOUNDARY_PROBE_PATH: &str = "/v1/__boundary_probe";

/// Dispatch by method so a non-POST yields the contract's 404 rather than a 405.
async fn call_create_dispatch(
    state: State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Response {
    let uri_path = uri.path().to_string();
    if method != Method::POST {
        return RelayError::UnknownEndpoint {
            method: method.to_string(),
            path: uri.path().to_string(),
        }
        .into_response();
    }
    let path = crate::live::call_create::RequestPath::from(uri_path);
    crate::live::handle_call_create(state, method, path, headers, body).await
}

#[cfg(test)]
async fn boundary_probe() -> Response {
    (StatusCode::OK, "protected").into_response()
}

/// Wrap a response in CORS headers. Every HTTP response goes through this,
/// including errors, matching the source behavior.
fn corsed(mut response: Response, headers: &HeaderMap, config: &Config) -> Response {
    cors::apply_cors(&mut response, headers, config);
    response
}

/// Run the trust boundary for a data-plane request, then the handler.
///
/// Every protected route calls this; it is the single place that decides the
/// order, so a later route cannot reorder or skip a check by accident.
pub async fn protect<F, Fut>(
    state: &AppState,
    method: &Method,
    headers: &HeaderMap,
    kind: RequestKind,
    handler: F,
) -> Response
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Response>,
{
    // Draining precedes everything, including the unauthenticated preflight:
    // a shutting-down proxy answers 503 rather than advertising CORS policy.
    if state.drain.is_draining() {
        return corsed(RelayError::Draining.into_response(), headers, &state.config);
    }

    // Preflight is never authenticated: a browser cannot attach the admission
    // credential to an OPTIONS, so requiring it would break every legitimate
    // cross-origin caller.
    if method == Method::OPTIONS {
        let status = cors::preflight_status(headers, &state.config);
        return corsed(status.into_response(), headers, &state.config);
    }

    if let Err(err) = guard(headers, &state.config, &state.drain, kind) {
        return corsed(err.into_response(), headers, &state.config);
    }

    corsed(handler().await, headers, &state.config)
}

async fn protected_fallback(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let path = uri.path().to_string();
    let reported = method.to_string();
    protect(
        &state,
        &method,
        &headers,
        RequestKind::Http,
        || async move {
            RelayError::UnknownEndpoint {
                method: reported,
                path,
            }
            .into_response()
        },
    )
    .await
}

/// Liveness. Deliberately outside the trust boundary: a supervisor must be able
/// to probe the process without holding a data-plane credential, and the reply
/// contains no configuration or account information.
async fn healthz() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "service": "gpt-live-proxy",
            "version": env!("CARGO_PKG_VERSION"),
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use crate::config::BearerToken;

    fn test_state() -> AppState {
        let config = Config::from_source(|k| match k {
            "GPT_LIVE_TOKEN" => Some("test-token".to_string()),
            _ => None,
        })
        .expect("test config");
        AppState::new(config).expect("test client")
    }

    fn remote_state(admission: &str) -> AppState {
        let mut config = Config::from_source(|k| match k {
            "GPT_LIVE_TOKEN" => Some("test-token".to_string()),
            "GPT_LIVE_BIND" => Some("0.0.0.0:10110".to_string()),
            _ => None,
        })
        .expect("test config");
        config.admission_token = Some(BearerToken::new(admission));
        AppState::new(config).expect("test client")
    }

    #[test]
    fn app_state_uses_grouped_limits() {
        let mut grouped = Config::from_source(|k| match k {
            "GPT_LIVE_TOKEN" => Some("test-token".to_string()),
            _ => None,
        })
        .unwrap();
        grouped.limits.request_bytes = 321;
        let grouped = AppState::new(grouped).unwrap();
        assert_eq!(grouped.config.limits.request_bytes, 321);
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

    #[tokio::test]
    async fn an_unknown_endpoint_reports_its_method_and_path() {
        let res = router(test_state())
            .oneshot(
                Request::builder()
                    .uri("/v1/nope")
                    .header("host", "127.0.0.1:10110")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["message"], "Unknown endpoint: GET /v1/nope");
    }

    #[tokio::test]
    async fn the_boundary_answers_before_the_route_table() {
        // An unauthorized prober gets 401, not a 404 that would confirm the path
        // does not exist.
        let res = router(remote_state("secret"))
            .oneshot(
                Request::builder()
                    .uri("/v1/nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn preflight_is_answered_without_authentication() {
        let res = router(remote_state("secret"))
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/v1/live")
                    .header("origin", "http://localhost:3000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let allow = res
            .headers()
            .get(http::header::ACCESS_CONTROL_ALLOW_HEADERS)
            .unwrap()
            .to_str()
            .unwrap()
            .to_ascii_lowercase();
        assert!(allow.contains("openai-alpha"));
    }

    #[tokio::test]
    async fn errors_carry_cors_headers_too() {
        let res = router(remote_state("secret"))
            .oneshot(
                Request::builder()
                    .uri("/v1/nope")
                    .header("origin", "http://localhost:3000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert!(res
            .headers()
            .get(http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_some());
    }

    #[tokio::test]
    async fn draining_answers_before_admission() {
        let state = remote_state("secret");
        state.drain.begin();
        let res = router(state)
            .oneshot(
                Request::builder()
                    .uri("/v1/nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(res.headers().get(http::header::RETRY_AFTER).unwrap(), "5");
        let bytes = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
        assert_eq!(&bytes[..], b"Service shutting down");
    }

    /// Draining must beat the *unauthenticated* preflight branch too, or a
    /// shutting-down proxy advertises CORS policy instead of 503.
    #[tokio::test]
    async fn draining_answers_before_preflight() {
        let state = remote_state("secret");
        state.drain.begin();
        let res = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/v1/live")
                    .header("origin", "http://localhost:3000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(res.headers().get(http::header::RETRY_AFTER).unwrap(), "5");
    }

    /// The probe handler in `protected_routes` does NOT call `protect` itself —
    /// it is a bare `async fn` returning 200. So if these assertions hold, the
    /// protection came from the layer, which is what a route added in a later
    /// phase will inherit without doing anything.
    #[tokio::test]
    async fn the_layer_protects_routes_that_do_nothing_themselves() {
        let app = router(remote_state("secret"));

        // Unauthorized: the layer answers and the bare handler never runs.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/__boundary_probe")
                    .header("origin", "http://localhost:3000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains("protected"),
            "the inner handler must not have run"
        );

        // Preflight on an explicit protected path: answered without a credential,
        // and not a 405 from the method router.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/v1/__boundary_probe")
                    .header("origin", "http://localhost:3000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        // Authorized: the handler runs, and CORS is still applied by the layer.
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/v1/__boundary_probe")
                    .header("x-gpt-live-api-key", "secret")
                    .header("origin", "http://localhost:3000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res
            .headers()
            .get(http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_some());
    }

    /// A protected route reached with an `Upgrade: websocket` header gets the
    /// upgrade-specific rejection message, so phase 030's sideband routes inherit
    /// the correct wording automatically.
    #[tokio::test]
    async fn the_layer_detects_a_websocket_upgrade_surface() {
        let res = router(test_state())
            .oneshot(
                Request::builder()
                    .uri("/v1/__boundary_probe")
                    .header("host", "127.0.0.1:10110")
                    .header("upgrade", "websocket")
                    .header("origin", "https://evil.test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body["error"]["message"],
            "WebSocket upgrade blocked: non-local Origin"
        );
    }

    #[tokio::test]
    async fn duplicate_authorization_is_refused_at_the_router() {
        let res = router(remote_state("secret"))
            .oneshot(
                Request::builder()
                    .uri("/v1/nope")
                    .header("x-gpt-live-api-key", "secret")
                    .header("authorization", "Bearer upstream")
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }
}
