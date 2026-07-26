//! The error taxonomy and its wire rendering.
//!
//! Every status / message / `type` / `code` combination here is pinned by
//! `docs/001_research-opencodex-relay.md` §10. Those literals are the contract:
//! a client distinguishes failure modes by them, so they are not free to drift.

use axum::response::{IntoResponse, Response};
use axum::Json;
use http::{header, Method, StatusCode};
use serde_json::json;

/// Which surface rejected a request. The two origin rejections carry different
/// exact messages, so a payload-free variant could not select between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Http,
    WebSocketUpgrade,
}

#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    // ---- request body ----
    #[error("live request body too large")]
    BodyTooLarge,
    #[error("live response too large ({0} bytes)")]
    ResponseTooLarge(usize),
    #[error("live request canceled by client")]
    ClientCanceled,
    #[error("live request body unreadable: {0}")]
    BodyUnreadable(String),

    // ---- multipart rewrite ----
    #[error("ChatGPT voice relay could not parse multipart call-create body")]
    MultipartParse,
    #[error("ChatGPT voice relay expects multipart field sdp on call-create")]
    MultipartMissingSdp,
    #[error("ChatGPT voice relay expected a string multipart session field")]
    MultipartSessionNotString,
    #[error("ChatGPT voice relay expected JSON in the multipart session field")]
    MultipartSessionNotJson,

    // ---- upstream ----
    #[error("Built-in voice needs an OpenAI upstream (ChatGPT login or an OpenAI API-key provider), but none is configured. Routed providers cannot serve voice call-create.")]
    NoUpstream,
    #[error("voice relay needs ChatGPT auth (Authorization header) or an OpenAI API-key provider")]
    NoCredential,
    #[error("live upstream timed out")]
    UpstreamTimeout,
    #[error("live relay failed: {0}")]
    UpstreamFailed(String),

    // ---- trust boundary (docs/015) ----
    #[error("gpt-live-proxy API key required")]
    AdmissionRequired,
    #[error("gpt-live-proxy admission credentials cannot be forwarded upstream")]
    AdmissionSecretNotForwardable,
    /// A repeated `Authorization` header. Ambiguous for a relay that must forward
    /// exactly one credential, and a duplicate-header bypass vector.
    #[error("ambiguous Authorization: send exactly one credential")]
    AmbiguousAuthorization,
    #[error("invalid or repeated Realtime header")]
    InvalidRealtimeHeader,
    #[error("invalid Realtime call_id")]
    InvalidRealtimeCallId,
    #[error("unsupported Realtime content type")]
    UnsupportedRealtimeContentType,
    #[error("Realtime operation is not supported by the configured upstream profile")]
    UnsupportedRealtimeCapability,
    #[error("Realtime request body timed out")]
    RealtimeRequestBodyTimeout,
    #[error("too many active Realtime requests")]
    TooManyActiveRealtimeRequests,
    #[error("origin rejected")]
    OriginBlocked(RequestKind),
    #[error("Service shutting down")]
    Draining,

    // ---- routing ----
    #[error("WebSocket upgrade failed")]
    UpgradeFailed,
    #[error("Unknown endpoint: {method} {path}")]
    UnknownEndpoint { method: String, path: String },
}

