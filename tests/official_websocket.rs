//! Real-socket conformance for the official OpenAI Realtime WebSocket surface.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequest, State};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use gpt_live_proxy::app::{router, AppState};
use gpt_live_proxy::config::{BearerToken, Config, UpstreamProfile};
use http::{HeaderMap, Method, StatusCode};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as ClientMessage;

type ClientSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone, Debug)]
struct Capture {
    method: Method,
    uri: String,
    headers: HeaderMap,
    body: Bytes,
}

#[derive(Clone)]
struct UpstreamBehavior {
    selected_protocol: Option<String>,
    server_frames: Vec<Message>,
    rest_status: StatusCode,
    rest_body: &'static str,
}

impl Default for UpstreamBehavior {
    fn default() -> Self {
        Self {
            selected_protocol: None,
            server_frames: Vec::new(),
            rest_status: StatusCode::OK,
            rest_body: r#"{"value":"ek_minted"}"#,
        }
    }
}

#[derive(Clone)]
struct UpstreamState {
    captures: Arc<Mutex<Vec<Capture>>>,
    behavior: UpstreamBehavior,
}

async fn echo(mut socket: WebSocket, frames: Vec<Message>) {
    for frame in frames {
        if socket.send(frame).await.is_err() {
            return;
        }
    }
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

async fn upstream_handler(
    State(state): State<UpstreamState>,
    method: Method,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    request: axum::extract::Request,
) -> Response {
    let headers = request.headers().clone();
    let is_websocket = headers
        .get(http::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    let (websocket, body) = if is_websocket {
        let websocket = WebSocketUpgrade::from_request(request, &()).await.ok();
        (websocket, Bytes::new())
    } else {
        let body = axum::body::to_bytes(request.into_body(), 1024 * 1024)
            .await
            .unwrap_or_default();
        (None, body)
    };
    state.captures.lock().expect("captures").push(Capture {
        method,
        uri: uri.to_string(),
        headers,
        body,
    });

    let Some(websocket) = websocket else {
        return (state.behavior.rest_status, state.behavior.rest_body).into_response();
    };
    let websocket = match &state.behavior.selected_protocol {
        Some(protocol) => websocket.protocols([protocol.clone()]),
        None => websocket,
    };
    let frames = state.behavior.server_frames.clone();
    let mut response = websocket.on_upgrade(move |socket| echo(socket, frames));
    response
        .headers_mut()
        .insert("x-request-id", "req-ws-safe".parse().unwrap());
    response
        .headers_mut()
        .insert("set-cookie", "must-not-pass=1".parse().unwrap());
    response
}

async fn start_ws_upstream(behavior: UpstreamBehavior) -> (String, Arc<Mutex<Vec<Capture>>>) {
    let captures = Arc::new(Mutex::new(Vec::new()));
    let state = UpstreamState {
        captures: captures.clone(),
        behavior,
    };
    let app = Router::new()
        .fallback(any(upstream_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind upstream");
    let address = listener.local_addr().expect("upstream address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), captures)
}

fn base_config(profile: UpstreamProfile) -> Config {
    let mut config = Config::from_source(|key| match key {
        "GPT_LIVE_TOKEN" => Some("configured-managed-token".to_string()),
        _ => None,
    })
    .expect("config");
    config.upstream = profile;
    config
}

struct Proxy {
    websocket: String,
    http: String,
    address: SocketAddr,
    state: AppState,
}

async fn start_proxy(mut config: Config) -> Proxy {
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind proxy");
    let address = listener.local_addr().expect("proxy address");
    config.bind = address;
    let state = AppState::new(config).expect("proxy state");
    let server_state = state.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(server_state)).await;
    });
    Proxy {
        websocket: format!("ws://{address}"),
        http: format!("http://{address}"),
        address,
        state,
    }
}

fn managed_profile(upstream: &str) -> UpstreamProfile {
    UpstreamProfile::ApiKeyManaged {
        base_url: format!("{upstream}/v1"),
        auth: BearerToken::new("managed-secret"),
    }
}

fn client_profile(upstream: &str) -> UpstreamProfile {
    UpstreamProfile::ApiKeyClient {
        base_url: format!("{upstream}/v1"),
    }
}

fn chatgpt_profile(upstream: &str) -> UpstreamProfile {
    UpstreamProfile::ChatGptBackend {
        base_url: format!("{upstream}/backend-api/codex"),
        auth: BearerToken::new("chatgpt-managed-secret"),
        account_id: None,
    }
}

async fn connect(url: &str) -> (ClientSocket, http::Response<Option<Vec<u8>>>) {
    tokio_tungstenite::connect_async(url)
        .await
        .expect("official WebSocket upgrade")
}

fn request_with_headers(url: &str, headers: &[(&str, &str)]) -> http::Request<()> {
    let mut request = url.into_client_request().expect("client request");
    for (name, value) in headers {
        request.headers_mut().append(
            http::HeaderName::from_bytes(name.as_bytes()).expect("header name"),
            value.parse().expect("header value"),
        );
    }
    request
}

fn captures(captures: &Arc<Mutex<Vec<Capture>>>) -> Vec<Capture> {
    captures.lock().expect("captures").clone()
}

fn event_inventory() -> Value {
    serde_json::from_str(include_str!("fixtures/official/realtime-events.json"))
        .expect("official event inventory")
}

fn event_frames(values: &Value, key: &str) -> Vec<String> {
    values[key]
        .as_array()
        .expect("event array")
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let kind = value.as_str().expect("event type");
            format!(
                "{{ \"future_unknown\":{{\"n\":{index}}}, \"type\":\"{kind}\", \"sentinel\":\"한글 ✅\" }}"
            )
        })
        .collect()
}

