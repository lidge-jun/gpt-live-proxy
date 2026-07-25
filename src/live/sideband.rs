//! Sideband join-target parsing and upstream URL construction.

use std::collections::HashMap;

use crate::wire::SIDEBAND_API_ROOT;

/// Maximum decoded call-id length, matching `^[A-Za-z0-9_-]{1,128}$`.
pub const MAX_CALL_ID_LEN: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebandTarget {
    /// `/v1/live/{call_id}` — the Frameless path style.
    FramelessPath { call_id: String },
    /// `/v1/realtime/calls/{call_id}`
    RealtimeCallsPath { call_id: String },
    /// `/v1/realtime?call_id={call_id}`
    RealtimeQuery { call_id: String },
}

impl SidebandTarget {
    pub fn call_id(&self) -> &str {
        match self {
            Self::FramelessPath { call_id }
            | Self::RealtimeCallsPath { call_id }
            | Self::RealtimeQuery { call_id } => call_id,
        }
    }
}

/// A hand-rolled check rather than a regex dependency: the pattern is fixed and
/// small enough that a scan is clearer than a compiled expression.
fn is_valid_call_id(candidate: &str) -> bool {
    if candidate.is_empty() || candidate.len() > MAX_CALL_ID_LEN {
        return false;
    }
    candidate
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Percent-decode a path segment.
///
/// A decode failure returns `None`. The TypeScript original lets
/// `decodeURIComponent` throw and never catches it, so `/v1/live/%zz` has no
/// defined mapping there; here it becomes the same `404` as any other invalid
/// id — a deliberate, tested divergence (docs/001 §1).
fn decode_segment(segment: &str) -> Option<String> {
    percent_encoding::percent_decode_str(segment)
        .decode_utf8()
        .ok()
        .map(|decoded| decoded.into_owned())
}

/// Parse an inbound path into a join target. One optional trailing slash is
/// accepted on every style.
pub fn parse_sideband_target(
    path: &str,
    query: &HashMap<String, String>,
) -> Option<SidebandTarget> {
    let trimmed = path.strip_suffix('/').unwrap_or(path);

    if let Some(rest) = trimmed.strip_prefix("/v1/live/") {
        if rest.contains('/') {
            return None;
        }
        let call_id = decode_segment(rest)?;
        return is_valid_call_id(&call_id).then_some(SidebandTarget::FramelessPath { call_id });
    }

    if let Some(rest) = trimmed.strip_prefix("/v1/realtime/calls/") {
        if rest.contains('/') {
            return None;
        }
        let call_id = decode_segment(rest)?;
        return is_valid_call_id(&call_id).then_some(SidebandTarget::RealtimeCallsPath { call_id });
    }

    if trimmed == "/v1/realtime" {
        // Query values arrive already decoded.
        let call_id = query.get("call_id")?.trim().to_string();
        return is_valid_call_id(&call_id).then_some(SidebandTarget::RealtimeQuery { call_id });
    }

    None
}

/// Build the upstream WebSocket URL.
///
/// A backend-shaped profile ignores its own base entirely and joins the public
/// API host. This is the `3b766d91` rule: `chatgpt.com/backend-api` rejects the
/// sideband upgrade before it opens, while the same bearer works unchanged on
/// `api.openai.com`.
pub fn sideband_upstream_url(base: &str, backend_shape: bool, target: &SidebandTarget) -> String {
    let root = if backend_shape {
        SIDEBAND_API_ROOT.to_string()
    } else {
        let trimmed = base.trim_end_matches('/');
        let without_v1 = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
        format!("{without_v1}/v1")
    };

    let http_url = match target {
        SidebandTarget::FramelessPath { call_id } => format!("{root}/live/{call_id}"),
        SidebandTarget::RealtimeCallsPath { call_id } => {
            format!("{root}/realtime/calls/{call_id}")
        }
        SidebandTarget::RealtimeQuery { call_id } => {
            format!("{root}/realtime?intent=quicksilver&call_id={call_id}")
        }
    };

    to_ws_scheme(&http_url)
}

/// `https` becomes `wss`, `http` becomes `ws`, anything else is left alone.
fn to_ws_scheme(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("https://") {
        return format!("wss://{rest}");
    }
    if let Some(rest) = url.strip_prefix("http://") {
        return format!("ws://{rest}");
    }
    url.to_string()
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{FromRequest, Query, State};
use axum::response::{IntoResponse, Response};
use http::HeaderMap;

use crate::app::AppState;
use crate::error::RelayError;
use crate::live::headers::merge_upstream_headers;
use crate::live::pump::run_pump;

/// True when the request announces a WebSocket upgrade.
fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// `GET /v1/live/{callId}`, `/v1/realtime/calls/{callId}`, `/v1/realtime?call_id=`.
///
/// Registered inside the protected subrouter, so the trust boundary — including
/// the upgrade-specific rejection wording — already ran before this is reached.
pub async fn handle_sideband(
    State(state): State<AppState>,
    method: http::Method,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    Query(query): Query<HashMap<String, String>>,
    request_headers: HeaderMap,
    request: axum::extract::Request,
) -> Response {
    let unknown = || {
        RelayError::UnknownEndpoint {
            method: method.to_string(),
            path: uri.path().to_string(),
        }
        .into_response()
    };

    let Some(target) = parse_sideband_target(uri.path(), &query) else {
        return unknown();
    };

    // A non-upgrade request on a sideband path is an unknown endpoint, not an
    // extractor rejection: extracting `WebSocketUpgrade` first would produce
    // axum's own error instead of the contract's 404.
    if !is_websocket_upgrade(&request_headers) {
        return unknown();
    }
    let upgrade = match WebSocketUpgrade::from_request(request, &()).await {
        Ok(upgrade) => upgrade,
        Err(_) => return unknown(),
    };

    let config = state.config.clone();
    let upstream_headers = match merge_upstream_headers(&request_headers, &config.upstream, None) {
        Ok(headers) => headers,
        Err(err) => return err.into_response(),
    };

    let join_style = match &target {
        SidebandTarget::FramelessPath { .. } => "frameless_path",
        SidebandTarget::RealtimeCallsPath { .. } => "realtime_calls_path",
        SidebandTarget::RealtimeQuery { .. } => "realtime_query",
    };

    let upstream_url = sideband_upstream_url(
        config.upstream.base_url(),
        config.upstream.uses_backend_shape(),
        &target,
    );

    // Service-owned: one writer for the whole process, cloned per upgrade.
    let frame_logger = state.frame_log.clone();

    // Host only, parsed rather than split: the URL embeds the call id, and a
    // split would also expose any userinfo.
    let upstream_host = crate::live::call_create::upstream_host_of(&upstream_url);

    // A real span, so every event inside the relay carries this context.
    // `outcome` and `code` are recorded when the relay ends, so the span itself
    // carries the terminal state rather than only its events.
    let span = tracing::info_span!(
        "sideband",
        join_style = join_style,
        upstream = %upstream_host,
        outcome = tracing::field::Empty,
        code = tracing::field::Empty
    );

    upgrade.on_upgrade(move |socket| {
        // `Instrument`, not `span.enter()`: holding an entered guard across an
        // await can attribute another task's events to this span when the task
        // suspends.
        use tracing::Instrument;
        async move {
            let connect = async move {
                use tokio_tungstenite::tungstenite::client::IntoClientRequest;

                // Built from IntoClientRequest and THEN extended: hand-rolling a
                // bare request would omit Sec-WebSocket-Key and friends.
                let mut request = match upstream_url.as_str().into_client_request() {
                    Ok(request) => request,
                    Err(err) => return Err(err.to_string()),
                };
                for (name, value) in upstream_headers.iter() {
                    request.headers_mut().insert(name.clone(), value.clone());
                }
                match tokio_tungstenite::connect_async(request).await {
                    Ok((stream, _response)) => Ok(stream),
                    Err(err) => Err(err.to_string()),
                }
            };

            // Logged before the pump runs, but described accurately: the upstream
            // handshake has not completed yet at this point.
            tracing::debug!("sideband downstream upgraded");
            let outcome = run_pump(socket, connect, frame_logger).await;
            // Kind and code only: `?outcome` would render the peer-controlled close
            // reason, which can carry transcript text or a credential.
            let span = tracing::Span::current();
            span.record("outcome", outcome.label());
            if let Some(code) = outcome.code() {
                span.record("code", code);
            }
            tracing::info!(
                outcome = outcome.label(),
                code = outcome.code(),
                "sideband relay finished"
            );
        }
        .instrument(span)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn all_three_styles_parse() {
        assert_eq!(
            parse_sideband_target("/v1/live/rtc_abc", &HashMap::new()),
            Some(SidebandTarget::FramelessPath {
                call_id: "rtc_abc".into()
            })
        );
        assert_eq!(
            parse_sideband_target("/v1/realtime/calls/rtc_abc", &HashMap::new()),
            Some(SidebandTarget::RealtimeCallsPath {
                call_id: "rtc_abc".into()
            })
        );
        assert_eq!(
            parse_sideband_target("/v1/realtime", &query(&[("call_id", "rtc_abc")])),
            Some(SidebandTarget::RealtimeQuery {
                call_id: "rtc_abc".into()
            })
        );
    }

    #[test]
    fn one_trailing_slash_is_accepted_on_every_style() {
        assert!(parse_sideband_target("/v1/live/rtc_abc/", &HashMap::new()).is_some());
        assert!(parse_sideband_target("/v1/realtime/calls/rtc_abc/", &HashMap::new()).is_some());
        assert!(
            parse_sideband_target("/v1/realtime/", &query(&[("call_id", "rtc_abc")])).is_some()
        );
    }

    #[test]
    fn the_call_id_length_boundary_is_exact() {
        let at_limit = "a".repeat(MAX_CALL_ID_LEN);
        assert!(parse_sideband_target(&format!("/v1/live/{at_limit}"), &HashMap::new()).is_some());

        let over = "a".repeat(MAX_CALL_ID_LEN + 1);
        assert!(parse_sideband_target(&format!("/v1/live/{over}"), &HashMap::new()).is_none());

        assert!(parse_sideband_target("/v1/live/a", &HashMap::new()).is_some());
    }

    #[test]
    fn invalid_call_ids_are_rejected() {
        for path in [
            "/v1/live/",                   // empty
            "/v1/live/has%2Fslash",        // decodes to a slash
            "/v1/live/has+plus",           // plus is not in the allowed set
            "/v1/live/%ED%95%9C%EA%B8%80", // decodes to non-ASCII
            "/v1/live/dot.dot",
            "/v1/live/a/b", // extra segment
        ] {
            assert!(
                parse_sideband_target(path, &HashMap::new()).is_none(),
                "{path} should not parse"
            );
        }
    }

    /// A malformed escape makes the TypeScript original throw; here it is the
    /// same 404 as any other invalid id.
    #[test]
    fn a_malformed_percent_escape_is_rejected_rather_than_panicking() {
        assert!(parse_sideband_target("/v1/live/%zz", &HashMap::new()).is_none());
        assert!(parse_sideband_target("/v1/live/%FF", &HashMap::new()).is_none());
    }

    #[test]
    fn a_missing_or_empty_query_id_is_rejected() {
        assert!(parse_sideband_target("/v1/realtime", &HashMap::new()).is_none());
        assert!(parse_sideband_target("/v1/realtime", &query(&[("call_id", "  ")])).is_none());
    }

    #[test]
    fn unrelated_paths_do_not_parse() {
        for path in ["/v1/live", "/v1/realtime/calls", "/healthz", "/"] {
            assert!(
                parse_sideband_target(path, &HashMap::new()).is_none(),
                "{path}"
            );
        }
    }

    /// The `3b766d91` rule: a ChatGPT backend call still joins the API host.
    #[test]
    fn a_backend_profile_joins_the_api_host() {
        let target = SidebandTarget::FramelessPath {
            call_id: "rtc_abc".into(),
        };
        assert_eq!(
            sideband_upstream_url("https://chatgpt.com/backend-api/codex", true, &target),
            "wss://api.openai.com/v1/live/rtc_abc"
        );
    }

    #[test]
    fn every_backend_style_maps_to_the_api_root() {
        let cases = [
            (
                SidebandTarget::FramelessPath {
                    call_id: "rtc_1".into(),
                },
                "wss://api.openai.com/v1/live/rtc_1",
            ),
            (
                SidebandTarget::RealtimeCallsPath {
                    call_id: "rtc_2".into(),
                },
                "wss://api.openai.com/v1/realtime/calls/rtc_2",
            ),
            (
                SidebandTarget::RealtimeQuery {
                    call_id: "rtc_3".into(),
                },
                "wss://api.openai.com/v1/realtime?intent=quicksilver&call_id=rtc_3",
            ),
        ];
        for (target, expected) in cases {
            assert_eq!(
                sideband_upstream_url("https://chatgpt.com/backend-api/codex", true, &target),
                expected
            );
        }
    }

    #[test]
    fn a_non_backend_profile_uses_its_own_root() {
        let target = SidebandTarget::FramelessPath {
            call_id: "rtc_abc".into(),
        };
        for base in [
            "https://api.openai.com/v1",
            "https://api.openai.com/v1/",
            "https://api.openai.com",
        ] {
            assert_eq!(
                sideband_upstream_url(base, false, &target),
                "wss://api.openai.com/v1/live/rtc_abc",
                "base {base}"
            );
        }
    }

    #[test]
    fn the_scheme_is_upgraded() {
        let target = SidebandTarget::RealtimeQuery {
            call_id: "rtc_x".into(),
        };
        assert!(sideband_upstream_url("http://local.test/v1", false, &target).starts_with("ws://"));
        assert!(
            sideband_upstream_url("https://local.test/v1", false, &target).starts_with("wss://")
        );
    }
}
