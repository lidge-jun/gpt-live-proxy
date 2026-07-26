//! Tracing setup and the credential-safe header renderer.

pub mod frame_log;

use http::HeaderMap;

pub use frame_log::{Direction, FrameLogger, FrameRecord};

/// Header names whose values are never rendered, whether or not the value was
/// marked sensitive at construction.
///
/// Besides direct bearer/API-key names, this includes account-routing,
/// idempotency, cookie, and WebSocket-subprotocol channels. The latter can carry
/// an `openai-insecure-api-key.*` token, so the whole value is treated as secret.
const CREDENTIAL_HEADERS: [&str; 13] = [
    "authorization",
    "chatgpt-account-id",
    "cookie",
    "idempotency-key",
    "openai-organization",
    "openai-project",
    "openai-safety-identifier",
    "proxy-authorization",
    "sec-websocket-protocol",
    "set-cookie",
    "x-api-key",
    "x-gpt-live-api-key",
    "x-oai-attestation",
];

/// The only sanctioned way to render headers.
///
/// A `HeaderMap`'s own `Debug` hides values marked sensitive, but relying on
/// that alone is fragile: a value constructed anywhere without `set_sensitive`
/// would print in full. This redacts by name as well, so both mechanisms have
/// to fail before a credential leaks.
pub fn redacted_headers(headers: &HeaderMap) -> String {
    let mut rendered: Vec<String> = headers
        .iter()
        .map(|(name, value)| {
            let name_str = name.as_str();
            let is_credential = value.is_sensitive()
                || CREDENTIAL_HEADERS
                    .iter()
                    .any(|candidate| candidate.eq_ignore_ascii_case(name_str));
            if is_credential {
                format!("{name_str}: <redacted>")
            } else {
                match value.to_str() {
                    Ok(value) => format!("{name_str}: {value}"),
                    Err(_) => format!("{name_str}: <non-utf8>"),
                }
            }
        })
        .collect();
    rendered.sort();
    format!("{{{}}}", rendered.join(", "))
}

