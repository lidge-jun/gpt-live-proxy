//! Wire-level proof of the sideband contract (docs/030, docs/000 §5).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use gpt_live_proxy::app::{router, AppState};
use gpt_live_proxy::config::{BearerToken, Config, UpstreamProfile};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as ClientMessage;

#[derive(Clone, Default)]
struct Seen {
    path: Arc<Mutex<Option<String>>>,
    headers: Arc<Mutex<Option<http::HeaderMap>>>,
}

/// An upstream that echoes every frame back unchanged, so any corruption in the
/// relay shows up as a difference at the client.
async fn start_ws_upstream() -> (String, Seen) {
    let seen = Seen::default();

    async fn upgrade(
        State(seen): State<Seen>,
        ws: WebSocketUpgrade,
        axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
        headers: http::HeaderMap,
    ) -> Response {
        *seen.path.lock().unwrap() = Some(uri.to_string());
        *seen.headers.lock().unwrap() = Some(headers);
        ws.on_upgrade(echo)
    }

    async fn echo(mut socket: WebSocket) {
        while let Some(Ok(message)) = socket.next().await {
            match message {
                Message::Close(frame) => {
                    let _ = socket.send(Message::Close(frame)).await;
                    return;
                }
                other => {
                    if socket.send(other).await.is_err() {
                        return;
                    }
                }
            }
        }
    }

    let app = Router::new()
        .fallback(get(upgrade))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind ws upstream");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (format!("http://{addr}"), seen)
}

/// Accept TCP connections but never answer the HTTP upgrade. This activates
/// the proxy's WebSocket connect timeout rather than a fast connection error.
async fn start_stalled_handshake_upstream() -> String {
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind stalled upstream");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let _socket = socket;
                std::future::pending::<()>().await;
            });
        }
    });
    format!("http://{addr}")
}

async fn start_proxy(upstream_base: &str) -> String {
    start_proxy_with(upstream_base, |_| {}).await
}

async fn start_proxy_with(upstream_base: &str, configure: impl FnOnce(&mut Config)) -> String {
    start_proxy_with_state(upstream_base, configure).await.0
}

async fn start_proxy_with_state(
    upstream_base: &str,
    configure: impl FnOnce(&mut Config),
) -> (String, Arc<tokio::sync::Semaphore>) {
    let mut config = Config::from_source(|k| match k {
        "GPT_LIVE_TOKEN" => Some("unused".to_string()),
        _ => None,
    })
    .expect("config");
    config.upstream = UpstreamProfile::ApiKeyManaged {
        base_url: format!("{upstream_base}/v1"),
        auth: BearerToken::new("sk-test"),
    };
    configure(&mut config);

    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().unwrap();
    // The origin policy compares the Host port against the configured bind.
    config.bind = addr;
    let state = AppState::new(config).expect("state");
    let active_connections = state.active_connections.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });
    (format!("ws://{addr}"), active_connections)
}

