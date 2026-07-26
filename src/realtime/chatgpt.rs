//! Positive-evidence classification for private ChatGPT call-create sessions.
//!
//! Unknown and absent session shapes stay compatible. Only explicit public
//! session shapes, contradictory markers, or a known private dialect mismatch
//! affect routing.

use serde_json::Value;

use super::contract::ApiDialect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEvidence {
    Absent,
    Opaque,
    Quicksilver,
    Frameless,
    OfficialRealtime,
    OfficialTranscription,
    Contradictory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEvidenceError {
    InvalidShape,
    OfficialRealtime,
    OfficialTranscription,
}

/// Classify only markers whose meaning is source-proven.
///
/// A Frameless session is explicitly type-less. Consequently, any top-level
/// `type` key combined with `delegation.type=client` is contradictory, even
/// when the type value is unknown or not a string.
pub fn classify_session(session: Option<&Value>) -> SessionEvidence {
    let Some(session) = session else {
        return SessionEvidence::Absent;
    };
    let Some(object) = session.as_object() else {
        return SessionEvidence::Opaque;
    };

    let has_top_level_type = object.contains_key("type");
    let frameless_marker = object
        .get("delegation")
        .and_then(Value::as_object)
        .and_then(|delegation| delegation.get("type"))
        .and_then(Value::as_str)
        == Some("client");

    if frameless_marker {
        return if has_top_level_type {
            SessionEvidence::Contradictory
        } else {
            SessionEvidence::Frameless
        };
    }

    match object.get("type").and_then(Value::as_str) {
        Some("quicksilver") => SessionEvidence::Quicksilver,
        Some("realtime") => SessionEvidence::OfficialRealtime,
        Some("transcription") => SessionEvidence::OfficialTranscription,
        _ => SessionEvidence::Opaque,
    }
}

/// Validate positive evidence against the already-negotiated private dialect.
pub fn validate_session(
    dialect: ApiDialect,
    evidence: SessionEvidence,
) -> Result<(), SessionEvidenceError> {
    match evidence {
        SessionEvidence::Absent | SessionEvidence::Opaque => Ok(()),
        SessionEvidence::Quicksilver if dialect == ApiDialect::QuicksilverV1 => Ok(()),
        SessionEvidence::Frameless if dialect == ApiDialect::Frameless => Ok(()),
        SessionEvidence::OfficialRealtime => Err(SessionEvidenceError::OfficialRealtime),
        SessionEvidence::OfficialTranscription => Err(SessionEvidenceError::OfficialTranscription),
        SessionEvidence::Quicksilver
        | SessionEvidence::Frameless
        | SessionEvidence::Contradictory => Err(SessionEvidenceError::InvalidShape),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn evidence_edge_matrix_is_exact() {
        let rows = [
            (None, SessionEvidence::Absent),
            (Some(json!(null)), SessionEvidence::Opaque),
            (Some(json!([])), SessionEvidence::Opaque),
            (Some(json!({})), SessionEvidence::Opaque),
            (Some(json!({"type": "future"})), SessionEvidence::Opaque),
            (
                Some(json!({"type": "quicksilver"})),
                SessionEvidence::Quicksilver,
            ),
            (
                Some(json!({"delegation": {"type": "client"}})),
                SessionEvidence::Frameless,
            ),
            (
                Some(json!({"type": "realtime"})),
                SessionEvidence::OfficialRealtime,
            ),
            (
                Some(json!({"type": "transcription"})),
                SessionEvidence::OfficialTranscription,
            ),
            (
                Some(json!({
                    "type": "quicksilver",
                    "delegation": {"type": "client"}
                })),
                SessionEvidence::Contradictory,
            ),
            (
                Some(json!({
                    "type": "future",
                    "delegation": {"type": "client"}
                })),
                SessionEvidence::Contradictory,
            ),
            (
                Some(json!({
                    "type": null,
                    "delegation": {"type": "client"}
                })),
                SessionEvidence::Contradictory,
            ),
        ];

        for (session, expected) in rows {
            assert_eq!(classify_session(session.as_ref()), expected);
        }
    }

    #[test]
    fn evidence_and_dialect_cross_product_is_exact() {
        use ApiDialect::{Frameless as FramelessDialect, QuicksilverV1};
        use SessionEvidence::*;

        let rows = [
            (QuicksilverV1, Absent, Ok(())),
            (QuicksilverV1, Opaque, Ok(())),
            (QuicksilverV1, Quicksilver, Ok(())),
            (
                QuicksilverV1,
                Frameless,
                Err(SessionEvidenceError::InvalidShape),
            ),
            (FramelessDialect, Absent, Ok(())),
            (FramelessDialect, Opaque, Ok(())),
            (FramelessDialect, Frameless, Ok(())),
            (
                FramelessDialect,
                Quicksilver,
                Err(SessionEvidenceError::InvalidShape),
            ),
            (
                FramelessDialect,
                Contradictory,
                Err(SessionEvidenceError::InvalidShape),
            ),
            (
                QuicksilverV1,
                OfficialRealtime,
                Err(SessionEvidenceError::OfficialRealtime),
            ),
            (
                FramelessDialect,
                OfficialTranscription,
                Err(SessionEvidenceError::OfficialTranscription),
            ),
        ];

        for (dialect, evidence, expected) in rows {
            assert_eq!(validate_session(dialect, evidence), expected);
        }
    }
}