async fn close(socket: &mut ClientSocket) {
    let _ = socket.send(ClientMessage::Close(None)).await;
    while let Some(message) = socket.next().await {
        if matches!(message, Ok(ClientMessage::Close(_)) | Err(_)) {
            break;
        }
    }
}

#[test]
fn dated_event_inventory_has_the_official_snapshot_counts() {
    let inventory = event_inventory();
    assert_eq!(inventory["source_date"], "2026-07-26");
    for (key, count) in [
        ("standard_client", 11),
        ("standard_server", 46),
        ("translation_client", 3),
        ("translation_server", 7),
    ] {
        let values = inventory[key].as_array().expect("event array");
        assert_eq!(values.len(), count, "{key}");
        let unique: std::collections::HashSet<_> = values.iter().collect();
        assert_eq!(unique.len(), count, "{key} contains duplicates");
    }
}

#[tokio::test]
async fn official_routes_preserve_raw_urls_and_never_add_avas() {
    let (upstream, seen) = start_ws_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(base_config(managed_profile(&upstream))).await;
    let urls = [
        "/v1/realtime?model=gpt-realtime-2.1&z=&model_hint=a%2Bb+q",
        "/v1/realtime?call_id=rtc_A-9&z=1&z=2",
        "/v1/realtime?model=ignored-1&call_id=rtc_join&model=ignored-2",
        "/v1/realtime/translations?model=gpt-realtime-translate&lang=ko%2DKR",
    ];

    for path in urls {
        let (mut socket, _) = connect(&format!("{}{path}", proxy.websocket)).await;
        close(&mut socket).await;
    }

    let received = captures(&seen);
    assert_eq!(
        received
            .iter()
            .map(|capture| capture.uri.as_str())
            .collect::<Vec<_>>(),
        urls
    );
    assert!(received
        .iter()
        .all(|capture| !capture.uri.contains("intent=quicksilver")));
}

