//! Sideband join-target parsing and upstream URL construction.

use std::collections::HashMap;

use crate::realtime::path::validate_call_id;
use crate::wire::SIDEBAND_API_ROOT;

pub use crate::realtime::path::MAX_CALL_ID_LEN;

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

    fn adapter(&self) -> WireAdapter {
        match self {
            Self::FramelessPath { .. } => WireAdapter::FramelessBidi,
            Self::RealtimeCallsPath { .. } | Self::RealtimeQuery { .. } => WireAdapter::V1,
        }
    }
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
        validate_call_id(&call_id).ok()?;
        return Some(SidebandTarget::FramelessPath { call_id });
    }

    if let Some(rest) = trimmed.strip_prefix("/v1/realtime/calls/") {
        if rest.contains('/') {
            return None;
        }
        let call_id = decode_segment(rest)?;
        validate_call_id(&call_id).ok()?;
        return Some(SidebandTarget::RealtimeCallsPath { call_id });
    }

    if trimmed == "/v1/realtime" {
        // Query values arrive already decoded.
        let call_id = query.get("call_id")?.to_string();
        validate_call_id(&call_id).ok()?;
        return Some(SidebandTarget::RealtimeQuery { call_id });
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
use crate::relay::pump::{run_private_pump, ClosePolicy, PumpPolicy, MAX_WEBSOCKET_FRAME_OVERHEAD};
use crate::wire::{SidebandJoinStyle, WireAdapter};

/// True when the request announces a WebSocket upgrade.
fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// Resolve the private protocol only when the path and `openai-alpha` form one
/// unambiguous pair. This runs before upgrade extraction, permit acquisition,
/// and upstream header construction so malformed negotiation is zero-contact.
fn private_adapter(
    headers: &HeaderMap,
    target: &SidebandTarget,
) -> Result<WireAdapter, RelayError> {
    let mut values = headers.get_all("openai-alpha").iter();
    let value = values.next().ok_or(RelayError::InvalidRealtimeHeader)?;
    if values.next().is_some() {
        return Err(RelayError::InvalidRealtimeHeader);
    }
    let value = value
        .to_str()
        .map_err(|_| RelayError::InvalidRealtimeHeader)?;
    let adapter = WireAdapter::from_openai_alpha(value).ok_or(RelayError::InvalidRealtimeHeader)?;
    if adapter != target.adapter() {
        return Err(RelayError::InvalidRealtimeHeader);
    }
    Ok(adapter)
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

    let adapter = match private_adapter(&request_headers, &target) {
        Ok(adapter) => adapter,
        Err(error) => return error.into_response(),
    };

    // Browser credential subprotocols belong exclusively to the public GA
    // surface. Reject them before taking a permit or contacting a private
    // upstream, including on the historical path aliases.
    match crate::realtime::subprotocol::parse(
        &request_headers,
        state.config.admission_token.as_ref(),
    ) {
        Ok(protocols) if protocols.offered.is_empty() => {}
        Ok(_) => return RelayError::InvalidRealtimeSubprotocol.into_response(),
        Err(error) => return error.into_response(),
    }

    let cap = state.config.limits.websocket_frame_bytes;
    let upgrade = match WebSocketUpgrade::from_request(request, &()).await {
        Ok(upgrade) => upgrade,
        Err(_) => return unknown(),
    }
    .write_buffer_size(0)
    .max_write_buffer_size(cap + MAX_WEBSOCKET_FRAME_OVERHEAD)
    .max_message_size(cap)
    .max_frame_size(cap);

    let config = state.config.clone();
    let upstream_headers =
        match merge_upstream_headers(&request_headers, &config.upstream, Some(adapter)) {
            Ok(headers) => headers,
            Err(err) => return err.into_response(),
        };

    let join_style = match &target {
        SidebandTarget::FramelessPath { .. } => "frameless_path",
        SidebandTarget::RealtimeCallsPath { .. } => "realtime_calls_path",
        SidebandTarget::RealtimeQuery { .. } => "realtime_query",
    };

    debug_assert_eq!(
        adapter.sideband_join(),
        match target {
            SidebandTarget::FramelessPath { .. } => SidebandJoinStyle::Path,
            SidebandTarget::RealtimeCallsPath { .. } | SidebandTarget::RealtimeQuery { .. } => {
                SidebandJoinStyle::Query
            }
        }
    );

    let upstream_url = sideband_upstream_url(
        config.upstream.base_url(),
        config.upstream.uses_backend_shape(),
        &target,
    );

    // Service-owned: one writer for the whole process, cloned per upgrade.
    let frame_logger = state.frame_log.clone();
    let permit = match state.active_connections.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return RelayError::TooManyActiveRealtimeConnections.into_response(),
    };
    let policy = PumpPolicy {
        frame_bytes: cap,
        send_timeout: state.config.limits.websocket_send_timeout,
        close_policy: ClosePolicy::PrivateNormalized,
    };
    let connect_timeout = state.config.limits.websocket_connect_timeout;
    let upstream_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .write_buffer_size(0)
        .max_write_buffer_size(cap + MAX_WEBSOCKET_FRAME_OVERHEAD)
        .max_message_size(Some(cap))
        .max_frame_size(Some(cap));

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
            let _permit = permit;
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
                match tokio::time::timeout(
                    connect_timeout,
                    tokio_tungstenite::connect_async_with_config(
                        request,
                        Some(upstream_config),
                        false,
                    ),
                )
                .await
                {
                    Ok(Ok((stream, _response))) => Ok(stream),
                    Ok(Err(_)) => Err("upstream handshake failed".to_string()),
                    Err(_) => Err("upstream handshake timed out".to_string()),
                }
            };

            // Logged before the pump runs, but described accurately: the upstream
            // handshake has not completed yet at this point.
            tracing::debug!("sideband downstream upgraded");
            let outcome = run_private_pump(socket, connect, policy, frame_logger).await;
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
    use crate::realtime::path::parse_rest_path;

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
    fn private_path_and_alpha_matrix_is_exact() {
        let frameless = SidebandTarget::FramelessPath {
            call_id: "rtc_f".into(),
        };
        let realtime_calls = SidebandTarget::RealtimeCallsPath {
            call_id: "rtc_c".into(),
        };
        let realtime_query = SidebandTarget::RealtimeQuery {
            call_id: "rtc_q".into(),
        };

        for (target, alpha, expected) in [
            (&frameless, Some("quicksilver=v2"), true),
            (&frameless, Some("quicksilver=v1"), false),
            (&realtime_calls, Some("quicksilver=v1"), true),
            (&realtime_calls, Some("quicksilver=v2"), false),
            (&realtime_query, Some("quicksilver=v1"), true),
            (&realtime_query, Some("quicksilver=v2"), false),
            (&frameless, Some("quicksilver=v9"), false),
            (&frameless, None, false),
        ] {
            let mut headers = HeaderMap::new();
            if let Some(alpha) = alpha {
                headers.insert("openai-alpha", alpha.parse().unwrap());
            }
            assert_eq!(
                private_adapter(&headers, target).is_ok(),
                expected,
                "target={target:?} alpha={alpha:?}"
            );
        }

        let mut repeated = HeaderMap::new();
        repeated.append("openai-alpha", "quicksilver=v2".parse().unwrap());
        repeated.append("openai-alpha", "quicksilver=v2".parse().unwrap());
        assert!(private_adapter(&repeated, &frameless).is_err());
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
        assert!(parse_sideband_target("/v1/realtime", &query(&[("call_id", " rtc_a ")])).is_none());
    }

    #[test]
    fn rest_controls_and_sideband_paths_share_decoded_call_id_decisions() {
        let cases = [
            ("a".to_string(), true),
            ("rtc_A0-z".to_string(), true),
            ("%72tc_1".to_string(), true),
            ("x".repeat(MAX_CALL_ID_LEN), true),
            (String::new(), false),
            ("x".repeat(MAX_CALL_ID_LEN + 1), false),
            ("has.dot".to_string(), false),
            ("has+plus".to_string(), false),
            ("has%2Fslash".to_string(), false),
            ("%252F".to_string(), false),
            ("%ED%95%9C%EA%B8%80".to_string(), false),
            ("%zz".to_string(), false),
            ("%FF".to_string(), false),
        ];

        for (raw_call_id, expected) in cases {
            let rest = parse_rest_path(&format!("/v1/realtime/calls/{raw_call_id}/accept")).is_ok();
            let frameless =
                parse_sideband_target(&format!("/v1/live/{raw_call_id}"), &HashMap::new())
                    .is_some();
            let realtime_calls = parse_sideband_target(
                &format!("/v1/realtime/calls/{raw_call_id}"),
                &HashMap::new(),
            )
            .is_some();

            assert_eq!(rest, expected, "REST raw_call_id={raw_call_id:?}");
            assert_eq!(frameless, rest, "Frameless raw_call_id={raw_call_id:?}");
            assert_eq!(
                realtime_calls, rest,
                "Realtime calls raw_call_id={raw_call_id:?}"
            );
        }
    }

    #[test]
    fn decoded_query_ids_use_the_same_shared_validator() {
        for decoded in [
            "rtc_a".to_string(),
            "x".repeat(MAX_CALL_ID_LEN),
            String::new(),
            " rtc_a ".to_string(),
            "x".repeat(MAX_CALL_ID_LEN + 1),
            "has/slash".to_string(),
            "한글".to_string(),
        ] {
            assert_eq!(
                parse_sideband_target("/v1/realtime", &query(&[("call_id", decoded.as_str())]))
                    .is_some(),
                validate_call_id(&decoded).is_ok(),
                "decoded call_id={decoded:?}"
            );
        }
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
    /// The two expressions of the join rule must agree: the adapter's own
    /// mapping and the style the parsed inbound target implies.
    #[test]
    fn sideband_join_agrees_with_the_adapter() {
        let frameless = SidebandTarget::FramelessPath {
            call_id: "rtc_1".into(),
        };
        assert_eq!(
            WireAdapter::FramelessBidi.sideband_join(),
            SidebandJoinStyle::Path
        );
        assert!(matches!(frameless, SidebandTarget::FramelessPath { .. }));

        let query = SidebandTarget::RealtimeQuery {
            call_id: "rtc_2".into(),
        };
        assert_eq!(WireAdapter::V1.sideband_join(), SidebandJoinStyle::Query);
        assert_eq!(
            WireAdapter::RealtimeV2.sideband_join(),
            SidebandJoinStyle::Query
        );
        assert!(matches!(query, SidebandTarget::RealtimeQuery { .. }));

        // And the URL each produces for the same call id is the documented one.
        for (target, expected) in [
            (
                SidebandTarget::FramelessPath {
                    call_id: "rtc_x".into(),
                },
                "wss://api.openai.com/v1/live/rtc_x",
            ),
            (
                SidebandTarget::RealtimeQuery {
                    call_id: "rtc_x".into(),
                },
                "wss://api.openai.com/v1/realtime?intent=quicksilver&call_id=rtc_x",
            ),
        ] {
            assert_eq!(
                sideband_upstream_url("https://chatgpt.com/backend-api/codex", true, &target),
                expected
            );
        }
    }

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