impl RelayError {
    pub fn from_rest_contract(
        error: crate::realtime::contract::RestContractError,
        method: &Method,
        path: &str,
    ) -> Self {
        use crate::realtime::contract::RestContractError;

        match error {
            RestContractError::UnknownRoute | RestContractError::MethodNotAllowed => {
                Self::UnknownEndpoint {
                    method: method.to_string(),
                    path: path.to_string(),
                }
            }
            RestContractError::InvalidCallId => Self::InvalidRealtimeCallId,
            RestContractError::UnsupportedContentType => Self::UnsupportedRealtimeContentType,
            RestContractError::PrivateDialectRequiresManaged
            | RestContractError::PrivateDialectNotSupported => Self::UnsupportedRealtimeCapability,
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::BodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::ResponseTooLarge(_) | Self::UpstreamFailed(_) => StatusCode::BAD_GATEWAY,
            // 499 is not an IANA status; the source emits it and clients key off it.
            Self::ClientCanceled => StatusCode::from_u16(499).expect("499 is a valid status code"),
            Self::BodyUnreadable(_)
            | Self::MultipartParse
            | Self::MultipartMissingSdp
            | Self::MultipartSessionNotString
            | Self::MultipartSessionNotJson
            | Self::NoUpstream => StatusCode::BAD_REQUEST,
            Self::AmbiguousAuthorization
            | Self::InvalidRealtimeHeader
            | Self::InvalidRealtimeCallId
            | Self::UnsupportedRealtimeContentType
            | Self::UnsupportedRealtimeCapability => StatusCode::BAD_REQUEST,
            Self::RealtimeRequestBodyTimeout => StatusCode::REQUEST_TIMEOUT,
            Self::TooManyActiveRealtimeRequests => StatusCode::TOO_MANY_REQUESTS,
            Self::UpstreamTimeout => StatusCode::GATEWAY_TIMEOUT,
            Self::AdmissionRequired | Self::AdmissionSecretNotForwardable | Self::NoCredential => {
                StatusCode::UNAUTHORIZED
            }
            Self::OriginBlocked(_) => StatusCode::FORBIDDEN,
            Self::Draining => StatusCode::SERVICE_UNAVAILABLE,
            Self::UpgradeFailed => StatusCode::UPGRADE_REQUIRED,
            Self::UnknownEndpoint { .. } => StatusCode::NOT_FOUND,
        }
    }

    /// The `type` field of the JSON envelope.
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::AdmissionRequired | Self::AdmissionSecretNotForwardable | Self::NoCredential => {
                "authentication_error"
            }
            Self::TooManyActiveRealtimeRequests => "rate_limit_error",
            Self::ResponseTooLarge(_) | Self::UpstreamFailed(_) | Self::UpstreamTimeout => {
                "server_error"
            }
            _ => "invalid_request_error",
        }
    }

    /// The `code` field of the JSON envelope.
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::ClientCanceled => "client_closed_request",
            Self::ResponseTooLarge(_) | Self::UpstreamFailed(_) | Self::UpstreamTimeout => {
                "upstream_server_error"
            }
            Self::AdmissionRequired | Self::AdmissionSecretNotForwardable | Self::NoCredential => {
                "invalid_api_key"
            }
            // The source passes an internal `origin_rejected` token that `classifyError`
            // intercepts ahead of the generic permission branch, so the wire values are
            // `invalid_request_error` / `origin_rejected` (docs/001 §10).
            Self::OriginBlocked(_) => "origin_rejected",
            Self::UpgradeFailed => "upgrade_required",
            Self::InvalidRealtimeCallId => "invalid_call_id",
            Self::UnsupportedRealtimeCapability => "unsupported_realtime_capability",
            Self::RealtimeRequestBodyTimeout => "request_timeout",
            Self::TooManyActiveRealtimeRequests => "rate_limit_exceeded",
            _ => "invalid_request_error",
        }
    }

    /// The exact `message` string. `OriginBlocked` differs by surface.
    pub fn message(&self) -> String {
        match self {
            Self::OriginBlocked(RequestKind::Http) => {
                "cross-origin data-plane request blocked".to_string()
            }
            Self::OriginBlocked(RequestKind::WebSocketUpgrade) => {
                "WebSocket upgrade blocked: non-local Origin".to_string()
            }
            other => other.to_string(),
        }
    }
}