/// Initialize tracing from `GPT_LIVE_LOG`, defaulting to `info`.
pub fn init_tracing() {
    use tracing_subscriber::prelude::*;

    let filter = tracing_subscriber::EnvFilter::try_from_env("GPT_LIVE_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let layer = tracing_subscriber::fmt::layer().with_filter(safe_tracing_filter(filter));
    let _ = tracing_subscriber::registry().with(layer).try_init();
}

/// User directives are advisory at the credential boundary. Tungstenite's
/// handshake and protocol targets can render complete serialized requests,
/// text messages, binary frame bytes, and close reasons. They are disabled by
/// an independent filter that a hostile `GPT_LIVE_LOG` value cannot override.
fn safe_tracing_filter<S>(
    user: tracing_subscriber::EnvFilter,
) -> impl tracing_subscriber::layer::Filter<S>
where
    S: tracing::Subscriber,
{
    use tracing_subscriber::filter::{filter_fn, FilterExt};

    filter_fn(|metadata| {
        let target = metadata.target();
        !(target == "tungstenite"
            || target.starts_with("tungstenite::")
            || target == "tokio_tungstenite"
            || target.starts_with("tokio_tungstenite::"))
    })
    .and(user)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use http::{HeaderName, HeaderValue};
    use std::sync::{Arc, Mutex, Once};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;
    use tokio_tungstenite::tungstenite::Message;
    use tracing_subscriber::prelude::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn credentials_are_redacted_by_name() {
        let map = headers(&[
            ("authorization", "Bearer super-secret"),
            ("chatgpt-account-id", "acct-secret"),
            ("cookie", "session=cookie-secret"),
            ("idempotency-key", "idem-secret"),
            ("openai-organization", "org-secret"),
            ("openai-project", "project-secret"),
            ("openai-safety-identifier", "safety-secret"),
            ("proxy-authorization", "Bearer proxy-auth-secret"),
            (
                "sec-websocket-protocol",
                "realtime, openai-insecure-api-key.ephemeral-secret",
            ),
            ("set-cookie", "session=set-cookie-secret"),
            ("x-api-key", "key-secret"),
            ("x-gpt-live-api-key", "admission-secret"),
            ("x-oai-attestation", "att-secret"),
            ("openai-alpha", "quicksilver=v2"),
        ]);
        let rendered = redacted_headers(&map);

        for secret in [
            "super-secret",
            "acct-secret",
            "cookie-secret",
            "idem-secret",
            "org-secret",
            "project-secret",
            "safety-secret",
            "proxy-auth-secret",
            "ephemeral-secret",
            "set-cookie-secret",
            "key-secret",
            "admission-secret",
            "att-secret",
        ] {
            assert!(!rendered.contains(secret), "{secret} leaked: {rendered}");
        }
        // Non-credential values remain visible, which is the point of the log.
        assert!(rendered.contains("quicksilver=v2"));
    }

    #[test]
    fn redaction_is_case_insensitive() {
        let map = headers(&[("Authorization", "Bearer secret")]);
        assert!(!redacted_headers(&map).contains("secret"));
    }

    /// Belt and braces: a value marked sensitive is redacted even under a name
    /// the list does not know about.
    #[test]
    fn a_sensitive_value_is_redacted_regardless_of_its_name() {
        let mut map = HeaderMap::new();
        let mut value = HeaderValue::from_static("unexpected-secret");
        value.set_sensitive(true);
        map.insert(HeaderName::from_static("x-something-custom"), value);
        assert!(!redacted_headers(&map).contains("unexpected-secret"));
    }

    #[test]
    fn a_non_utf8_value_is_labelled_not_printed() {
        let mut map = HeaderMap::new();
        map.insert(
            HeaderName::from_static("x-weird"),
            HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
        );
        assert!(redacted_headers(&map).contains("<non-utf8>"));
    }

    #[test]
    fn the_rendering_is_stable() {
        let map = headers(&[("b-header", "2"), ("a-header", "1")]);
        // Sorted, so a log line does not churn between runs.
        assert_eq!(redacted_headers(&map), "{a-header: 1, b-header: 2}");
    }

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // The callback signature is fixed by tungstenite and its error response is
    // intentionally an HTTP response, so boxing it locally would not narrow
    // the dependency API exercised by this test.
    #[allow(clippy::result_large_err)]
    fn select_trace_protocol(
        request: &Request,
        mut response: Response,
    ) -> Result<Response, ErrorResponse> {
        if request
            .headers()
            .get(http::header::SEC_WEBSOCKET_PROTOCOL)
            .is_some_and(|value| {
                value
                    .to_str()
                    .is_ok_and(|value| value.contains("browser-trace-canary"))
            })
        {
            response.headers_mut().insert(
                http::header::SEC_WEBSOCKET_PROTOCOL,
                HeaderValue::from_static("realtime"),
            );
        }
        Ok(response)
    }

    #[test]
    fn hostile_directives_cannot_enable_tungstenite_client_handshakes() {
        let captured = Captured::default();
        let sink = captured.clone();
        let user = tracing_subscriber::EnvFilter::new("trace,tungstenite::handshake::client=trace");
        let layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(move || sink.clone())
            .with_filter(safe_tracing_filter(user));
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::trace!(
                target: "tungstenite::handshake::client",
                "GET / HTTP/1.1\r\nAuthorization: Bearer trace-canary\r\n"
            );
            tracing::trace!(
                target: "tungstenite::handshake::client::machine",
                "openai-insecure-api-key.child-canary"
            );
            tracing::trace!(
                target: "tungstenite::protocol",
                "frame-text-canary"
            );
            tracing::trace!(
                target: "tungstenite::protocol::frame",
                "binary-hex-canary"
            );
            tracing::trace!(
                target: "tokio_tungstenite::compat",
                "close-reason-canary"
            );
            tracing::trace!(target: "gpt_live_proxy::safe", "safe-target-survives");
        });

        let output = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert!(output.contains("safe-target-survives"));
        assert!(
            !output.contains("trace-canary"),
            "handshake leaked: {output}"
        );
        assert!(
            !output.contains("child-canary"),
            "child target leaked: {output}"
        );
        for canary in [
            "frame-text-canary",
            "binary-hex-canary",
            "close-reason-canary",
        ] {
            assert!(!output.contains(canary), "dependency log leaked: {output}");
        }
        assert!(!output.contains("tungstenite"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hostile_trace_cannot_log_real_websocket_credentials_or_frames() {
        static LOG_TRACER: Once = Once::new();
        LOG_TRACER.call_once(|| {
            tracing_log::LogTracer::init().expect("install log-to-tracing bridge");
        });

        let captured = Captured::default();
        let sink = captured.clone();
        let hostile =
            tracing_subscriber::EnvFilter::new("trace,tungstenite=trace,tokio_tungstenite=trace");
        let layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(move || sink.clone())
            .with_filter(safe_tracing_filter(hostile));
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        // This proves LogTracer is active and the filter still admits a safe
        // dependency-style target. The WebSocket operations below therefore
        // exercise real log records rather than merely relying on silence.
        log::trace!(
            target: "gpt_live_proxy::safe_dependency",
            "log-bridge-safe-marker"
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind trace websocket server");
        let address = listener.local_addr().expect("trace websocket address");
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let (stream, _) = listener.accept().await.expect("accept trace websocket");
                let mut socket = tokio_tungstenite::accept_hdr_async(stream, select_trace_protocol)
                    .await
                    .expect("accept websocket handshake");

                for _ in 0..2 {
                    let message = socket
                        .next()
                        .await
                        .expect("trace websocket frame")
                        .expect("valid trace websocket frame");
                    socket.send(message).await.expect("echo trace frame");
                }
                let close = socket
                    .next()
                    .await
                    .expect("trace close frame")
                    .expect("valid trace close frame");
                assert!(matches!(close, Message::Close(_)));
            }
        });

        for (mode, credential_header) in [
            (
                "managed",
                (http::header::AUTHORIZATION, "Bearer managed-trace-canary"),
            ),
            (
                "client",
                (http::header::AUTHORIZATION, "Bearer client-trace-canary"),
            ),
            (
                "browser",
                (
                    http::header::SEC_WEBSOCKET_PROTOCOL,
                    "realtime, openai-insecure-api-key.browser-trace-canary",
                ),
            ),
        ] {
            let mut request = format!("ws://{address}/v1/realtime?model={mode}")
                .into_client_request()
                .expect("trace client request");
            request.headers_mut().insert(
                credential_header.0,
                HeaderValue::from_str(credential_header.1).expect("trace credential header"),
            );
            let (mut socket, _) = tokio_tungstenite::connect_async(request)
                .await
                .expect("trace websocket client");

            let text = format!("text-frame-{mode}-trace-canary");
            socket
                .send(Message::Text(text.clone().into()))
                .await
                .expect("send trace text");
            assert!(matches!(
                socket.next().await.expect("text echo").expect("valid text echo"),
                Message::Text(value) if value.as_str() == text
            ));

            let binary = format!("binary-frame-{mode}-trace-canary").into_bytes();
            socket
                .send(Message::Binary(binary.clone().into()))
                .await
                .expect("send trace binary");
            assert!(matches!(
                socket.next().await.expect("binary echo").expect("valid binary echo"),
                Message::Binary(value) if value.as_ref() == binary.as_slice()
            ));

            socket
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Normal,
                    reason: format!("close-{mode}-trace-canary").into(),
                })))
                .await
                .expect("send trace close");
            let _ = socket.next().await;
        }
        server.await.expect("trace websocket server");

        let output = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert!(output.contains("log-bridge-safe-marker"), "{output}");
        for canary in [
            "managed-trace-canary",
            "client-trace-canary",
            "browser-trace-canary",
            "text-frame-managed-trace-canary",
            "text-frame-client-trace-canary",
            "text-frame-browser-trace-canary",
            "binary-frame-managed-trace-canary",
            "binary-frame-client-trace-canary",
            "binary-frame-browser-trace-canary",
            "close-managed-trace-canary",
            "close-client-trace-canary",
            "close-browser-trace-canary",
        ] {
            assert!(!output.contains(canary), "dependency log leaked: {output}");
        }
        assert!(
            !output.contains("tungstenite"),
            "dependency target leaked: {output}"
        );
    }
}
