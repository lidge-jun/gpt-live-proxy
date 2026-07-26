//! Real-socket hardening gates for exact byte limits, readiness, and resource recovery.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequest, State};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use gpt_live_proxy::app::{router, AppState};
use gpt_live_proxy::config::{BearerToken, Config, UpstreamProfile, MAX_BODY_BYTES};
use http::StatusCode;
use tokio::sync::{mpsc, oneshot, Notify};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message as ClientMessage};

type ClientSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone)]
struct UpstreamState {
    active: Arc<AtomicUsize>,
    accepted: Arc<AtomicUsize>,
    changed: Arc<Notify>,
    response_bytes: Arc<AtomicUsize>,
    last_request_bytes: Arc<AtomicUsize>,
    gates: Option<mpsc::UnboundedSender<oneshot::Sender<()>>>,
}

struct ActiveGuard {
    active: Arc<AtomicUsize>,
    changed: Arc<Notify>,
}

impl ActiveGuard {
    fn enter(state: &UpstreamState) -> Self {
        state.active.fetch_add(1, Ordering::SeqCst);
        state.changed.notify_waiters();
        Self {
            active: state.active.clone(),
            changed: state.changed.clone(),
        }
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
        self.changed.notify_waiters();
    }
}

async fn echo(mut socket: WebSocket, state: UpstreamState) {
    let _guard = ActiveGuard::enter(&state);
    while let Some(message) = socket.next().await {
        match message {
            Ok(Message::Close(frame)) => {
                let _ = socket.send(Message::Close(frame)).await;
                return;
            }
            Ok(other) => {
                if socket.send(other).await.is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

async fn upstream(State(state): State<UpstreamState>, request: axum::extract::Request) -> Response {
    state.accepted.fetch_add(1, Ordering::SeqCst);
    let websocket = request
        .headers()
        .get(http::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    if websocket {
        return match WebSocketUpgrade::from_request(request, &()).await {
            Ok(upgrade) => {
                let served = state.clone();
                upgrade.on_upgrade(move |socket| echo(socket, served))
            }
            Err(_) => StatusCode::BAD_REQUEST.into_response(),
        };
    }

    let _guard = ActiveGuard::enter(&state);
    let body = match axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES + 1).await {
        Ok(body) => body,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    state.last_request_bytes.store(body.len(), Ordering::SeqCst);
    if let Some(gates) = &state.gates {
        let (release, wait) = oneshot::channel();
        if gates.send(release).is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        let _ = wait.await;
    }
    let response_len = state.response_bytes.load(Ordering::SeqCst);
    Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(vec![b'x'; response_len]))
        .unwrap()
}

struct Upstream {
    base: String,
    state: UpstreamState,
    gates: Option<mpsc::UnboundedReceiver<oneshot::Sender<()>>>,
}

async fn start_upstream(response_bytes: usize, gated: bool) -> Upstream {
    let (gate_tx, gate_rx) = mpsc::unbounded_channel();
    let state = UpstreamState {
        active: Arc::new(AtomicUsize::new(0)),
        accepted: Arc::new(AtomicUsize::new(0)),
        changed: Arc::new(Notify::new()),
        response_bytes: Arc::new(AtomicUsize::new(response_bytes)),
        last_request_bytes: Arc::new(AtomicUsize::new(0)),
        gates: gated.then_some(gate_tx),
    };
    let app = Router::new()
        .fallback(any(upstream))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Upstream {
        base: format!("http://{address}"),
        state,
        gates: gated.then_some(gate_rx),
    }
}

struct Proxy {
    http: String,
    websocket: String,
    state: AppState,
}

async fn start_proxy(upstream: &str, configure: impl FnOnce(&mut Config)) -> Proxy {
    let mut config = Config::from_source(|key| match key {
        "GPT_LIVE_TOKEN" => Some("hardening-secret".to_string()),
        _ => None,
    })
    .unwrap();
    config.upstream = UpstreamProfile::ApiKeyManaged {
        base_url: format!("{upstream}/v1"),
        auth: BearerToken::new("hardening-secret"),
    };
    configure(&mut config);
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    config.bind = address;
    let state = AppState::new(config).unwrap();
    let served = state.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(served)).await;
    });
    Proxy {
        http: format!("http://{address}"),
        websocket: format!("ws://{address}"),
        state,
    }
}

async fn wait_active(state: &UpstreamState, expected: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let changed = state.changed.notified();
            if state.active.load(Ordering::SeqCst) == expected {
                return;
            }
            changed.await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("upstream active count never reached {expected}"));
}

async fn wait_connection_permit(state: &AppState) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if state.active_connections.available_permits() == 1 {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("connection permit recovery");
}

async fn public_connect(proxy: &Proxy) -> Result<ClientSocket, WsError> {
    tokio_tungstenite::connect_async(format!(
        "{}/v1/realtime?model=gpt-realtime-2.1",
        proxy.websocket
    ))
    .await
    .map(|(socket, _)| socket)
}

async fn private_connect(proxy: &Proxy) -> Result<ClientSocket, WsError> {
    let mut request = format!("{}/v1/live/rtc_hardening", proxy.websocket)
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("openai-alpha", "quicksilver=v2".parse().unwrap());
    tokio_tungstenite::connect_async(request)
        .await
        .map(|(socket, _)| socket)
}

fn assert_http_429(error: WsError) {
    let WsError::Http(response) = error else {
        panic!("expected HTTP rejection, got {error}");
    };
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response.headers().get(http::header::RETRY_AFTER).unwrap(),
        "1"
    );
}

