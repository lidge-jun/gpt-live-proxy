//! Wire-level proof of the call-create contract (docs/020, docs/000 §2).

mod support;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use gpt_live_proxy::app::{router, AppState};
use gpt_live_proxy::config::{AccountId, BearerToken, Config, UpstreamProfile};
use gpt_live_proxy::wire::MULTIPART_BOUNDARY;
use http::StatusCode;
use support::{start_upstream, start_upstream_with_drop_signal, UpstreamBehavior};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type MultipartField<'a> = (&'a str, &'a str, &'a str);

fn config_for(profile: UpstreamProfile) -> Config {
    let mut config = Config::from_source(|k| match k {
        "GPT_LIVE_TOKEN" => Some("unused".to_string()),
        _ => None,
    })
    .expect("config");
    config.upstream = profile;
    config
}

/// Serve the router on a real socket so requests traverse actual HTTP.
///
/// The bind is ephemeral, so `config.bind` is rewritten to the port actually
/// obtained: the origin policy compares the request `Host` port against it, and
/// a stale placeholder would make every request a legitimate 403.
async fn start_proxy(mut config: Config) -> String {
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().expect("proxy addr");
    config.bind = addr;
    let state = AppState::new(config).expect("state");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });
    format!("http://{addr}")
}

fn multipart_body(session: Option<&str>) -> (Vec<u8>, String) {
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"sdp\"\r\n");
    body.extend_from_slice(b"Content-Type: application/sdp\r\n\r\n");
    body.extend_from_slice(b"v=0\r\na=offer");
    body.extend_from_slice(b"\r\n");
    if let Some(session) = session {
        body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(b"Content-Disposition: form-data; name=\"session\"\r\n");
        body.extend_from_slice(b"Content-Type: application/json\r\n\r\n");
        body.extend_from_slice(session.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}--\r\n").as_bytes());
    (
        body,
        format!("multipart/form-data; boundary={MULTIPART_BOUNDARY}"),
    )
}

fn multipart_body_with_fields(fields: &[MultipartField<'_>]) -> (Vec<u8>, String) {
    let mut body = Vec::new();
    for (name, content_type, value) in fields {
        body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n").as_bytes(),
        );
        body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}--\r\n").as_bytes());
    (
        body,
        format!("multipart/form-data; boundary={MULTIPART_BOUNDARY}"),
    )
}

#[tokio::test]
async fn a_chatgpt_backend_call_rewrites_multipart_into_json() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(config_for(UpstreamProfile::ChatGptBackend {
        // `/backend-api` in the path is what selects the JSON shape.
        base_url: format!("{upstream}/backend-api/codex"),
        auth: BearerToken::new("upstream-token"),
        account_id: Some(AccountId::new("acct-42")),
    }))
    .await;

    let (body, content_type) = multipart_body(Some(
        r#"{"id":"sess_1","model":"gpt-live-1-boulder-alpha","audio":{"output":{"voice":"cove"}}}"#,
    ));

    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("content-type", content_type)
        .header("openai-alpha", "quicksilver=v2")
        .header("x-session-id", "sess-abc")
        .header("thread-id", "thread-abc")
        .body(body)
        .send()
        .await
        .expect("call-create");

    assert_eq!(response.status(), StatusCode::CREATED);

    let captured = captures.last();
    assert_eq!(
        captured.method,
        http::Method::POST,
        "call-create is always POST"
    );
    assert!(
        captured
            .uri
            .ends_with("/realtime/calls?intent=quicksilver&architecture=avas"),
        "unexpected upstream URI: {}",
        captured.uri
    );
    assert_eq!(
        captured.headers.get("content-type").unwrap(),
        "application/json"
    );

    let sent: serde_json::Value = serde_json::from_slice(&captured.body).expect("json body");
    assert_eq!(sent["sdp"], "v=0\r\na=offer");
    assert_eq!(sent["session"]["audio"]["output"]["voice"], "cove");
    assert!(
        sent["session"].get("id").is_none(),
        "the session id must be stripped before call-create"
    );
    // No top-level type: adding one would convert a Frameless body into a V1 body.
    assert!(sent["session"].get("type").is_none());
}

