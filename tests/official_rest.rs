//! Real-socket conformance for the official Realtime REST relay (docs/100).

mod support;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use axum::body::Bytes;
use gpt_live_proxy::app::{router, AppState};
use gpt_live_proxy::config::{BearerToken, Config, UpstreamProfile};
use gpt_live_proxy::relay::http::{begin_exchange, ExchangeTerminal};
use gpt_live_proxy::wire::MULTIPART_BOUNDARY;
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use support::{
    start_upstream, start_upstream_with_drop_signal, start_upstream_with_response_gate,
    UpstreamBehavior,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct TestProxy {
    base: String,
    state: AppState,
}

async fn start_proxy(mut config: Config, remote_admission: bool) -> TestProxy {
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().expect("proxy address");
    config.bind = if remote_admission {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), addr.port())
    } else {
        addr
    };
    let state = AppState::new(config).expect("proxy state");
    let served = state.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(served)).await;
    });
    TestProxy {
        base: format!("http://{addr}"),
        state,
    }
}

fn managed_config(base_url: String) -> Config {
    let mut config = Config::from_source(|key| match key {
        "GPT_LIVE_TOKEN" => Some("configured-secret".to_string()),
        _ => None,
    })
    .expect("managed config");
    config.upstream = UpstreamProfile::ApiKeyManaged {
        base_url,
        auth: BearerToken::new("configured-secret"),
    };
    config
}

fn client_config(base_url: String) -> Config {
    let mut config = Config::from_source(|key| match key {
        "GPT_LIVE_UPSTREAM_MODE" => Some("apikey".to_string()),
        "GPT_LIVE_CREDENTIAL_MODE" => Some("client".to_string()),
        _ => None,
    })
    .expect("client config");
    config.upstream = UpstreamProfile::ApiKeyClient { base_url };
    config
}

fn multipart_body() -> (Vec<u8>, String) {
    let body = format!(
        "--{MULTIPART_BOUNDARY}\r\nContent-Disposition: form-data; name=\"sdp\"\r\nContent-Type: application/sdp\r\n\r\nv=0\r\na=offer\r\n--{MULTIPART_BOUNDARY}--\r\n"
    )
    .into_bytes();
    (
        body,
        format!("multipart/form-data; boundary={MULTIPART_BOUNDARY}"),
    )
}

async fn error_fields(response: reqwest::Response) -> (StatusCode, String, String) {
    let status = response.status();
    let value: serde_json::Value = response.json().await.expect("JSON error");
    (
        status,
        value["error"]["message"].as_str().unwrap().to_string(),
        value["error"]["code"].as_str().unwrap().to_string(),
    )
}

async fn wait_for_permits(state: &AppState, expected: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if state.active_requests.available_permits() == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("active request permits never reached {expected}"));
}

fn proxy_addr(proxy: &TestProxy) -> SocketAddr {
    proxy.base.strip_prefix("http://").unwrap().parse().unwrap()
}

