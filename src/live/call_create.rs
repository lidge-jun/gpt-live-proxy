//! The call-create handler.
//!
//! Cancellation ownership is the subtle part. If the upstream call lived inside
//! the handler future, a client disconnect would drop that future and it could
//! never observe its own cancellation — so the work is spawned, and a guard the
//! handler owns records the outcome when it drops.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use tokio_util::sync::CancellationToken;

use crate::app::AppState;
use crate::config::Config;
use crate::error::RelayError;
use crate::live::{body, headers, url};
use crate::relay::body::read_capped;
use crate::wire::WireAdapter;

/// Response headers relayed back downstream. Everything else — cookies, request
/// ids, cache and retry headers — is dropped.
const RELAY_HEADERS: [&str; 2] = ["content-type", "location"];

const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    InFlight,
    Completed(u16),
    TimedOut,
    Failed(String),
    ClientCanceled,
}

/// A one-shot outcome slot. Terminal transitions apply only from `InFlight`, so
/// whichever side finishes first wins and a late completion can never overwrite
/// a recorded cancellation.
#[derive(Debug, Clone, Default)]
pub struct CallOutcome(Arc<Mutex<Option<Outcome>>>);

impl CallOutcome {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Some(Outcome::InFlight))))
    }

    /// Returns true when this call actually performed the transition.
    pub fn finish(&self, next: Outcome) -> bool {
        let Ok(mut slot) = self.0.lock() else {
            return false;
        };
        if matches!(slot.as_ref(), Some(Outcome::InFlight)) {
            *slot = Some(next);
            return true;
        }
        false
    }

    pub fn get(&self) -> Outcome {
        self.0
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
            .unwrap_or(Outcome::InFlight)
    }
}

/// Owns both halves of cancellation: it records the outcome and cancels the
/// token, so a dropped handler future actually stops the spawned work.
pub struct OutcomeGuard {
    slot: CallOutcome,
    token: CancellationToken,
}

impl OutcomeGuard {
    pub fn new(slot: CallOutcome, token: CancellationToken) -> Self {
        Self { slot, token }
    }
}

impl Drop for OutcomeGuard {
    fn drop(&mut self) {
        if self.slot.finish(Outcome::ClientCanceled) {
            self.token.cancel();
        }
    }
}