#[tokio::test]
async fn backend_base_shape_rewrites_even_for_an_api_key_profile() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(config_for(UpstreamProfile::ApiKeyManaged {
        base_url: format!("{upstream}/backend-api/codex"),
        auth: BearerToken::new("sk-test"),
    }))
    .await;

    let (body, content_type) = multipart_body(Some(r#"{"id":"strip-me","voice":"cove"}"#));
    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("openai-alpha", "quicksilver=v2")
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let captured = captures.last();
    assert_eq!(
        captured.uri,
        "/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas"
    );
    assert_eq!(
        captured.headers.get("content-type").unwrap(),
        "application/json"
    );
    let sent: serde_json::Value = serde_json::from_slice(&captured.body).unwrap();
    assert_eq!(sent["sdp"], "v=0\r\na=offer");
    assert_eq!(sent["session"]["voice"], "cove");
    assert!(sent["session"].get("id").is_none());
}

#[tokio::test]
async fn direct_base_shape_preserves_multipart_even_for_a_chatgpt_profile() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(config_for(UpstreamProfile::ChatGptBackend {
        base_url: upstream.clone(),
        auth: BearerToken::new("chatgpt-token"),
        account_id: None,
    }))
    .await;

    let (body, content_type) = multipart_body(Some(
        r#"{"id":"keep-me","delegation":{"type":"client"},"voice":"cove"}"#,
    ));
    let expected_body = body.clone();
    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("openai-alpha", "quicksilver=v2")
        .header("content-type", &content_type)
        .body(body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let captured = captures.last();
    assert_eq!(captured.uri, "/v1/live");
    assert_eq!(captured.body.as_ref(), expected_body.as_slice());
    assert_eq!(
        captured.headers.get("content-type").unwrap(),
        content_type.as_str()
    );
}

#[tokio::test]
async fn chatgpt_session_evidence_and_dialect_cross_product_is_enforced_before_contact() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(config_for(UpstreamProfile::ChatGptBackend {
        base_url: format!("{upstream}/backend-api/codex"),
        auth: BearerToken::new("chatgpt-token"),
        account_id: None,
    }))
    .await;

    let rows = [
        (
            "/v1/realtime/calls",
            "quicksilver=v1",
            Some(r#"{"type":"quicksilver"}"#),
            StatusCode::CREATED,
            None,
            true,
        ),
        (
            "/v1/live",
            "quicksilver=v2",
            Some(r#"{"delegation":{"type":"client"}}"#),
            StatusCode::CREATED,
            None,
            true,
        ),
        (
            "/v1/realtime/calls",
            "quicksilver=v1",
            None,
            StatusCode::CREATED,
            None,
            true,
        ),
        (
            "/v1/live",
            "quicksilver=v2",
            Some(r#"{"type":"future","voice":"cove"}"#),
            StatusCode::CREATED,
            None,
            true,
        ),
        (
            "/v1/realtime/calls",
            "quicksilver=v1",
            Some(r#"{"delegation":{"type":"client"}}"#),
            StatusCode::BAD_REQUEST,
            Some("invalid_realtime_session_shape"),
            false,
        ),
        (
            "/v1/live",
            "quicksilver=v2",
            Some(r#"{"type":"quicksilver"}"#),
            StatusCode::BAD_REQUEST,
            Some("invalid_realtime_session_shape"),
            false,
        ),
        (
            "/v1/live",
            "quicksilver=v2",
            Some(r#"{"type":"future","delegation":{"type":"client"}}"#),
            StatusCode::BAD_REQUEST,
            Some("invalid_realtime_session_shape"),
            false,
        ),
        (
            "/v1/realtime/calls",
            "quicksilver=v1",
            Some(r#"{"type":"realtime"}"#),
            StatusCode::BAD_REQUEST,
            Some("unsupported_realtime_capability"),
            false,
        ),
        (
            "/v1/live",
            "quicksilver=v2",
            Some(r#"{"type":"transcription"}"#),
            StatusCode::BAD_REQUEST,
            Some("unsupported_realtime_capability"),
            false,
        ),
    ];

    for (path, alpha, session, expected_status, expected_code, contacts_upstream) in rows {
        let before = captures.count();
        let (body, content_type) = multipart_body(session);
        let response = reqwest::Client::new()
            .post(format!("{proxy}{path}"))
            .header("openai-alpha", alpha)
            .header("content-type", content_type)
            .body(body)
            .send()
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            expected_status,
            "path={path} alpha={alpha}"
        );
        if let Some(expected_code) = expected_code {
            let value: serde_json::Value = response.json().await.unwrap();
            assert_eq!(
                value["error"]["code"], expected_code,
                "path={path} alpha={alpha}"
            );
        }
        assert_eq!(
            captures.count(),
            before + usize::from(contacts_upstream),
            "path={path} alpha={alpha}"
        );
    }
}

#[tokio::test]
async fn direct_chatgpt_base_validates_evidence_without_rewriting_original_bytes() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(config_for(UpstreamProfile::ChatGptBackend {
        base_url: upstream,
        auth: BearerToken::new("chatgpt-token"),
        account_id: None,
    }))
    .await;

    let (matching_body, matching_type) = multipart_body(Some(
        r#"{"delegation":{"type":"client"},"opaque_future":{"x":1}}"#,
    ));
    let expected = matching_body.clone();
    let accepted = reqwest::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("openai-alpha", "quicksilver=v2")
        .header("content-type", matching_type)
        .body(matching_body)
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::CREATED);
    assert_eq!(captures.last().body.as_ref(), expected.as_slice());

    let before = captures.count();
    let (mismatch_body, mismatch_type) = multipart_body(Some(r#"{"type":"quicksilver"}"#));
    let rejected = reqwest::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("openai-alpha", "quicksilver=v2")
        .header("content-type", mismatch_type)
        .body(mismatch_body)
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    let value: serde_json::Value = rejected.json().await.unwrap();
    assert_eq!(value["error"]["code"], "invalid_realtime_session_shape");
    assert_eq!(captures.count(), before);
}

#[tokio::test]
async fn chatgpt_duplicate_contract_fields_are_rejected_before_upstream_contact() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(config_for(UpstreamProfile::ChatGptBackend {
        // A direct base is important here: this path forwards original bytes,
        // so duplicate detection cannot rely on the backend JSON rewrite.
        base_url: upstream,
        auth: BearerToken::new("chatgpt-token"),
        account_id: None,
    }))
    .await;

    let rows: [(&str, Vec<MultipartField<'_>>); 3] = [
        (
            "matching session followed by an official session",
            vec![
                ("sdp", "application/sdp", "v=0"),
                (
                    "session",
                    "application/json",
                    r#"{"delegation":{"type":"client"}}"#,
                ),
                ("session", "application/json", r#"{"type":"realtime"}"#),
            ],
        ),
        (
            "opaque session followed by a mismatched private session",
            vec![
                ("sdp", "application/sdp", "v=0"),
                (
                    "session",
                    "application/json",
                    r#"{"type":"future","voice":"cove"}"#,
                ),
                ("session", "application/json", r#"{"type":"quicksilver"}"#),
            ],
        ),
        (
            "duplicate sdp",
            vec![
                ("sdp", "application/sdp", "v=0-first"),
                ("sdp", "application/sdp", "v=0-second"),
                (
                    "session",
                    "application/json",
                    r#"{"delegation":{"type":"client"}}"#,
                ),
            ],
        ),
    ];

    for (case, fields) in rows {
        let before = captures.count();
        let (body, content_type) = multipart_body_with_fields(&fields);
        let response = reqwest::Client::new()
            .post(format!("{proxy}/v1/live"))
            .header("openai-alpha", "quicksilver=v2")
            .header("content-type", content_type)
            .body(body)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{case}");
        let value: serde_json::Value = response.json().await.unwrap();
        assert_eq!(
            value["error"]["code"], "invalid_realtime_session_shape",
            "{case}"
        );
        assert_eq!(captures.count(), before, "{case} contacted upstream");
    }
}

#[tokio::test]
async fn an_api_key_call_preserves_multipart_verbatim() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(config_for(UpstreamProfile::ApiKeyManaged {
        base_url: format!("{upstream}/v1"),
        auth: BearerToken::new("sk-test"),
    }))
    .await;

    let (body, content_type) = multipart_body(Some(r#"{"voice":"cove"}"#));
    let sent_body = body.clone();

    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/realtime/calls"))
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .expect("call-create");

    assert_eq!(response.status(), StatusCode::CREATED);

    let captured = captures.last();
    assert_eq!(captured.uri, "/v1/realtime/calls");
    assert_eq!(
        captured.body.as_ref(),
        sent_body.as_slice(),
        "the keyed path must forward the body byte for byte"
    );
    assert!(captured
        .headers
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .contains(MULTIPART_BOUNDARY));
    assert_eq!(
        captured.headers.get("authorization").unwrap(),
        "Bearer sk-test"
    );
}

#[tokio::test]
async fn api_key_v1_uses_realtime_calls_with_avas_and_preserves_the_body() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(config_for(UpstreamProfile::ApiKeyManaged {
        base_url: format!("{upstream}/v1"),
        auth: BearerToken::new("sk-test"),
    }))
    .await;

    let (body, content_type) = multipart_body(Some(r#"{"voice":"cove"}"#));
    let expected_body = body.clone();
    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/realtime/calls"))
        .header("openai-alpha", "quicksilver=v1")
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let captured = captures.last();
    assert_eq!(
        captured.uri,
        "/v1/realtime/calls?intent=quicksilver&architecture=avas"
    );
    assert_eq!(captured.body.as_ref(), expected_body.as_slice());
    assert_eq!(
        captured.headers.get("openai-alpha").unwrap(),
        "quicksilver=v1"
    );
}

#[tokio::test]
async fn chatgpt_v1_uses_backend_avas_and_rewrites_the_body() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(config_for(UpstreamProfile::ChatGptBackend {
        base_url: format!("{upstream}/backend-api/codex"),
        auth: BearerToken::new("chatgpt-token"),
        account_id: None,
    }))
    .await;

    let (body, content_type) = multipart_body(Some(r#"{"id":"sess_v1","voice":"cove"}"#));
    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/realtime/calls"))
        .header("openai-alpha", "quicksilver=v1")
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let captured = captures.last();
    assert_eq!(
        captured.uri,
        "/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas"
    );
    assert_eq!(
        captured.headers.get("content-type").unwrap(),
        "application/json"
    );
    let body: serde_json::Value = serde_json::from_slice(&captured.body).unwrap();
    assert_eq!(body["sdp"], "v=0\r\na=offer");
    assert_eq!(body["session"]["voice"], "cove");
    assert!(body["session"].get("id").is_none());
}

#[tokio::test]
async fn protocol_headers_are_forwarded_and_credentials_are_replaced() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(config_for(UpstreamProfile::ChatGptBackend {
        base_url: format!("{upstream}/backend-api/codex"),
        auth: BearerToken::new("proxy-selected-token"),
        account_id: Some(AccountId::new("proxy-account")),
    }))
    .await;

    let (body, content_type) = multipart_body(None);
    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("content-type", content_type)
        .header("openai-alpha", "quicksilver=v2")
        .header("x-session-id", "sess-1")
        .header("session-id", "conv-1")
        .header("thread-id", "thread-1")
        .header("originator", "codex_cli_rs")
        .header("x-oai-attestation", "att-1")
        // These must not survive.
        .header("authorization", "Bearer caller-token")
        .header("chatgpt-account-id", "caller-account")
        .header("x-openai-fedramp", "true")
        .header("cookie", "session=abc")
        .body(body)
        .send()
        .await
        .expect("call-create");

    assert_eq!(response.status(), StatusCode::CREATED);
    let captured = captures.last();

    for (name, expected) in [
        ("openai-alpha", "quicksilver=v2"),
        ("x-session-id", "sess-1"),
        ("session-id", "conv-1"),
        ("thread-id", "thread-1"),
        ("originator", "codex_cli_rs"),
        ("x-oai-attestation", "att-1"),
    ] {
        assert_eq!(
            captured.headers.get(name).unwrap(),
            expected,
            "{name} was not forwarded"
        );
    }

    assert_eq!(
        captured.headers.get("authorization").unwrap(),
        "Bearer proxy-selected-token"
    );
    assert_eq!(
        captured.headers.get("chatgpt-account-id").unwrap(),
        "proxy-account"
    );
    assert!(captured.headers.get("x-openai-fedramp").is_none());
    assert!(captured.headers.get("cookie").is_none());
}

#[tokio::test]
async fn an_absent_private_protocol_header_is_rejected_before_contact() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(config_for(UpstreamProfile::ChatGptBackend {
        base_url: format!("{upstream}/backend-api/codex"),
        auth: BearerToken::new("t"),
        account_id: None,
    }))
    .await;

    let (body, content_type) = multipart_body(None);
    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .expect("call-create");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let value: serde_json::Value = response.json().await.unwrap();
    assert_eq!(value["error"]["code"], "unsupported_realtime_capability");
    assert_eq!(captures.count(), 0);
}

#[tokio::test]
async fn private_call_create_rejects_wrong_path_dialect_and_content_before_contact() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(config_for(UpstreamProfile::ChatGptBackend {
        base_url: format!("{upstream}/backend-api/codex"),
        auth: BearerToken::new("t"),
        account_id: None,
    }))
    .await;
    let (multipart, multipart_type) = multipart_body(None);

    let rows = [
        (
            "/v1/live",
            Some("quicksilver=v2"),
            "application/sdp",
            b"v=0".as_slice(),
            "invalid_request_error",
        ),
        (
            "/v1/realtime/calls",
            Some("quicksilver=v1"),
            "application/sdp",
            b"v=0".as_slice(),
            "invalid_request_error",
        ),
        (
            "/v1/live",
            Some("quicksilver=v1"),
            multipart_type.as_str(),
            multipart.as_slice(),
            "unsupported_realtime_capability",
        ),
        (
            "/v1/realtime/calls",
            Some("quicksilver=v2"),
            multipart_type.as_str(),
            multipart.as_slice(),
            "unsupported_realtime_capability",
        ),
        (
            "/v1/live",
            Some("future=v9"),
            multipart_type.as_str(),
            multipart.as_slice(),
            "unsupported_realtime_capability",
        ),
        (
            "/v1/live",
            Some("quicksilver=v2"),
            "text/plain",
            b"not multipart".as_slice(),
            "invalid_request_error",
        ),
        (
            "/v1/realtime/calls",
            None,
            multipart_type.as_str(),
            multipart.as_slice(),
            "unsupported_realtime_capability",
        ),
    ];

    for (path, alpha, content_type, body, expected_code) in rows {
        let mut request = reqwest::Client::new()
            .post(format!("{proxy}{path}"))
            .header("content-type", content_type)
            .body(body.to_vec());
        if let Some(alpha) = alpha {
            request = request.header("openai-alpha", alpha);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "path={path}");
        let value: serde_json::Value = response.json().await.unwrap();
        assert_eq!(value["error"]["code"], expected_code, "path={path}");
    }

    let duplicate_alpha = reqwest::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("content-type", multipart_type)
        .header("openai-alpha", "quicksilver=v2")
        .header("openai-alpha", "quicksilver=v2")
        .body(multipart)
        .send()
        .await
        .unwrap();
    assert_eq!(duplicate_alpha.status(), StatusCode::BAD_REQUEST);
    let value: serde_json::Value = duplicate_alpha.json().await.unwrap();
    assert_eq!(value["error"]["code"], "invalid_request_error");
    assert_eq!(captures.count(), 0);
}

#[tokio::test]
async fn only_content_type_and_location_come_back() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(config_for(UpstreamProfile::ApiKeyManaged {
        base_url: format!("{upstream}/v1"),
        auth: BearerToken::new("sk-test"),
    }))
    .await;

    let (body, content_type) = multipart_body(None);
    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("openai-alpha", "quicksilver=v2")
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .expect("call-create");

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(captures.last().uri, "/v1/live");
    assert_eq!(
        response.headers().get("location").unwrap(),
        "/v1/realtime/calls/rtc_test_call"
    );
    // Positively asserted, not merely "other headers are absent": the client
    // needs this to parse the answer SDP.
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/sdp"
    );
    for dropped in ["set-cookie", "x-request-id", "cache-control"] {
        assert!(
            response.headers().get(dropped).is_none(),
            "{dropped} must not be relayed downstream"
        );
    }
    assert_eq!(response.text().await.unwrap(), "v=0\r\na=answer");
}

#[tokio::test]
async fn a_307_is_relayed_without_replaying_body_or_credential_to_its_target() {
    let target_hits = Arc::new(AtomicUsize::new(0));
    let target = axum::Router::new().fallback(axum::routing::any({
        let target_hits = target_hits.clone();
        move || {
            let target_hits = target_hits.clone();
            async move {
                target_hits.fetch_add(1, Ordering::SeqCst);
                StatusCode::NO_CONTENT
            }
        }
    }));
    let target_listener =
        tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
    let target_addr = target_listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(target_listener, target).await;
    });

    let redirect_location = format!("http://{target_addr}/stolen");
    let redirector = axum::Router::new().fallback(axum::routing::any({
        let redirect_location = redirect_location.clone();
        move || {
            let redirect_location = redirect_location.clone();
            async move {
                (
                    StatusCode::TEMPORARY_REDIRECT,
                    [(http::header::LOCATION, redirect_location)],
                )
            }
        }
    }));
    let redirect_listener =
        tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
    let redirect_addr = redirect_listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(redirect_listener, redirector).await;
    });

    let proxy = start_proxy(config_for(UpstreamProfile::ApiKeyManaged {
        base_url: format!("http://{redirect_addr}/v1"),
        auth: BearerToken::new("credential-that-must-not-be-replayed"),
    }))
    .await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let (body, content_type) = multipart_body(None);
    let response = client
        .post(format!("{proxy}/v1/realtime/calls"))
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        response.headers().get(http::header::LOCATION).unwrap(),
        redirect_location.as_str()
    );
    assert_eq!(
        target_hits.load(Ordering::SeqCst),
        0,
        "redirect target received the upstream credential and request body"
    );
}

#[tokio::test]
async fn client_credential_mode_relays_an_official_call_create() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let config = Config::from_source(|key| match key {
        "GPT_LIVE_UPSTREAM_MODE" => Some("apikey".to_string()),
        "GPT_LIVE_CREDENTIAL_MODE" => Some("client".to_string()),
        "GPT_LIVE_BASE_URL" => Some(format!("{upstream}/v1")),
        _ => None,
    })
    .expect("client config needs no managed token");
    let proxy = start_proxy(config).await;

    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/realtime/calls"))
        .header("content-type", "application/sdp")
        .header("authorization", "Bearer caller-owned-token")
        .body("v=0\r\na=offer")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        captures.count(),
        1,
        "the official client-credential request must reach the upstream once"
    );
    let captured = captures.last();
    assert_eq!(captured.uri, "/v1/realtime/calls");
    assert_eq!(
        captured.headers.get(http::header::AUTHORIZATION).unwrap(),
        "Bearer caller-owned-token"
    );
}