async fn raw_request(proxy: &TestProxy, request: &[u8]) -> Vec<u8> {
    let mut stream = tokio::net::TcpStream::connect(proxy_addr(proxy))
        .await
        .expect("connect raw client");
    stream.write_all(request).await.expect("write raw request");
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
        .await
        .expect("raw response timeout")
        .expect("read raw response");
    response
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OracleProvenance {
    OpenApiSnapshot,
    GuideDerivedTranslationCall,
}

struct RestRow {
    path: String,
    content_type: Option<String>,
    body: Vec<u8>,
    expected_bearer: &'static str,
    provenance: OracleProvenance,
}

#[tokio::test]
async fn all_ten_routes_preserve_request_response_and_provenance() {
    let binary = Bytes::from_static(&[0x00, 0xff, 0x80, b'O', b'K']);
    let (upstream, captures) = start_upstream(UpstreamBehavior {
        status: StatusCode::IM_A_TEAPOT,
        raw_body: Some(binary.clone()),
        extra_headers: vec![
            ("content-type", "application/octet-stream"),
            ("set-cookie", "must=drop"),
            ("cache-control", "private"),
            ("x-request-id", "req-official"),
            ("retry-after", "7"),
            ("openai-processing-ms", "12"),
            ("openai-version", "2026-07-26"),
            ("x-ratelimit-remaining-requests", "3"),
        ],
        ..Default::default()
    })
    .await;
    let proxy = start_proxy(managed_config(format!("{upstream}/v1/")), false).await;
    let (multipart, multipart_type) = multipart_body();
    let rows = vec![
        RestRow {
            path: "/v1/realtime/calls?dup=1&blank=&plus=+&encoded=%2B&utf8=%ED%95%9C&dup=2".into(),
            content_type: Some(multipart_type),
            body: multipart,
            expected_bearer: "Bearer configured-secret",
            provenance: OracleProvenance::OpenApiSnapshot,
        },
        RestRow {
            path: "/v1/realtime/calls/%72tc_A-1/accept".into(),
            content_type: Some("application/json".into()),
            body: br#"{"type":"realtime"}"#.to_vec(),
            expected_bearer: "Bearer configured-secret",
            provenance: OracleProvenance::OpenApiSnapshot,
        },
        RestRow {
            path: "/v1/realtime/calls/rtc_A-1/reject".into(),
            content_type: Some("application/json".into()),
            body: br#"{"status_code":486}"#.to_vec(),
            expected_bearer: "Bearer configured-secret",
            provenance: OracleProvenance::OpenApiSnapshot,
        },
        RestRow {
            path: "/v1/realtime/calls/rtc_A-1/refer".into(),
            content_type: Some("application/json".into()),
            body: br#"{"target_uri":"tel:+12025550123"}"#.to_vec(),
            expected_bearer: "Bearer configured-secret",
            provenance: OracleProvenance::OpenApiSnapshot,
        },
        RestRow {
            path: "/v1/realtime/calls/rtc_A-1/hangup".into(),
            content_type: None,
            body: Vec::new(),
            expected_bearer: "Bearer configured-secret",
            provenance: OracleProvenance::OpenApiSnapshot,
        },
        RestRow {
            path: "/v1/realtime/client_secrets".into(),
            content_type: Some("application/json".into()),
            body: br#"{"expires_after":{"anchor":"created_at","seconds":60}}"#.to_vec(),
            expected_bearer: "Bearer configured-secret",
            provenance: OracleProvenance::OpenApiSnapshot,
        },
        RestRow {
            path: "/v1/realtime/sessions".into(),
            content_type: Some("application/json".into()),
            body: br#"{"model":"gpt-realtime"}"#.to_vec(),
            expected_bearer: "Bearer configured-secret",
            provenance: OracleProvenance::OpenApiSnapshot,
        },
        RestRow {
            path: "/v1/realtime/transcription_sessions".into(),
            content_type: Some("application/json".into()),
            body: br#"{"type":"transcription"}"#.to_vec(),
            expected_bearer: "Bearer configured-secret",
            provenance: OracleProvenance::OpenApiSnapshot,
        },
        RestRow {
            path: "/v1/realtime/translations/client_secrets".into(),
            content_type: Some("application/json".into()),
            body: br#"{"type":"translation"}"#.to_vec(),
            expected_bearer: "Bearer configured-secret",
            provenance: OracleProvenance::OpenApiSnapshot,
        },
        RestRow {
            path: "/v1/realtime/translations/calls".into(),
            content_type: Some("application/sdp".into()),
            body: b"v=0\r\na=translation".to_vec(),
            expected_bearer: "Bearer translation-ephemeral",
            provenance: OracleProvenance::GuideDerivedTranslationCall,
        },
    ];

    let client = reqwest::Client::new();
    for row in &rows {
        let caller_bearer = if row.provenance == OracleProvenance::GuideDerivedTranslationCall {
            "Bearer translation-ephemeral"
        } else {
            "Bearer caller-canary"
        };
        let mut request = client
            .post(format!("{}{}", proxy.base, row.path))
            .header("authorization", caller_bearer)
            .header("openai-organization", "org_1")
            .header("cookie", "must-not-cross")
            .header("x-gpt-live-api-key", "admission-canary")
            .body(row.body.clone());
        if let Some(content_type) = &row.content_type {
            request = request.header("content-type", content_type);
        }
        let response = request.send().await.expect("official request");
        assert_eq!(
            response.status(),
            StatusCode::IM_A_TEAPOT,
            "path={}",
            row.path
        );
        for (name, value) in [
            ("content-type", "application/octet-stream"),
            ("location", "/v1/realtime/calls/rtc_test_call"),
            ("x-request-id", "req-official"),
            ("retry-after", "7"),
            ("openai-processing-ms", "12"),
            ("openai-version", "2026-07-26"),
            ("x-ratelimit-remaining-requests", "3"),
        ] {
            assert_eq!(
                response.headers().get(name).unwrap(),
                value,
                "path={}",
                row.path
            );
        }
        assert!(response.headers().get("set-cookie").is_none());
        assert!(response.headers().get("cache-control").is_none());
        assert_eq!(response.bytes().await.unwrap(), binary, "path={}", row.path);
    }

    let captured = captures.all();
    assert_eq!(captured.len(), rows.len());
    for (request, row) in captured.iter().zip(&rows) {
        assert_eq!(request.method, Method::POST);
        assert_eq!(request.uri, row.path);
        assert_eq!(request.body.as_ref(), row.body.as_slice());
        assert_eq!(
            request.headers.get("authorization").unwrap(),
            row.expected_bearer
        );
        assert_eq!(request.headers.get("openai-organization").unwrap(), "org_1");
        assert!(request.headers.get("cookie").is_none());
        assert!(request.headers.get("x-gpt-live-api-key").is_none());
    }
    assert_eq!(
        rows.iter()
            .filter(|row| row.provenance == OracleProvenance::GuideDerivedTranslationCall)
            .count(),
        1,
        "the translation-call oracle must remain explicitly guide-derived"
    );
}

#[tokio::test]
async fn managed_client_and_ephemeral_credentials_activate_exactly() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let managed = start_proxy(managed_config(format!("{upstream}/v1")), false).await;
    let (multipart, multipart_type) = multipart_body();
    reqwest::Client::new()
        .post(format!("{}/v1/realtime/calls", managed.base))
        .header("content-type", multipart_type)
        .header("authorization", "Bearer ignored-caller")
        .body(multipart)
        .send()
        .await
        .unwrap();
    reqwest::Client::new()
        .post(format!("{}/v1/realtime/calls", managed.base))
        .header("content-type", "application/sdp")
        .header("authorization", "Bearer voice-ephemeral")
        .body("v=0")
        .send()
        .await
        .unwrap();

    let client_proxy = start_proxy(client_config(format!("{upstream}/v1")), false).await;
    let client_rows = [
        (
            "/v1/realtime/calls",
            Some("multipart/form-data; boundary=x"),
            "Bearer client-multipart",
            "--x--\r\n",
        ),
        (
            "/v1/realtime/calls",
            Some("application/sdp"),
            "Bearer voice-ephemeral",
            "v=0\r\na=offer",
        ),
        (
            "/v1/realtime/calls/rtc_client/accept",
            Some("application/json"),
            "Bearer client-accept",
            r#"{"type":"realtime"}"#,
        ),
        (
            "/v1/realtime/calls/rtc_client/reject",
            Some("application/json"),
            "Bearer client-reject",
            r#"{"status_code":486}"#,
        ),
        (
            "/v1/realtime/calls/rtc_client/refer",
            Some("application/json"),
            "Bearer client-refer",
            r#"{"target_uri":"tel:+12025550123"}"#,
        ),
        (
            "/v1/realtime/calls/rtc_client/hangup",
            None,
            "Bearer client-hangup",
            "",
        ),
        (
            "/v1/realtime/client_secrets",
            Some("application/json"),
            "Bearer client-secret",
            "{}",
        ),
        (
            "/v1/realtime/sessions",
            Some("application/json"),
            "Bearer client-session",
            r#"{"model":"gpt-realtime-2.1"}"#,
        ),
        (
            "/v1/realtime/transcription_sessions",
            Some("application/json"),
            "Bearer client-transcription",
            r#"{"type":"transcription"}"#,
        ),
        (
            "/v1/realtime/translations/client_secrets",
            Some("application/json"),
            "Bearer client-translation-secret",
            r#"{"type":"translation"}"#,
        ),
        (
            "/v1/realtime/translations/calls",
            Some("application/sdp"),
            "Bearer translation-ephemeral",
            "v=0",
        ),
    ];
    for (path, content_type, bearer, body) in client_rows {
        let mut request = reqwest::Client::new()
            .post(format!("{}{path}", client_proxy.base))
            .header("authorization", bearer)
            .header("x-gpt-live-api-key", "client-admission-canary")
            .body(body);
        if let Some(content_type) = content_type {
            request = request.header("content-type", content_type);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED, "path={path}");
    }

    let captured = captures.all();
    let auth: Vec<_> = captured
        .iter()
        .map(|request| {
            request
                .headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap()
        })
        .collect();
    assert_eq!(
        auth,
        [
            "Bearer configured-secret",
            "Bearer voice-ephemeral",
            "Bearer client-multipart",
            "Bearer voice-ephemeral",
            "Bearer client-accept",
            "Bearer client-reject",
            "Bearer client-refer",
            "Bearer client-hangup",
            "Bearer client-secret",
            "Bearer client-session",
            "Bearer client-transcription",
            "Bearer client-translation-secret",
            "Bearer translation-ephemeral",
        ]
    );
    for (request, (path, _, _, body)) in captured.iter().skip(2).zip(client_rows) {
        assert_eq!(request.uri, path);
        assert_eq!(request.body.as_ref(), body.as_bytes(), "path={path}");
        assert!(request.headers.get("x-gpt-live-api-key").is_none());
    }
}

