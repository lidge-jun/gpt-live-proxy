//! Real-socket composition tests for official WebRTC call creation and GA sideband.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequest, OriginalUri, State};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use gpt_live_proxy::app::{router, AppState};
use gpt_live_proxy::config::{BearerToken, Config, UpstreamProfile};
use gpt_live_proxy::observability::FrameLogger;
use gpt_live_proxy::realtime::path::validate_call_id;
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message as ClientMessage;

const MANAGED_BEARER: &str = "managed-webrtc-bearer-canary";
const EPHEMERAL_BEARER: &str = "ephemeral-webrtc-bearer-canary";
const CLIENT_EVENT: &str = r#"{"type":"session.update","canary":"webrtc-client-event-canary"}"#;
const SERVER_EVENT: &str = r#"{"type":"session.created","canary":"webrtc-server-event-canary"}"#;
const CLOSE_REASON: &str = "webrtc-close-reason-canary";
const SAFE_TRACE_MARKER: &str = "official-webrtc-trace-capture-active";

#[derive(Clone, Debug)]
struct Capture {
    method: Method,
    uri: String,
    headers: HeaderMap,
    body: Bytes,
}

#[derive(Clone)]
struct UpstreamState {
    captures: Arc<Mutex<Vec<Capture>>>,
    location: String,
    answer: String,
}

async fn echo(mut socket: WebSocket) {
    if socket
        .send(Message::Text(SERVER_EVENT.into()))
        .await
        .is_err()
    {
        return;
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
    OriginalUri(uri): OriginalUri,
    request: axum::extract::Request,
) -> Response {
    let headers = request.headers().clone();
    let is_websocket = headers
        .get(http::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));

    if is_websocket {
        let websocket = WebSocketUpgrade::from_request(request, &())
            .await
            .expect("valid upstream WebSocket upgrade");
        state.captures.lock().expect("captures").push(Capture {
            method,
            uri: uri.to_string(),
            headers,
            body: Bytes::new(),
        });
        return websocket.on_upgrade(echo);
    }

    let body = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .expect("read upstream offer");
    state.captures.lock().expect("captures").push(Capture {
        method,
        uri: uri.to_string(),
        headers,
        body,
    });

    let mut response = (StatusCode::CREATED, state.answer.clone()).into_response();
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/sdp"),
    );
    response.headers_mut().insert(
        http::header::LOCATION,
        HeaderValue::from_str(&state.location).expect("location header"),
    );
    response.headers_mut().insert(
        "x-request-id",
        HeaderValue::from_static("req-webrtc-metadata-canary"),
    );
    response
        .headers_mut()
        .insert("openai-processing-ms", HeaderValue::from_static("19"));
    response.headers_mut().insert(
        http::header::SET_COOKIE,
        HeaderValue::from_static("must-not-cross=webrtc-cookie-canary"),
    );
    response
}

async fn start_upstream(location: &str, answer: &str) -> (String, Arc<Mutex<Vec<Capture>>>) {
    let captures = Arc::new(Mutex::new(Vec::new()));
    let state = UpstreamState {
        captures: captures.clone(),
        location: location.to_string(),
        answer: answer.to_string(),
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

struct Proxy {
    http: String,
    websocket: String,
}

async fn start_proxy(profile: UpstreamProfile) -> Proxy {
    start_proxy_with_frame_log(profile, FrameLogger::disabled()).await
}

async fn start_proxy_with_frame_log(profile: UpstreamProfile, frame_log: FrameLogger) -> Proxy {
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind proxy");
    let address = listener.local_addr().expect("proxy address");
    let mut config = Config::from_source(|key| match key {
        "GPT_LIVE_TOKEN" => Some("local-admission-canary".to_string()),
        _ => None,
    })
    .expect("test config");
    config.bind = address;
    config.upstream = profile;
    let mut state = AppState::new(config).expect("proxy state");
    state.frame_log = frame_log;
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });
    Proxy {
        http: format!("http://{address}"),
        websocket: format!("ws://{address}"),
    }
}