#[tokio::test]
async fn an_sdp_only_body_is_accepted() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(config_for(UpstreamProfile::ChatGptBackend {
        base_url: format!("{upstream}/backend-api/codex"),
        auth: BearerToken::new("t"),
        account_id: None,
    }))
    .await;

    let (body, content_type) = multipart_body(None);
    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("openai-alpha", "quicksilver=v2")
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .expect("call-create");

    assert_eq!(response.status(), StatusCode::CREATED);
    let sent: serde_json::Value = serde_json::from_slice(&captures.last().body).unwrap();
    assert_eq!(sent["sdp"], "v=0\r\na=offer");
    assert!(sent.as_object().unwrap().get("session").is_none());
}

#[tokio::test]
async fn a_malformed_multipart_body_is_rejected_with_the_exact_message() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(config_for(UpstreamProfile::ChatGptBackend {
        base_url: format!("{upstream}/backend-api/codex"),
        auth: BearerToken::new("t"),
        account_id: None,
    }))
    .await;

    let (body, content_type) = multipart_body(Some("{not json"));
    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("openai-alpha", "quicksilver=v2")
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .expect("call-create");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let value: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        value["error"]["message"],
        "ChatGPT voice relay expected JSON in the multipart session field"
    );
    assert_eq!(
        captures.count(),
        0,
        "a bad body must never reach the upstream"
    );
}