#[tokio::test]
async fn route_method_and_call_id_boundaries_are_zero_contact() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(managed_config(format!("{upstream}/v1")), false).await;
    let client = reqwest::Client::new();

    for (method, path) in [
        (Method::GET, "/v1/realtime/calls"),
        (Method::PUT, "/v1/realtime/calls/rtc_a/accept"),
        (Method::POST, "/v1/realtime/calls/"),
        (Method::POST, "/v1/realtime/calls//accept"),
        (Method::POST, "/v1/realtime/calls/rtc_a/unknown"),
        (Method::POST, "/v1/realtime/calls/rtc_a/accept/extra"),
    ] {
        let response = client
            .request(method.clone(), format!("{}{path}", proxy.base))
            .send()
            .await
            .unwrap();
        let (status, message, code) = error_fields(response).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
        assert_eq!(message, format!("Unknown endpoint: {method} {path}"));
        assert_eq!(code, "invalid_request_error");
    }

    for raw_id in [
        "has.dot".to_string(),
        "has%2Fslash".to_string(),
        "%ED%95%9C%EA%B8%80".to_string(),
        "x".repeat(129),
    ] {
        let path = format!("/v1/realtime/calls/{raw_id}/accept");
        let response = client
            .post(format!("{}{}", proxy.base, path))
            .send()
            .await
            .unwrap();
        let (status, message, code) = error_fields(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "path={path}");
        assert_eq!(message, "invalid Realtime call_id");
        assert_eq!(code, "invalid_call_id");
    }

    let addr = proxy_addr(&proxy);
    let malformed = format!(
        "POST /v1/realtime/calls/%zz/accept HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let response = raw_request(&proxy, malformed.as_bytes()).await;
    assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 400"));
    assert!(String::from_utf8_lossy(&response).contains("invalid_call_id"));
    assert_eq!(captures.count(), 0);
}