#[derive(Clone, Default)]
struct CapturedTrace(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CapturedTrace {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("trace capture")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl CapturedTrace {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("trace capture")).to_string()
    }
}

fn frame_log_path() -> std::path::PathBuf {
    let directory =
        std::env::temp_dir().join(format!("gpt-live-official-webrtc-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("frame log directory");
    let path = directory.join("hostile-trace.jsonl");
    let _ = std::fs::remove_file(&path);
    path
}

fn strict_call_id(location: &str) -> String {
    let without_fragment = location.split('#').next().expect("location");
    let without_query = without_fragment.split('?').next().expect("location");
    let path = if let Some((_, authority_and_path)) = without_query.split_once("://") {
        let slash = authority_and_path
            .find('/')
            .expect("absolute Location must contain a path");
        &authority_and_path[slash..]
    } else {
        without_query
    };
    let call_id = path
        .strip_prefix("/v1/realtime/calls/")
        .expect("official call Location prefix");
    assert!(
        !call_id.contains('/'),
        "Location has trailing path segments"
    );
    validate_call_id(call_id).expect("Location carries a contract-valid call ID");
    call_id.to_string()
}

fn multipart_offer() -> (Vec<u8>, String) {
    let boundary = "webrtc-exact-boundary";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"sdp\"\r\nContent-Type: application/sdp\r\n\r\nv=0\r\na=x-webrtc-multipart-sdp-canary\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"session\"\r\nContent-Type: application/json\r\n\r\n{{\"model\":\"gpt-realtime-2.1\",\"canary\":\"webrtc-session-json-canary\"}}\r\n--{boundary}--\r\n"
    );
    (
        body.into_bytes(),
        format!("multipart/form-data; boundary={boundary}"),
    )
}

async fn close(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) {
    let _ = socket.send(ClientMessage::Close(None)).await;
    while let Some(message) = socket.next().await {
        if matches!(message, Ok(ClientMessage::Close(_)) | Err(_)) {
            break;
        }
    }
}

struct OfferCase {
    name: &'static str,
    location: &'static str,
    path: &'static str,
    content_type: String,
    body: Vec<u8>,
    caller_bearer: &'static str,
    upstream_bearer: &'static str,
}

#[tokio::test]
async fn official_offer_location_and_sideband_compose_for_rtc_and_uuid_ids() {
    let (multipart_body, multipart_type) = multipart_offer();
    let cases = [
        OfferCase {
            name: "multipart-rtc",
            location: "/v1/realtime/calls/rtc_webrtc_A-9?opaque=location-canary#fragment",
            path: "/v1/realtime/calls?mode=multipart&dup=1&dup=2&plus=+&encoded=%2B",
            content_type: multipart_type,
            body: multipart_body,
            caller_bearer: "caller-multipart-canary",
            upstream_bearer: MANAGED_BEARER,
        },
        OfferCase {
            name: "raw-sdp-uuid",
            location: "https://api.openai.invalid/v1/realtime/calls/01234567-89ab-cdef-0123-456789abcdef?opaque=location-canary",
            path: "/v1/realtime/calls?mode=raw&blank=&encoded=%ED%95%9C",
            content_type: "application/sdp".to_string(),
            body: b"v=0\r\na=x-webrtc-raw-sdp-canary\r\n".to_vec(),
            caller_bearer: EPHEMERAL_BEARER,
            upstream_bearer: EPHEMERAL_BEARER,
        },
    ];

    for case in cases {
        let answer = format!("v=0\r\na=answer-{}-canary\r\n", case.name);
        let (upstream, captures) = start_upstream(case.location, &answer).await;
        let proxy = start_proxy(UpstreamProfile::ApiKeyManaged {
            base_url: format!("{upstream}/v1"),
            auth: BearerToken::new(MANAGED_BEARER),
        })
        .await;

        let response = reqwest::Client::new()
            .post(format!("{}{}", proxy.http, case.path))
            .header(http::header::CONTENT_TYPE, &case.content_type)
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {}", case.caller_bearer),
            )
            .body(case.body.clone())
            .send()
            .await
            .expect("official WebRTC offer");

        assert_eq!(response.status(), StatusCode::CREATED, "{}", case.name);
        assert_eq!(
            response.headers().get(http::header::CONTENT_TYPE).unwrap(),
            "application/sdp"
        );
        assert_eq!(
            response.headers().get(http::header::LOCATION).unwrap(),
            case.location
        );
        assert_eq!(
            response.headers().get("x-request-id").unwrap(),
            "req-webrtc-metadata-canary"
        );
        assert_eq!(
            response.headers().get("openai-processing-ms").unwrap(),
            "19"
        );
        assert!(!response.headers().contains_key(http::header::SET_COOKIE));
        assert_eq!(response.bytes().await.unwrap(), answer.as_bytes());

        let call_id = strict_call_id(case.location);
        let (mut socket, _) = tokio_tungstenite::connect_async(format!(
            "{}/v1/realtime?call_id={call_id}",
            proxy.websocket
        ))
        .await
        .expect("official GA sideband upgrade");

        let server_event = socket
            .next()
            .await
            .expect("server event")
            .expect("valid server event");
        assert_eq!(server_event.into_text().unwrap(), SERVER_EVENT);
        socket
            .send(ClientMessage::Text(CLIENT_EVENT.into()))
            .await
            .expect("send client event");
        let echoed = socket
            .next()
            .await
            .expect("echoed event")
            .expect("valid echoed event");
        assert_eq!(echoed.into_text().unwrap(), CLIENT_EVENT);
        close(&mut socket).await;

        let received = captures.lock().expect("captures").clone();
        assert_eq!(received.len(), 2, "{}", case.name);
        assert_eq!(received[0].method, Method::POST);
        assert_eq!(received[0].uri, case.path);
        assert_eq!(received[0].body.as_ref(), case.body.as_slice());
        assert_eq!(
            received[0].headers.get(http::header::CONTENT_TYPE).unwrap(),
            case.content_type.as_str()
        );
        assert_eq!(
            received[0]
                .headers
                .get(http::header::AUTHORIZATION)
                .unwrap(),
            format!("Bearer {}", case.upstream_bearer).as_str()
        );
        assert_eq!(received[1].method, Method::GET);
        assert_eq!(received[1].uri, format!("/v1/realtime?call_id={call_id}"));
        assert!(!received[1].uri.contains("intent=quicksilver"));
        assert_eq!(
            received[1]
                .headers
                .get(http::header::AUTHORIZATION)
                .unwrap(),
            format!("Bearer {MANAGED_BEARER}").as_str()
        );
        assert!(received[1].body.is_empty());
    }
}

