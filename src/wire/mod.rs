//! The three wire adapters and the protocol policy they own.
//!
//! This module is the single authority for the AVAS query rule, the
//! `openai-alpha` negotiation value, and the sideband join style. The relay
//! consumes those decisions rather than restating them, so there is exactly one
//! place where a protocol rule can be wrong.
//!
//! Scope (docs/002 D1b): this is relay-side modeling plus a serialization
//! library, not a Realtime client. Nothing here emits `session.update` or
//! normalizes events.

pub mod call_body;
pub mod session;

/// Call-create query for AVAS WebRTC calls.
pub const AVAS_QUERY: &str = "intent=quicksilver&architecture=avas";

/// The sideband join host. A ChatGPT `backend-api` call-create still joins here
/// (opencodex `3b766d91`); see `docs/000` §5.2.
pub const SIDEBAND_API_ROOT: &str = "https://api.openai.com/v1";

/// The fixed multipart boundary used by upstream call-create bodies.
pub const MULTIPART_BOUNDARY: &str = "codex-realtime-call-boundary";

/// How a call id is attached to the sideband URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebandJoinStyle {
    /// `/v1/live/{call_id}`
    Path,
    /// `/v1/realtime?call_id={call_id}`
    Query,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireAdapter {
    /// v1 quicksilver.
    V1,
    /// v3 Frameless bidi, the GPT-Live protocol.
    FramelessBidi,
    /// v2 Realtime. Modeled only far enough to be rejected for WebRTC.
    RealtimeV2,
}

impl WireAdapter {
    /// The public app-server version string maps onto an adapter, and the two
    /// version axes do not agree: public `v3` negotiates `quicksilver=v2`.
    pub fn from_app_server_version(version: &str) -> Option<Self> {
        match version.trim() {
            "v1" => Some(Self::V1),
            "v2" => Some(Self::RealtimeV2),
            "v3" => Some(Self::FramelessBidi),
            _ => None,
        }
    }

    /// The `openai-alpha` negotiation value, if the adapter sends one.
    ///
    /// Losing this header is what made a Frameless session get validated as v1
    /// and 400 (opencodex `75344b09`).
    pub fn openai_alpha(self) -> Option<&'static str> {
        match self {
            Self::V1 => Some("quicksilver=v1"),
            Self::FramelessBidi => Some("quicksilver=v2"),
            Self::RealtimeV2 => None,
        }
    }

    /// Derive the adapter from an inbound `openai-alpha` value.
    ///
    /// Used to *observe* what the client negotiated. It never causes the relay
    /// to invent the header: absent stays absent.
    pub fn from_openai_alpha(value: &str) -> Option<Self> {
        match value.trim() {
            "quicksilver=v1" => Some(Self::V1),
            "quicksilver=v2" => Some(Self::FramelessBidi),
            _ => None,
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            Self::V1 | Self::RealtimeV2 => "gpt-realtime-1.5",
            Self::FramelessBidi => "gpt-live-1-boulder-alpha",
        }
    }

    pub fn standalone_ws_path(self) -> &'static str {
        match self {
            Self::V1 | Self::RealtimeV2 => "/v1/realtime",
            Self::FramelessBidi => "/v1/live",
        }
    }

    pub fn sideband_join(self) -> SidebandJoinStyle {
        match self {
            Self::FramelessBidi => SidebandJoinStyle::Path,
            Self::V1 | Self::RealtimeV2 => SidebandJoinStyle::Query,
        }
    }

    /// Mirrors the upstream expression `parser == V1 || (backend_shape && parser
    /// == FramelessBidi)` so the mapping stays auditable against docs/000 §2.2.
    pub fn wants_avas_query(self, backend_shape: bool) -> bool {
        match self {
            Self::V1 => true,
            Self::FramelessBidi => backend_shape,
            Self::RealtimeV2 => false,
        }
    }

    /// `initialize_session || parser != FramelessBidi` (docs/000 §5.3).
    ///
    /// A Frameless WebRTC sideband attaches to a session the call-create body
    /// already started, so it must not send an update after joining.
    pub fn sends_session_update_after_join(self, initialize_session: bool) -> bool {
        initialize_session || self != Self::FramelessBidi
    }

    /// WebRTC allows only V1 and Frameless; RealtimeV2 is rejected before the
    /// request is built.
    pub fn allows_webrtc(self) -> bool {
        self != Self::RealtimeV2
    }
}