#[tokio::test]
async fn an_upstream_timeout_reports_504() {
    let (upstream, _captures) = start_upstream(UpstreamBehavior {
        delay: Some(std::time::Duration::from_secs(30)),
        ..Default::default()
    })
    .await;

    let mut config = config_for(UpstreamProfile::ApiKeyManaged {
        base_url: format!("{upstream}/v1"),
        auth: BearerToken::new("sk-test"),
    });
    config.limits.upstream_timeout = std::time::Duration::from_millis(150);
    let proxy = start_proxy(config).await;

    let (body, content_type) = multipart_body(None);
    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("openai-alpha", "quicksilver=v2")
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .expect("call-create");

    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    let value: serde_json::Value = response.json().await.unwrap();
    assert_eq!(value["error"]["message"], "live upstream timed out");
}

#[tokio::test]
async fn an_unreachable_upstream_reports_502() {
    // Bind and immediately drop, so the port is almost certainly closed.
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let dead = listener.local_addr().unwrap();
    drop(listener);

    let proxy = start_proxy(config_for(UpstreamProfile::ApiKeyManaged {
        base_url: format!("http://{dead}/v1"),
        auth: BearerToken::new("sk-test"),
    }))
    .await;

    let (body, content_type) = multipart_body(None);
    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("openai-alpha", "quicksilver=v2")
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .expect("call-create");

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let value: serde_json::Value = response.json().await.unwrap();
    assert!(value["error"]["message"]
        .as_str()
        .unwrap()
        .starts_with("live relay failed:"));
}

