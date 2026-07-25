//! Call-create body builders.
//!
//! Two shapes exist: multipart for a direct API base, and JSON when the base
//! contains `/backend-api`. Part order, content types, and CRLF placement are
//! fixed by the upstream builder (docs/000 §2.3) and are asserted byte for byte.

use serde_json::Value;

use super::session::strip_session_id;
use super::MULTIPART_BOUNDARY;

/// Remove the forbidden top-level `id` without mutating the caller's value.
///
/// Every call-create body strips it, regardless of adapter (docs/000 §3.4), so
/// the builders enforce it rather than trusting a caller to remember.
fn sanitized(session: &Value) -> Value {
    let mut owned = session.clone();
    strip_session_id(&mut owned);
    owned
}

/// The exact `Content-Type` for a multipart call-create request.
pub fn multipart_content_type() -> String {
    format!("multipart/form-data; boundary={MULTIPART_BOUNDARY}")
}

/// Build the multipart body: `sdp` first as `application/sdp`, then `session`
/// as `application/json`, then the closing boundary.
pub fn multipart_call_body(sdp: &str, session: &Value) -> Vec<u8> {
    let session = sanitized(session);
    // Invariant, not a fallback: serializing an already-parsed `Value` has no
    // reachable data error. A silent `"null"` here would produce a malformed
    // body, so this asserts rather than degrades. Broadening the input type to
    // something fallibly serializable means returning `Result` instead.
    let session_json =
        serde_json::to_string(&session).expect("a serde_json::Value always serializes");
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"sdp\"\r\n");
    body.extend_from_slice(b"Content-Type: application/sdp\r\n\r\n");
    body.extend_from_slice(sdp.as_bytes());
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"session\"\r\n");
    body.extend_from_slice(b"Content-Type: application/json\r\n\r\n");
    body.extend_from_slice(session_json.as_bytes());
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}--\r\n").as_bytes());
    body
}

/// Build the backend JSON body. `session` is omitted entirely when absent, which
/// the relay permits for SDP-only requests (docs/000 §2.4).
pub fn backend_json_call_body(sdp: &str, session: Option<&Value>) -> Vec<u8> {
    let value = match session.map(sanitized) {
        Some(session) => serde_json::json!({ "sdp": sdp, "session": session }),
        None => serde_json::json!({ "sdp": sdp }),
    };
    // Same invariant: an empty body would be silently malformed.
    serde_json::to_vec(&value).expect("a serde_json::Value always serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_content_type_carries_the_fixed_boundary() {
        assert_eq!(
            multipart_content_type(),
            "multipart/form-data; boundary=codex-realtime-call-boundary"
        );
    }

    #[test]
    fn multipart_layout_is_byte_exact() {
        let session = serde_json::json!({ "voice": "cove" });
        let body = multipart_call_body("v=0", &session);
        let text = String::from_utf8(body).unwrap();
        assert_eq!(
            text,
            "--codex-realtime-call-boundary\r\n\
Content-Disposition: form-data; name=\"sdp\"\r\n\
Content-Type: application/sdp\r\n\
\r\n\
v=0\r\n\
--codex-realtime-call-boundary\r\n\
Content-Disposition: form-data; name=\"session\"\r\n\
Content-Type: application/json\r\n\
\r\n\
{\"voice\":\"cove\"}\r\n\
--codex-realtime-call-boundary--\r\n"
        );
    }

    #[test]
    fn the_sdp_part_precedes_the_session_part() {
        let body = String::from_utf8(multipart_call_body("v=0", &serde_json::json!({}))).unwrap();
        let sdp_at = body.find("name=\"sdp\"").unwrap();
        let session_at = body.find("name=\"session\"").unwrap();
        assert!(sdp_at < session_at, "part order is fixed by the contract");
    }

    #[test]
    fn backend_json_carries_both_fields() {
        let session = serde_json::json!({ "voice": "cove" });
        let body = backend_json_call_body("v=0", Some(&session));
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["sdp"], "v=0");
        assert_eq!(value["session"]["voice"], "cove");
    }

    #[test]
    fn an_sdp_only_body_omits_the_session_key() {
        let body = backend_json_call_body("v=0", None);
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["sdp"], "v=0");
        assert!(
            value.as_object().unwrap().get("session").is_none(),
            "session must be absent, not null"
        );
    }

    #[test]
    fn a_frameless_session_survives_the_backend_body_without_gaining_a_type() {
        let session = serde_json::to_value(super::super::session::FramelessSession::new(
            "instructions",
            "cove",
        ))
        .unwrap();
        let body = backend_json_call_body("v=0", Some(&session));
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert!(value["session"].get("type").is_none());
        assert_eq!(value["session"]["delegation"]["type"], "client");
    }

    /// Every call-create body strips the session id, regardless of adapter and
    /// regardless of whether the caller remembered to.
    #[test]
    fn both_builders_strip_a_session_id() {
        let session = serde_json::json!({ "id": "sess_123", "voice": "cove" });

        let json = backend_json_call_body("v=0", Some(&session));
        let value: Value = serde_json::from_slice(&json).unwrap();
        assert!(value["session"].get("id").is_none());
        assert_eq!(value["session"]["voice"], "cove");

        let multipart = String::from_utf8(multipart_call_body("v=0", &session)).unwrap();
        assert!(
            !multipart.contains("sess_123"),
            "multipart kept the id: {multipart}"
        );
        assert!(multipart.contains("\"voice\":\"cove\""));
    }

    #[test]
    fn stripping_does_not_mutate_the_caller_value() {
        let session = serde_json::json!({ "id": "sess_123" });
        let _ = backend_json_call_body("v=0", Some(&session));
        assert_eq!(
            session["id"], "sess_123",
            "the caller's value must be untouched"
        );
    }
}
