//! Wire-level proof of the call-create contract (docs/020, docs/000 §2).

mod support;

use std::net::SocketAddr;

use gpt_live_proxy::app::{router, AppState};
use gpt_live_proxy::config::{AccountId, BearerToken, Config, UpstreamProfile};
use gpt_live_proxy::wire::MULTIPART_BOUNDARY;
use http::StatusCode;
use support::{start_upstream, start_upstream_with_drop_signal, UpstreamBehavior};

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
async fn an_api_key_call_preserves_multipart_verbatim() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(config_for(UpstreamProfile::ApiKey {
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
    assert!(
        captured
            .uri
            .ends_with("/v1/realtime/calls?intent=quicksilver&architecture=avas"),
        "unexpected upstream URI: {}",
        captured.uri
    );
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
async fn an_absent_protocol_header_is_not_invented() {
    let (upstream, captures) = start_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(config_for(UpstreamProfile::ChatGptBackend {
        base_url: format!("{upstream}/backend-api/codex"),
        auth: BearerToken::new("t"),
        account_id: None,
    }))
    .await;

    let (body, content_type) = multipart_body(None);
    let _ = reqwest::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .expect("call-create");

    assert!(
        captures.last().headers.get("openai-alpha").is_none(),
        "the relay must not negotiate a protocol the client never asked for"
    );
}

#[tokio::test]
async fn only_content_type_and_location_come_back() {
    let (upstream, _captures) = start_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(config_for(UpstreamProfile::ApiKey {
        base_url: format!("{upstream}/v1"),
        auth: BearerToken::new("sk-test"),
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

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        response.headers().get("location").unwrap(),
        "/v1/realtime/calls/rtc_test_call"
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

    let mut config = config_for(UpstreamProfile::ApiKey {
        base_url: format!("{upstream}/v1"),
        auth: BearerToken::new("sk-test"),
    });
    config.upstream_timeout = std::time::Duration::from_millis(150);
    let proxy = start_proxy(config).await;

    let (body, content_type) = multipart_body(None);
    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/live"))
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

    let proxy = start_proxy(config_for(UpstreamProfile::ApiKey {
        base_url: format!("http://{dead}/v1"),
        auth: BearerToken::new("sk-test"),
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
    let mut config = config_for(UpstreamProfile::ApiKey {
        base_url: format!("{upstream}/v1"),
        auth: BearerToken::new("sk-test"),
    });
    config.max_body_bytes = 512;
    let proxy = start_proxy(config).await;

    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("content-type", "application/octet-stream")
        .body(vec![b'x'; 4096])
        .send()
        .await
        .expect("call-create");

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(captures.count(), 0);
}

#[tokio::test]
async fn a_get_on_the_call_create_path_is_not_found() {
    let (upstream, _captures) = start_upstream(UpstreamBehavior::default()).await;
    let proxy = start_proxy(config_for(UpstreamProfile::ApiKey {
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

    let mut config = config_for(UpstreamProfile::ApiKey {
        base_url: format!("{upstream}/v1"),
        auth: BearerToken::new("sk-test"),
    });
    config.max_response_bytes = 1024;
    let proxy = start_proxy(config).await;

    let (body, content_type) = multipart_body(None);
    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/live"))
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

    let mut config = config_for(UpstreamProfile::ApiKey {
        base_url: format!("{upstream}/v1"),
        auth: BearerToken::new("sk-test"),
    });
    config.max_response_bytes = 1024;
    let proxy = start_proxy(config).await;

    let (body, content_type) = multipart_body(None);
    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/live"))
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
    let proxy = start_proxy(config_for(UpstreamProfile::ApiKey {
        base_url: format!("{upstream}/v1"),
        auth: BearerToken::new("bad\nvalue"),
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

    let mut config = config_for(UpstreamProfile::ApiKey {
        base_url: format!("{upstream}/v1"),
        auth: BearerToken::new("sk-test"),
    });
    // Long enough that a timeout cannot be mistaken for a cancellation.
    config.upstream_timeout = std::time::Duration::from_secs(300);
    let proxy = start_proxy(config).await;

    let (body, content_type) = multipart_body(None);
    let handle = tokio::spawn(
        reqwest::Client::new()
            .post(format!("{proxy}/v1/live"))
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