#[tokio::test]
async fn an_oversized_body_is_rejected_before_reaching_the_upstream() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let mut config = config_for(UpstreamProfile::ApiKeyManaged {
        base_url: format!("{upstream}/v1"),
        auth: BearerToken::new("sk-test"),
    });
    config.limits.request_bytes = 512;
    let proxy = start_proxy(config).await;

    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("openai-alpha", "quicksilver=v2")
        .header("content-type", "multipart/form-data; boundary=cap")
        .body(vec![b'x'; 4096])
        .send()
        .await
        .expect("call-create");

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(captures.count(), 0);
}

#[tokio::test]
async fn a_legacy_partial_body_times_out_before_upstream_contact_and_recovers() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let mut config = config_for(UpstreamProfile::ApiKeyManaged {
        base_url: format!("{upstream}/v1"),
        auth: BearerToken::new("sk-test"),
    });
    config.limits.active_requests = 1;
    config.limits.request_read_timeout = std::time::Duration::from_millis(50);
    let proxy = start_proxy(config).await;
    let addr: SocketAddr = proxy.strip_prefix("http://").unwrap().parse().unwrap();

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let partial = format!(
        "POST /v1/live HTTP/1.1\r\nHost: {addr}\r\nOpenAI-Alpha: quicksilver=v2\r\nContent-Type: multipart/form-data; boundary=partial\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{{"
    );
    stream.write_all(partial.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_to_end(&mut response),
    )
    .await
    .expect("legacy read-timeout response stalled")
    .unwrap();
    let response = String::from_utf8_lossy(&response);
    assert!(response.starts_with("HTTP/1.1 408"), "{response}");
    assert!(response.contains("request_timeout"), "{response}");
    assert_eq!(captures.count(), 0);

    let recovered = reqwest::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("openai-alpha", "quicksilver=v2")
        .header("content-type", "multipart/form-data; boundary=recovered")
        .body("complete")
        .send()
        .await
        .unwrap();
    assert_eq!(recovered.status(), StatusCode::CREATED);
    assert_eq!(captures.count(), 1);
}