#[tokio::test]
async fn managed_client_and_browser_credentials_use_separate_channels() {
    let (managed_upstream, managed_seen) = start_ws_upstream(UpstreamBehavior::default()).await;
    let managed = start_proxy(base_config(managed_profile(&managed_upstream))).await;
    let (mut socket, _) = connect(&format!(
        "{}/v1/realtime?model=gpt-realtime-2.1",
        managed.websocket
    ))
    .await;
    close(&mut socket).await;
    let managed_capture = captures(&managed_seen).pop().unwrap();
    assert_eq!(
        managed_capture.headers.get("authorization").unwrap(),
        "Bearer managed-secret"
    );

    let (client_upstream, client_seen) = start_ws_upstream(UpstreamBehavior::default()).await;
    let client = start_proxy(base_config(client_profile(&client_upstream))).await;
    let request = request_with_headers(
        &format!("{}/v1/realtime?model=gpt-realtime-2.1", client.websocket),
        &[
            ("authorization", "Bearer caller-secret"),
            ("openai-beta", "realtime=v1"),
        ],
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("client-mode WebSocket");
    close(&mut socket).await;
    let client_capture = captures(&client_seen).pop().unwrap();
    assert_eq!(
        client_capture.headers.get("authorization").unwrap(),
        "Bearer caller-secret"
    );
    assert_eq!(
        client_capture.headers.get("openai-beta").unwrap(),
        "realtime=v1"
    );

    let (browser_upstream, browser_seen) = start_ws_upstream(UpstreamBehavior {
        selected_protocol: Some("realtime".to_string()),
        ..UpstreamBehavior::default()
    })
    .await;
    let browser = start_proxy(base_config(managed_profile(&browser_upstream))).await;
    let request = request_with_headers(
        &format!(
            "{}/v1/realtime?model=gpt-realtime-2.1",
            browser.websocket
        ),
        &[(
            "sec-websocket-protocol",
            "realtime, openai-insecure-api-key.ek_browser, openai-organization.org_1, openai-project.proj_1",
        )],
    );
    let (mut socket, response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("browser WebSocket");
    assert_eq!(
        response.headers().get("sec-websocket-protocol").unwrap(),
        "realtime"
    );
    assert_eq!(
        response.headers().get("x-request-id").unwrap(),
        "req-ws-safe"
    );
    assert!(!response.headers().contains_key("set-cookie"));
    close(&mut socket).await;
    let browser_capture = captures(&browser_seen).pop().unwrap();
    assert!(!browser_capture.headers.contains_key("authorization"));
    assert_eq!(
        browser_capture
            .headers
            .get("sec-websocket-protocol")
            .unwrap(),
        "realtime, openai-insecure-api-key.ek_browser, openai-organization.org_1, openai-project.proj_1"
    );
}

#[tokio::test]
async fn minted_rest_secret_drives_browser_websocket_without_managed_authorization() {
    let (upstream, seen) = start_ws_upstream(UpstreamBehavior {
        selected_protocol: Some("realtime".to_string()),
        ..UpstreamBehavior::default()
    })
    .await;
    let proxy = start_proxy(base_config(managed_profile(&upstream))).await;

    let minted: Value = reqwest::Client::new()
        .post(format!("{}/v1/realtime/client_secrets", proxy.http))
        .json(&serde_json::json!({"session":{"type":"realtime"}}))
        .send()
        .await
        .expect("mint request")
        .error_for_status()
        .expect("mint status")
        .json()
        .await
        .expect("mint body");
    let ephemeral = minted["value"].as_str().expect("ephemeral value");

    let protocol = format!("realtime, openai-insecure-api-key.{ephemeral}");
    let request = request_with_headers(
        &format!("{}/v1/realtime?model=gpt-realtime-2.1", proxy.websocket),
        &[("sec-websocket-protocol", &protocol)],
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("minted browser WebSocket");
    close(&mut socket).await;

    let received = captures(&seen);
    assert_eq!(received.len(), 2);
    assert_eq!(received[0].method, Method::POST);
    assert_eq!(received[0].uri, "/v1/realtime/client_secrets");
    assert!(!received[0].body.is_empty());
    assert!(!received[1].headers.contains_key("authorization"));
    assert_eq!(
        received[1].headers.get("sec-websocket-protocol").unwrap(),
        protocol.as_str()
    );
}

async fn expect_http_error(request: http::Request<()>) -> http::Response<Option<Vec<u8>>> {
    match tokio_tungstenite::connect_async(request).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => *response,
        Ok(_) => panic!("request unexpectedly upgraded"),
        Err(error) => panic!("expected HTTP handshake error, got {error:?}"),
    }
}

#[tokio::test]
async fn selector_protocol_private_and_slash_rejections_have_zero_upstream_contact() {
    let (upstream, seen) = start_ws_upstream(UpstreamBehavior::default()).await;
    let mut config = base_config(managed_profile(&upstream));
    config.admission_token = Some(BearerToken::new("admission-canary"));
    let proxy = start_proxy(config).await;

    let cases = [
        ("/v1/realtime", vec![], StatusCode::BAD_REQUEST),
        (
            "/v1/realtime?model=a&model=b",
            vec![],
            StatusCode::BAD_REQUEST,
        ),
        (
            "/v1/realtime?call_id=a&call_id=b",
            vec![],
            StatusCode::BAD_REQUEST,
        ),
        (
            "/v1/realtime?call_id=has%2Fslash",
            vec![],
            StatusCode::BAD_REQUEST,
        ),
        (
            "/v1/realtime/translations?model=x&call_id=rtc_x",
            vec![],
            StatusCode::BAD_REQUEST,
        ),
        ("/v1/realtime/", vec![], StatusCode::NOT_FOUND),
        ("/v1/realtime/translations/", vec![], StatusCode::NOT_FOUND),
        (
            "/v1/realtime?model=x",
            vec![(
                "sec-websocket-protocol",
                "realtime, openai-insecure-api-key.admission-canary",
            )],
            StatusCode::UNAUTHORIZED,
        ),
        (
            "/v1/realtime?model=x",
            vec![
                ("authorization", "Bearer caller"),
                (
                    "sec-websocket-protocol",
                    "realtime, openai-insecure-api-key.ek_ambiguous",
                ),
            ],
            StatusCode::BAD_REQUEST,
        ),
        (
            "/v1/realtime?model=x",
            vec![
                ("openai-alpha", "quicksilver=v1"),
                ("sec-websocket-protocol", "realtime"),
            ],
            StatusCode::BAD_REQUEST,
        ),
        (
            "/v1/live/rtc_private",
            vec![("sec-websocket-protocol", "realtime")],
            StatusCode::BAD_REQUEST,
        ),
        (
            "/v1/realtime/calls/rtc_private",
            vec![("sec-websocket-protocol", "realtime")],
            StatusCode::BAD_REQUEST,
        ),
    ];

    for (path, headers, expected) in cases {
        let request = request_with_headers(&format!("{}{path}", proxy.websocket), &headers);
        let response = expect_http_error(request).await;
        assert_eq!(response.status(), expected, "path={path}");
    }
    assert_eq!(captures(&seen).len(), 0);
}

#[tokio::test]
async fn private_browser_protocols_and_malformed_non_upgrades_are_zero_contact() {
    let (upstream, seen) = start_ws_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(base_config(chatgpt_profile(&upstream))).await;
    let browser = "realtime, openai-insecure-api-key.private-browser";

    let official = request_with_headers(
        &format!("{}/v1/realtime?model=gpt-realtime-2.1", proxy.websocket),
        &[
            ("authorization", "Bearer conflicting"),
            ("sec-websocket-protocol", browser),
        ],
    );
    let response = expect_http_error(official).await;
    let body: Value = serde_json::from_slice(response.body().as_deref().unwrap()).unwrap();
    assert_eq!(body["error"]["code"], "unsupported_realtime_capability");

    for (path, alpha) in [
        ("/v1/realtime?model=private", "quicksilver=v1"),
        ("/v1/realtime?model=private", "quicksilver=v2"),
        ("/v1/realtime?call_id=rtc_private", "quicksilver=v1"),
        ("/v1/realtime?call_id=rtc_private", "quicksilver=v2"),
    ] {
        let request = request_with_headers(
            &format!("{}{path}", proxy.websocket),
            &[("openai-alpha", alpha), ("sec-websocket-protocol", browser)],
        );
        let response = expect_http_error(request).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{alpha} {path}");
        let body: Value = serde_json::from_slice(response.body().as_deref().unwrap()).unwrap();
        assert_eq!(
            body["error"]["code"], "invalid_realtime_subprotocol",
            "{alpha} {path}"
        );
    }

    for path in ["/v1/live/rtc_private", "/v1/realtime/calls/rtc_private"] {
        let request = request_with_headers(
            &format!("{}{path}", proxy.websocket),
            &[("sec-websocket-protocol", browser)],
        );
        let response = expect_http_error(request).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
    }

    let response = reqwest::get(format!("{}/v1/realtime?model=%FF", proxy.http))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "invalid_request_error");
    assert_eq!(captures(&seen).len(), 0);
}

#[tokio::test]
async fn repeated_and_non_utf8_websocket_metadata_are_rejected_before_contact() {
    let (upstream, seen) = start_ws_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(base_config(managed_profile(&upstream))).await;
    let url = format!("{}/v1/realtime?model=gpt-realtime-2.1", proxy.websocket);

    for name in [
        "origin",
        "openai-organization",
        "openai-project",
        "openai-safety-identifier",
    ] {
        let first = if name == "origin" {
            proxy.http.as_str()
        } else {
            "first"
        };
        let second = if name == "origin" {
            proxy.http.as_str()
        } else {
            "second"
        };
        let request = request_with_headers(&url, &[(name, first), (name, second)]);
        let response = expect_http_error(request).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{name}");
    }

    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert(
        "openai-organization",
        http::HeaderValue::from_bytes(&[0xff]).unwrap(),
    );
    let result = tokio_tungstenite::connect_async(request).await;
    match result {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status(), StatusCode::BAD_REQUEST)
        }
        // Some HTTP parsers reject obs-text before routing; either outcome is
        // still zero-contact and cannot produce a successful upgrade.
        Err(_) => {}
        Ok(_) => panic!("non-UTF-8 metadata unexpectedly upgraded"),
    }
    assert_eq!(captures(&seen).len(), 0);
}

