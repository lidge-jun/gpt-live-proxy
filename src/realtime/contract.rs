//! Pre-routing classification for official Realtime and private Live dialects.

use http::Method;
use thiserror::Error;

use crate::config::UpstreamCredentialMode;

use super::path::{parse_rest_path, PathError, RestOperation};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiDialect {
    OfficialGa,
    QuicksilverV1,
    Frameless,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Http,
    WebRtcCall,
    StandaloneWebSocket,
    ExistingCallWebSocket,
    TranslationWebSocket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Realtime,
    Transcription,
    Translation,
    Opaque,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialPolicy {
    Managed,
    ClientBearer,
    Ephemeral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolSelection {
    pub dialect: ApiDialect,
    pub transport: Transport,
    pub session_kind: SessionKind,
    pub credential: CredentialPolicy,
}

pub struct RouteFacts<'a> {
    pub method: &'a Method,
    pub path: &'a str,
    pub query: &'a [(String, String)],
    pub content_type: Option<&'a str>,
    pub openai_alpha: Option<&'a str>,
    pub credential_mode: UpstreamCredentialMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedRest {
    pub operation: RestOperation,
    pub selection: ProtocolSelection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RestContractError {
    #[error("unknown Realtime REST route")]
    UnknownRoute,
    #[error("method is not allowed for this Realtime REST route")]
    MethodNotAllowed,
    #[error("invalid Realtime call_id")]
    InvalidCallId,
    #[error("unsupported content type for this Realtime REST route")]
    UnsupportedContentType,
    #[error("private Realtime dialect requires managed credentials")]
    PrivateDialectRequiresManaged,
    #[error("private Realtime dialect is not supported on this REST route")]
    PrivateDialectNotSupported,
}

impl From<PathError> for RestContractError {
    fn from(error: PathError) -> Self {
        match error {
            PathError::UnknownRoute => Self::UnknownRoute,
            PathError::InvalidCallId => Self::InvalidCallId,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ContractError {
    #[error("method is not allowed for this Realtime route")]
    MethodNotAllowed,
    #[error("unsupported content type for this Realtime route")]
    UnsupportedContentType,
    #[error("unknown Realtime route")]
    UnknownRoute,
    #[error("invalid Realtime call_id")]
    InvalidCallId,
    #[error("Realtime query has conflicting or duplicate selectors")]
    AmbiguousQuery,
    #[error("Realtime query is missing its selector")]
    MissingSelector,
    #[error("private Realtime dialect requires managed credentials")]
    PrivateDialectRequiresManaged,
    #[error("private Realtime dialect is not supported on this route")]
    PrivateDialectNotSupported,
}

impl From<RestContractError> for ContractError {
    fn from(error: RestContractError) -> Self {
        match error {
            RestContractError::UnknownRoute => Self::UnknownRoute,
            RestContractError::MethodNotAllowed => Self::MethodNotAllowed,
            RestContractError::InvalidCallId => Self::InvalidCallId,
            RestContractError::UnsupportedContentType => Self::UnsupportedContentType,
            RestContractError::PrivateDialectRequiresManaged => Self::PrivateDialectRequiresManaged,
            RestContractError::PrivateDialectNotSupported => Self::PrivateDialectNotSupported,
        }
    }
}

pub fn classify(facts: &RouteFacts<'_>) -> Result<ProtocolSelection, ContractError> {
    if facts.path == "/v1/realtime" {
        return classify_websocket(facts);
    }
    if facts.path == "/v1/realtime/translations" {
        return classify_translation_websocket(facts);
    }
    classify_rest(facts)
        .map(|classified| classified.selection)
        .map_err(ContractError::from)
}

pub fn classify_rest(facts: &RouteFacts<'_>) -> Result<ClassifiedRest, RestContractError> {
    let operation = parse_rest_path(facts.path)?;
    if facts.method != Method::POST {
        return Err(RestContractError::MethodNotAllowed);
    }

    let selection = match &operation {
        RestOperation::CreateCall => classify_rest_call_create(facts)?,
        operation => classify_official_rest(operation, facts)?,
    };

    Ok(ClassifiedRest {
        operation,
        selection,
    })
}

fn classify_rest_call_create(
    facts: &RouteFacts<'_>,
) -> Result<ProtocolSelection, RestContractError> {
    let raw_sdp = match media_type(facts.content_type) {
        Some(value) if value.eq_ignore_ascii_case("multipart/form-data") => false,
        Some(value) if value.eq_ignore_ascii_case("application/sdp") => true,
        _ => return Err(RestContractError::UnsupportedContentType),
    };
    let dialect = private_dialect(facts.openai_alpha).unwrap_or(ApiDialect::OfficialGa);
    let credential = if dialect != ApiDialect::OfficialGa {
        match facts.credential_mode {
            UpstreamCredentialMode::Managed => CredentialPolicy::Managed,
            UpstreamCredentialMode::Client => {
                return Err(RestContractError::PrivateDialectRequiresManaged);
            }
        }
    } else if raw_sdp {
        CredentialPolicy::Ephemeral
    } else {
        configured_credential(facts.credential_mode)
    };

    Ok(ProtocolSelection {
        dialect,
        transport: Transport::WebRtcCall,
        session_kind: SessionKind::Opaque,
        credential,
    })
}

fn classify_official_rest(
    operation: &RestOperation,
    facts: &RouteFacts<'_>,
) -> Result<ProtocolSelection, RestContractError> {
    if private_dialect(facts.openai_alpha).is_some() {
        return Err(RestContractError::PrivateDialectNotSupported);
    }

    let (session_kind, credential) = match operation {
        RestOperation::AcceptCall { .. }
        | RestOperation::CreateClientSecret
        | RestOperation::CreateLegacySession => (
            SessionKind::Realtime,
            configured_credential(facts.credential_mode),
        ),
        RestOperation::CreateTranscriptionSession => (
            SessionKind::Transcription,
            configured_credential(facts.credential_mode),
        ),
        RestOperation::CreateTranslationClientSecret => (
            SessionKind::Translation,
            configured_credential(facts.credential_mode),
        ),
        RestOperation::CreateTranslationCall => {
            match media_type(facts.content_type) {
                Some(value) if value.eq_ignore_ascii_case("application/sdp") => {}
                _ => return Err(RestContractError::UnsupportedContentType),
            }
            (SessionKind::Translation, CredentialPolicy::Ephemeral)
        }
        RestOperation::RejectCall { .. }
        | RestOperation::ReferCall { .. }
        | RestOperation::HangupCall { .. } => (
            SessionKind::Opaque,
            configured_credential(facts.credential_mode),
        ),
        RestOperation::CreateCall => unreachable!("call create has its own classifier"),
    };

    Ok(ProtocolSelection {
        dialect: ApiDialect::OfficialGa,
        transport: Transport::Http,
        session_kind,
        credential,
    })
}

fn classify_websocket(facts: &RouteFacts<'_>) -> Result<ProtocolSelection, ContractError> {
    if facts.method != Method::GET {
        return Err(ContractError::MethodNotAllowed);
    }
    let transport = selector_transport(facts.query)?;
    let dialect = private_dialect(facts.openai_alpha).unwrap_or(ApiDialect::OfficialGa);
    if dialect != ApiDialect::OfficialGa {
        require_managed(facts.credential_mode)?;
    }

    Ok(ProtocolSelection {
        dialect,
        transport,
        session_kind: if transport == Transport::StandaloneWebSocket {
            SessionKind::Realtime
        } else {
            SessionKind::Opaque
        },
        credential: if dialect == ApiDialect::OfficialGa {
            configured_credential(facts.credential_mode)
        } else {
            CredentialPolicy::Managed
        },
    })
}

fn classify_translation_websocket(
    facts: &RouteFacts<'_>,
) -> Result<ProtocolSelection, ContractError> {
    if facts.method != Method::GET {
        return Err(ContractError::MethodNotAllowed);
    }
    if private_dialect(facts.openai_alpha).is_some() {
        return Err(ContractError::PrivateDialectNotSupported);
    }

    let models = selector_values(facts.query, "model");
    let call_ids = selector_values(facts.query, "call_id");
    match (models.as_slice(), call_ids.as_slice()) {
        ([model], []) if !model.trim().is_empty() => Ok(ProtocolSelection {
            dialect: ApiDialect::OfficialGa,
            transport: Transport::TranslationWebSocket,
            session_kind: SessionKind::Translation,
            credential: configured_credential(facts.credential_mode),
        }),
        ([], []) | ([_], []) => Err(ContractError::MissingSelector),
        _ => Err(ContractError::AmbiguousQuery),
    }
}

fn selector_transport(query: &[(String, String)]) -> Result<Transport, ContractError> {
    let models = selector_values(query, "model");
    let call_ids = selector_values(query, "call_id");
    match (models.as_slice(), call_ids.as_slice()) {
        ([model], []) if !model.trim().is_empty() => Ok(Transport::StandaloneWebSocket),
        ([], [call_id]) if !call_id.trim().is_empty() => Ok(Transport::ExistingCallWebSocket),
        ([], []) | ([_], []) | ([], [_]) => Err(ContractError::MissingSelector),
        _ => Err(ContractError::AmbiguousQuery),
    }
}

fn selector_values<'a>(query: &'a [(String, String)], key: &str) -> Vec<&'a str> {
    query
        .iter()
        .filter(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
        .collect()
}

fn media_type(content_type: Option<&str>) -> Option<&str> {
    let raw = content_type?;
    let parts = split_mime_parts(raw)?;
    let base = parts.first()?.trim();
    let (major, minor) = base.split_once('/')?;
    if !is_mime_token(major) || !is_mime_token(minor) {
        return None;
    }

    let mut names = Vec::new();
    let mut has_boundary = false;
    for parameter in parts.iter().skip(1) {
        let parameter = parameter.trim();
        let (name, value) = parameter.split_once('=')?;
        let name = name.trim();
        let value = value.trim();
        if !is_mime_token(name) || !valid_parameter_value(value) {
            return None;
        }
        let normalized = name.to_ascii_lowercase();
        if names.iter().any(|seen| seen == &normalized) {
            return None;
        }
        has_boundary |= normalized == "boundary";
        names.push(normalized);
    }

    if base.eq_ignore_ascii_case("multipart/form-data") && !has_boundary {
        return None;
    }
    Some(base)
}

fn split_mime_parts(raw: &str) -> Option<Vec<&str>> {
    let bytes = raw.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;

    for (index, byte) in bytes.iter().copied().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if quoted && byte == b'\\' {
            escaped = true;
            continue;
        }
        if byte == b'"' {
            quoted = !quoted;
        } else if byte == b';' && !quoted {
            parts.push(&raw[start..index]);
            start = index + 1;
        }
    }
    if quoted || escaped {
        return None;
    }
    parts.push(&raw[start..]);
    Some(parts)
}

fn is_mime_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn valid_parameter_value(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.first() == Some(&b'"') {
        if bytes.len() <= 2 || bytes.last() != Some(&b'"') {
            return false;
        }
        let mut escaped = false;
        for byte in bytes[1..bytes.len() - 1].iter().copied() {
            if escaped {
                if byte < 0x20 || byte == 0x7f {
                    return false;
                }
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' || byte < 0x20 || byte == 0x7f {
                return false;
            }
        }
        return !escaped;
    }
    is_mime_token(value)
}

fn private_dialect(openai_alpha: Option<&str>) -> Option<ApiDialect> {
    match openai_alpha.map(str::trim) {
        Some("quicksilver=v1") => Some(ApiDialect::QuicksilverV1),
        Some("quicksilver=v2") => Some(ApiDialect::Frameless),
        _ => None,
    }
}

fn configured_credential(mode: UpstreamCredentialMode) -> CredentialPolicy {
    match mode {
        UpstreamCredentialMode::Managed => CredentialPolicy::Managed,
        UpstreamCredentialMode::Client => CredentialPolicy::ClientBearer,
    }
}

fn require_managed(mode: UpstreamCredentialMode) -> Result<(), ContractError> {
    match mode {
        UpstreamCredentialMode::Managed => Ok(()),
        UpstreamCredentialMode::Client => Err(ContractError::PrivateDialectRequiresManaged),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts<'a>(
        method: &'a Method,
        path: &'a str,
        query: &'a [(String, String)],
        content_type: Option<&'a str>,
        alpha: Option<&'a str>,
        credential_mode: UpstreamCredentialMode,
    ) -> RouteFacts<'a> {
        RouteFacts {
            method,
            path,
            query,
            content_type,
            openai_alpha: alpha,
            credential_mode,
        }
    }

    fn selection(
        dialect: ApiDialect,
        transport: Transport,
        session_kind: SessionKind,
        credential: CredentialPolicy,
    ) -> ProtocolSelection {
        ProtocolSelection {
            dialect,
            transport,
            session_kind,
            credential,
        }
    }

    #[test]
    fn call_create_truth_table_covers_content_dialect_and_credentials() {
        let empty = [];
        let rows = [
            (
                "multipart/form-data; boundary=abc",
                None,
                UpstreamCredentialMode::Managed,
                Ok(selection(
                    ApiDialect::OfficialGa,
                    Transport::WebRtcCall,
                    SessionKind::Opaque,
                    CredentialPolicy::Managed,
                )),
            ),
            (
                "Multipart/Form-Data; boundary=abc",
                None,
                UpstreamCredentialMode::Client,
                Ok(selection(
                    ApiDialect::OfficialGa,
                    Transport::WebRtcCall,
                    SessionKind::Opaque,
                    CredentialPolicy::ClientBearer,
                )),
            ),
            (
                "application/sdp",
                None,
                UpstreamCredentialMode::Managed,
                Ok(selection(
                    ApiDialect::OfficialGa,
                    Transport::WebRtcCall,
                    SessionKind::Opaque,
                    CredentialPolicy::Ephemeral,
                )),
            ),
            (
                "application/sdp",
                None,
                UpstreamCredentialMode::Client,
                Ok(selection(
                    ApiDialect::OfficialGa,
                    Transport::WebRtcCall,
                    SessionKind::Opaque,
                    CredentialPolicy::Ephemeral,
                )),
            ),
            (
                "multipart/form-data; boundary=x",
                Some("quicksilver=v1"),
                UpstreamCredentialMode::Managed,
                Ok(selection(
                    ApiDialect::QuicksilverV1,
                    Transport::WebRtcCall,
                    SessionKind::Opaque,
                    CredentialPolicy::Managed,
                )),
            ),
            (
                "application/sdp",
                Some("quicksilver=v2"),
                UpstreamCredentialMode::Managed,
                Ok(selection(
                    ApiDialect::Frameless,
                    Transport::WebRtcCall,
                    SessionKind::Opaque,
                    CredentialPolicy::Managed,
                )),
            ),
            (
                "multipart/form-data; boundary=x",
                Some("quicksilver=v1"),
                UpstreamCredentialMode::Client,
                Err(ContractError::PrivateDialectRequiresManaged),
            ),
            (
                "multipart/form-data; boundary=x",
                Some("quicksilver=v2"),
                UpstreamCredentialMode::Client,
                Err(ContractError::PrivateDialectRequiresManaged),
            ),
            (
                "multipart/form-data; boundary=x",
                Some("future=v9"),
                UpstreamCredentialMode::Client,
                Ok(selection(
                    ApiDialect::OfficialGa,
                    Transport::WebRtcCall,
                    SessionKind::Opaque,
                    CredentialPolicy::ClientBearer,
                )),
            ),
            (
                "multipart/form-data; boundary=x",
                Some("future=v9"),
                UpstreamCredentialMode::Managed,
                Ok(selection(
                    ApiDialect::OfficialGa,
                    Transport::WebRtcCall,
                    SessionKind::Opaque,
                    CredentialPolicy::Managed,
                )),
            ),
        ];

        for (content_type, alpha, mode, expected) in rows {
            assert_eq!(
                classify(&facts(
                    &Method::POST,
                    "/v1/realtime/calls",
                    &empty,
                    Some(content_type),
                    alpha,
                    mode,
                )),
                expected,
                "content_type={content_type:?} alpha={alpha:?} mode={mode:?}"
            );
        }
    }

    #[test]
    fn websocket_selector_matrix_preserves_duplicate_and_order_information() {
        let rows = [
            (
                vec![("model".into(), "gpt-realtime-2.1".into())],
                Ok(Transport::StandaloneWebSocket),
            ),
            (
                vec![("call_id".into(), "rtc_a".into())],
                Ok(Transport::ExistingCallWebSocket),
            ),
            (vec![], Err(ContractError::MissingSelector)),
            (
                vec![("model".into(), "   ".into())],
                Err(ContractError::MissingSelector),
            ),
            (
                vec![("call_id".into(), "".into())],
                Err(ContractError::MissingSelector),
            ),
            (
                vec![
                    ("model".into(), "gpt-realtime-2.1".into()),
                    ("call_id".into(), "rtc_a".into()),
                ],
                Err(ContractError::AmbiguousQuery),
            ),
            (
                vec![
                    ("call_id".into(), "rtc_a".into()),
                    ("model".into(), "gpt-realtime-2.1".into()),
                ],
                Err(ContractError::AmbiguousQuery),
            ),
            (
                vec![("model".into(), "a".into()), ("model".into(), "b".into())],
                Err(ContractError::AmbiguousQuery),
            ),
            (
                vec![
                    ("call_id".into(), "rtc_a".into()),
                    ("call_id".into(), "rtc_b".into()),
                ],
                Err(ContractError::AmbiguousQuery),
            ),
        ];

        for (query, expected_transport) in rows {
            for mode in [
                UpstreamCredentialMode::Managed,
                UpstreamCredentialMode::Client,
            ] {
                let result = classify(&facts(
                    &Method::GET,
                    "/v1/realtime",
                    &query,
                    None,
                    None,
                    mode,
                ));
                assert_eq!(
                    result.map(|value| (value.transport, value.credential)),
                    expected_transport.map(|transport| (transport, configured_credential(mode))),
                    "query={query:?} mode={mode:?}"
                );
            }
        }
    }

    #[test]
    fn websocket_rows_cover_session_dialect_and_both_configured_modes() {
        let model = [("model".into(), "gpt-realtime-2.1".into())];
        let call = [("call_id".into(), "rtc_x".into())];

        assert_eq!(
            classify(&facts(
                &Method::GET,
                "/v1/realtime",
                &model,
                None,
                Some("unknown=v1"),
                UpstreamCredentialMode::Client,
            )),
            Ok(selection(
                ApiDialect::OfficialGa,
                Transport::StandaloneWebSocket,
                SessionKind::Realtime,
                CredentialPolicy::ClientBearer,
            ))
        );
        assert_eq!(
            classify(&facts(
                &Method::GET,
                "/v1/realtime",
                &call,
                None,
                Some("quicksilver=v1"),
                UpstreamCredentialMode::Managed,
            )),
            Ok(selection(
                ApiDialect::QuicksilverV1,
                Transport::ExistingCallWebSocket,
                SessionKind::Opaque,
                CredentialPolicy::Managed,
            ))
        );
        assert_eq!(
            classify(&facts(
                &Method::GET,
                "/v1/realtime",
                &model,
                None,
                Some("quicksilver=v2"),
                UpstreamCredentialMode::Client,
            )),
            Err(ContractError::PrivateDialectRequiresManaged)
        );
    }

    #[test]
    fn translation_websocket_requires_one_model_and_never_accepts_private_alpha() {
        let valid = [("model".into(), "gpt-realtime-translate".into())];
        for mode in [
            UpstreamCredentialMode::Managed,
            UpstreamCredentialMode::Client,
        ] {
            assert_eq!(
                classify(&facts(
                    &Method::GET,
                    "/v1/realtime/translations",
                    &valid,
                    None,
                    None,
                    mode,
                )),
                Ok(selection(
                    ApiDialect::OfficialGa,
                    Transport::TranslationWebSocket,
                    SessionKind::Translation,
                    configured_credential(mode),
                ))
            );
        }

        for query in [
            vec![],
            vec![("model".into(), " ".into())],
            vec![("call_id".into(), "rtc_a".into())],
            vec![("model".into(), "a".into()), ("model".into(), "b".into())],
            vec![
                ("model".into(), "a".into()),
                ("call_id".into(), "rtc_a".into()),
            ],
        ] {
            for mode in [
                UpstreamCredentialMode::Managed,
                UpstreamCredentialMode::Client,
            ] {
                assert!(classify(&facts(
                    &Method::GET,
                    "/v1/realtime/translations",
                    &query,
                    None,
                    None,
                    mode,
                ))
                .is_err());
            }
        }
        for alpha in ["quicksilver=v1", "quicksilver=v2"] {
            assert_eq!(
                classify(&facts(
                    &Method::GET,
                    "/v1/realtime/translations",
                    &valid,
                    None,
                    Some(alpha),
                    UpstreamCredentialMode::Managed,
                )),
                Err(ContractError::PrivateDialectNotSupported)
            );
        }
    }

    #[test]
    fn all_ten_rest_operations_preserve_path_and_selection_through_classify() {
        let empty = [];
        let rows = [
            (
                "/v1/realtime/calls",
                Some("multipart/form-data; boundary=abc"),
                RestOperation::CreateCall,
                Transport::WebRtcCall,
                SessionKind::Opaque,
                false,
            ),
            (
                "/v1/realtime/calls/rtc_a/accept",
                None,
                RestOperation::AcceptCall {
                    call_id: "rtc_a".into(),
                },
                Transport::Http,
                SessionKind::Realtime,
                false,
            ),
            (
                "/v1/realtime/calls/rtc_a/reject",
                None,
                RestOperation::RejectCall {
                    call_id: "rtc_a".into(),
                },
                Transport::Http,
                SessionKind::Opaque,
                false,
            ),
            (
                "/v1/realtime/calls/rtc_a/refer",
                None,
                RestOperation::ReferCall {
                    call_id: "rtc_a".into(),
                },
                Transport::Http,
                SessionKind::Opaque,
                false,
            ),
            (
                "/v1/realtime/calls/rtc_a/hangup",
                None,
                RestOperation::HangupCall {
                    call_id: "rtc_a".into(),
                },
                Transport::Http,
                SessionKind::Opaque,
                false,
            ),
            (
                "/v1/realtime/client_secrets",
                None,
                RestOperation::CreateClientSecret,
                Transport::Http,
                SessionKind::Realtime,
                false,
            ),
            (
                "/v1/realtime/sessions",
                None,
                RestOperation::CreateLegacySession,
                Transport::Http,
                SessionKind::Realtime,
                false,
            ),
            (
                "/v1/realtime/transcription_sessions",
                None,
                RestOperation::CreateTranscriptionSession,
                Transport::Http,
                SessionKind::Transcription,
                false,
            ),
            (
                "/v1/realtime/translations/client_secrets",
                None,
                RestOperation::CreateTranslationClientSecret,
                Transport::Http,
                SessionKind::Translation,
                false,
            ),
            (
                "/v1/realtime/translations/calls",
                Some("application/sdp"),
                RestOperation::CreateTranslationCall,
                Transport::Http,
                SessionKind::Translation,
                true,
            ),
        ];

        for (path, content_type, operation, transport, kind, ephemeral) in rows {
            for mode in [
                UpstreamCredentialMode::Managed,
                UpstreamCredentialMode::Client,
            ] {
                let facts = facts(
                    &Method::POST,
                    path,
                    &empty,
                    content_type,
                    Some("future=v9"),
                    mode,
                );
                let classified = classify_rest(&facts).unwrap();
                let selected = classified.selection;
                assert_eq!(classified.operation, operation, "path={path}");
                assert_eq!(selected.dialect, ApiDialect::OfficialGa, "path={path}");
                assert_eq!(selected.transport, transport, "path={path}");
                assert_eq!(selected.session_kind, kind, "path={path}");
                assert_eq!(
                    selected.credential,
                    if ephemeral {
                        CredentialPolicy::Ephemeral
                    } else {
                        configured_credential(mode)
                    },
                    "path={path} mode={mode:?}"
                );
                assert_eq!(classify(&facts), Ok(selected), "path={path}");
            }
        }

        assert_eq!(
            classify_rest(&facts(
                &Method::POST,
                "/v1/realtime/translations/calls",
                &empty,
                Some("application/json"),
                None,
                UpstreamCredentialMode::Client,
            )),
            Err(RestContractError::UnsupportedContentType)
        );
    }

    #[test]
    fn wrong_method_content_type_private_rest_and_unknown_route_are_distinct() {
        let empty = [];
        assert_eq!(
            classify(&facts(
                &Method::GET,
                "/v1/realtime/calls",
                &empty,
                Some("multipart/form-data"),
                None,
                UpstreamCredentialMode::Managed,
            )),
            Err(ContractError::MethodNotAllowed)
        );
        assert_eq!(
            classify(&facts(
                &Method::POST,
                "/v1/realtime/calls",
                &empty,
                Some("application/json"),
                None,
                UpstreamCredentialMode::Managed,
            )),
            Err(ContractError::UnsupportedContentType)
        );
        let escaped_terminator = "multipart/form-data; boundary=\"abc\\\"";
        assert!(
            escaped_terminator.as_bytes().ends_with(b"\\\""),
            "fixture must end with an escaped DQUOTE, not only a backslash"
        );
        for malformed in [
            "multipart/form-data",
            "multipart/form-data; boundary=",
            "multipart/form-data; =broken",
            "multipart/form-data; boundary=a; boundary=b",
            "multipart/form-data; boundary=a;",
            r#"multipart/form-data; boundary="abc\"#,
            escaped_terminator,
        ] {
            assert_eq!(
                classify(&facts(
                    &Method::POST,
                    "/v1/realtime/calls",
                    &empty,
                    Some(malformed),
                    None,
                    UpstreamCredentialMode::Managed,
                )),
                Err(ContractError::UnsupportedContentType),
                "{malformed} should be rejected"
            );
        }
        for valid in [
            r#"multipart/form-data; boundary="a;b""#,
            r#"multipart/form-data; boundary="a\"b""#,
        ] {
            assert!(classify(&facts(
                &Method::POST,
                "/v1/realtime/calls",
                &empty,
                Some(valid),
                None,
                UpstreamCredentialMode::Managed,
            ))
            .is_ok());
        }
        assert_eq!(
            classify(&facts(
                &Method::POST,
                "/v1/realtime/client_secrets",
                &empty,
                None,
                Some("quicksilver=v1"),
                UpstreamCredentialMode::Managed,
            )),
            Err(ContractError::PrivateDialectNotSupported)
        );
        assert_eq!(
            classify(&facts(
                &Method::POST,
                "/v1/realtime/not-real",
                &empty,
                None,
                None,
                UpstreamCredentialMode::Managed,
            )),
            Err(ContractError::UnknownRoute)
        );
        assert_eq!(
            classify(&facts(
                &Method::POST,
                "/v1/realtime/calls//accept",
                &empty,
                None,
                None,
                UpstreamCredentialMode::Managed,
            )),
            Err(ContractError::UnknownRoute)
        );
    }

    #[test]
    fn rest_error_taxonomy_and_broader_conversion_are_exhaustive() {
        let empty = [];
        let rows = [
            (
                facts(
                    &Method::POST,
                    "/v1/realtime/not-real",
                    &empty,
                    None,
                    None,
                    UpstreamCredentialMode::Managed,
                ),
                RestContractError::UnknownRoute,
            ),
            (
                facts(
                    &Method::GET,
                    "/v1/realtime/client_secrets",
                    &empty,
                    None,
                    None,
                    UpstreamCredentialMode::Managed,
                ),
                RestContractError::MethodNotAllowed,
            ),
            (
                facts(
                    &Method::POST,
                    "/v1/realtime/calls/has%2Fslash/accept",
                    &empty,
                    None,
                    None,
                    UpstreamCredentialMode::Managed,
                ),
                RestContractError::InvalidCallId,
            ),
            (
                facts(
                    &Method::POST,
                    "/v1/realtime/calls",
                    &empty,
                    Some("application/json"),
                    None,
                    UpstreamCredentialMode::Managed,
                ),
                RestContractError::UnsupportedContentType,
            ),
            (
                facts(
                    &Method::POST,
                    "/v1/realtime/calls",
                    &empty,
                    Some("multipart/form-data; boundary=x"),
                    Some("quicksilver=v1"),
                    UpstreamCredentialMode::Client,
                ),
                RestContractError::PrivateDialectRequiresManaged,
            ),
            (
                facts(
                    &Method::POST,
                    "/v1/realtime/client_secrets",
                    &empty,
                    None,
                    Some("quicksilver=v1"),
                    UpstreamCredentialMode::Managed,
                ),
                RestContractError::PrivateDialectNotSupported,
            ),
        ];

        for (facts, expected) in rows {
            assert_eq!(classify_rest(&facts), Err(expected));
            assert_eq!(classify(&facts), Err(ContractError::from(expected)));
        }

        let conversions = [
            (RestContractError::UnknownRoute, ContractError::UnknownRoute),
            (
                RestContractError::MethodNotAllowed,
                ContractError::MethodNotAllowed,
            ),
            (
                RestContractError::InvalidCallId,
                ContractError::InvalidCallId,
            ),
            (
                RestContractError::UnsupportedContentType,
                ContractError::UnsupportedContentType,
            ),
            (
                RestContractError::PrivateDialectRequiresManaged,
                ContractError::PrivateDialectRequiresManaged,
            ),
            (
                RestContractError::PrivateDialectNotSupported,
                ContractError::PrivateDialectNotSupported,
            ),
        ];
        for (rest, broader) in conversions {
            assert_eq!(ContractError::from(rest), broader);
        }
    }
}