async fn connect(
    url: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let request = private_request(url, None);
    let (stream, _response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("sideband upgrade");
    stream
}

fn private_request(
    url: &str,
    alpha: Option<&str>,
) -> tokio_tungstenite::tungstenite::http::Request<()> {
    let mut request = url.into_client_request().expect("sideband request");
    let alpha = if url.contains("/v1/live/") {
        alpha.unwrap_or("quicksilver=v2")
    } else {
        alpha.unwrap_or("quicksilver=v1")
    };
    request
        .headers_mut()
        .insert("openai-alpha", alpha.parse().unwrap());
    request
}

#[tokio::test]
async fn a_frameless_join_reaches_the_expected_upstream_path() {
    let (upstream, seen) = start_ws_upstream().await;
    let proxy = start_proxy(&upstream).await;

    let mut client = connect(&format!("{proxy}/v1/live/rtc_abc")).await;
    client
        .send(ClientMessage::Text("ping".into()))
        .await
        .expect("send");
    let _ = client.next().await;

    let path = seen.path.lock().unwrap().clone().expect("upstream path");
    assert_eq!(path, "/v1/live/rtc_abc");
}

#[tokio::test]
async fn a_realtime_query_join_carries_the_intent_parameter() {
    let (upstream, seen) = start_ws_upstream().await;
    let proxy = start_proxy(&upstream).await;

    let mut request = format!("{proxy}/v1/realtime?call_id=rtc_q")
        .into_client_request()
        .expect("private query request");
    request
        .headers_mut()
        .insert("openai-alpha", "quicksilver=v1".parse().unwrap());
    let (mut client, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("private query upgrade");
    client
        .send(ClientMessage::Text("ping".into()))
        .await
        .expect("send");
    let _ = client.next().await;

    let path = seen.path.lock().unwrap().clone().expect("upstream path");
    assert_eq!(path, "/v1/realtime?intent=quicksilver&call_id=rtc_q");
}

#[tokio::test]
async fn private_path_and_alpha_cross_product_rejects_before_upstream_contact() {
    let (upstream, seen) = start_ws_upstream().await;
    let proxy = start_proxy_with(&upstream, |config| {
        config.limits.active_connections = 1;
    })
    .await;

    for (path, alpha) in [
        ("/v1/live/rtc_matrix", Some("quicksilver=v1")),
        ("/v1/realtime/calls/rtc_matrix", Some("quicksilver=v2")),
        ("/v1/live/rtc_matrix", Some("quicksilver=v9")),
    ] {
        let request = private_request(&format!("{proxy}{path}"), alpha);
        match tokio_tungstenite::connect_async(request).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
            }
            other => panic!("expected zero-contact rejection for {path} {alpha:?}, got {other:?}"),
        }
        assert!(
            seen.path.lock().unwrap().is_none(),
            "invalid pair contacted upstream: {path} {alpha:?}"
        );
    }

    let missing = format!("{proxy}/v1/live/rtc_missing")
        .into_client_request()
        .unwrap();
    match tokio_tungstenite::connect_async(missing).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
        }
        other => panic!("expected missing-alpha rejection, got {other:?}"),
    }
    assert!(seen.path.lock().unwrap().is_none());

    let mut repeated = private_request(
        &format!("{proxy}/v1/live/rtc_repeated"),
        Some("quicksilver=v2"),
    );
    repeated
        .headers_mut()
        .append("openai-alpha", "quicksilver=v2".parse().unwrap());
    match tokio_tungstenite::connect_async(repeated).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
        }
        other => panic!("expected repeated-alpha rejection, got {other:?}"),
    }
    assert!(seen.path.lock().unwrap().is_none());

    // The rejected matrix consumed neither the sole permit nor an upstream
    // contact. A valid V1 alias can immediately take both.
    let valid = private_request(
        &format!("{proxy}/v1/realtime/calls/rtc_valid"),
        Some("quicksilver=v1"),
    );
    let (mut client, _) = tokio_tungstenite::connect_async(valid)
        .await
        .expect("valid V1 pair upgrades after rejections");
    client
        .send(ClientMessage::Text("ping".into()))
        .await
        .unwrap();
    assert!(matches!(
        client.next().await,
        Some(Ok(ClientMessage::Text(_)))
    ));
    assert_eq!(
        seen.path.lock().unwrap().as_deref(),
        Some("/v1/realtime/calls/rtc_valid")
    );
}