#[tokio::test]
async fn chatgpt_profile_rejects_official_ga_before_upstream_contact() {
    let (upstream, captures) = start_upstream(
        "/v1/realtime/calls/rtc_must-not-appear",
        "v=0\r\na=must-not-appear",
    )
    .await;
    let proxy = start_proxy(UpstreamProfile::ChatGptBackend {
        base_url: format!("{upstream}/backend-api/codex"),
        auth: BearerToken::new("chatgpt-official-ga-bearer-canary"),
        account_id: None,
    })
    .await;
    let (body, content_type) = multipart_offer();

    let response = reqwest::Client::new()
        .post(format!("{}/v1/realtime/calls", proxy.http))
        .header(http::header::CONTENT_TYPE, content_type)
        .header(
            http::header::AUTHORIZATION,
            "Bearer caller-must-not-cross-canary",
        )
        .body(body)
        .send()
        .await
        .expect("unsupported official GA request");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: serde_json::Value = response.json().await.expect("JSON error");
    assert_eq!(error["error"]["code"], "unsupported_realtime_capability");
    assert!(captures.lock().expect("captures").is_empty());
}

#[tokio::test]
async fn hostile_trace_and_frame_log_never_capture_official_webrtc_secrets_or_payloads() {
    let trace = CapturedTrace::default();
    let sink = trace.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || sink.clone())
        .with_ansi(false)
        // Mirrors the independent production target guard: a hostile user
        // directive may enable every application target, but dependency
        // protocol logs that render raw headers and frames remain disabled.
        .with_env_filter("trace,tungstenite=off,tokio_tungstenite=off")
        .finish();
    let frame_path = frame_log_path();
    let location = "/v1/realtime/calls/rtc_webrtc_trace_location_canary";
    let answer = "v=0\r\na=webrtc-answer-sdp-canary\r\n";
    let (body, content_type) = multipart_offer();

    {
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::trace!(target: "gpt_live_proxy::test", "{SAFE_TRACE_MARKER}");

        let (upstream, _) = start_upstream(location, answer).await;
        let proxy = start_proxy_with_frame_log(
            UpstreamProfile::ApiKeyManaged {
                base_url: format!("{upstream}/v1"),
                auth: BearerToken::new(MANAGED_BEARER),
            },
            FrameLogger::new(&frame_path),
        )
        .await;

        let response = reqwest::Client::new()
            .post(format!("{}/v1/realtime/calls", proxy.http))
            .header(http::header::CONTENT_TYPE, content_type)
            .header(
                http::header::AUTHORIZATION,
                "Bearer caller-webrtc-trace-bearer-canary",
            )
            .body(body)
            .send()
            .await
            .expect("official WebRTC offer under hostile TRACE");
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response.headers().get(http::header::LOCATION).unwrap(),
            location
        );
        assert_eq!(response.bytes().await.unwrap(), answer.as_bytes());

        let call_id = strict_call_id(location);
        let (mut socket, _) = tokio_tungstenite::connect_async(format!(
            "{}/v1/realtime?call_id={call_id}",
            proxy.websocket
        ))
        .await
        .expect("official GA sideband under hostile TRACE");
        assert_eq!(
            socket
                .next()
                .await
                .expect("server event")
                .expect("valid server event")
                .into_text()
                .unwrap(),
            SERVER_EVENT
        );
        socket
            .send(ClientMessage::Text(CLIENT_EVENT.into()))
            .await
            .expect("send client event");
        assert_eq!(
            socket
                .next()
                .await
                .expect("echoed event")
                .expect("valid echoed event")
                .into_text()
                .unwrap(),
            CLIENT_EVENT
        );
        socket
            .send(ClientMessage::Close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: CLOSE_REASON.into(),
            })))
            .await
            .expect("send close canary");
        while let Some(message) = socket.next().await {
            if matches!(message, Ok(ClientMessage::Close(_)) | Err(_)) {
                break;
            }
        }
    }

    let trace_text = trace.text();
    assert!(
        trace_text.contains(SAFE_TRACE_MARKER),
        "TRACE capture was not active: {trace_text}"
    );
    let frame_text = std::fs::read_to_string(&frame_path).expect("real frame log");
    assert!(
        frame_text.contains("\"kind\":\"text\"")
            && frame_text.contains("\"dir\":\"c2u\"")
            && frame_text.contains("\"dir\":\"u2c\""),
        "frame capture was not active: {frame_text}"
    );

    for forbidden in [
        "x-webrtc-multipart-sdp-canary",
        "webrtc-session-json-canary",
        "webrtc-answer-sdp-canary",
        MANAGED_BEARER,
        "caller-webrtc-trace-bearer-canary",
        "rtc_webrtc_trace_location_canary",
        "webrtc-client-event-canary",
        "webrtc-server-event-canary",
        CLOSE_REASON,
    ] {
        assert!(
            !trace_text.contains(forbidden),
            "{forbidden} leaked to TRACE: {trace_text}"
        );
        assert!(
            !frame_text.contains(forbidden),
            "{forbidden} leaked to frame log: {frame_text}"
        );
    }

    let _ = std::fs::remove_file(frame_path);
}
