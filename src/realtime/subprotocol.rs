//! Parser for the official browser WebSocket subprotocol authentication shape.

use std::fmt;

use http::{header, HeaderMap, HeaderValue};

use crate::config::BearerToken;
use crate::error::RelayError;

const REALTIME: &str = "realtime";
const CREDENTIAL_PREFIX: &str = "openai-insecure-api-key.";
const ORGANIZATION_PREFIX: &str = "openai-organization.";
const PROJECT_PREFIX: &str = "openai-project.";
const MAX_PROTOCOL_TOKEN_BYTES: usize = 4096;
const MAX_PROTOCOL_AGGREGATE_BYTES: usize = 8192;

pub struct ParsedProtocols {
    pub offered: Vec<String>,
    pub upstream_header: Option<HeaderValue>,
    pub browser_credential: Option<HeaderValue>,
    pub(crate) has_organization: bool,
    pub(crate) has_project: bool,
}

impl fmt::Debug for ParsedProtocols {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ParsedProtocols")
            .field("offered_count", &self.offered.len())
            .field("has_upstream_header", &self.upstream_header.is_some())
            .field("has_browser_credential", &self.browser_credential.is_some())
            .field("has_organization", &self.has_organization)
            .field("has_project", &self.has_project)
            .finish()
    }
}

pub fn parse(
    headers: &HeaderMap,
    admission: Option<&BearerToken>,
) -> Result<ParsedProtocols, RelayError> {
    let mut offered = Vec::new();
    let mut aggregate = 0usize;
    let mut has_realtime = false;
    let mut credential: Option<HeaderValue> = None;
    let mut has_organization = false;
    let mut has_project = false;

    for field in headers.get_all(header::SEC_WEBSOCKET_PROTOCOL).iter() {
        let raw = field
            .to_str()
            .map_err(|_| RelayError::InvalidRealtimeSubprotocol)?;
        aggregate = aggregate
            .checked_add(raw.len())
            .ok_or(RelayError::InvalidRealtimeSubprotocol)?;
        if aggregate > MAX_PROTOCOL_AGGREGATE_BYTES {
            return Err(RelayError::InvalidRealtimeSubprotocol);
        }

        for raw_token in raw.split(',') {
            let token = raw_token.trim_matches([' ', '\t']);
            if token.is_empty()
                || token.len() > MAX_PROTOCOL_TOKEN_BYTES
                || !token.bytes().all(is_rfc_token_byte)
                || offered.iter().any(|seen| seen == token)
            {
                return Err(RelayError::InvalidRealtimeSubprotocol);
            }

            if token == REALTIME {
                if has_realtime {
                    return Err(RelayError::InvalidRealtimeSubprotocol);
                }
                has_realtime = true;
            } else if let Some(suffix) = token.strip_prefix(CREDENTIAL_PREFIX) {
                if suffix.is_empty() || credential.is_some() {
                    return Err(RelayError::InvalidRealtimeSubprotocol);
                }
                if admission.is_some_and(|secret| secret.ct_eq(suffix)) {
                    return Err(RelayError::AdmissionSecretNotForwardable);
                }
                let mut value = HeaderValue::from_str(suffix)
                    .map_err(|_| RelayError::InvalidRealtimeSubprotocol)?;
                value.set_sensitive(true);
                credential = Some(value);
            } else if let Some(suffix) = token.strip_prefix(ORGANIZATION_PREFIX) {
                if suffix.is_empty() || has_organization {
                    return Err(RelayError::InvalidRealtimeSubprotocol);
                }
                has_organization = true;
            } else if let Some(suffix) = token.strip_prefix(PROJECT_PREFIX) {
                if suffix.is_empty() || has_project {
                    return Err(RelayError::InvalidRealtimeSubprotocol);
                }
                has_project = true;
            } else {
                return Err(RelayError::InvalidRealtimeSubprotocol);
            }
            offered.push(token.to_string());
        }
    }

    if (credential.is_some() || has_organization || has_project) && !has_realtime {
        return Err(RelayError::InvalidRealtimeSubprotocol);
    }

    let canonical_len = offered
        .iter()
        .enumerate()
        .try_fold(0usize, |length, (index, token)| {
            length
                .checked_add(usize::from(index != 0) * 2)
                .and_then(|length| length.checked_add(token.len()))
        });
    if canonical_len.is_none_or(|length| length > MAX_PROTOCOL_AGGREGATE_BYTES) {
        return Err(RelayError::InvalidRealtimeSubprotocol);
    }

    let upstream_header = if offered.is_empty() {
        None
    } else {
        let mut value = HeaderValue::from_str(&offered.join(", "))
            .map_err(|_| RelayError::InvalidRealtimeSubprotocol)?;
        value.set_sensitive(true);
        Some(value)
    };

    Ok(ParsedProtocols {
        offered,
        upstream_header,
        browser_credential: credential,
        has_organization,
        has_project,
    })
}

