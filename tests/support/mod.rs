//! A real upstream server on a real socket, recording what it received.
//!
//! Asserting against a captured URI string is what makes the URL rules testable
//! rather than merely documented.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use http::{HeaderMap, Method, StatusCode, Uri};
use tokio::sync::oneshot;

#[derive(Debug, Clone)]
pub struct CapturedRequest {
    pub method: Method,
    pub uri: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

#[derive(Clone, Default)]
pub struct Captures(Arc<Mutex<Vec<CapturedRequest>>>);

impl Captures {
    pub fn last(&self) -> CapturedRequest {
        self.0
            .lock()
            .expect("captures lock")
            .last()
            .cloned()
            .expect("the upstream received no request")
    }

    pub fn count(&self) -> usize {
        self.0.lock().expect("captures lock").len()
    }
}

#[derive(Clone)]
pub struct UpstreamBehavior {
    pub status: StatusCode,
    pub body: &'static str,
    /// When set, the upstream returns this many bytes instead of `body`.
    pub body_bytes: Option<usize>,
    pub location: Option<&'static str>,
    /// Extra headers the upstream returns, to prove they are NOT relayed back.
    pub extra_headers: Vec<(&'static str, &'static str)>,
    pub delay: Option<std::time::Duration>,
}

impl Default for UpstreamBehavior {
    fn default() -> Self {
        Self {
            status: StatusCode::CREATED,
            body: "v=0\r\na=answer",
            body_bytes: None,
            location: Some("/v1/realtime/calls/rtc_test_call"),
            extra_headers: vec![
                ("content-type", "application/sdp"),
                ("set-cookie", "session=leaked"),
                ("x-request-id", "req-123"),
                ("cache-control", "no-store"),
            ],
            delay: None,
        }
    }
}

#[derive(Clone)]
struct UpstreamState {
    captures: Captures,
    behavior: UpstreamBehavior,
    dropped: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    started: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

/// Fires when the upstream's streaming response body is dropped, which happens
/// when the relay abandons the call. This is what proves cancellation actually
/// propagates across the runtime rather than merely being modelled.
pub struct BodyDropSignal(pub oneshot::Receiver<()>);

/// Fires once the streaming response body has actually produced a frame, so a
/// test can wait for a real in-flight state instead of sleeping and hoping.
pub struct BodyStartSignal(pub oneshot::Receiver<()>);

struct DropGuard(Arc<Mutex<Option<oneshot::Sender<()>>>>);

impl Drop for DropGuard {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.0.lock() {
            if let Some(tx) = slot.take() {
                let _ = tx.send(());
            }
        }
    }
}

/// Start a mock upstream on an ephemeral port. Returns its base URL and the
/// capture handle.
pub async fn start_upstream(behavior: UpstreamBehavior) -> (String, Captures) {
    let (base, captures, _drop, _start) = start_upstream_with_drop_signal(behavior).await;
    (base, captures)
}

/// Like [`start_upstream`], plus a signal that fires when a streaming response
/// body is dropped.
pub async fn start_upstream_with_drop_signal(
    behavior: UpstreamBehavior,
) -> (String, Captures, BodyDropSignal, BodyStartSignal) {
    let captures = Captures::default();
    let (drop_tx, drop_rx) = oneshot::channel();
    let (start_tx, start_rx) = oneshot::channel();
    let state = UpstreamState {
        captures: captures.clone(),
        behavior,
        dropped: Arc::new(Mutex::new(Some(drop_tx))),
        started: Arc::new(Mutex::new(Some(start_tx))),
    };

    let app = Router::new().fallback(any(record)).with_state(state);

    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (
        format!("http://{addr}"),
        captures,
        BodyDropSignal(drop_rx),
        BodyStartSignal(start_rx),
    )
}

async fn record(
    State(state): State<UpstreamState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    state
        .captures
        .0
        .lock()
        .expect("captures lock")
        .push(CapturedRequest {
            method,
            uri: uri.to_string(),
            headers,
            body,
        });

    if let Some(delay) = state.behavior.delay {
        // A streaming body that never completes: the relay holds it open, so
        // dropping it is observable evidence that the relay gave up.
        let guard = DropGuard(state.dropped.clone());
        let started = state.started.clone();
        let stream =
            futures_util::stream::unfold((guard, started), move |(guard, started)| async move {
                tokio::time::sleep(delay).await;
                // Announce the first frame so a test can wait for a genuinely
                // in-flight stream rather than sleeping a guessed interval.
                if let Ok(mut slot) = started.lock() {
                    if let Some(tx) = slot.take() {
                        let _ = tx.send(());
                    }
                }
                Some((
                    Ok::<_, std::io::Error>(Bytes::from_static(b"x")),
                    (guard, started),
                ))
            });
        return axum::body::Body::from_stream(stream).into_response();
    }

    let mut response = match state.behavior.body_bytes {
        Some(size) => (state.behavior.status, vec![b'x'; size]).into_response(),
        None => (state.behavior.status, state.behavior.body).into_response(),
    };
    if let Some(location) = state.behavior.location {
        response.headers_mut().insert(
            http::header::LOCATION,
            http::HeaderValue::from_static(location),
        );
    }
    for (name, value) in &state.behavior.extra_headers {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            response.headers_mut().insert(name, value);
        }
    }
    response
}
