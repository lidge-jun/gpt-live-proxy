//! The call-create handler.
//!
//! Cancellation ownership is the subtle part. The shared HTTP exchange owns the
//! spawned send/read work while this handler keeps only Live-specific policy.

use axum::body::Body;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use http::{HeaderMap, HeaderName, HeaderValue, Method};

use crate::app::AppState;
use crate::error::RelayError;
use crate::live::{body, headers, url};
use crate::relay::body::read_capped;
use crate::relay::http::{
    begin_exchange, spawn_execute, ExchangeLifecycle, ExchangeTerminal, OpaqueResponse,
};
use crate::wire::WireAdapter;

/// Response headers relayed back downstream. Everything else — cookies, request
/// ids, cache and retry headers — is dropped.
const RELAY_HEADERS: [&str; 2] = ["content-type", "location"];

const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";

/// Legacy `POST /v1/live`, also used by recognized private call-create dialects.
#[tracing::instrument(
    name = "call_create",
    skip_all,
    fields(
        method = %method,
        path = %path,
        upstream = tracing::field::Empty,
        status = tracing::field::Empty,
        elapsed_ms = tracing::field::Empty,
        outcome = tracing::field::Empty
    )
)]
pub async fn handle_call_create(
    State(state): State<AppState>,
    method: Method,
    path: RequestPath,
    request_headers: HeaderMap,
    request_body: Body,
) -> Response {
    let started = std::time::Instant::now();
    let config = state.config.clone();

    // Recorded at entry, not after the body read: the configured host is
    // already known, so an early failure must not leave the span incomplete.
    tracing::Span::current().record(
        "upstream",
        upstream_host_of(config.upstream.base_url()).as_str(),
    );
    let inbound_content_type = request_headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(DEFAULT_CONTENT_TYPE)
        .to_string();

    // Observed only; never written back into the outgoing map.
    let adapter = request_headers
        .get("openai-alpha")
        .and_then(|v| v.to_str().ok())
        .and_then(WireAdapter::from_openai_alpha);
    let backend_shape = config.upstream.uses_backend_shape();
    let keyed = config.upstream.is_keyed();
    let mut upstream_headers =
        match headers::merge_upstream_headers(&request_headers, &config.upstream, adapter) {
            Ok(headers) => headers,
            Err(err) => return failed(err, started, ExchangeTerminal::Failed),
        };

    // Installed before the body read, so a client that vanishes mid-body is
    // observed by the same mechanism as one that vanishes mid-response.
    let (lifecycle, _guard) = begin_exchange();
    debug_assert_eq!(method, Method::POST);

    let permit = match state.active_requests.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            lifecycle.finish(ExchangeTerminal::Failed);
            return failed(
                RelayError::TooManyActiveRealtimeRequests,
                started,
                lifecycle.terminal(),
            );
        }
    };

    let body_bytes = match tokio::time::timeout(
        config.limits.request_read_timeout,
        read_capped(request_body, config.limits.request_bytes),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(err)) => {
            finish_error(&lifecycle, &err);
            return failed(err, started, lifecycle.terminal());
        }
        Err(_) => {
            let err = RelayError::RealtimeRequestBodyTimeout;
            finish_error(&lifecycle, &err);
            return failed(err, started, lifecycle.terminal());
        }
    };

    let (outbound_body, outbound_content_type) =
        if !keyed && backend_shape && body::is_multipart(&inbound_content_type) {
            match body::backend_json_from_multipart(body_bytes, &inbound_content_type).await {
                Ok((rewritten, content_type)) => (rewritten, content_type.to_string()),
                Err(err) => {
                    finish_error(&lifecycle, &err);
                    return failed(err, started, lifecycle.terminal());
                }
            }
        } else {
            // The keyed path forwards the original bytes and boundary verbatim.
            (body_bytes, inbound_content_type)
        };

    let target = if keyed {
        url::keyed_call_create_url(config.upstream.base_url())
    } else {
        url::forward_call_create_url(config.upstream.base_url(), backend_shape, adapter)
    };

    if let Ok(value) = HeaderValue::from_str(&outbound_content_type) {
        // Proxy-owned, applied last so neither client nor provider can override it.
        upstream_headers.insert(http::header::CONTENT_TYPE, value);
    }

    let upstream_host_for_log = upstream_host_of(&target);
    let request = match state
        .http
        .request(method, target)
        .headers(upstream_headers)
        .body(outbound_body)
        .build()
    {
        Ok(request) => request,
        Err(err) => {
            let err = RelayError::UpstreamFailed(err.to_string());
            finish_error(&lifecycle, &err);
            return failed(err, started, lifecycle.terminal());
        }
    };
    let task = spawn_execute(
        state.http.clone(),
        request,
        config.limits.response_bytes,
        config.limits.upstream_timeout,
        lifecycle.clone(),
        permit,
    );

    // Already recorded at entry from the configured base. Re-record only if the
    // resolved target actually differs, so the span carries one value rather
    // than the same host twice.
    if upstream_host_for_log != upstream_host_of(config.upstream.base_url()) {
        tracing::Span::current().record("upstream", upstream_host_for_log.as_str());
    }

    match task.await {
        Ok(Ok(result)) => {
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let span = tracing::Span::current();
            span.record("status", result.status.as_u16());
            span.record("elapsed_ms", elapsed_ms);
            span.record("outcome", terminal_label(lifecycle.terminal()));
            tracing::info!(
                status = result.status.as_u16(),
                elapsed_ms,
                "call-create completed"
            );
            relay_response(result)
        }
        Ok(Err(err)) => failed(err, started, lifecycle.terminal()),
        // The task was cancelled or panicked; the guard already recorded why,
        // but the response still gets its log line like every other path.
        Err(_) => {
            lifecycle.finish(ExchangeTerminal::Failed);
            failed(RelayError::ClientCanceled, started, lifecycle.terminal())
        }
    }
}