/// The error upstream returns when a RealtimeV2 WebRTC call is attempted.
pub const AVAS_REQUIRES_V1_OR_V3: &str = "AVAS realtime calls require realtime v1 or v3";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_server_versions_map_onto_adapters() {
        assert_eq!(
            WireAdapter::from_app_server_version("v1"),
            Some(WireAdapter::V1)
        );
        assert_eq!(
            WireAdapter::from_app_server_version("v2"),
            Some(WireAdapter::RealtimeV2)
        );
        assert_eq!(
            WireAdapter::from_app_server_version("v3"),
            Some(WireAdapter::FramelessBidi)
        );
        assert_eq!(WireAdapter::from_app_server_version("v4"), None);
    }

    /// Public `v3` negotiates `quicksilver=v2`. The two version axes differ, and
    /// conflating them is exactly the confusion docs/000 §11.1 warns about.
    #[test]
    fn the_two_version_axes_are_distinct() {
        let adapter = WireAdapter::from_app_server_version("v3").unwrap();
        assert_eq!(adapter.openai_alpha(), Some("quicksilver=v2"));
    }

    #[test]
    fn openai_alpha_values_are_exact() {
        assert_eq!(WireAdapter::V1.openai_alpha(), Some("quicksilver=v1"));
        assert_eq!(
            WireAdapter::FramelessBidi.openai_alpha(),
            Some("quicksilver=v2")
        );
        assert_eq!(WireAdapter::RealtimeV2.openai_alpha(), None);
    }

    #[test]
    fn openai_alpha_round_trips() {
        for adapter in [WireAdapter::V1, WireAdapter::FramelessBidi] {
            let value = adapter.openai_alpha().unwrap();
            assert_eq!(WireAdapter::from_openai_alpha(value), Some(adapter));
        }
        assert_eq!(WireAdapter::from_openai_alpha("quicksilver=v9"), None);
        assert_eq!(WireAdapter::from_openai_alpha(""), None);
    }

    /// The full truth table from docs/000 §2.2.
    #[test]
    fn avas_query_truth_table() {
        let rows = [
            (WireAdapter::V1, false, true),
            (WireAdapter::V1, true, true),
            (WireAdapter::FramelessBidi, false, false),
            (WireAdapter::FramelessBidi, true, true),
            (WireAdapter::RealtimeV2, false, false),
            (WireAdapter::RealtimeV2, true, false),
        ];
        for (adapter, backend_shape, expected) in rows {
            assert_eq!(
                adapter.wants_avas_query(backend_shape),
                expected,
                "{adapter:?} with backend_shape={backend_shape}"
            );
        }
    }

    /// The full truth table from docs/000 §5.3.
    #[test]
    fn session_update_truth_table() {
        let rows = [
            (WireAdapter::V1, true, true),
            (WireAdapter::V1, false, true),
            (WireAdapter::FramelessBidi, true, true),
            (WireAdapter::FramelessBidi, false, false),
            (WireAdapter::RealtimeV2, true, true),
            (WireAdapter::RealtimeV2, false, true),
        ];
        for (adapter, initialize, expected) in rows {
            assert_eq!(
                adapter.sends_session_update_after_join(initialize),
                expected,
                "{adapter:?} with initialize_session={initialize}"
            );
        }
    }

    #[test]
    fn a_frameless_webrtc_sideband_never_updates_the_session() {
        assert!(!WireAdapter::FramelessBidi.sends_session_update_after_join(false));
    }

    #[test]
    fn realtime_v2_is_rejected_for_webrtc() {
        assert!(!WireAdapter::RealtimeV2.allows_webrtc());
        assert!(WireAdapter::V1.allows_webrtc());
        assert!(WireAdapter::FramelessBidi.allows_webrtc());
    }

    #[test]
    fn join_styles_and_paths_match_the_contract() {
        assert_eq!(
            WireAdapter::FramelessBidi.sideband_join(),
            SidebandJoinStyle::Path
        );
        assert_eq!(WireAdapter::V1.sideband_join(), SidebandJoinStyle::Query);
        assert_eq!(
            WireAdapter::RealtimeV2.sideband_join(),
            SidebandJoinStyle::Query
        );

        assert_eq!(WireAdapter::FramelessBidi.standalone_ws_path(), "/v1/live");
        assert_eq!(WireAdapter::V1.standalone_ws_path(), "/v1/realtime");
        assert_eq!(WireAdapter::RealtimeV2.standalone_ws_path(), "/v1/realtime");
    }

    #[test]
    fn default_models_match_the_contract() {
        assert_eq!(WireAdapter::V1.default_model(), "gpt-realtime-1.5");
        assert_eq!(
            WireAdapter::FramelessBidi.default_model(),
            "gpt-live-1-boulder-alpha"
        );
        assert_eq!(WireAdapter::RealtimeV2.default_model(), "gpt-realtime-1.5");
    }

    #[test]
    fn constants_are_verbatim() {
        assert_eq!(AVAS_QUERY, "intent=quicksilver&architecture=avas");
        assert_eq!(SIDEBAND_API_ROOT, "https://api.openai.com/v1");
        assert_eq!(MULTIPART_BOUNDARY, "codex-realtime-call-boundary");
    }
}