struct RawUpstream {
    base: String,
    contacts: Arc<AtomicUsize>,
    reached: oneshot::Receiver<()>,
    release: Option<oneshot::Sender<()>>,
}

struct HeldDisconnectUpstream {
    base: String,
    reached: oneshot::Receiver<()>,
    disconnected: oneshot::Receiver<()>,
}

async fn start_held_disconnect_upstream() -> HeldDisconnectUpstream {
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind held disconnect upstream");
    let address = listener.local_addr().expect("held disconnect address");
    let (reached_tx, reached_rx) = oneshot::channel();
    let (disconnected_tx, disconnected_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("held disconnect accept");
        let mut buffer = [0u8; 4096];
        let mut request = Vec::new();
        loop {
            let read = socket
                .read(&mut buffer)
                .await
                .expect("held disconnect read");
            if read == 0 {
                return;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let _ = reached_tx.send(());
        loop {
            match socket.read(&mut buffer).await {
                Ok(0) | Err(_) => {
                    let _ = disconnected_tx.send(());
                    return;
                }
                Ok(_) => {}
            }
        }
    });
    HeldDisconnectUpstream {
        base: format!("http://{address}"),
        reached: reached_rx,
        disconnected: disconnected_rx,
    }
}

async fn start_raw_upstream(response: Vec<u8>, held: bool) -> RawUpstream {
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind raw upstream");
    let address = listener.local_addr().expect("raw address");
    let contacts = Arc::new(AtomicUsize::new(0));
    let contacts_task = contacts.clone();
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("raw accept");
        contacts_task.fetch_add(1, Ordering::SeqCst);
        let mut request = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let read = socket.read(&mut buffer).await.expect("raw read");
            if read == 0 {
                return;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let _ = reached_tx.send(());
        if held {
            let _ = release_rx.await;
        }
        let _ = socket.write_all(&response).await;
        let _ = socket.shutdown().await;
    });
    RawUpstream {
        base: format!("http://{address}"),
        contacts,
        reached: reached_rx,
        release: held.then_some(release_tx),
    }
}

