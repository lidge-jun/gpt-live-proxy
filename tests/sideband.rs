//! Wire-level proof of the sideband contract (docs/030, docs/000 §5).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use gpt_live_proxy::app::{router, AppState};
use gpt_live_proxy::config::{BearerToken, Config, UpstreamProfile};
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

async fn start_proxy(upstream_base: &str) -> String {
    start_proxy_with(upstream_base, |_| {}).await
}

async fn start_proxy_with(upstream_base: &str, configure: impl FnOnce(&mut Config)) -> String {
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
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });
    format!("ws://{addr}")
}

async fn connect(
    url: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let (stream, _response) = tokio_tungstenite::connect_async(url)
        .await
        .expect("sideband upgrade");
    stream
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

    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
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
async fn protocol_headers_reach_the_upstream_handshake() {
    let (upstream, seen) = start_ws_upstream().await;
    let proxy = start_proxy(&upstream).await;

    let request = {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
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

    match tokio_tungstenite::connect_async(&url).await {
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
    let recovered = tokio_tungstenite::connect_async(&url).await;
    assert!(recovered.is_ok(), "permit did not recover: {recovered:?}");
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