#[tokio::test]
async fn profile_dispatch_is_atomic_and_private_failures_never_contact_upstream() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let mut chatgpt_config = managed_config(format!("{upstream}/backend-api/codex"));
    chatgpt_config.upstream = UpstreamProfile::ChatGptBackend {
        base_url: format!("{upstream}/backend-api/codex"),
        auth: BearerToken::new("chatgpt-secret"),
        account_id: None,
    };
    let chatgpt = start_proxy(chatgpt_config, false).await;
    let (body, content_type) = multipart_body();

    let official = reqwest::Client::new()
        .post(format!("{}/v1/realtime/calls", chatgpt.base))
        .header("content-type", content_type.clone())
        .body(body.clone())
        .send()
        .await
        .unwrap();
    let (status, _, code) = error_fields(official).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code, "unsupported_realtime_capability");
    assert_eq!(captures.count(), 0);

    let private = reqwest::Client::new()
        .post(format!("{}/v1/live", chatgpt.base))
        .header("content-type", content_type)
        .header("openai-alpha", "quicksilver=v2")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(private.status(), StatusCode::CREATED);
    assert!(captures
        .last()
        .uri
        .contains("intent=quicksilver&architecture=avas"));
    let contacted = captures.count();

    let mut repeated_alpha = HeaderMap::new();
    repeated_alpha.append("openai-alpha", HeaderValue::from_static("quicksilver=v2"));
    repeated_alpha.append("openai-alpha", HeaderValue::from_static("quicksilver=v2"));
    repeated_alpha.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("multipart/form-data; boundary=x"),
    );
    let repeated_private = reqwest::Client::new()
        .post(format!("{}/v1/live", chatgpt.base))
        .headers(repeated_alpha)
        .body("--x--\r\n")
        .send()
        .await
        .unwrap();
    let (status, _, _) = error_fields(repeated_private).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(captures.count(), contacted);

    let client_proxy = start_proxy(client_config(format!("{upstream}/v1")), false).await;
    let (body, content_type) = multipart_body();
    let private_client = reqwest::Client::new()
        .post(format!("{}/v1/realtime/calls", client_proxy.base))
        .header("content-type", content_type)
        .header("openai-alpha", "quicksilver=v1")
        .header("authorization", "Bearer caller")
        .body(body)
        .send()
        .await
        .unwrap();
    let (_, _, code) = error_fields(private_client).await;
    assert_eq!(code, "unsupported_realtime_capability");

    let private_non_call = reqwest::Client::new()
        .post(format!("{}/v1/realtime/sessions", chatgpt.base))
        .header("openai-alpha", "quicksilver=v2")
        .body("{}")
        .send()
        .await
        .unwrap();
    let (_, _, code) = error_fields(private_non_call).await;
    assert_eq!(code, "unsupported_realtime_capability");
    assert_eq!(captures.count(), contacted);
}

