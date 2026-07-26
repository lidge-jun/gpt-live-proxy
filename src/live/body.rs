//! Multipart-to-JSON rewriting for private GPT-Live call creation.

use bytes::Bytes;
use serde_json::Value;

use crate::error::RelayError;

pub use crate::relay::body::read_capped;

/// True when a content type announces multipart form data.
pub fn is_multipart(content_type: &str) -> bool {
    content_type
        .to_ascii_lowercase()
        .contains("multipart/form-data")
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedPrivateCall {
    pub sdp: String,
    pub session: Option<Value>,
    pub sdp_fields: usize,
    pub session_fields: usize,
}

impl ParsedPrivateCall {
    /// Whether an ambiguous duplicate could be interpreted differently by an
    /// upstream multipart implementation.
    pub fn has_duplicate_contract_fields(&self) -> bool {
        self.sdp_fields > 1 || self.session_fields > 1
    }
}

/// Parse a private multipart call once for evidence and optional rewriting.
///
/// `sdp` must be textual UTF-8. That is not a preference: the backend JSON
/// shape contains a JSON string, and accepting arbitrary bytes here would
/// silently invent an encoding. Direct API-shaped forwarding keeps a separate
/// copy of the original bytes and therefore remains byte-identical.
pub async fn parse_private_multipart(
    body: Bytes,
    content_type: &str,
) -> Result<ParsedPrivateCall, RelayError> {
    let boundary = multer::parse_boundary(content_type).map_err(|_| RelayError::MultipartParse)?;
    let stream = futures_util::stream::once(async move { Ok::<_, std::io::Error>(body) });
    let mut multipart = multer::Multipart::new(stream, boundary);

    let mut sdp: Option<String> = None;
    let mut session_raw: Option<String> = None;
    let mut sdp_fields = 0;
    let mut session_fields = 0;

    loop {
        let field = multipart
            .next_field()
            .await
            .map_err(|_| RelayError::MultipartParse)?;
        let Some(field) = field else { break };

        // First occurrence wins; later duplicates are ignored rather than
        // silently overriding what the client sent first.
        let name = field.name().map(str::to_string);
        match name.as_deref() {
            Some("sdp") => {
                sdp_fields += 1;
                if sdp.is_some() {
                    // Retain duplicate evidence while preserving the legacy
                    // first-wins value for profiles that permit it.
                    let _ = field.bytes().await;
                    continue;
                }
                // A file-valued field is not the textual field the contract
                // specifies, and accepting it would smuggle an upload through a
                // path that promises a string.
                if field.file_name().is_some() {
                    return Err(RelayError::MultipartMissingSdp);
                }
                // `text()` decodes lossily, replacing invalid bytes with U+FFFD,
                // which would silently corrupt an SDP instead of refusing it.
                let raw = field
                    .bytes()
                    .await
                    .map_err(|_| RelayError::MultipartMissingSdp)?;
                let decoded =
                    String::from_utf8(raw.to_vec()).map_err(|_| RelayError::MultipartMissingSdp)?;
                sdp = Some(decoded);
            }
            Some("session") => {
                session_fields += 1;
                if session_raw.is_some() {
                    // See the `sdp` branch: the caller decides whether
                    // duplicates are compatible with its profile policy.
                    let _ = field.bytes().await;
                    continue;
                }
                if field.file_name().is_some() {
                    return Err(RelayError::MultipartSessionNotString);
                }
                let raw = field
                    .bytes()
                    .await
                    .map_err(|_| RelayError::MultipartSessionNotString)?;
                let decoded = String::from_utf8(raw.to_vec())
                    .map_err(|_| RelayError::MultipartSessionNotString)?;
                session_raw = Some(decoded);
            }
            _ => {
                // Drain so the parser advances past this field.
                let _ = field.bytes().await;
            }
        }
    }

    let sdp = sdp.ok_or(RelayError::MultipartMissingSdp)?;
    let session = session_raw
        .map(|raw| serde_json::from_str(&raw).map_err(|_| RelayError::MultipartSessionNotJson))
        .transpose()?;

    Ok(ParsedPrivateCall {
        sdp,
        session,
        sdp_fields,
        session_fields,
    })
}

/// Build the private backend JSON shape from an already parsed call.
pub fn backend_json_from_parsed(parsed: &ParsedPrivateCall) -> (Bytes, &'static str) {
    let payload =
        crate::wire::call_body::backend_json_call_body(&parsed.sdp, parsed.session.as_ref());

    (Bytes::from(payload), "application/json")
}

/// Backwards-compatible one-shot helper for callers that do not need evidence.
pub async fn backend_json_from_multipart(
    body: Bytes,
    content_type: &str,
) -> Result<(Bytes, &'static str), RelayError> {
    let parsed = parse_private_multipart(body, content_type).await?;

    Ok(backend_json_from_parsed(&parsed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::MULTIPART_BOUNDARY;

    fn multipart(parts: &[(&str, &str, &str)]) -> (Bytes, String) {
        let mut body = Vec::new();
        for (name, content_type, value) in parts {
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
            Bytes::from(body),
            format!("multipart/form-data; boundary={MULTIPART_BOUNDARY}"),
        )
    }

    #[test]
    fn multipart_detection_is_case_insensitive() {
        assert!(is_multipart("Multipart/Form-Data; boundary=x"));
        assert!(!is_multipart("application/json"));
    }

    #[tokio::test]
    async fn a_full_body_becomes_backend_json() {
        let (body, content_type) = multipart(&[
            ("sdp", "application/sdp", "v=0"),
            ("session", "application/json", r#"{"voice":"cove"}"#),
        ]);
        let (out, out_type) = backend_json_from_multipart(body, &content_type)
            .await
            .unwrap();
        assert_eq!(out_type, "application/json");
        let value: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["sdp"], "v=0");
        assert_eq!(value["session"]["voice"], "cove");
    }

    #[tokio::test]
    async fn an_sdp_only_body_omits_the_session_key() {
        let (body, content_type) = multipart(&[("sdp", "application/sdp", "v=0")]);
        let (out, _) = backend_json_from_multipart(body, &content_type)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["sdp"], "v=0");
        assert!(value.as_object().unwrap().get("session").is_none());
    }

    #[tokio::test]
    async fn a_session_id_is_stripped_on_the_rewrite_path() {
        let (body, content_type) = multipart(&[
            ("sdp", "application/sdp", "v=0"),
            (
                "session",
                "application/json",
                r#"{"id":"sess_1","voice":"cove"}"#,
            ),
        ]);
        let (out, _) = backend_json_from_multipart(body, &content_type)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&out).unwrap();
        assert!(value["session"].get("id").is_none());
        assert_eq!(value["session"]["voice"], "cove");
    }

    #[tokio::test]
    async fn the_first_occurrence_of_a_duplicate_field_wins() {
        let (body, content_type) = multipart(&[
            ("sdp", "application/sdp", "first"),
            ("sdp", "application/sdp", "second"),
        ]);
        let (out, _) = backend_json_from_multipart(body, &content_type)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["sdp"], "first");
    }

    #[tokio::test]
    async fn duplicate_contract_fields_are_retained_as_evidence() {
        let (body, content_type) = multipart(&[
            ("sdp", "application/sdp", "first"),
            ("session", "application/json", r#"{"voice":"first"}"#),
            ("sdp", "application/sdp", "second"),
            ("session", "application/json", r#"{"type":"realtime"}"#),
        ]);
        let parsed = parse_private_multipart(body, &content_type).await.unwrap();

        assert_eq!(parsed.sdp, "first");
        assert_eq!(parsed.session.as_ref().unwrap()["voice"], "first");
        assert_eq!(parsed.sdp_fields, 2);
        assert_eq!(parsed.session_fields, 2);
        assert!(parsed.has_duplicate_contract_fields());
    }

    #[tokio::test]
    async fn a_missing_sdp_field_is_rejected() {
        let (body, content_type) = multipart(&[("session", "application/json", "{}")]);
        let err = backend_json_from_multipart(body, &content_type)
            .await
            .unwrap_err();
        assert_eq!(
            err.message(),
            "ChatGPT voice relay expects multipart field sdp on call-create"
        );
    }

    #[tokio::test]
    async fn an_unparsable_session_is_rejected() {
        let (body, content_type) = multipart(&[
            ("sdp", "application/sdp", "v=0"),
            ("session", "application/json", "{not json"),
        ]);
        let err = backend_json_from_multipart(body, &content_type)
            .await
            .unwrap_err();
        assert_eq!(
            err.message(),
            "ChatGPT voice relay expected JSON in the multipart session field"
        );
    }

    #[tokio::test]
    async fn an_unparsable_body_is_rejected() {
        let err = backend_json_from_multipart(Bytes::from_static(b"garbage"), "text/plain")
            .await
            .unwrap_err();
        assert_eq!(
            err.message(),
            "ChatGPT voice relay could not parse multipart call-create body"
        );
    }

    #[tokio::test]
    async fn a_non_utf8_sdp_is_rejected_rather_than_mangled() {
        // A JSON string cannot carry arbitrary bytes, so this must fail loudly
        // instead of producing a lossy body.
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(b"Content-Disposition: form-data; name=\"sdp\"\r\n");
        body.extend_from_slice(b"Content-Type: application/sdp\r\n\r\n");
        body.extend_from_slice(&[0xff, 0xfe, 0xfd]);
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}--\r\n").as_bytes());

        let content_type = format!("multipart/form-data; boundary={MULTIPART_BOUNDARY}");
        let err = backend_json_from_multipart(Bytes::from(body), &content_type)
            .await
            .unwrap_err();
        assert_eq!(
            err.message(),
            "ChatGPT voice relay expects multipart field sdp on call-create"
        );
    }

    #[tokio::test]
    async fn a_file_valued_sdp_is_rejected() {
        // A file field is not the textual field the contract specifies.
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"sdp\"; filename=\"offer.sdp\"\r\n",
        );
        body.extend_from_slice(b"Content-Type: application/sdp\r\n\r\n");
        body.extend_from_slice(b"v=0");
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}--\r\n").as_bytes());

        let content_type = format!("multipart/form-data; boundary={MULTIPART_BOUNDARY}");
        let err = backend_json_from_multipart(Bytes::from(body), &content_type)
            .await
            .unwrap_err();
        assert_eq!(
            err.message(),
            "ChatGPT voice relay expects multipart field sdp on call-create"
        );
    }

    #[tokio::test]
    async fn a_file_valued_session_is_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(b"Content-Disposition: form-data; name=\"sdp\"\r\n\r\n");
        body.extend_from_slice(b"v=0\r\n");
        body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"session\"; filename=\"s.json\"\r\n\r\n",
        );
        body.extend_from_slice(b"{}\r\n");
        body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}--\r\n").as_bytes());

        let content_type = format!("multipart/form-data; boundary={MULTIPART_BOUNDARY}");
        let err = backend_json_from_multipart(Bytes::from(body), &content_type)
            .await
            .unwrap_err();
        assert_eq!(
            err.message(),
            "ChatGPT voice relay expected a string multipart session field"
        );
    }

    #[tokio::test]
    async fn a_non_utf8_session_is_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(b"Content-Disposition: form-data; name=\"sdp\"\r\n\r\n");
        body.extend_from_slice(b"v=0\r\n");
        body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(b"Content-Disposition: form-data; name=\"session\"\r\n\r\n");
        body.extend_from_slice(&[0xff, 0xfe]);
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}--\r\n").as_bytes());

        let content_type = format!("multipart/form-data; boundary={MULTIPART_BOUNDARY}");
        let err = backend_json_from_multipart(Bytes::from(body), &content_type)
            .await
            .unwrap_err();
        assert_eq!(
            err.message(),
            "ChatGPT voice relay expected a string multipart session field"
        );
    }

    #[tokio::test]
    async fn the_first_session_field_wins() {
        let (body, content_type) = multipart(&[
            ("sdp", "application/sdp", "v=0"),
            ("session", "application/json", r#"{"voice":"first"}"#),
            ("session", "application/json", r#"{"voice":"second"}"#),
        ]);
        let (out, _) = backend_json_from_multipart(body, &content_type)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["session"]["voice"], "first");
    }
}