/// A fully materialized upstream response: nothing droppable is left behind.
pub struct UpstreamResult {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// `POST /v1/live` and `POST /v1/realtime/calls`.
///
/// Both paths are handled identically; `/v1/realtime/calls` is the public
/// Realtime alias.
#[tracing::instrument(
    name = "call_create",
    skip_all,
    fields(
        method = %method,
        path = %path,
        upstream = tracing::field::Empty,
        status = tracing::field::Empty,
        elapsed_ms = tracing::field::Empty
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

    let slot = CallOutcome::new();
    let token = CancellationToken::new();
    // Installed before the body read, so a client that vanishes mid-body is
    // observed by the same mechanism as one that vanishes mid-response.
    let _guard = OutcomeGuard::new(slot.clone(), token.clone());

    debug_assert_eq!(method, Method::POST);

    let body_bytes = match read_capped(request_body, config.limits.request_bytes).await {
        Ok(bytes) => bytes,
        Err(err) => {
            slot.finish(Outcome::Failed(err.message()));
            return failed(err, started);
        }
    };

    // Observed only; never written back into the outgoing map.
    let adapter = request_headers
        .get("openai-alpha")
        .and_then(|v| v.to_str().ok())
        .and_then(WireAdapter::from_openai_alpha);

    let backend_shape = config.upstream.uses_backend_shape();
    let keyed = config.upstream.is_keyed();

    let (outbound_body, outbound_content_type) =
        if !keyed && backend_shape && body::is_multipart(&inbound_content_type) {
            match body::backend_json_from_multipart(body_bytes, &inbound_content_type).await {
                Ok((rewritten, content_type)) => (rewritten, content_type.to_string()),
                Err(err) => {
                    slot.finish(Outcome::Failed(err.message()));
                    return failed(err, started);
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

    let mut upstream_headers =
        match headers::merge_upstream_headers(&request_headers, &config.upstream, adapter) {
            Ok(headers) => headers,
            Err(err) => {
                slot.finish(Outcome::Failed(err.message()));
                return failed(err, started);
            }
        };
    if let Ok(value) = HeaderValue::from_str(&outbound_content_type) {
        // Proxy-owned, applied last so neither client nor provider can override it.
        upstream_headers.insert(http::header::CONTENT_TYPE, value);
    }

    let upstream_host_for_log = upstream_host_of(&target);
    let task = spawn_upstream(
        state.http.clone(),
        config.clone(),
        target,
        upstream_headers,
        outbound_body,
        slot.clone(),
        token,
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
            tracing::info!(
                status = result.status.as_u16(),
                elapsed_ms,
                "call-create completed"
            );
            relay_response(result)
        }
        Ok(Err(err)) => failed(err, started),
        // The task was cancelled or panicked; the guard already recorded why,
        // but the response still gets its log line like every other path.
        Err(_) => failed(RelayError::ClientCanceled, started),
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_upstream(
    client: reqwest::Client,
    config: Arc<Config>,
    target: String,
    headers: HeaderMap,
    body: Bytes,
    slot: CallOutcome,
    token: CancellationToken,
) -> tokio::task::JoinHandle<Result<UpstreamResult, RelayError>> {
    tokio::spawn(async move {
        let work = async {
            let response = client
                .post(&target)
                .headers(headers)
                .body(body)
                .send()
                .await
                .map_err(|err| RelayError::UpstreamFailed(err.to_string()))?;

            let status = response.status();
            let mut relayed = HeaderMap::new();
            for name in RELAY_HEADERS {
                if let Some(value) = response.headers().get(name) {
                    if !value.is_empty() {
                        if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
                            relayed.insert(name, value.clone());
                        }
                    }
                }
            }

            // Buffering belongs to this task: leaving it in the handler would
            // put a droppable await back on the cancellation path.
            //
            // Read incrementally and stop the moment the cap is crossed. Calling
            // `bytes()` first would buffer an unbounded response into memory
            // before deciding to reject it, which is a denial-of-service rather
            // than a 502.
            let mut buffer = bytes::BytesMut::new();
            let mut response = response;
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|err| RelayError::UpstreamFailed(err.to_string()))?
            {
                // Empty data frames carry no payload but would still grow a
                // per-chunk vector, so they are skipped rather than retained.
                if chunk.is_empty() {
                    continue;
                }
                if buffer.len().saturating_add(chunk.len()) > config.limits.response_bytes {
                    return Err(RelayError::ResponseTooLarge(
                        buffer.len().saturating_add(chunk.len()),
                    ));
                }
                // One growing buffer, not a vector of chunks: retaining each
                // chunk separately lets many tiny frames cost far more in
                // metadata than the cap allows in payload.
                buffer.extend_from_slice(&chunk);
            }
            let bytes = buffer.freeze();

            Ok(UpstreamResult {
                status,
                headers: relayed,
                body: bytes,
            })
        };

        tokio::select! {
            biased;
            () = token.cancelled() => Err(RelayError::ClientCanceled),
            result = tokio::time::timeout(config.limits.upstream_timeout, work) => match result {
                Ok(Ok(result)) => {
                    slot.finish(Outcome::Completed(result.status.as_u16()));
                    Ok(result)
                }
                Ok(Err(err)) => {
                    slot.finish(Outcome::Failed(err.message()));
                    Err(err)
                }
                Err(_) => {
                    slot.finish(Outcome::TimedOut);
                    Err(RelayError::UpstreamTimeout)
                }
            },
        }
    })
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
fn failed(err: RelayError, started: std::time::Instant) -> Response {
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let span = tracing::Span::current();
    span.record("status", err.status().as_u16());
    span.record("elapsed_ms", elapsed_ms);
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

fn relay_response(result: UpstreamResult) -> Response {
    let mut response = Response::new(Body::from(result.body));
    *response.status_mut() = result.status;
    *response.headers_mut() = result.headers;
    response
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn the_first_terminal_transition_wins() {
        let slot = CallOutcome::new();
        assert!(slot.finish(Outcome::Completed(201)));
        assert!(!slot.finish(Outcome::ClientCanceled));
        assert_eq!(slot.get(), Outcome::Completed(201));
    }

    #[test]
    fn a_cancellation_cannot_be_overwritten_by_a_late_completion() {
        let slot = CallOutcome::new();
        assert!(slot.finish(Outcome::ClientCanceled));
        assert!(!slot.finish(Outcome::Completed(201)));
        assert_eq!(slot.get(), Outcome::ClientCanceled);
    }

    #[test]
    fn dropping_the_guard_records_cancellation_and_cancels_the_token() {
        let slot = CallOutcome::new();
        let token = CancellationToken::new();
        {
            let _guard = OutcomeGuard::new(slot.clone(), token.clone());
        }
        assert_eq!(slot.get(), Outcome::ClientCanceled);
        assert!(token.is_cancelled());
    }

    #[test]
    fn dropping_the_guard_after_completion_is_a_no_op() {
        let slot = CallOutcome::new();
        let token = CancellationToken::new();
        {
            let _guard = OutcomeGuard::new(slot.clone(), token.clone());
            slot.finish(Outcome::Completed(201));
        }
        assert_eq!(slot.get(), Outcome::Completed(201));
        assert!(
            !token.is_cancelled(),
            "a completed call must not cancel its own token on drop"
        );
    }
}
