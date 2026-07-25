//! Request-body reading and the multipart-to-JSON rewrite.

use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::Value;

use crate::error::RelayError;

/// Read a body incrementally, aborting as soon as the cap is exceeded.
///
/// Incremental rather than "read then measure": a 10 GiB upload must not be
/// buffered before being rejected.
pub async fn read_capped(body: axum::body::Body, max_bytes: usize) -> Result<Bytes, RelayError> {
    let mut stream = body.into_data_stream();
    // A single growing buffer, not a vector of frames. Retaining each frame
    // separately would let a client send the cap as one-byte frames and pay for
    // it in `Bytes` metadata instead of payload — under the cap, over the memory.
    let mut buffer = bytes::BytesMut::new();

    while let Some(next) = stream.next().await {
        let chunk = next.map_err(classify_body_error)?;
        if chunk.is_empty() {
            continue;
        }
        if buffer.len().saturating_add(chunk.len()) > max_bytes {
            return Err(RelayError::BodyTooLarge);
        }
        buffer.extend_from_slice(&chunk);
    }

    Ok(buffer.freeze())
}

/// A client that vanished mid-body is a cancellation, not a malformed request.
///
/// Distinguishing them matters: `400` blames the caller for a body it never
/// finished sending, while `499` records that the caller left.
///
/// The decision walks the error's source chain looking for a real transport
/// error rather than matching on message text, which is locale- and
/// version-dependent.
fn classify_body_error(err: axum::Error) -> RelayError {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&err);
    while let Some(current) = source {
        if let Some(io) = current.downcast_ref::<std::io::Error>() {
            return if is_disconnect_kind(io.kind()) {
                RelayError::ClientCanceled
            } else {
                RelayError::BodyUnreadable(err.to_string())
            };
        }
        if let Some(hyper) = current.downcast_ref::<hyper::Error>() {
            // Hyper classifies an incomplete message as the peer going away
            // mid-body, which is precisely the cancellation case.
            if hyper.is_incomplete_message() || hyper.is_canceled() || hyper.is_closed() {
                return RelayError::ClientCanceled;
            }
        }
        source = current.source();
    }
    RelayError::BodyUnreadable(err.to_string())
}

/// IO error kinds that mean the peer went away rather than sent bad data.
fn is_disconnect_kind(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::TimedOut
    )
}

/// True when a content type announces multipart form data.
pub fn is_multipart(content_type: &str) -> bool {
    content_type
        .to_ascii_lowercase()
        .contains("multipart/form-data")
}

/// Rewrite a multipart call-create body into the backend JSON shape.
///
/// `sdp` must be textual UTF-8. That is not a preference: the emitted body is
/// JSON containing `"sdp": "<string>"`, and a JSON string is UTF-8 by
/// definition, so arbitrary bytes cannot be carried there without inventing an
/// encoding. The keyed path never rewrites and stays byte-lossless.
pub async fn backend_json_from_multipart(
    body: Bytes,
    content_type: &str,
) -> Result<(Bytes, &'static str), RelayError> {
    let boundary = multer::parse_boundary(content_type).map_err(|_| RelayError::MultipartParse)?;
    let stream = futures_util::stream::once(async move { Ok::<_, std::io::Error>(body) });
    let mut multipart = multer::Multipart::new(stream, boundary);

    let mut sdp: Option<String> = None;
    let mut session_raw: Option<String> = None;

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
            Some("sdp") if sdp.is_none() => {
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
            Some("session") if session_raw.is_none() => {
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
    let payload = match session_raw {
        Some(raw) => {
            let session: Value =
                serde_json::from_str(&raw).map_err(|_| RelayError::MultipartSessionNotJson)?;
            crate::wire::call_body::backend_json_call_body(&sdp, Some(&session))
        }
        None => crate::wire::call_body::backend_json_call_body(&sdp, None),
    };

    Ok((Bytes::from(payload), "application/json"))
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

    #[test]
    fn a_transport_disconnect_is_a_cancellation_not_a_bad_request() {
        // Classification walks the source chain for a real IO kind, so it does
        // not depend on message wording.
        for kind in [
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::UnexpectedEof,
        ] {
            let err = axum::Error::new(std::io::Error::new(kind, "peer went away"));
            assert!(
                matches!(classify_body_error(err), RelayError::ClientCanceled),
                "{kind:?} should be a cancellation"
            );
        }
    }

    #[test]
    fn a_data_error_remains_a_bad_request() {
        let err = axum::Error::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            // Deliberately contains the word "closed" to prove the classifier
            // is not matching on message text.
            "stream closed unexpectedly while decoding",
        ));
        assert!(matches!(
            classify_body_error(err),
            RelayError::BodyUnreadable(_)
        ));
    }

    #[tokio::test]
    async fn the_cap_is_exact() {
        let at_limit = axum::body::Body::from(vec![b'x'; 16]);
        assert_eq!(read_capped(at_limit, 16).await.unwrap().len(), 16);

        let over = axum::body::Body::from(vec![b'x'; 17]);
        assert!(matches!(
            read_capped(over, 16).await,
            Err(RelayError::BodyTooLarge)
        ));
    }

    #[tokio::test]
    async fn an_empty_body_reads_as_zero_bytes() {
        let body = axum::body::Body::empty();
        assert!(read_capped(body, 16).await.unwrap().is_empty());
    }
}