/// The inbound path, extracted so the span records the route that was actually
/// requested rather than a hardcoded one.
pub struct RequestPath(String);

impl From<String> for RequestPath {
    fn from(path: String) -> Self {
        Self(path)
    }
}

impl std::fmt::Display for RequestPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for RequestPath {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(parts.uri.path().to_string()))
    }
}

/// Emit the failure event and the response together, so no early return can
/// produce a response without a corresponding log line.
fn finish_error(lifecycle: &ExchangeLifecycle, err: &RelayError) {
    let terminal = match err {
        RelayError::ClientCanceled => ExchangeTerminal::Canceled,
        RelayError::UpstreamTimeout => ExchangeTerminal::TimedOut,
        _ => ExchangeTerminal::Failed,
    };
    lifecycle.finish(terminal);
}

fn terminal_label(terminal: ExchangeTerminal) -> &'static str {
    match terminal {
        ExchangeTerminal::InFlight => "in_flight",
        ExchangeTerminal::Completed => "completed",
        ExchangeTerminal::Failed => "failed",
        ExchangeTerminal::TimedOut => "timed_out",
        ExchangeTerminal::Canceled => "client_canceled",
    }
}

fn failed(err: RelayError, started: std::time::Instant, terminal: ExchangeTerminal) -> Response {
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let span = tracing::Span::current();
    span.record("status", err.status().as_u16());
    span.record("elapsed_ms", elapsed_ms);
    span.record("outcome", terminal_label(terminal));
    tracing::warn!(
        status = err.status().as_u16(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        // The error CODE, not its message: a message can embed upstream text.
        error = err.error_code(),
        "call-create failed"
    );
    err.into_response()
}

/// Host and port only. The full URL can carry a call id or other identifiers,
/// so it never reaches a log line.
pub fn upstream_host_of(url: &str) -> String {
    // Parsed, not split: a split would happily log `user:secret@host`.
    match reqwest::Url::parse(url) {
        Ok(parsed) => match (parsed.host_str(), parsed.port()) {
            (Some(host), Some(port)) => format!("{host}:{port}"),
            (Some(host), None) => host.to_string(),
            (None, _) => "unknown".to_string(),
        },
        Err(_) => "unknown".to_string(),
    }
}

fn relay_response(result: OpaqueResponse) -> Response {
    let mut response = Response::new(Body::from(result.body));
    *response.status_mut() = result.status;
    for name in RELAY_HEADERS {
        if let Some(value) = result.headers.get(name) {
            if !value.is_empty() {
                if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
                    response.headers_mut().insert(name, value.clone());
                }
            }
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http::StatusCode;

    #[test]
    fn only_the_two_relay_headers_are_listed() {
        assert_eq!(RELAY_HEADERS, ["content-type", "location"]);
    }

    #[test]
    fn a_url_with_userinfo_never_reaches_a_log_line() {
        // Splitting on "://" and "/" would return `user:secret@host`.
        assert_eq!(
            upstream_host_of("https://user:secret@api.openai.com/v1/realtime/calls"),
            "api.openai.com"
        );
        assert_eq!(
            upstream_host_of("http://127.0.0.1:8080/v1/live"),
            "127.0.0.1:8080"
        );
        assert_eq!(upstream_host_of("not a url"), "unknown");
    }

    #[test]
    fn the_host_excludes_the_path_and_query() {
        // The path carries the call id and the query can carry identifiers.
        assert_eq!(
            upstream_host_of("https://api.openai.com/v1/realtime?call_id=rtc_secret"),
            "api.openai.com"
        );
    }

    #[test]
    fn shared_terminals_keep_live_span_labels_stable() {
        assert_eq!(terminal_label(ExchangeTerminal::InFlight), "in_flight");
        assert_eq!(terminal_label(ExchangeTerminal::Completed), "completed");
        assert_eq!(terminal_label(ExchangeTerminal::Failed), "failed");
        assert_eq!(terminal_label(ExchangeTerminal::TimedOut), "timed_out");
        assert_eq!(
            terminal_label(ExchangeTerminal::Canceled),
            "client_canceled"
        );
    }

    #[test]
    fn private_response_policy_keeps_only_content_type_and_location() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/sdp"),
        );
        headers.insert(
            http::header::LOCATION,
            HeaderValue::from_static("/v1/realtime/calls/rtc_test"),
        );
        headers.insert(
            http::header::SET_COOKIE,
            HeaderValue::from_static("private=leak"),
        );

        let response = relay_response(OpaqueResponse {
            status: StatusCode::CREATED,
            headers,
            body: Bytes::from_static(b"answer"),
        });

        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response.headers()[http::header::CONTENT_TYPE],
            "application/sdp"
        );
        assert_eq!(
            response.headers()[http::header::LOCATION],
            "/v1/realtime/calls/rtc_test"
        );
        assert!(!response.headers().contains_key(http::header::SET_COOKIE));
    }
}
