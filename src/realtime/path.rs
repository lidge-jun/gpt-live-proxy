//! Canonical path parsing for the official Realtime REST surface.

use percent_encoding::percent_decode_str;
use thiserror::Error;

/// Maximum decoded call-id length, matching `^[A-Za-z0-9_-]{1,128}$`.
pub const MAX_CALL_ID_LEN: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestOperation {
    CreateCall,
    AcceptCall { call_id: String },
    RejectCall { call_id: String },
    ReferCall { call_id: String },
    HangupCall { call_id: String },
    CreateClientSecret,
    CreateLegacySession,
    CreateTranscriptionSession,
    CreateTranslationClientSecret,
    CreateTranslationCall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PathError {
    #[error("unknown Realtime REST route")]
    UnknownRoute,
    #[error("invalid Realtime call_id")]
    InvalidCallId,
}

/// Parse one of the official Realtime REST paths.
///
/// Fixed route and action segments are matched without decoding. Only the
/// dynamic call-id segment is percent-decoded, exactly once.
pub fn parse_rest_path(raw_path: &str) -> Result<RestOperation, PathError> {
    match raw_path {
        "/v1/realtime/calls" => return Ok(RestOperation::CreateCall),
        "/v1/realtime/client_secrets" => return Ok(RestOperation::CreateClientSecret),
        "/v1/realtime/sessions" => return Ok(RestOperation::CreateLegacySession),
        "/v1/realtime/transcription_sessions" => {
            return Ok(RestOperation::CreateTranscriptionSession);
        }
        "/v1/realtime/translations/client_secrets" => {
            return Ok(RestOperation::CreateTranslationClientSecret);
        }
        "/v1/realtime/translations/calls" => {
            return Ok(RestOperation::CreateTranslationCall);
        }
        _ => {}
    }

    let rest = raw_path
        .strip_prefix("/v1/realtime/calls/")
        .ok_or(PathError::UnknownRoute)?;
    let mut segments = rest.split('/');
    let raw_call_id = segments.next().ok_or(PathError::UnknownRoute)?;
    let action = segments.next().ok_or(PathError::UnknownRoute)?;
    if raw_call_id.is_empty() || action.is_empty() || segments.next().is_some() {
        return Err(PathError::UnknownRoute);
    }
    if !matches!(action, "accept" | "reject" | "refer" | "hangup") {
        return Err(PathError::UnknownRoute);
    }

    let call_id = decode_call_id(raw_call_id)?;
    validate_call_id(&call_id)?;

    match action {
        "accept" => Ok(RestOperation::AcceptCall { call_id }),
        "reject" => Ok(RestOperation::RejectCall { call_id }),
        "refer" => Ok(RestOperation::ReferCall { call_id }),
        "hangup" => Ok(RestOperation::HangupCall { call_id }),
        _ => unreachable!("action was exhaustively checked above"),
    }
}

/// Validate an already-decoded Realtime call ID.
pub fn validate_call_id(decoded: &str) -> Result<(), PathError> {
    if decoded.is_empty() || decoded.len() > MAX_CALL_ID_LEN {
        return Err(PathError::InvalidCallId);
    }
    if decoded
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        Ok(())
    } else {
        Err(PathError::InvalidCallId)
    }
}

fn decode_call_id(raw: &str) -> Result<String, PathError> {
    let bytes = raw.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return Err(PathError::InvalidCallId);
            }
            index += 3;
        } else {
            index += 1;
        }
    }

    percent_decode_str(raw)
        .decode_utf8()
        .map(|decoded| decoded.into_owned())
        .map_err(|_| PathError::InvalidCallId)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_ten_rest_operations_have_one_exact_path() {
        let rows = [
            ("/v1/realtime/calls", RestOperation::CreateCall),
            (
                "/v1/realtime/calls/rtc_a/accept",
                RestOperation::AcceptCall {
                    call_id: "rtc_a".into(),
                },
            ),
            (
                "/v1/realtime/calls/rtc_b/reject",
                RestOperation::RejectCall {
                    call_id: "rtc_b".into(),
                },
            ),
            (
                "/v1/realtime/calls/rtc_c/refer",
                RestOperation::ReferCall {
                    call_id: "rtc_c".into(),
                },
            ),
            (
                "/v1/realtime/calls/rtc_d/hangup",
                RestOperation::HangupCall {
                    call_id: "rtc_d".into(),
                },
            ),
            (
                "/v1/realtime/client_secrets",
                RestOperation::CreateClientSecret,
            ),
            ("/v1/realtime/sessions", RestOperation::CreateLegacySession),
            (
                "/v1/realtime/transcription_sessions",
                RestOperation::CreateTranscriptionSession,
            ),
            (
                "/v1/realtime/translations/client_secrets",
                RestOperation::CreateTranslationClientSecret,
            ),
            (
                "/v1/realtime/translations/calls",
                RestOperation::CreateTranslationCall,
            ),
        ];

        for (path, expected) in rows {
            assert_eq!(parse_rest_path(path), Ok(expected), "path={path}");
        }
    }

    #[test]
    fn call_id_is_strictly_decoded_once() {
        assert_eq!(
            parse_rest_path("/v1/realtime/calls/%72tc_1/accept"),
            Ok(RestOperation::AcceptCall {
                call_id: "rtc_1".into(),
            })
        );

        for path in [
            "/v1/realtime/calls/%/accept",
            "/v1/realtime/calls/%2/accept",
            "/v1/realtime/calls/%zz/accept",
            "/v1/realtime/calls/%FF/accept",
            "/v1/realtime/calls/has%2Fslash/accept",
            "/v1/realtime/calls/%252F/accept",
            "/v1/realtime/calls/%ED%95%9C%EA%B8%80/accept",
        ] {
            assert_eq!(
                parse_rest_path(path),
                Err(PathError::InvalidCallId),
                "path={path}"
            );
        }
    }

    #[test]
    fn call_id_character_and_length_boundaries_are_exact() {
        for valid in ["a".to_string(), "A0_-z".to_string(), "x".repeat(128)] {
            assert_eq!(validate_call_id(&valid), Ok(()), "call_id={valid:?}");
        }
        for invalid in [
            String::new(),
            "x".repeat(129),
            "has.dot".into(),
            "has+plus".into(),
            "has/slash".into(),
            "한글".into(),
        ] {
            assert_eq!(
                validate_call_id(&invalid),
                Err(PathError::InvalidCallId),
                "call_id={invalid:?}"
            );
        }
    }

    #[test]
    fn route_shape_mutations_do_not_expand_the_path_table() {
        for path in [
            "/v1/realtime/calls/",
            "/v1/realtime/calls//accept",
            "/v1/realtime/calls/rtc_a",
            "/v1/realtime/calls/rtc_a/",
            "/v1/realtime/calls/rtc_a/unknown",
            "/v1/realtime/calls/rtc_a/%61ccept",
            "/v1/realtime/calls/rtc_a/accept/",
            "/v1/realtime/calls/rtc_a/accept/extra",
            "/v1/realtime/client_secrets/",
            "/v1/realtime/translations/calls/",
            "/v1/realtime/not-real",
            "/v1/realtime",
        ] {
            assert_eq!(
                parse_rest_path(path),
                Err(PathError::UnknownRoute),
                "path={path}"
            );
        }
    }
}