#[tokio::test]
async fn credential_negatives_are_rejected_before_body_or_upstream() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(client_config(format!("{upstream}/v1")), false).await;

    for authorization in [None, Some("Basic nope"), Some("Bearer")] {
        let mut request = reqwest::Client::new()
            .post(format!("{}/v1/realtime/sessions", proxy.base))
            .header("content-type", "application/json")
            .body("{}");
        if let Some(value) = authorization {
            request = request.header("authorization", value);
        }
        let (status, _, code) = error_fields(request.send().await.unwrap()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(code, "invalid_api_key");
    }

    let mut repeated = HeaderMap::new();
    repeated.append(
        http::header::AUTHORIZATION,
        HeaderValue::from_static("Bearer first"),
    );
    repeated.append(
        http::header::AUTHORIZATION,
        HeaderValue::from_static("Bearer second"),
    );
    let (status, message, _) = error_fields(
        reqwest::Client::new()
            .post(format!("{}/v1/realtime/sessions", proxy.base))
            .headers(repeated)
            .body("{}")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        message,
        "ambiguous Authorization: send exactly one credential"
    );

    let mut remote_config = client_config(format!("{upstream}/v1"));
    remote_config.admission_token = Some(BearerToken::new("admit-only"));
    let remote = start_proxy(remote_config, true).await;
    let (status, _, code) = error_fields(
        reqwest::Client::new()
            .post(format!("{}/v1/realtime/sessions", remote.base))
            .header("x-gpt-live-api-key", "admit-only")
            .body("{}")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code, "invalid_api_key");
    assert_eq!(captures.count(), 0);
}

