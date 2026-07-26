//! Real-socket conformance for the centralized Realtime capability policy.

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
use gpt_live_proxy::wire::MULTIPART_BOUNDARY;
use http::{Method, StatusCode};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as ClientMessage;

type ClientSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone, Default)]
struct Contacts {
    http: Arc<AtomicUsize>,
    websocket: Arc<AtomicUsize>,
    paths: Arc<Mutex<Vec<String>>>,
}

impl Contacts {
    fn total(&self) -> usize {
        self.http.load(Ordering::SeqCst) + self.websocket.load(Ordering::SeqCst)
    }
}

async fn echo(mut socket: WebSocket) {
    while let Some(message) = socket.next().await {
        match message {
            Ok(Message::Close(frame)) => {
                let _ = socket.send(Message::Close(frame)).await;
                return;
            }
            Ok(message) => {
                if socket.send(message).await.is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

async fn mock_upstream(
    State(contacts): State<Contacts>,
    method: Method,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    request: axum::extract::Request,
) -> Response {
    let is_websocket = request
        .headers()
        .get(http::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    contacts
        .paths
        .lock()
        .expect("contact paths")
        .push(format!("{method} {uri}"));

    if is_websocket {
        contacts.websocket.fetch_add(1, Ordering::SeqCst);
        let upgrade = match WebSocketUpgrade::from_request(request, &()).await {
            Ok(upgrade) => upgrade,
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        };
        return upgrade.on_upgrade(echo);
    }

    contacts.http.fetch_add(1, Ordering::SeqCst);
    let _ = axum::body::to_bytes(request.into_body(), 2 * 1024 * 1024).await;
    (
        StatusCode::CREATED,
        [
            (http::header::CONTENT_TYPE, "application/sdp"),
            (http::header::LOCATION, "/v1/realtime/calls/rtc_capability"),
        ],
        Bytes::from_static(b"v=0\r\na=answer"),
    )
        .into_response()
}

async fn start_upstream() -> (String, Contacts) {
    let contacts = Contacts::default();
    let app = Router::new()
        .fallback(any(mock_upstream))
        .with_state(contacts.clone());
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind mock upstream");
    let address = listener.local_addr().expect("mock upstream address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), contacts)
}

struct Proxy {
    http: String,
    websocket: String,
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
    let served = state.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(served)).await;
    });
    Proxy {
        http: format!("http://{address}"),
        websocket: format!("ws://{address}"),
        address,
        state,
    }
}

fn base_config(profile: UpstreamProfile) -> Config {
    let mut config = Config::from_source(|key| match key {
        "GPT_LIVE_TOKEN" => Some("configured-token".to_string()),
        _ => None,
    })
    .expect("test config");
    config.upstream = profile;
    config
}

fn chatgpt_profile(upstream: &str) -> UpstreamProfile {
    UpstreamProfile::ChatGptBackend {
        base_url: format!("{upstream}/backend-api/codex"),
        auth: BearerToken::new("chatgpt-managed-token"),
        account_id: None,
    }
}

fn managed_profile(upstream: &str) -> UpstreamProfile {
    UpstreamProfile::ApiKeyManaged {
        base_url: format!("{upstream}/v1"),
        auth: BearerToken::new("api-managed-token"),
    }
}

fn client_profile(upstream: &str) -> UpstreamProfile {
    UpstreamProfile::ApiKeyClient {
        base_url: format!("{upstream}/v1"),
    }
}

fn multipart(session: Option<&str>) -> (Vec<u8>, String) {
    let mut body = format!(
        "--{MULTIPART_BOUNDARY}\r\nContent-Disposition: form-data; name=\"sdp\"\r\nContent-Type: application/sdp\r\n\r\nv=0\r\na=offer\r\n"
    )
    .into_bytes();
    if let Some(session) = session {
        body.extend_from_slice(
            format!(
                "--{MULTIPART_BOUNDARY}\r\nContent-Disposition: form-data; name=\"session\"\r\nContent-Type: application/json\r\n\r\n{session}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}--\r\n").as_bytes());
    (
        body,
        format!("multipart/form-data; boundary={MULTIPART_BOUNDARY}"),
    )
}

fn assert_capability_error(
    status: StatusCode,
    body: &Value,
    capability: &str,
    configured_profile: &str,
    required_profiles: &[&str],
) {
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["code"], "unsupported_realtime_capability");
    assert_eq!(body["error"]["source"], "gpt-live-proxy");
    assert_eq!(body["error"]["param"], "upstream_profile");
    assert_eq!(body["error"]["capability"], capability);
    assert_eq!(body["error"]["configured_profile"], configured_profile);
    assert_eq!(body["error"]["required_profiles"], json!(required_profiles));
}

async fn websocket_error(request: http::Request<()>) -> (StatusCode, Value) {
    match tokio_tungstenite::connect_async(request).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            let status = response.status();
            let bytes = response
                .body()
                .as_deref()
                .expect("WebSocket error response body");
            (
                status,
                serde_json::from_slice(bytes).expect("WebSocket JSON error"),
            )
        }
        Ok(_) => panic!("unsupported WebSocket unexpectedly upgraded"),
        Err(error) => panic!("expected HTTP handshake error, got {error:?}"),
    }
}

fn websocket_request(url: &str, headers: &[(&str, &str)]) -> http::Request<()> {
    let mut request = url.into_client_request().expect("WebSocket request");
    for (name, value) in headers {
        request.headers_mut().append(
            http::HeaderName::from_bytes(name.as_bytes()).expect("header name"),
            value.parse().expect("header value"),
        );
    }
    request
}

async fn connect(request: http::Request<()>) -> ClientSocket {
    tokio_tungstenite::connect_async(request)
        .await
        .expect("WebSocket upgrade")
        .0
}

async fn assert_echo(socket: &mut ClientSocket, text: &str, binary: &[u8]) {
    socket
        .send(ClientMessage::Text(text.to_string().into()))
        .await
        .expect("send text");
    assert_eq!(
        socket.next().await.expect("text echo").expect("text frame"),
        ClientMessage::Text(text.to_string().into())
    );
    socket
        .send(ClientMessage::Binary(binary.to_vec().into()))
        .await
        .expect("send binary");
    assert_eq!(
        socket
            .next()
            .await
            .expect("binary echo")
            .expect("binary frame"),
        ClientMessage::Binary(binary.to_vec().into())
    );
}

#[tokio::test]
async fn chatgpt_rejects_all_official_rest_and_websocket_capabilities_zero_contact() {
    let (upstream, contacts) = start_upstream().await;
    let proxy = start_proxy(base_config(chatgpt_profile(&upstream))).await;
    let (offer, multipart_type) = multipart(None);
    let rest_rows = [
        (
            "/v1/realtime/calls",
            Some(multipart_type.as_str()),
            offer.as_slice(),
            "official_webrtc_call_create",
        ),
        (
            "/v1/realtime/calls/rtc_a/accept",
            Some("application/json"),
            b"{}".as_slice(),
            "official_call_accept",
        ),
        (
            "/v1/realtime/calls/rtc_a/reject",
            Some("application/json"),
            b"{}".as_slice(),
            "official_call_reject",
        ),
        (
            "/v1/realtime/calls/rtc_a/refer",
            Some("application/json"),
            b"{}".as_slice(),
            "official_call_refer",
        ),
        (
            "/v1/realtime/calls/rtc_a/hangup",
            None,
            b"".as_slice(),
            "official_call_hangup",
        ),
        (
            "/v1/realtime/client_secrets",
            Some("application/json"),
            b"{}".as_slice(),
            "official_realtime_client_secret",
        ),
        (
            "/v1/realtime/sessions",
            Some("application/json"),
            b"{}".as_slice(),
            "official_legacy_session_token",
        ),
        (
            "/v1/realtime/transcription_sessions",
            Some("application/json"),
            b"{}".as_slice(),
            "official_transcription_session_token",
        ),
        (
            "/v1/realtime/translations/client_secrets",
            Some("application/json"),
            b"{}".as_slice(),
            "official_translation_client_secret",
        ),
        (
            "/v1/realtime/translations/calls",
            Some("application/sdp"),
            b"v=0".as_slice(),
            "official_translation_webrtc_call",
        ),
    ];
    let client = reqwest::Client::new();
    for (path, content_type, body, capability) in rest_rows {
        let mut request = client
            .post(format!("{}{path}", proxy.http))
            .body(body.to_vec());
        if let Some(content_type) = content_type {
            request = request.header(http::header::CONTENT_TYPE, content_type);
        }
        let response = request.send().await.expect("official REST rejection");
        let status = response.status();
        let body: Value = response.json().await.expect("official REST JSON error");
        assert_capability_error(
            status,
            &body,
            capability,
            "chatgpt",
            &["apikey_managed", "apikey_client"],
        );
    }

    for (path, capability) in [
        (
            "/v1/realtime?model=gpt-realtime-2.1",
            "official_standalone_websocket",
        ),
        (
            "/v1/realtime?call_id=rtc_existing",
            "official_existing_call_websocket",
        ),
        (
            "/v1/realtime/translations?model=gpt-realtime-translate",
            "official_translation_websocket",
        ),
    ] {
        let request = websocket_request(&format!("{}{path}", proxy.websocket), &[]);
        let (status, body) = websocket_error(request).await;
        assert_capability_error(
            status,
            &body,
            capability,
            "chatgpt",
            &["apikey_managed", "apikey_client"],
        );
    }
    assert_eq!(
        contacts.total(),
        0,
        "unsupported official traffic reached upstream"
    );
}

#[tokio::test]
async fn chatgpt_private_call_create_and_sideband_work_but_standalone_is_rejected() {
    let (upstream, contacts) = start_upstream().await;
    // A direct API-shaped ChatGPT base keeps the source-proven sideband host
    // configurable for this real-socket test. Production backend-shaped
    // profiles deliberately join the fixed public API host instead.
    let proxy = start_proxy(base_config(UpstreamProfile::ChatGptBackend {
        base_url: upstream.clone(),
        auth: BearerToken::new("chatgpt-managed-token"),
        account_id: None,
    }))
    .await;
    let client = reqwest::Client::new();

    for (path, alpha, session) in [
        (
            "/v1/realtime/calls",
            "quicksilver=v1",
            r#"{"type":"quicksilver"}"#,
        ),
        (
            "/v1/live",
            "quicksilver=v2",
            r#"{"delegation":{"type":"client"}}"#,
        ),
    ] {
        let (body, content_type) = multipart(Some(session));
        let response = client
            .post(format!("{}{path}", proxy.http))
            .header("openai-alpha", alpha)
            .header(http::header::CONTENT_TYPE, content_type)
            .body(body)
            .send()
            .await
            .expect("private call-create");
        assert_eq!(response.status(), StatusCode::CREATED, "{path}");
    }

    for (path, alpha) in [
        ("/v1/realtime/calls/rtc_v1", "quicksilver=v1"),
        ("/v1/live/rtc_v2", "quicksilver=v2"),
    ] {
        let request = websocket_request(
            &format!("{}{path}", proxy.websocket),
            &[("openai-alpha", alpha)],
        );
        let mut socket = connect(request).await;
        assert_echo(
            &mut socket,
            r#"{"type":"future.private.event","opaque":{"한글":true}}"#,
            &[0, 0xff, 0x80, 7],
        )
        .await;
        let _ = socket.close(None).await;
    }

    let before_rejections = contacts.total();
    for (alpha, capability) in [
        ("quicksilver=v1", "private_v1_standalone_websocket"),
        ("quicksilver=v2", "private_frameless_standalone_websocket"),
    ] {
        let request = websocket_request(
            &format!("{}/v1/realtime?model=private", proxy.websocket),
            &[("openai-alpha", alpha)],
        );
        let (status, body) = websocket_error(request).await;
        assert_capability_error(status, &body, capability, "chatgpt", &["apikey_managed"]);
    }
    assert_eq!(contacts.total(), before_rejections);
    assert_eq!(contacts.http.load(Ordering::SeqCst), 2);
    assert_eq!(contacts.websocket.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn api_key_managed_native_and_adapted_paths_preserve_opaque_frames() {
    let (upstream, contacts) = start_upstream().await;
    let proxy = start_proxy(base_config(managed_profile(&upstream))).await;

    let native = reqwest::Client::new()
        .post(format!("{}/v1/realtime/sessions", proxy.http))
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(r#"{"model":"gpt-realtime-2.1"}"#)
        .send()
        .await
        .expect("native official REST");
    assert_eq!(native.status(), StatusCode::CREATED);

    let official = websocket_request(
        &format!("{}/v1/realtime?model=gpt-realtime-2.1", proxy.websocket),
        &[],
    );
    let mut official = connect(official).await;
    assert_echo(
        &mut official,
        r#"{"type":"future.ga.event","unknown":[1,2,3]}"#,
        &[0xde, 0xad, 0xbe, 0xef],
    )
    .await;
    let _ = official.close(None).await;

    let (body, content_type) = multipart(Some(r#"{"type":"quicksilver"}"#));
    let adapted = reqwest::Client::new()
        .post(format!("{}/v1/realtime/calls", proxy.http))
        .header("openai-alpha", "quicksilver=v1")
        .header(http::header::CONTENT_TYPE, content_type)
        .body(body)
        .send()
        .await
        .expect("adapted private call-create");
    assert_eq!(adapted.status(), StatusCode::CREATED);

    assert_eq!(contacts.http.load(Ordering::SeqCst), 2);
    assert_eq!(contacts.websocket.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn api_key_client_private_requests_have_exact_managed_profile_error() {
    let (upstream, contacts) = start_upstream().await;
    let proxy = start_proxy(base_config(client_profile(&upstream))).await;
    let (body, content_type) = multipart(Some(r#"{"type":"quicksilver"}"#));
    let response = reqwest::Client::new()
        .post(format!("{}/v1/realtime/calls", proxy.http))
        .header("authorization", "Bearer caller-token")
        .header("openai-alpha", "quicksilver=v1")
        .header(http::header::CONTENT_TYPE, content_type)
        .body(body)
        .send()
        .await
        .expect("private client rejection");
    let status = response.status();
    let body: Value = response.json().await.expect("private client JSON error");
    assert_capability_error(
        status,
        &body,
        "private_v1_call_create",
        "apikey_client",
        &["apikey_managed", "chatgpt"],
    );
    assert_eq!(contacts.total(), 0);
}

#[tokio::test]
async fn unsupported_pre_body_request_does_not_read_body_or_take_a_permit() {
    let (upstream, contacts) = start_upstream().await;
    let mut config = base_config(chatgpt_profile(&upstream));
    config.limits.active_requests = 1;
    config.limits.request_read_timeout = Duration::from_secs(30);
    let proxy = start_proxy(config).await;

    let mut stream = tokio::net::TcpStream::connect(proxy.address)
        .await
        .expect("raw client connect");
    let request = format!(
        "POST /v1/realtime/sessions HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: 999999\r\nConnection: close\r\n\r\n",
        proxy.address
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write stalled headers");
    let mut response = vec![0_u8; 16 * 1024];
    let read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut response))
        .await
        .expect("capability rejection waited for body")
        .expect("read capability rejection");
    let response = String::from_utf8_lossy(&response[..read]);
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    assert!(
        response.contains("unsupported_realtime_capability"),
        "{response}"
    );
    assert_eq!(proxy.state.active_requests.available_permits(), 1);
    assert_eq!(contacts.total(), 0);
    drop(stream);

    let (body, content_type) = multipart(Some(r#"{"delegation":{"type":"client"}}"#));
    let supported = reqwest::Client::new()
        .post(format!("{}/v1/live", proxy.http))
        .header("openai-alpha", "quicksilver=v2")
        .header(http::header::CONTENT_TYPE, content_type)
        .body(body)
        .send()
        .await
        .expect("supported probe after stalled rejection");
    assert_eq!(supported.status(), StatusCode::CREATED);
    assert_eq!(contacts.http.load(Ordering::SeqCst), 1);
}