fn raw_http(status: u16, reason: &str, extra: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n{extra}\r\n{body}",
        body.len()
    )
    .into_bytes()
}

#[tokio::test]
async fn upstream_non_101_statuses_use_fixed_bounded_json_and_safe_headers() {
    for (status, reason) in [
        (401, "Unauthorized"),
        (403, "Forbidden"),
        (429, "Too Many Requests"),
        (302, "Found"),
        (500, "Internal Server Error"),
    ] {
        let canary = format!("upstream-body-canary-{status}");
        let raw = start_raw_upstream(
            raw_http(
                status,
                reason,
                "X-Request-Id: req-raw\r\nRetry-After: 7\r\nSet-Cookie: secret=1\r\n",
                &canary,
            ),
            false,
        )
        .await;
        let proxy = start_proxy(base_config(managed_profile(&raw.base))).await;
        let request = format!("{}/v1/realtime?model=gpt-realtime-2.1", proxy.websocket)
            .into_client_request()
            .unwrap();
        let response = expect_http_error(request).await;
        assert_eq!(response.status().as_u16(), status);
        assert_eq!(response.headers().get("x-request-id").unwrap(), "req-raw");
        assert_eq!(response.headers().get("retry-after").unwrap(), "7");
        assert!(!response.headers().contains_key("set-cookie"));
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/json"
        );
        let body = String::from_utf8(response.body().clone().unwrap_or_default()).unwrap();
        assert_eq!(
            body,
            r#"{"error":{"message":"Realtime upstream WebSocket handshake rejected","type":"server_error","code":"upstream_websocket_rejected"}}"#
        );
        assert!(!body.contains(&canary));
        raw.reached.await.expect("raw handshake");
        assert_eq!(raw.contacts.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn downstream_never_observes_101_before_the_upstream_handshake_finishes() {
    let raw = start_raw_upstream(
        raw_http(401, "Unauthorized", "", "held-upstream-canary"),
        true,
    )
    .await;
    let proxy = start_proxy(base_config(managed_profile(&raw.base))).await;
    let url = format!("{}/v1/realtime?model=gpt-realtime-2.1", proxy.websocket);
    let task = tokio::spawn(async move { tokio_tungstenite::connect_async(url).await });

    raw.reached.await.expect("upstream handshake reached");
    tokio::task::yield_now().await;
    assert!(
        !task.is_finished(),
        "downstream completed before the held upstream handshake"
    );
    raw.release.unwrap().send(()).expect("release handshake");
    match task.await.expect("join") {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED)
        }
        other => panic!("expected downstream 401, got {other:?}"),
    }
}