#[tokio::test]
async fn protocol_headers_reach_the_upstream_handshake() {
    let (upstream, seen) = start_ws_upstream().await;
    let proxy = start_proxy(&upstream).await;

    let request = {
        let mut request = format!("{proxy}/v1/live/rtc_h")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("openai-alpha", "quicksilver=v2".parse().unwrap());
        request
            .headers_mut()
            .insert("thread-id", "thread-9".parse().unwrap());
        request
    };
    let (mut client, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("upgrade");
    client
        .send(ClientMessage::Text("ping".into()))
        .await
        .expect("send");
    let _ = client.next().await;

    let headers = seen.headers.lock().unwrap().clone().expect("headers");
    assert_eq!(headers.get("openai-alpha").unwrap(), "quicksilver=v2");
    assert_eq!(headers.get("thread-id").unwrap(), "thread-9");
    // Proxy-owned authentication replaced whatever the client sent.
    assert_eq!(headers.get("authorization").unwrap(), "Bearer sk-test");
}

#[tokio::test]
async fn text_frames_survive_byte_identical() {
    let (upstream, _seen) = start_ws_upstream().await;
    let proxy = start_proxy(&upstream).await;
    let mut client = connect(&format!("{proxy}/v1/live/rtc_t")).await;

    // Multibyte text is where a lossy relay would show up first.
    let payload = "가볍게 얘기해봐요 — ünïcødé ✅";
    client
        .send(ClientMessage::Text(payload.into()))
        .await
        .expect("send");

    match client.next().await.expect("reply").expect("frame") {
        ClientMessage::Text(text) => assert_eq!(text.as_str(), payload),
        other => panic!("expected text, got {other:?}"),
    }
}

#[tokio::test]
async fn binary_frames_stay_binary() {
    let (upstream, _seen) = start_ws_upstream().await;
    let proxy = start_proxy(&upstream).await;
    let mut client = connect(&format!("{proxy}/v1/live/rtc_b")).await;

    // Invalid UTF-8 on purpose: a relay that decodes would corrupt this.
    let payload = vec![0x00u8, 0xff, 0xfe, 0x10, 0x80];
    client
        .send(ClientMessage::Binary(payload.clone().into()))
        .await
        .expect("send");

    match client.next().await.expect("reply").expect("frame") {
        ClientMessage::Binary(bytes) => assert_eq!(bytes.as_ref(), payload.as_slice()),
        other => panic!("expected binary, got {other:?}"),
    }
}

#[tokio::test]
async fn a_large_frame_survives_intact() {
    let (upstream, _seen) = start_ws_upstream().await;
    let proxy = start_proxy(&upstream).await;
    let mut client = connect(&format!("{proxy}/v1/live/rtc_l")).await;

    // ~1.3 MiB of Korean text, the case the original forensics work chased.
    let payload = "가".repeat(450_000);
    client
        .send(ClientMessage::Text(payload.clone().into()))
        .await
        .expect("send");

    match client.next().await.expect("reply").expect("frame") {
        ClientMessage::Text(text) => {
            assert_eq!(text.len(), payload.len());
            assert_eq!(text.as_str(), payload);
        }
        other => panic!("expected text, got {other:?}"),
    }
}

/// An upstream that closes on its OWN initiative with a distinctive code.
///
/// The previous version of this test had the client initiate the close, which
/// `run_pump` normalizes before it ever reaches the upstream — so the 1000 it
/// observed was the local close handshake and proved nothing.
async fn start_closing_upstream(code: u16, reason: &'static str) -> String {
    async fn upgrade(
        State((code, reason)): State<(u16, &'static str)>,
        ws: WebSocketUpgrade,
    ) -> Response {
        ws.on_upgrade(move |mut socket| async move {
            let _ = socket
                .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                    code,
                    reason: reason.into(),
                })))
                .await;
        })
    }

    let app = Router::new()
        .fallback(get(upgrade))
        .with_state((code, reason));
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn an_upstream_close_propagates_its_code_and_reason() {
    let upstream = start_closing_upstream(4321, "bespoke").await;
    let proxy = start_proxy(&upstream).await;
    let mut client = connect(&format!("{proxy}/v1/live/rtc_c")).await;

    while let Some(Ok(message)) = client.next().await {
        if let ClientMessage::Close(frame) = message {
            let frame = frame.expect("a close frame");
            assert_eq!(
                u16::from(frame.code),
                4321,
                "the upstream code must survive"
            );
            assert_eq!(frame.reason.as_str(), "bespoke");
            return;
        }
    }
    panic!("the upstream close never reached the client");
}