impl IntoResponse for RelayError {
    fn into_response(self) -> Response {
        // Draining is plain text with Retry-After, not a JSON envelope.
        if matches!(self, Self::Draining) {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [
                    (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                    (header::RETRY_AFTER, "5"),
                ],
                "Service shutting down",
            )
                .into_response();
        }

        let retry_after = matches!(self, Self::TooManyActiveRealtimeRequests);
        let body = json!({
            "error": {
                "message": self.message(),
                "type": self.error_type(),
                "code": self.error_code(),
            }
        });
        let mut response = (self.status(), Json(body)).into_response();
        if retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, http::HeaderValue::from_static("1"));
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn rendered(err: RelayError) -> (StatusCode, serde_json::Value) {
        let res = err.into_response();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), 64 * 1024).await.expect("body");
        let json = serde_json::from_slice(&bytes).expect("error responses are JSON");
        (status, json)
    }

    /// The contract table from docs/001 §10. Both the wire test and the coverage
    /// test read this one definition, so they cannot drift apart.
    fn contract_rows() -> Vec<(RelayError, u16, &'static str, &'static str, &'static str)> {
        vec![
            (
                RelayError::BodyTooLarge,
                413,
                "live request body too large",
                "invalid_request_error",
                "invalid_request_error",
            ),
            (
                RelayError::ResponseTooLarge(17_000_000),
                502,
                "live response too large (17000000 bytes)",
                "server_error",
                "upstream_server_error",
            ),
            (
                RelayError::ClientCanceled,
                499,
                "live request canceled by client",
                "invalid_request_error",
                "client_closed_request",
            ),
            (
                RelayError::BodyUnreadable("stream reset".into()),
                400,
                "live request body unreadable: stream reset",
                "invalid_request_error",
                "invalid_request_error",
            ),
            (
                RelayError::MultipartParse,
                400,
                "ChatGPT voice relay could not parse multipart call-create body",
                "invalid_request_error",
                "invalid_request_error",
            ),
            (
                RelayError::MultipartMissingSdp,
                400,
                "ChatGPT voice relay expects multipart field sdp on call-create",
                "invalid_request_error",
                "invalid_request_error",
            ),
            (
                RelayError::MultipartSessionNotString,
                400,
                "ChatGPT voice relay expected a string multipart session field",
                "invalid_request_error",
                "invalid_request_error",
            ),
            (
                RelayError::MultipartSessionNotJson,
                400,
                "ChatGPT voice relay expected JSON in the multipart session field",
                "invalid_request_error",
                "invalid_request_error",
            ),
            (
                RelayError::UpstreamTimeout,
                504,
                "live upstream timed out",
                "server_error",
                "upstream_server_error",
            ),
            (
                RelayError::NoUpstream,
                400,
                "Built-in voice needs an OpenAI upstream (ChatGPT login or an OpenAI API-key provider), but none is configured. Routed providers cannot serve voice call-create.",
                "invalid_request_error",
                "invalid_request_error",
            ),
            (
                RelayError::NoCredential,
                401,
                "voice relay needs ChatGPT auth (Authorization header) or an OpenAI API-key provider",
                "authentication_error",
                "invalid_api_key",
            ),
            (
                RelayError::UpstreamFailed("connection refused".into()),
                502,
                "live relay failed: connection refused",
                "server_error",
                "upstream_server_error",
            ),
            (
                RelayError::AdmissionRequired,
                401,
                "gpt-live-proxy API key required",
                "authentication_error",
                "invalid_api_key",
            ),
            (
                RelayError::AdmissionSecretNotForwardable,
                401,
                "gpt-live-proxy admission credentials cannot be forwarded upstream",
                "authentication_error",
                "invalid_api_key",
            ),
            (
                RelayError::OriginBlocked(RequestKind::Http),
                403,
                "cross-origin data-plane request blocked",
                "invalid_request_error",
                "origin_rejected",
            ),
            (
                RelayError::OriginBlocked(RequestKind::WebSocketUpgrade),
                403,
                "WebSocket upgrade blocked: non-local Origin",
                "invalid_request_error",
                "origin_rejected",
            ),
            (
                RelayError::UpgradeFailed,
                426,
                "WebSocket upgrade failed",
                "invalid_request_error",
                "upgrade_required",
            ),
            (
                RelayError::UnknownEndpoint {
                    method: "GET".into(),
                    path: "/v1/live".into(),
                },
                404,
                "Unknown endpoint: GET /v1/live",
                "invalid_request_error",
                "invalid_request_error",
            ),
            (
                RelayError::Draining,
                503,
                "Service shutting down",
                "invalid_request_error",
                "invalid_request_error",
            ),
            (
                RelayError::AmbiguousAuthorization,
                400,
                "ambiguous Authorization: send exactly one credential",
                "invalid_request_error",
                "invalid_request_error",
            ),
            (
                RelayError::InvalidRealtimeHeader,
                400,
                "invalid or repeated Realtime header",
                "invalid_request_error",
                "invalid_request_error",
            ),
            (
                RelayError::InvalidRealtimeCallId,
                400,
                "invalid Realtime call_id",
                "invalid_request_error",
                "invalid_call_id",
            ),
            (
                RelayError::UnsupportedRealtimeContentType,
                400,
                "unsupported Realtime content type",
                "invalid_request_error",
                "invalid_request_error",
            ),
            (
                RelayError::UnsupportedRealtimeCapability,
                400,
                "Realtime operation is not supported by the configured upstream profile",
                "invalid_request_error",
                "unsupported_realtime_capability",
            ),
            (
                RelayError::RealtimeRequestBodyTimeout,
                408,
                "Realtime request body timed out",
                "invalid_request_error",
                "request_timeout",
            ),
            (
                RelayError::TooManyActiveRealtimeRequests,
                429,
                "too many active Realtime requests",
                "rate_limit_error",
                "rate_limit_exceeded",
            ),
        ]
    }

    #[tokio::test]
    async fn error_rows_match_the_pinned_contract() {
        for (err, status, message, ty, code) in contract_rows() {
            let label = format!("{err:?}");
            let is_draining = matches!(err, RelayError::Draining);
            assert_eq!(err.status().as_u16(), status, "status for {label}");
            assert_eq!(err.message(), message, "message for {label}");
            assert_eq!(err.error_type(), ty, "type for {label}");
            assert_eq!(err.error_code(), code, "code for {label}");

            // Draining is plain text by contract; its rendering has a dedicated test.
            if is_draining {
                continue;
            }

            // Render it for real: helper methods agreeing proves nothing about the
            // envelope the client actually receives.
            let (rendered_status, body) = rendered(err).await;
            assert_eq!(
                rendered_status.as_u16(),
                status,
                "rendered status for {label}"
            );
            assert_eq!(
                body["error"]["message"], message,
                "rendered message for {label}"
            );
            assert_eq!(body["error"]["type"], ty, "rendered type for {label}");
            assert_eq!(body["error"]["code"], code, "rendered code for {label}");
            assert_eq!(
                body.as_object().map(|o| o.len()),
                Some(1),
                "the envelope has exactly one top-level key for {label}"
            );
        }
    }

    /// A stable discriminant per variant. The exhaustive match means the compiler
    /// rejects a newly added variant until it is named here.
    fn discriminant(err: &RelayError) -> u8 {
        match err {
            RelayError::BodyTooLarge => 1,
            RelayError::ResponseTooLarge(_) => 2,
            RelayError::ClientCanceled => 3,
            RelayError::BodyUnreadable(_) => 4,
            RelayError::MultipartParse => 5,
            RelayError::MultipartMissingSdp => 6,
            RelayError::MultipartSessionNotString => 7,
            RelayError::MultipartSessionNotJson => 8,
            RelayError::NoUpstream => 9,
            RelayError::NoCredential => 10,
            RelayError::UpstreamTimeout => 11,
            RelayError::UpstreamFailed(_) => 12,
            RelayError::AdmissionRequired => 13,
            RelayError::AdmissionSecretNotForwardable => 14,
            RelayError::AmbiguousAuthorization => 15,
            RelayError::OriginBlocked(RequestKind::Http) => 16,
            RelayError::OriginBlocked(RequestKind::WebSocketUpgrade) => 17,
            RelayError::Draining => 18,
            RelayError::UpgradeFailed => 19,
            RelayError::UnknownEndpoint { .. } => 20,
            RelayError::InvalidRealtimeHeader => 21,
            RelayError::InvalidRealtimeCallId => 22,
            RelayError::UnsupportedRealtimeContentType => 23,
            RelayError::UnsupportedRealtimeCapability => 24,
            RelayError::RealtimeRequestBodyTimeout => 25,
            RelayError::TooManyActiveRealtimeRequests => 26,
        }
    }

    /// The count the table must cover. Bumping it without extending the table
    /// fails `every_variant_is_covered_by_the_contract_table`.
    const VARIANT_COUNT: usize = 26;

    /// Mechanically ties the table to the enum: adding a variant breaks compilation
    /// of `discriminant`, and deleting a row from the table breaks this test.
    #[test]
    fn every_variant_is_covered_by_the_contract_table() {
        let mut seen: Vec<u8> = contract_rows()
            .iter()
            .map(|(err, ..)| discriminant(err))
            .collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            VARIANT_COUNT,
            "the contract table must cover every RelayError variant; covered discriminants: {seen:?}"
        );
    }

    #[tokio::test]
    async fn draining_is_plain_text_not_json() {
        let res = RelayError::Draining.into_response();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(res.headers().get(header::RETRY_AFTER).unwrap(), "5");
        assert!(res
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/plain"));

        let bytes = to_bytes(res.into_body(), 4096).await.expect("body");
        assert_eq!(&bytes[..], b"Service shutting down");
    }

    #[tokio::test]
    async fn active_request_limit_has_retry_after_one() {
        let res = RelayError::TooManyActiveRealtimeRequests.into_response();
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(res.headers().get(header::RETRY_AFTER).unwrap(), "1");
    }

    #[test]
    fn every_rest_contract_error_has_one_wire_mapping() {
        use crate::realtime::contract::RestContractError;

        let method = Method::PATCH;
        let path = "/v1/realtime/calls/rtc_a/accept";
        let rows = [
            (
                RestContractError::UnknownRoute,
                StatusCode::NOT_FOUND,
                "invalid_request_error",
            ),
            (
                RestContractError::MethodNotAllowed,
                StatusCode::NOT_FOUND,
                "invalid_request_error",
            ),
            (
                RestContractError::InvalidCallId,
                StatusCode::BAD_REQUEST,
                "invalid_call_id",
            ),
            (
                RestContractError::UnsupportedContentType,
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
            ),
            (
                RestContractError::PrivateDialectRequiresManaged,
                StatusCode::BAD_REQUEST,
                "unsupported_realtime_capability",
            ),
            (
                RestContractError::PrivateDialectNotSupported,
                StatusCode::BAD_REQUEST,
                "unsupported_realtime_capability",
            ),
        ];

        for (contract, status, code) in rows {
            let wire = RelayError::from_rest_contract(contract, &method, path);
            assert_eq!(wire.status(), status, "contract={contract:?}");
            assert_eq!(wire.error_code(), code, "contract={contract:?}");
        }
    }

    #[test]
    fn origin_rejection_messages_differ_by_surface() {
        assert_eq!(
            RelayError::OriginBlocked(RequestKind::Http).message(),
            "cross-origin data-plane request blocked"
        );
        assert_eq!(
            RelayError::OriginBlocked(RequestKind::WebSocketUpgrade).message(),
            "WebSocket upgrade blocked: non-local Origin"
        );
    }
}