#[tokio::test]
async fn a_get_on_the_call_create_path_is_not_found() {
    let (upstream, _captures) = start_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(config_for(UpstreamProfile::ApiKeyManaged {
        base_url: format!("{upstream}/v1"),
        auth: BearerToken::new("sk-test"),
    }))
    .await;

    let response = reqwest::get(format!("{proxy}/v1/live")).await.expect("get");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_oversized_upstream_response_reports_502() {
    let (upstream, _captures) = start_upstream(UpstreamBehavior {
        body_bytes: Some(4096),
        ..Default::default()
    })
    .await;

    let mut config = config_for(UpstreamProfile::ApiKeyManaged {
        base_url: format!("{upstream}/v1"),
        auth: BearerToken::new("sk-test"),
    });
    config.limits.response_bytes = 1024;
    let proxy = start_proxy(config).await;

    let (body, content_type) = multipart_body(None);
    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("openai-alpha", "quicksilver=v2")
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .expect("call-create");

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let value: serde_json::Value = response.json().await.unwrap();
    // The exact message, byte count included: a prefix check would pass with
    // the wrong total. The count is what the buffer WOULD have reached, so a
    // single 4096-byte frame reports 4096 rather than cap+1.
    assert_eq!(
        value["error"]["message"], "live response too large (4096 bytes)",
        "unexpected message"
    );
}

#[tokio::test]
async fn a_response_at_exactly_the_cap_is_relayed() {
    let (upstream, _captures) = start_upstream(UpstreamBehavior {
        body_bytes: Some(1024),
        ..Default::default()
    })
    .await;

    let mut config = config_for(UpstreamProfile::ApiKeyManaged {
        base_url: format!("{upstream}/v1"),
        auth: BearerToken::new("sk-test"),
    });
    config.limits.response_bytes = 1024;
    let proxy = start_proxy(config).await;

    let (body, content_type) = multipart_body(None);
    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("openai-alpha", "quicksilver=v2")
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .expect("call-create");

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.bytes().await.unwrap().len(), 1024);
}