/// The other half of the asymmetry: whatever the client says, the upstream is
/// told 1000 / "client closed".
#[tokio::test]
async fn a_client_close_reaches_the_upstream_normalized() {
    // A oneshot rather than a polling loop: the test waits for the event
    // instead of sampling shared state and hoping.
    let (tx, rx) = tokio::sync::oneshot::channel::<(u16, String)>();
    let sender = Arc::new(Mutex::new(Some(tx)));

    type CloseSender = Arc<Mutex<Option<tokio::sync::oneshot::Sender<(u16, String)>>>>;

    async fn upgrade(State(sender): State<CloseSender>, ws: WebSocketUpgrade) -> Response {
        ws.on_upgrade(move |mut socket| async move {
            while let Some(message) = socket.next().await {
                match message {
                    Ok(Message::Close(frame)) => {
                        if let Some(frame) = frame {
                            if let Some(tx) = sender.lock().unwrap().take() {
                                let _ = tx.send((frame.code, frame.reason.to_string()));
                            }
                        }
                        return;
                    }
                    // Echo so the client can confirm steady state before closing.
                    Ok(other) => {
                        if socket.send(other).await.is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        })
    }

    let app = Router::new()
        .fallback(get(upgrade))
        .with_state(sender.clone());
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let proxy = start_proxy(&format!("http://{addr}")).await;
    let mut client = connect(&format!("{proxy}/v1/live/rtc_n")).await;

    // Reach steady state first. A close sent during the handshake window is a
    // different case: there is no open upstream to normalize it to.
    client
        .send(ClientMessage::Text("hello".into()))
        .await
        .expect("send");
    let echoed = client.next().await.expect("reply").expect("frame");
    assert!(matches!(echoed, ClientMessage::Text(_)));

    client
        .send(ClientMessage::Close(Some(
            tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::from(
                    4321u16,
                ),
                reason: "client said this".into(),
            },
        )))
        .await
        .expect("send close");

    let seen = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
        .await
        .expect("the upstream never saw a close")
        .expect("the close sender was dropped");
    assert_eq!(seen.0, 1000, "the client's own code must be discarded");
    assert_eq!(seen.1, "client closed");
}

#[tokio::test]
async fn an_invalid_call_id_is_not_upgraded() {
    let (upstream, _seen) = start_ws_upstream().await;
    let proxy = start_proxy(&upstream).await;

    // A slash-bearing id cannot match the call-id pattern.
    let result = tokio_tungstenite::connect_async(format!("{proxy}/v1/live/has%2Fslash")).await;
    assert!(result.is_err(), "an invalid call id must not upgrade");
}

#[tokio::test]
async fn an_unreachable_upstream_closes_the_downstream() {
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let dead = listener.local_addr().unwrap();
    drop(listener);

    let proxy = start_proxy(&format!("http://{dead}")).await;
    let mut client = connect(&format!("{proxy}/v1/live/rtc_dead")).await;

    // The downstream upgrade succeeds, then the relay reports the failure —
    // a local 101 alone never proved an upstream connection.
    let mut saw_close = false;
    while let Some(message) = client.next().await {
        match message {
            Ok(ClientMessage::Close(Some(frame))) => {
                assert_eq!(u16::from(frame.code), 1011);
                assert_eq!(frame.reason.as_str(), "upstream connect failed");
                saw_close = true;
                break;
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    assert!(saw_close, "the relay never reported the failed upstream");
}

/// An upstream whose handshake is held open until the test releases it.
///
/// A sleep would only make the queue window *probable*: if the frames took
/// longer to send than the sleep, the test would silently run in steady state
/// and prove nothing. This blocks until the test says otherwise, so the frames
/// are guaranteed to be queued.
struct HeldUpstream {
    base: String,
    /// Fires when the upstream handshake has actually been reached.
    reached: tokio::sync::oneshot::Receiver<()>,
    /// Dropping or sending releases the handshake.
    release: tokio::sync::oneshot::Sender<()>,
}

/// A raw upstream that deliberately withholds the first HTTP 101 and observes
/// the proxy-side TCP EOF. Once that socket is gone it accepts a second,
/// ordinary WebSocket so recovery is proved end to end rather than inferred
/// from a semaphore counter alone.
struct CancellationBoundaryUpstream {
    base: String,
    first_request: tokio::sync::oneshot::Receiver<()>,
    first_closed: tokio::sync::oneshot::Receiver<()>,
    second_closed: tokio::sync::oneshot::Receiver<()>,
}

async fn start_cancellation_boundary_upstream() -> CancellationBoundaryUpstream {
    use tokio::io::AsyncReadExt;

    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind cancellation-boundary upstream");
    let addr = listener.local_addr().unwrap();
    let (first_request_tx, first_request_rx) = tokio::sync::oneshot::channel();
    let (first_closed_tx, first_closed_rx) = tokio::sync::oneshot::channel();
    let (second_closed_tx, second_closed_rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.expect("accept first upstream");
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = first.read(&mut chunk).await.expect("read first handshake");
            assert!(read > 0, "first upstream closed before its HTTP request");
            request.extend_from_slice(&chunk[..read]);
        }
        let _ = first_request_tx.send(());

        loop {
            match first.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        let _ = first_closed_tx.send(());

        let (second, _) = listener.accept().await.expect("accept recovered upstream");
        let mut websocket = tokio_tungstenite::accept_async(second)
            .await
            .expect("upgrade recovered upstream");
        websocket
            .send(ClientMessage::Text("recovered".into()))
            .await
            .expect("send recovery marker");
        while let Some(message) = websocket.next().await {
            match message {
                Ok(ClientMessage::Close(frame)) => {
                    let _ = websocket.send(ClientMessage::Close(frame)).await;
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let _ = second_closed_tx.send(());
    });

    CancellationBoundaryUpstream {
        base: format!("http://{addr}"),
        first_request: first_request_rx,
        first_closed: first_closed_rx,
        second_closed: second_closed_rx,
    }
}

#[derive(Clone)]
struct ReadyBoundaryState {
    next: Arc<AtomicUsize>,
    ready: tokio::sync::mpsc::UnboundedSender<usize>,
    closed: tokio::sync::mpsc::UnboundedSender<usize>,
}

/// A normal WebSocket upstream whose post-101 readiness and socket closure are
/// independently observable for every connection.
async fn start_ready_boundary_upstream() -> (
    String,
    tokio::sync::mpsc::UnboundedReceiver<usize>,
    tokio::sync::mpsc::UnboundedReceiver<usize>,
) {
    async fn upgrade(State(state): State<ReadyBoundaryState>, ws: WebSocketUpgrade) -> Response {
        let id = state.next.fetch_add(1, Ordering::SeqCst);
        ws.on_upgrade(move |mut socket| async move {
            socket
                .send(Message::Text(format!("ready-{id}").into()))
                .await
                .expect("send ready marker");
            let _ = state.ready.send(id);
            while let Some(message) = socket.next().await {
                match message {
                    Ok(Message::Close(frame)) => {
                        let _ = socket.send(Message::Close(frame)).await;
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            let _ = state.closed.send(id);
        })
    }

    let (ready_tx, ready_rx) = tokio::sync::mpsc::unbounded_channel();
    let (closed_tx, closed_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = ReadyBoundaryState {
        next: Arc::new(AtomicUsize::new(0)),
        ready: ready_tx,
        closed: closed_tx,
    };
    let app = Router::new().fallback(get(upgrade)).with_state(state);
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind ready-boundary upstream");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), ready_rx, closed_rx)
}

async fn await_available_connection_permit(permits: Arc<tokio::sync::Semaphore>) {
    let permit = tokio::time::timeout(std::time::Duration::from_secs(2), permits.acquire_owned())
        .await
        .expect("connection permit was not released")
        .expect("connection semaphore was closed");
    drop(permit);
}

async fn start_held_upstream() -> HeldUpstream {
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let reached = Arc::new(Mutex::new(Some(reached_tx)));
    let release = Arc::new(tokio::sync::Mutex::new(Some(release_rx)));

    type Held = (
        Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
        Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
    );

    async fn upgrade(State((reached, release)): State<Held>, ws: WebSocketUpgrade) -> Response {
        if let Some(tx) = reached.lock().unwrap().take() {
            let _ = tx.send(());
        }
        if let Some(rx) = release.lock().await.take() {
            let _ = rx.await;
        }
        ws.on_upgrade(|mut socket| async move {
            while let Some(Ok(message)) = socket.next().await {
                match message {
                    Message::Close(frame) => {
                        let _ = socket.send(Message::Close(frame)).await;
                        return;
                    }
                    other => {
                        if socket.send(other).await.is_err() {
                            return;
                        }
                    }
                }
            }
        })
    }

    let app = Router::new()
        .fallback(get(upgrade))
        .with_state((reached, release));
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    HeldUpstream {
        base: format!("http://{addr}"),
        reached: reached_rx,
        release: release_tx,
    }
}

#[tokio::test]
async fn frames_sent_during_the_handshake_are_queued_and_flushed_in_order() {
    let held = start_held_upstream().await;
    let proxy = start_proxy(&held.base).await;
    let mut client = connect(&format!("{proxy}/v1/live/rtc_q")).await;

    // The upstream handshake is provably reached and held.
    held.reached.await.expect("upstream handshake reached");

    for index in 0..8 {
        client
            .send(ClientMessage::Text(format!("frame-{index}").into()))
            .await
            .expect("send");
    }

    // Only now can the upstream open, so those eight frames were queued.
    let _ = held.release.send(());

    for index in 0..8 {
        match client.next().await.expect("reply").expect("frame") {
            ClientMessage::Text(text) => assert_eq!(text.as_str(), format!("frame-{index}")),
            other => panic!("expected text, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn the_thirty_third_queued_frame_closes_with_1009() {
    let held = start_held_upstream().await;
    let proxy = start_proxy(&held.base).await;
    let mut client = connect(&format!("{proxy}/v1/live/rtc_flood")).await;

    held.reached.await.expect("upstream handshake reached");

    // 32 fit; the 33rd must be refused. The handshake is still held, so every
    // one of these is a queued frame.
    for index in 0..40 {
        if client
            .send(ClientMessage::Text(format!("f{index}").into()))
            .await
            .is_err()
        {
            break;
        }
    }

    let mut saw_policy_close = false;
    while let Some(message) = client.next().await {
        match message {
            Ok(ClientMessage::Close(Some(frame))) => {
                assert_eq!(u16::from(frame.code), 1009);
                assert_eq!(frame.reason.as_str(), "too many pending frames");
                saw_policy_close = true;
                break;
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    let _ = held.release.send(());
    assert!(
        saw_policy_close,
        "flooding the pre-open queue must close with 1009"
    );
}

#[tokio::test]
async fn keepalives_do_not_consume_the_queue_budget() {
    let held = start_held_upstream().await;
    let proxy = start_proxy(&held.base).await;
    let mut client = connect(&format!("{proxy}/v1/live/rtc_ping")).await;

    held.reached.await.expect("upstream handshake reached");

    // Far more pings than the queue bound, all provably during the handshake
    // window. If pings counted, this would close with 1009.
    for _ in 0..50 {
        client
            .send(ClientMessage::Ping(Vec::new().into()))
            .await
            .expect("ping");
    }
    client
        .send(ClientMessage::Text("survived".into()))
        .await
        .expect("send");

    let _ = held.release.send(());

    loop {
        match client.next().await.expect("reply").expect("frame") {
            ClientMessage::Text(text) => {
                assert_eq!(text.as_str(), "survived");
                return;
            }
            ClientMessage::Close(frame) => {
                panic!("keepalives consumed the queue budget: {frame:?}");
            }
            _ => continue,
        }
    }
}

#[tokio::test]
async fn queued_frames_are_bounded_by_aggregate_bytes_not_only_count() {
    let held = start_held_upstream().await;
    let proxy = start_proxy_with(&held.base, |config| {
        config.limits.websocket_frame_bytes = 64;
    })
    .await;
    let mut client = connect(&format!("{proxy}/v1/live/rtc_bytes")).await;

    held.reached.await.expect("upstream handshake reached");
    client
        .send(ClientMessage::Text("a".repeat(40).into()))
        .await
        .expect("first queued frame");
    client
        .send(ClientMessage::Text("b".repeat(40).into()))
        .await
        .expect("aggregate-overflow frame");

    while let Some(message) = client.next().await {
        match message {
            Ok(ClientMessage::Close(Some(frame))) => {
                assert_eq!(u16::from(frame.code), 1009);
                assert_eq!(frame.reason.as_str(), "queued frames too large");
                let _ = held.release.send(());
                return;
            }
            Ok(_) => continue,
            Err(error) => panic!("expected aggregate-cap close, got {error:?}"),
        }
    }
    let _ = held.release.send(());
    panic!("aggregate queued-byte overflow did not close the connection");
}

#[tokio::test]
async fn active_private_connection_permit_rejects_then_recovers() {
    let held = start_held_upstream().await;
    let proxy = start_proxy_with(&held.base, |config| {
        config.limits.active_connections = 1;
    })
    .await;
    let url = format!("{proxy}/v1/live/rtc_permit");
    let mut first = connect(&url).await;
    held.reached
        .await
        .expect("first upstream handshake reached");

    match tokio_tungstenite::connect_async(private_request(&url, None)).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status(), http::StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(response.headers().get("retry-after").unwrap(), "1");
        }
        other => panic!("expected permit 429, got {other:?}"),
    }

    held.release.send(()).expect("release upstream");
    first
        .send(ClientMessage::Close(None))
        .await
        .expect("close first");
    while let Some(message) = first.next().await {
        if matches!(message, Ok(ClientMessage::Close(_)) | Err(_)) {
            break;
        }
    }
    tokio::task::yield_now().await;

    // The original proxy remains configured to the first upstream, which has
    // completed its one accepted connection. Reaching a local 101 here proves
    // the permit was released; the later private upstream failure is separate.
    let recovered = tokio_tungstenite::connect_async(private_request(&url, None)).await;
    assert!(recovered.is_ok(), "permit did not recover: {recovered:?}");
}

#[tokio::test]
async fn disconnect_during_a_held_upstream_handshake_closes_socket_and_recovers() {
    let CancellationBoundaryUpstream {
        base,
        first_request,
        first_closed,
        second_closed,
    } = start_cancellation_boundary_upstream().await;
    let (proxy, permits) = start_proxy_with_state(&base, |config| {
        config.limits.active_connections = 1;
    })
    .await;
    let url = format!("{proxy}/v1/live/rtc_disconnect");
    let first = connect(&url).await;
    first_request.await.expect("held handshake reached");

    // Private joins are downstream-first: dropping this local 101 must cancel
    // the still-pending upstream handshake. The raw upstream observes EOF, so
    // this checks the actual TCP resource rather than only the relay outcome.
    drop(first);
    tokio::time::timeout(std::time::Duration::from_secs(2), first_closed)
        .await
        .expect("pending upstream TCP socket stayed open")
        .expect("upstream close observer was dropped");
    await_available_connection_permit(permits).await;

    // The same listener now performs a real second WebSocket handshake. Seeing
    // its marker proves both permit recovery and a usable replacement relay.
    let mut recovered = connect(&url).await;
    match recovered
        .next()
        .await
        .expect("recovery marker")
        .expect("frame")
    {
        ClientMessage::Text(text) => assert_eq!(text.as_str(), "recovered"),
        other => panic!("expected recovery marker, got {other:?}"),
    }
    recovered
        .send(ClientMessage::Close(None))
        .await
        .expect("close recovered connection");
    tokio::time::timeout(std::time::Duration::from_secs(2), second_closed)
        .await
        .expect("recovered upstream socket stayed open")
        .expect("recovered close observer was dropped");
}

#[tokio::test]
async fn disconnect_at_upstream_ready_boundary_closes_socket_and_recovers() {
    let (upstream, mut ready, mut closed) = start_ready_boundary_upstream().await;
    let (proxy, permits) = start_proxy_with_state(&upstream, |config| {
        config.limits.active_connections = 1;
    })
    .await;
    let url = format!("{proxy}/v1/live/rtc_ready_cancel");
    let first = connect(&url).await;

    // `Ready(0)` is emitted from the upstream's post-101 callback, after its
    // first frame has been flushed. Drop immediately at that ownership edge.
    assert_eq!(ready.recv().await, Some(0));
    drop(first);
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), closed.recv())
            .await
            .expect("ready upstream socket stayed open"),
        Some(0)
    );
    await_available_connection_permit(permits).await;

    let mut recovered = connect(&url).await;
    assert_eq!(ready.recv().await, Some(1));
    match recovered
        .next()
        .await
        .expect("ready marker")
        .expect("frame")
    {
        ClientMessage::Text(text) => assert_eq!(text.as_str(), "ready-1"),
        other => panic!("expected ready marker, got {other:?}"),
    }
    recovered
        .send(ClientMessage::Close(None))
        .await
        .expect("close recovered connection");
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), closed.recv())
            .await
            .expect("recovered ready upstream socket stayed open"),
        Some(1)
    );
}

#[tokio::test]
async fn private_connect_timeout_closes_and_releases_the_permit() {
    let upstream = start_stalled_handshake_upstream().await;
    let proxy = start_proxy_with(&upstream, |config| {
        config.limits.active_connections = 1;
        config.limits.websocket_connect_timeout = std::time::Duration::from_millis(30);
    })
    .await;
    let url = format!("{proxy}/v1/live/rtc_timeout");
    let mut first = connect(&url).await;

    let close = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(message) = first.next().await {
            if let Ok(ClientMessage::Close(Some(frame))) = message {
                return frame;
            }
        }
        panic!("timeout connection ended without a close frame");
    })
    .await
    .expect("proxy did not apply the private connect timeout");
    assert_eq!(u16::from(close.code), 1011);
    assert_eq!(close.reason.as_str(), "upstream connect failed");

    // A second downstream 101 proves the timed-out task dropped its sole
    // permit. It will independently time out against the same stalled mock.
    let second = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        tokio_tungstenite::connect_async(private_request(&url, None)),
    )
    .await
    .expect("second downstream handshake stalled")
    .expect("permit was not released after timeout");
    drop(second);
}

#[tokio::test]
async fn a_non_upgrade_request_on_a_sideband_path_is_not_found() {
    let (upstream, _seen) = start_ws_upstream().await;
    let proxy = start_proxy(&upstream).await;
    let http_base = proxy.replace("ws://", "http://");

    let response = reqwest::get(format!("{http_base}/v1/live/rtc_plain"))
        .await
        .expect("get");
    assert_eq!(response.status(), 404);
    let value: serde_json::Value = response.json().await.expect("json");
    assert_eq!(
        value["error"]["message"],
        "Unknown endpoint: GET /v1/live/rtc_plain"
    );
}

#[tokio::test]
async fn a_trailing_slash_still_reaches_the_parser() {
    let (upstream, seen) = start_ws_upstream().await;
    let proxy = start_proxy(&upstream).await;

    let mut client = connect(&format!("{proxy}/v1/live/rtc_slash/")).await;
    client
        .send(ClientMessage::Text("ping".into()))
        .await
        .expect("send");
    let _ = client.next().await;

    let path = seen.path.lock().unwrap().clone().expect("upstream path");
    assert_eq!(path, "/v1/live/rtc_slash");
}