#[tokio::test]
async fn literal_sixteen_mib_request_and_response_boundaries_are_exact() {
    let upstream = start_upstream(1, false).await;
    let proxy = start_proxy(&upstream.base, |_| {}).await;
    let client = reqwest::Client::new();
    let route = format!("{}/v1/realtime/sessions", proxy.http);

    let at = client
        .post(&route)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(vec![b'a'; MAX_BODY_BYTES])
        .send()
        .await
        .unwrap();
    assert_eq!(at.status(), StatusCode::OK);
    assert_eq!(
        upstream.state.last_request_bytes.load(Ordering::SeqCst),
        MAX_BODY_BYTES
    );
    assert_eq!(upstream.state.accepted.load(Ordering::SeqCst), 1);

    let over = client
        .post(&route)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(vec![b'a'; MAX_BODY_BYTES + 1])
        .send()
        .await
        .unwrap();
    assert_eq!(over.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(upstream.state.accepted.load(Ordering::SeqCst), 1);
    assert_eq!(proxy.state.active_requests.available_permits(), 128);

    upstream
        .state
        .response_bytes
        .store(MAX_BODY_BYTES, Ordering::SeqCst);
    let at = client
        .post(&route)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Bytes::from_static(b"{}"))
        .send()
        .await
        .unwrap();
    assert_eq!(at.status(), StatusCode::OK);
    assert_eq!(at.bytes().await.unwrap().len(), MAX_BODY_BYTES);

    upstream
        .state
        .response_bytes
        .store(MAX_BODY_BYTES + 1, Ordering::SeqCst);
    let over = client
        .post(&route)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Bytes::from_static(b"{}"))
        .send()
        .await
        .unwrap();
    assert_eq!(over.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(proxy.state.active_requests.available_permits(), 128);
}

#[tokio::test]
async fn sixty_four_http_capacity_rounds_recover_tasks_sockets_and_readiness() {
    let mut upstream = start_upstream(1, true).await;
    let proxy = start_proxy(&upstream.base, |config| {
        config.limits.active_requests = 1;
    })
    .await;
    let client = reqwest::Client::new();
    let route = format!("{}/v1/realtime/sessions", proxy.http);

    for round in 0..64 {
        let first_client = client.clone();
        let first_route = route.clone();
        let first = tokio::spawn(async move {
            first_client
                .post(first_route)
                .header(http::header::CONTENT_TYPE, "application/json")
                .body("{}")
                .send()
                .await
                .unwrap()
        });
        let release = upstream
            .gates
            .as_mut()
            .unwrap()
            .recv()
            .await
            .unwrap_or_else(|| panic!("missing HTTP gate in round {round}"));
        assert_eq!(proxy.state.active_requests.available_permits(), 0);
        let rejected = client
            .post(&route)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(rejected.headers()[http::header::RETRY_AFTER], "1");
        let _ = release.send(());
        assert_eq!(first.await.unwrap().status(), StatusCode::OK);
        wait_active(&upstream.state, 0).await;
        assert_eq!(proxy.state.active_requests.available_permits(), 1);
        assert_eq!(
            client
                .get(format!("{}/readyz", proxy.http))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }
}

async fn websocket_soak(
    proxy: &Proxy,
    upstream: &UpstreamState,
    connect: impl AsyncFn(&Proxy) -> Result<ClientSocket, WsError>,
) {
    for _round in 0..64 {
        let mut first = connect(proxy).await.unwrap();
        wait_active(upstream, 1).await;
        assert_eq!(proxy.state.active_connections.available_permits(), 0);
        assert_http_429(connect(proxy).await.unwrap_err());
        first.send(ClientMessage::Close(None)).await.unwrap();
        drop(first);
        wait_active(upstream, 0).await;
        wait_connection_permit(&proxy.state).await;
    }
}

#[tokio::test]
async fn sixty_four_public_and_private_websocket_rounds_recover_all_resources() {
    let upstream = start_upstream(1, false).await;
    let proxy = start_proxy(&upstream.base, |config| {
        config.limits.active_connections = 1;
    })
    .await;

    websocket_soak(&proxy, &upstream.state, public_connect).await;
    websocket_soak(&proxy, &upstream.state, private_connect).await;
    assert_eq!(upstream.state.active.load(Ordering::SeqCst), 0);
    assert_eq!(proxy.state.active_connections.available_permits(), 1);
}

#[tokio::test]
async fn connected_idle_timeout_closes_and_releases_public_and_private_permits() {
    let upstream = start_upstream(1, false).await;
    let proxy = start_proxy(&upstream.base, |config| {
        config.limits.active_connections = 1;
        config.limits.websocket_idle_timeout = Duration::from_millis(25);
    })
    .await;

    for private in [false, true] {
        let mut socket = if private {
            private_connect(&proxy).await.unwrap()
        } else {
            public_connect(&proxy).await.unwrap()
        };
        wait_active(&upstream.state, 1).await;
        // More than one idle interval elapses overall, but every received frame
        // on both legs resets the connected deadline.
        for sequence in 0..3 {
            tokio::time::sleep(Duration::from_millis(15)).await;
            let payload = format!("idle-reset-{sequence}");
            socket
                .send(ClientMessage::Text(payload.clone().into()))
                .await
                .unwrap();
            let echoed = socket.next().await.unwrap().unwrap();
            assert!(matches!(echoed, ClientMessage::Text(text) if text == payload));
        }
        let close = tokio::time::timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("idle close deadline")
            .expect("idle close frame")
            .expect("valid idle close frame");
        let ClientMessage::Close(Some(frame)) = close else {
            panic!("expected idle close, got {close:?}");
        };
        assert_eq!(u16::from(frame.code), 1001);
        assert_eq!(frame.reason, "idle timeout");
        drop(socket);
        wait_active(&upstream.state, 0).await;
        wait_connection_permit(&proxy.state).await;
    }
}