#[tokio::test]
async fn client_disconnect_during_held_upstream_handshake_releases_socket_and_permit() {
    let held = start_held_disconnect_upstream().await;
    let mut config = base_config(managed_profile(&held.base));
    config.limits.active_connections = 1;
    config.limits.websocket_connect_timeout = Duration::from_secs(60);
    let proxy = start_proxy(config).await;

    let mut downstream = tokio::net::TcpStream::connect(proxy.address)
        .await
        .expect("connect raw downstream");
    // Keep the RFC sample value split so generic secret scanners do not
    // mistake this synthetic WebSocket nonce for a credential.
    let websocket_nonce = ["dGhlIHNhbXBsZSBu", "b25jZQ=="].concat();
    let request = format!(
        "GET /v1/realtime?model=gpt-realtime-2.1 HTTP/1.1\r\nHost: {}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {websocket_nonce}\r\n\r\n",
        proxy.address,
    );
    downstream
        .write_all(request.as_bytes())
        .await
        .expect("write raw downstream handshake");

    held.reached.await.expect("upstream handshake held");
    assert_eq!(proxy.state.active_connections.available_permits(), 0);
    downstream
        .shutdown()
        .await
        .expect("close raw downstream connection");
    drop(downstream);

    tokio::time::timeout(Duration::from_secs(2), held.disconnected)
        .await
        .expect("proxy retained held upstream socket after client disconnect")
        .expect("held upstream disconnect signal");
    assert_eq!(proxy.state.active_connections.available_permits(), 1);
}