pub fn validate_selected(
    upstream: &HeaderMap,
    offered: &ParsedProtocols,
) -> Result<Option<String>, RelayError> {
    let mut selected = upstream.get_all(header::SEC_WEBSOCKET_PROTOCOL).iter();
    let Some(value) = selected.next() else {
        return Ok(None);
    };
    if selected.next().is_some() {
        return Err(RelayError::UpstreamWebSocketProtocol);
    }
    let token = value
        .to_str()
        .map_err(|_| RelayError::UpstreamWebSocketProtocol)?
        .trim_matches([' ', '\t']);
    if token != REALTIME
        || !token.bytes().all(is_rfc_token_byte)
        || !offered.offered.iter().any(|offered| offered == token)
    {
        return Err(RelayError::UpstreamWebSocketProtocol);
    }
    Ok(Some(REALTIME.to_string()))
}

fn is_rfc_token_byte(byte: u8) -> bool {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn protocols(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(
                header::SEC_WEBSOCKET_PROTOCOL,
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn repeated_fields_are_canonicalized_in_wire_order_and_redacted() {
        let parsed = parse(
            &protocols(&[
                "realtime, openai-insecure-api-key.ephemeral-secret",
                "openai-organization.org_1, openai-project.proj_1",
            ]),
            None,
        )
        .unwrap();
        assert_eq!(
            parsed.offered,
            [
                "realtime",
                "openai-insecure-api-key.ephemeral-secret",
                "openai-organization.org_1",
                "openai-project.proj_1",
            ]
        );
        assert_eq!(
            parsed.upstream_header.as_ref().unwrap(),
            "realtime, openai-insecure-api-key.ephemeral-secret, openai-organization.org_1, openai-project.proj_1"
        );
        assert!(parsed.upstream_header.as_ref().unwrap().is_sensitive());
        assert!(parsed.browser_credential.as_ref().unwrap().is_sensitive());
        assert!(!format!("{parsed:?}").contains("ephemeral-secret"));
    }

    #[test]
    fn empty_headers_are_valid_and_realtime_alone_is_valid() {
        let empty = parse(&HeaderMap::new(), None).unwrap();
        assert!(empty.offered.is_empty());
        assert!(empty.upstream_header.is_none());
        assert!(empty.browser_credential.is_none());

        let realtime = parse(&protocols(&["realtime"]), None).unwrap();
        assert_eq!(realtime.offered, ["realtime"]);
    }

    #[test]
    fn rejects_unknown_empty_duplicate_class_and_missing_realtime() {
        let repeated = "a".repeat(MAX_PROTOCOL_TOKEN_BYTES + 1);
        let cases = [
            vec!["".to_string()],
            vec!["realtime,".to_string()],
            vec!["chat".to_string()],
            vec!["realtime, realtime".to_string()],
            vec!["realtime, openai-insecure-api-key.a, openai-insecure-api-key.b".to_string()],
            vec!["realtime, openai-organization.a, openai-organization.b".to_string()],
            vec!["realtime, openai-project.a, openai-project.b".to_string()],
            vec!["openai-insecure-api-key.a".to_string()],
            vec!["openai-organization.a".to_string()],
            vec![format!("realtime, {repeated}")],
        ];
        for values in cases {
            let refs: Vec<&str> = values.iter().map(String::as_str).collect();
            assert!(matches!(
                parse(&protocols(&refs), None),
                Err(RelayError::InvalidRealtimeSubprotocol)
            ));
        }
    }

    #[test]
    fn aggregate_limit_and_admission_crossover_fail_before_use() {
        let a = format!("openai-organization.{}", "a".repeat(4070));
        let b = format!("openai-project.{}", "b".repeat(4080));
        assert!(matches!(
            parse(&protocols(&["realtime", &a, &b]), None),
            Err(RelayError::InvalidRealtimeSubprotocol)
        ));

        assert!(matches!(
            parse(
                &protocols(&["realtime, openai-insecure-api-key.proxy-secret"]),
                Some(&BearerToken::new("proxy-secret")),
            ),
            Err(RelayError::AdmissionSecretNotForwardable)
        ));
    }

    #[test]
    fn selected_protocol_must_be_one_offered_safe_realtime_value() {
        let offered = parse(
            &protocols(&["realtime, openai-insecure-api-key.secret"]),
            None,
        )
        .unwrap();
        assert_eq!(
            validate_selected(&HeaderMap::new(), &offered).unwrap(),
            None
        );
        assert_eq!(
            validate_selected(&protocols(&["realtime"]), &offered).unwrap(),
            Some("realtime".into())
        );
        for invalid in [
            protocols(&["openai-insecure-api-key.secret"]),
            protocols(&["other"]),
            protocols(&["realtime, other"]),
            protocols(&["realtime", "realtime"]),
        ] {
            assert!(matches!(
                validate_selected(&invalid, &offered),
                Err(RelayError::UpstreamWebSocketProtocol)
            ));
        }
        let none_offered = parse(&HeaderMap::new(), None).unwrap();
        assert!(matches!(
            validate_selected(&protocols(&["realtime"]), &none_offered),
            Err(RelayError::UpstreamWebSocketProtocol)
        ));
    }
}