#[tokio::test]
async fn request_and_response_caps_are_exact_and_recover() {
    let (upstream, captures) = start_upstream(UpstreamBehavior {
        raw_body: Some(Bytes::from_static(b"12345678")),
        ..Default::default()
    })
    .await;
    let mut config = managed_config(format!("{upstream}/v1"));
    config.limits.request_bytes = 8;
    config.limits.response_bytes = 8;
    let proxy = start_proxy(config, false).await;

    let exact = reqwest::Client::new()
        .post(format!("{}/v1/realtime/sessions", proxy.base))
        .header("content-type", "application/json")
        .body(vec![b'x'; 8])
        .send()
        .await
        .unwrap();
    assert_eq!(exact.status(), StatusCode::CREATED);
    assert_eq!(exact.bytes().await.unwrap().len(), 8);

    let over = reqwest::Client::new()
        .post(format!("{}/v1/realtime/sessions", proxy.base))
        .header("content-type", "application/json")
        .body(vec![b'x'; 9])
        .send()
        .await
        .unwrap();
    assert_eq!(over.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(captures.count(), 1);
    wait_for_permits(&proxy.state, 128).await;

    let (large_upstream, _) = start_upstream(UpstreamBehavior {
        raw_body: Some(Bytes::from_static(b"123456789")),
        ..Default::default()
    })
    .await;
    let mut config = managed_config(format!("{large_upstream}/v1"));
    config.limits.response_bytes = 8;
    let large = start_proxy(config, false).await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/realtime/sessions", large.base))
        .body("{}")
        .send()
        .await
        .unwrap();
    let (status, message, _) = error_fields(response).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(message, "live response too large (9 bytes)");
    wait_for_permits(&large.state, 128).await;
}

#[tokio::test]
async fn request_timeout_upstream_timeout_reset_and_binary_errors_are_exact() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let mut config = managed_config(format!("{upstream}/v1"));
    config.limits.request_read_timeout = Duration::from_millis(50);
    let proxy = start_proxy(config, false).await;
    let addr = proxy_addr(&proxy);
    let partial = format!(
        "POST /v1/realtime/sessions HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: 10\r\nConnection: close\r\n\r\n{{"
    );
    let response = raw_request(&proxy, partial.as_bytes()).await;
    let text = String::from_utf8_lossy(&response);
    assert!(text.starts_with("HTTP/1.1 408"), "{text}");
    assert!(text.contains("request_timeout"), "{text}");
    assert_eq!(captures.count(), 0);
    wait_for_permits(&proxy.state, 128).await;

    for (behavior, expected_status) in [
        (
            UpstreamBehavior {
                delay: Some(Duration::from_secs(30)),
                ..Default::default()
            },
            StatusCode::GATEWAY_TIMEOUT,
        ),
        (
            UpstreamBehavior {
                response_reset: true,
                ..Default::default()
            },
            StatusCode::BAD_GATEWAY,
        ),
    ] {
        let (fault_upstream, _) = start_upstream(behavior).await;
        let mut config = managed_config(format!("{fault_upstream}/v1"));
        config.limits.upstream_timeout = Duration::from_millis(75);
        let fault_proxy = start_proxy(config, false).await;
        let response = reqwest::Client::new()
            .post(format!("{}/v1/realtime/sessions", fault_proxy.base))
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), expected_status);
        wait_for_permits(&fault_proxy.state, 128).await;
    }

    for status in [
        StatusCode::BAD_REQUEST,
        StatusCode::UNAUTHORIZED,
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::INTERNAL_SERVER_ERROR,
    ] {
        let bytes = Bytes::from(vec![status.as_u16() as u8, 0xff, 0x00]);
        let (status_upstream, _) = start_upstream(UpstreamBehavior {
            status,
            raw_body: Some(bytes.clone()),
            ..Default::default()
        })
        .await;
        let status_proxy =
            start_proxy(managed_config(format!("{status_upstream}/v1")), false).await;
        let response = reqwest::Client::new()
            .post(format!("{}/v1/realtime/sessions", status_proxy.base))
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(response.bytes().await.unwrap(), bytes);
    }
}