#[tokio::test]
async fn reset_and_malformed_handshake_fail_before_downstream_upgrade() {
    let reset = start_raw_upstream(Vec::new(), false).await;
    let reset_proxy = start_proxy(base_config(managed_profile(&reset.base))).await;
    let reset_response = expect_http_error(
        format!(
            "{}/v1/realtime?model=gpt-realtime-2.1",
            reset_proxy.websocket
        )
        .into_client_request()
        .unwrap(),
    )
    .await;
    assert_eq!(reset_response.status(), StatusCode::BAD_GATEWAY);

    let malformed = start_raw_upstream(
        b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n"
            .to_vec(),
        false,
    )
    .await;
    let malformed_proxy = start_proxy(base_config(managed_profile(&malformed.base))).await;
    let malformed_response = expect_http_error(
        format!(
            "{}/v1/realtime?model=gpt-realtime-2.1",
            malformed_proxy.websocket
        )
        .into_client_request()
        .unwrap(),
    )
    .await;
    assert_eq!(malformed_response.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn all_standard_and_translation_events_are_relayed_as_opaque_exact_text() {
    let inventory = event_inventory();

    let client_frames = event_frames(&inventory, "standard_client");
    let (client_upstream, _) = start_ws_upstream(UpstreamBehavior::default()).await;
    let client_proxy = start_proxy(base_config(managed_profile(&client_upstream))).await;
    let (mut client, _) = connect(&format!(
        "{}/v1/realtime?model=gpt-realtime-2.1",
        client_proxy.websocket
    ))
    .await;
    for frame in &client_frames {
        client
            .send(ClientMessage::Text(frame.clone().into()))
            .await
            .expect("send client event");
        match client.next().await.expect("echo").expect("frame") {
            ClientMessage::Text(echoed) => assert_eq!(echoed.as_str(), frame),
            other => panic!("expected exact text, got {other:?}"),
        }
    }
    close(&mut client).await;

    for (path, key) in [
        ("/v1/realtime?model=gpt-realtime-2.1", "standard_server"),
        (
            "/v1/realtime/translations?model=gpt-realtime-translate",
            "translation_server",
        ),
    ] {
        let expected = event_frames(&inventory, key);
        let upstream_frames = expected
            .iter()
            .cloned()
            .map(|frame| Message::Text(frame.into()))
            .collect();
        let (upstream, _) = start_ws_upstream(UpstreamBehavior {
            server_frames: upstream_frames,
            ..UpstreamBehavior::default()
        })
        .await;
        let proxy = start_proxy(base_config(managed_profile(&upstream))).await;
        let (mut client, _) = connect(&format!("{}{path}", proxy.websocket)).await;
        for frame in expected {
            match client.next().await.expect("server event").expect("frame") {
                ClientMessage::Text(received) => assert_eq!(received.as_str(), frame),
                other => panic!("expected exact text, got {other:?}"),
            }
        }
        close(&mut client).await;
    }

    let translation_client = event_frames(&inventory, "translation_client");
    let (upstream, _) = start_ws_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(base_config(managed_profile(&upstream))).await;
    let (mut client, _) = connect(&format!(
        "{}/v1/realtime/translations?model=gpt-realtime-translate",
        proxy.websocket
    ))
    .await;
    for frame in translation_client {
        client
            .send(ClientMessage::Text(frame.clone().into()))
            .await
            .expect("send translation event");
        match client.next().await.expect("echo").expect("frame") {
            ClientMessage::Text(echoed) => assert_eq!(echoed.as_str(), frame),
            other => panic!("expected exact text, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn text_binary_close_and_frame_cap_are_enforced_on_public_connections() {
    let (upstream, _) = start_ws_upstream(UpstreamBehavior::default()).await;
    let mut config = base_config(managed_profile(&upstream));
    config.limits.websocket_frame_bytes = 32;
    let proxy = start_proxy(config).await;
    let (mut client, _) = connect(&format!(
        "{}/v1/realtime?model=gpt-realtime-2.1",
        proxy.websocket
    ))
    .await;

    let text = "한글✅";
    client
        .send(ClientMessage::Text(text.into()))
        .await
        .expect("text");
    assert!(matches!(
        client.next().await.unwrap().unwrap(),
        ClientMessage::Text(value) if value.as_str() == text
    ));
    let binary = vec![0, 0xff, 0xfe, 7];
    client
        .send(ClientMessage::Binary(binary.clone().into()))
        .await
        .expect("binary");
    assert!(matches!(
        client.next().await.unwrap().unwrap(),
        ClientMessage::Binary(value) if value.as_ref() == binary.as_slice()
    ));

    client
        .send(ClientMessage::Binary(vec![9; 32].into()))
        .await
        .expect("at cap");
    assert!(matches!(
        client.next().await.unwrap().unwrap(),
        ClientMessage::Binary(value) if value.len() == 32
    ));
    client
        .send(ClientMessage::Binary(vec![9; 33].into()))
        .await
        .expect("over cap");
    while let Some(message) = client.next().await {
        match message {
            Ok(ClientMessage::Close(Some(frame))) => {
                assert_eq!(u16::from(frame.code), 1009);
                assert_eq!(frame.reason.as_str(), "frame too large");
                return;
            }
            Ok(_) => continue,
            Err(error) => panic!("expected 1009 close, got {error:?}"),
        }
    }
    panic!("over-cap frame did not close the public connection");
}

#[tokio::test]
async fn active_connection_permit_rejects_then_recovers_after_close() {
    let (upstream, _) = start_ws_upstream(UpstreamBehavior::default()).await;
    let mut config = base_config(managed_profile(&upstream));
    config.limits.active_connections = 1;
    let proxy = start_proxy(config).await;
    let url = format!("{}/v1/realtime?model=gpt-realtime-2.1", proxy.websocket);

    let (mut first, _) = connect(&url).await;
    let response = expect_http_error(url.as_str().into_client_request().unwrap()).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers().get("retry-after").unwrap(), "1");

    close(&mut first).await;
    tokio::task::yield_now().await;
    let (mut recovered, _) = connect(&url).await;
    close(&mut recovered).await;
}

#[tokio::test]
async fn a_sensitive_protocol_selected_by_upstream_is_rejected_before_local_101() {
    let selected = "openai-insecure-api-key.ek_do_not_reflect";
    let (upstream, _) = start_ws_upstream(UpstreamBehavior {
        selected_protocol: Some(selected.to_string()),
        ..UpstreamBehavior::default()
    })
    .await;
    let proxy = start_proxy(base_config(managed_profile(&upstream))).await;
    let request = request_with_headers(
        &format!("{}/v1/realtime?model=gpt-realtime-2.1", proxy.websocket),
        &[(
            "sec-websocket-protocol",
            "realtime, openai-insecure-api-key.ek_do_not_reflect",
        )],
    );
    let response = expect_http_error(request).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = String::from_utf8(response.body().clone().unwrap_or_default()).unwrap();
    assert!(!body.contains("ek_do_not_reflect"));
}

#[tokio::test]
async fn websocket_connect_timeout_is_a_504_and_releases_the_permit() {
    let held = start_raw_upstream(Vec::new(), true).await;
    let mut config = base_config(managed_profile(&held.base));
    config.limits.websocket_connect_timeout = Duration::from_millis(25);
    config.limits.active_connections = 1;
    let proxy = start_proxy(config).await;
    let request = format!("{}/v1/realtime?model=gpt-realtime-2.1", proxy.websocket)
        .into_client_request()
        .unwrap();
    let response = expect_http_error(request).await;
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    held.reached.await.expect("held handshake reached");
    assert_eq!(held.contacts.load(Ordering::SeqCst), 1);
    assert_eq!(proxy.state.active_connections.available_permits(), 1);
}