#[tokio::test]
async fn an_invalid_configured_credential_fails_loudly() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    // A newline cannot appear in a header value; the relay must refuse rather
    // than send an unauthenticated request upstream.
    let proxy = start_proxy(config_for(UpstreamProfile::ApiKeyManaged {
        base_url: format!("{upstream}/v1"),
        auth: BearerToken::new("bad\nvalue"),
    }))
    .await;

    let (body, content_type) = multipart_body(None);
    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("openai-alpha", "quicksilver=v2")
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .expect("call-create");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        captures.count(),
        0,
        "an unauthenticated request must never reach the upstream"
    );
}

/// Proof that a downstream disconnect actually reaches the upstream call.
///
/// The ownership model in call_create.rs exists precisely because a dropped
/// handler future cannot observe its own cancellation. Unit tests cover the
/// guard's local behavior; this covers the cross-runtime behavior it depends
/// on — that Axum drops the handler when the socket goes away, and that the
/// drop propagates into the in-flight reqwest response body.
#[tokio::test]
async fn a_downstream_disconnect_cancels_the_upstream_call() {
    let (upstream, _captures, drop_signal, start_signal) =
        start_upstream_with_drop_signal(UpstreamBehavior {
            // A body that streams forever, so the relay is mid-response when the
            // client leaves.
            delay: Some(std::time::Duration::from_millis(20)),
            ..Default::default()
        })
        .await;

    let mut config = config_for(UpstreamProfile::ApiKeyManaged {
        base_url: format!("{upstream}/v1"),
        auth: BearerToken::new("sk-test"),
    });
    // Long enough that a timeout cannot be mistaken for a cancellation.
    config.limits.upstream_timeout = std::time::Duration::from_secs(300);
    let proxy = start_proxy(config).await;

    let (body, content_type) = multipart_body(None);
    let handle = tokio::spawn(
        reqwest::Client::new()
            .post(format!("{proxy}/v1/live"))
            .header("openai-alpha", "quicksilver=v2")
            .header("content-type", content_type)
            .body(body)
            .send(),
    );

    // Wait for a genuinely in-flight stream rather than sleeping a guess.
    tokio::time::timeout(std::time::Duration::from_secs(5), start_signal.0)
        .await
        .expect("the upstream never started streaming")
        .expect("the start signal was dropped without firing");

    let mut drop_rx = drop_signal.0;
    // Nothing has been dropped yet: the call is alive.
    assert!(
        drop_rx.try_recv().is_err(),
        "the upstream body was dropped before the client disconnected"
    );

    handle.abort();
    let _ = handle.await;

    // Strict: `Ok(Ok(()))` only. A sender dropped without signalling would
    // surface as Ok(Err(RecvError)) and must not count as success.
    let dropped = tokio::time::timeout(std::time::Duration::from_secs(5), drop_rx).await;
    assert!(
        matches!(dropped, Ok(Ok(()))),
        "cancellation did not propagate to the upstream body: {dropped:?}"
    );
}