#[tokio::test]
async fn active_request_limit_rejects_max_plus_one_then_recovers() {
    let (upstream, captures, started, release) =
        start_upstream_with_response_gate(UpstreamBehavior::default()).await;
    let mut config = managed_config(format!("{upstream}/v1"));
    config.limits.active_requests = 1;
    let proxy = start_proxy(config, false).await;

    let first = tokio::spawn(
        reqwest::Client::new()
            .post(format!("{}/v1/realtime/sessions", proxy.base))
            .body("{}")
            .send(),
    );
    tokio::time::timeout(Duration::from_secs(5), started.0)
        .await
        .expect("first request did not reach upstream")
        .expect("request-start signal dropped");
    wait_for_permits(&proxy.state, 0).await;

    let rejected = reqwest::Client::new()
        .post(format!("{}/v1/realtime/sessions", proxy.base))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(rejected.headers().get("retry-after").unwrap(), "1");
    assert_eq!(captures.count(), 1);

    release.release();
    assert_eq!(first.await.unwrap().unwrap().status(), StatusCode::CREATED);
    wait_for_permits(&proxy.state, 1).await;

    release.release();
    let recovered = reqwest::Client::new()
        .post(format!("{}/v1/realtime/sessions", proxy.base))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(recovered.status(), StatusCode::CREATED);
    assert_eq!(captures.count(), 2);
}

#[tokio::test]
async fn disconnect_during_inbound_body_releases_handler_owned_permit() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let mut config = managed_config(format!("{upstream}/v1"));
    config.limits.active_requests = 1;
    config.limits.request_read_timeout = Duration::from_secs(30);
    let proxy = start_proxy(config, false).await;
    let addr = proxy_addr(&proxy);

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let partial = format!(
        "POST /v1/realtime/sessions HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{{"
    );
    stream.write_all(partial.as_bytes()).await.unwrap();
    wait_for_permits(&proxy.state, 0).await;
    drop(stream);
    wait_for_permits(&proxy.state, 1).await;

    let recovered = reqwest::Client::new()
        .post(format!("{}/v1/realtime/sessions", proxy.base))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(recovered.status(), StatusCode::CREATED);
    assert_eq!(captures.count(), 1);
}

#[tokio::test]
async fn downstream_disconnect_cancels_spawned_exchange_and_releases_permit() {
    let (upstream, _captures, dropped, started) =
        start_upstream_with_drop_signal(UpstreamBehavior {
            delay: Some(Duration::from_millis(10)),
            ..Default::default()
        })
        .await;
    let mut config = managed_config(format!("{upstream}/v1"));
    config.limits.active_requests = 1;
    config.limits.upstream_timeout = Duration::from_secs(300);
    let proxy = start_proxy(config, false).await;

    let request = tokio::spawn(
        reqwest::Client::new()
            .post(format!("{}/v1/realtime/sessions", proxy.base))
            .body("{}")
            .send(),
    );
    tokio::time::timeout(Duration::from_secs(5), started.0)
        .await
        .expect("upstream did not stream")
        .expect("start signal dropped");
    wait_for_permits(&proxy.state, 0).await;
    request.abort();
    let _ = request.await;
    let dropped = tokio::time::timeout(Duration::from_secs(5), dropped.0).await;
    assert!(matches!(dropped, Ok(Ok(()))), "upstream body: {dropped:?}");
    wait_for_permits(&proxy.state, 1).await;
}

#[test]
fn completed_exchange_cannot_be_relabelled_by_handler_drop() {
    let (lifecycle, guard) = begin_exchange();
    assert!(lifecycle.finish(ExchangeTerminal::Completed));
    drop(guard);
    assert_eq!(lifecycle.terminal(), ExchangeTerminal::Completed);
}
