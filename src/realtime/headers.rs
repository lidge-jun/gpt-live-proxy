//! Exact allowlist-based header policy for public Realtime traffic.

use http::{header, HeaderMap, HeaderName, HeaderValue};

use crate::config::{BearerToken, UpstreamProfile};
use crate::error::RelayError;
use crate::realtime::contract::{ApiDialect, CredentialPolicy, ProtocolSelection};
use crate::realtime::subprotocol::{self, ParsedProtocols};

pub const REQUEST_HEADER_ALLOWLIST: [&str; 9] = [
    "content-type",
    "accept",
    "openai-organization",
    "openai-project",
    "openai-safety-identifier",
    "openai-beta",
    "idempotency-key",
    "openai-alpha",
    "x-oai-attestation",
];

pub const RESPONSE_HEADER_ALLOWLIST: [&str; 6] = [
    "content-type",
    "location",
    "retry-after",
    "x-request-id",
    "openai-processing-ms",
    "openai-version",
];

pub const WEBSOCKET_RESPONSE_HEADER_ALLOWLIST: [&str; 3] =
    ["x-request-id", "openai-processing-ms", "openai-version"];

pub struct WebSocketHeaders {
    pub headers: HeaderMap,
    pub protocols: ParsedProtocols,
}

const SINGLETON_REQUEST_HEADERS: [&str; 8] = [
    "content-type",
    "openai-organization",
    "openai-project",
    "openai-safety-identifier",
    "idempotency-key",
    "openai-alpha",
    "authorization",
    "x-oai-attestation",
];

const SENSITIVE_REQUEST_HEADERS: [&str; 5] = [
    "openai-organization",
    "openai-project",
    "openai-safety-identifier",
    "idempotency-key",
    "x-oai-attestation",
];

pub fn upstream_headers(
    client: &HeaderMap,
    profile: &UpstreamProfile,
    selection: &ProtocolSelection,
) -> Result<HeaderMap, RelayError> {
    reject_duplicate_singletons(client, selection.dialect)?;

    let mut out = HeaderMap::new();
    for name in REQUEST_HEADER_ALLOWLIST {
        if name == "openai-alpha" {
            copy_private_alpha(client, &mut out, selection.dialect)?;
        } else if name == "x-oai-attestation" {
            if selection.dialect != ApiDialect::OfficialGa {
                copy_single_non_empty(client, &mut out, name, true);
            }
        } else if matches!(name, "accept" | "openai-beta") {
            append_non_empty_values(client, &mut out, name, false);
        } else {
            copy_single_non_empty(
                client,
                &mut out,
                name,
                SENSITIVE_REQUEST_HEADERS.contains(&name),
            );
        }
    }

    if selection.dialect != ApiDialect::OfficialGa {
        if let Some(account_id) = profile.account_id_raw() {
            let mut value = HeaderValue::from_str(account_id).map_err(|_| invalid_header())?;
            value.set_sensitive(true);
            out.insert(HeaderName::from_static("chatgpt-account-id"), value);
        }
    }

    let authorization = authorization(client, profile, selection.credential)?;
    out.insert(header::AUTHORIZATION, authorization);
    Ok(out)
}

pub fn upstream_websocket_headers(
    inbound: &HeaderMap,
    profile: &UpstreamProfile,
    selection: &ProtocolSelection,
    admission: Option<&BearerToken>,
) -> Result<WebSocketHeaders, RelayError> {
    let protocols = subprotocol::parse(inbound, admission)?;
    reject_websocket_singletons(inbound, selection.dialect)?;
    validate_websocket_metadata(inbound)?;

    if selection.dialect != ApiDialect::OfficialGa && !protocols.offered.is_empty() {
        return Err(RelayError::InvalidRealtimeSubprotocol);
    }

    if (protocols.has_organization && inbound.contains_key("openai-organization"))
        || (protocols.has_project && inbound.contains_key("openai-project"))
    {
        return Err(RelayError::InvalidRealtimeSubprotocol);
    }

    let authorization_count = inbound.get_all(header::AUTHORIZATION).iter().count();
    if protocols.browser_credential.is_some() && authorization_count != 0 {
        return Err(RelayError::InvalidRealtimeSubprotocol);
    }

    let mut out = HeaderMap::new();
    copy_single_non_empty(inbound, &mut out, "origin", false);
    copy_single_non_empty(inbound, &mut out, "openai-organization", true);
    copy_single_non_empty(inbound, &mut out, "openai-project", true);
    copy_single_non_empty(inbound, &mut out, "openai-safety-identifier", true);
    append_non_empty_values(inbound, &mut out, "openai-beta", false);
    copy_private_alpha(inbound, &mut out, selection.dialect)?;
    if selection.dialect != ApiDialect::OfficialGa {
        copy_single_non_empty(inbound, &mut out, "x-oai-attestation", true);
        if let Some(account_id) = profile.account_id_raw() {
            let mut value =
                HeaderValue::from_str(account_id).map_err(|_| RelayError::InvalidRealtimeHeader)?;
            value.set_sensitive(true);
            out.insert(HeaderName::from_static("chatgpt-account-id"), value);
        }
    }

    if protocols.browser_credential.is_none() {
        out.insert(
            header::AUTHORIZATION,
            authorization(inbound, profile, selection.credential)?,
        );
    }
    if let Some(value) = &protocols.upstream_header {
        out.insert(header::SEC_WEBSOCKET_PROTOCOL, value.clone());
    }

    Ok(WebSocketHeaders {
        headers: out,
        protocols,
    })
}

pub fn response_headers(upstream: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for name in upstream.keys() {
        let normalized = name.as_str();
        if RESPONSE_HEADER_ALLOWLIST.contains(&normalized) || normalized.starts_with("x-ratelimit-")
        {
            for value in upstream.get_all(name).iter() {
                out.append(name.clone(), value.clone());
            }
        }
    }
    out
}

pub fn websocket_response_headers(upstream: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for name in upstream.keys() {
        let normalized = name.as_str();
        if WEBSOCKET_RESPONSE_HEADER_ALLOWLIST.contains(&normalized)
            || normalized.starts_with("x-ratelimit-")
        {
            for value in upstream.get_all(name).iter() {
                out.append(name.clone(), value.clone());
            }
        }
    }
    out
}

fn reject_websocket_singletons(inbound: &HeaderMap, dialect: ApiDialect) -> Result<(), RelayError> {
    for name in [
        "origin",
        "openai-organization",
        "openai-project",
        "openai-safety-identifier",
        "openai-alpha",
        "x-oai-attestation",
    ] {
        if name == "x-oai-attestation" && dialect == ApiDialect::OfficialGa {
            continue;
        }
        if inbound.get_all(name).iter().count() > 1 {
            return Err(RelayError::InvalidRealtimeHeader);
        }
    }
    if inbound.get_all(header::AUTHORIZATION).iter().count() > 1 {
        return Err(RelayError::AmbiguousAuthorization);
    }
    Ok(())
}

fn validate_websocket_metadata(inbound: &HeaderMap) -> Result<(), RelayError> {
    for name in [
        "origin",
        "openai-organization",
        "openai-project",
        "openai-safety-identifier",
        "openai-alpha",
        "x-oai-attestation",
        "openai-beta",
    ] {
        if inbound
            .get_all(name)
            .iter()
            .any(|value| value.to_str().is_err())
        {
            return Err(RelayError::InvalidRealtimeHeader);
        }
    }
    Ok(())
}

fn reject_duplicate_singletons(client: &HeaderMap, dialect: ApiDialect) -> Result<(), RelayError> {
    for name in SINGLETON_REQUEST_HEADERS {
        if name == "x-oai-attestation" && dialect == ApiDialect::OfficialGa {
            continue;
        }
        if client.get_all(name).iter().count() > 1 {
            return if name == "authorization" {
                Err(RelayError::AmbiguousAuthorization)
            } else {
                Err(invalid_header())
            };
        }
    }
    Ok(())
}

fn append_non_empty_values(
    client: &HeaderMap,
    out: &mut HeaderMap,
    name: &'static str,
    sensitive: bool,
) {
    let header_name = HeaderName::from_static(name);
    for value in client.get_all(name).iter().filter(|value| !is_empty(value)) {
        let mut value = value.clone();
        value.set_sensitive(sensitive);
        out.append(header_name.clone(), value);
    }
}

fn copy_single_non_empty(
    client: &HeaderMap,
    out: &mut HeaderMap,
    name: &'static str,
    sensitive: bool,
) {
    let Some(value) = client.get(name).filter(|value| !is_empty(value)) else {
        return;
    };
    let mut value = value.clone();
    value.set_sensitive(sensitive);
    out.insert(HeaderName::from_static(name), value);
}

fn copy_private_alpha(
    client: &HeaderMap,
    out: &mut HeaderMap,
    dialect: ApiDialect,
) -> Result<(), RelayError> {
    let expected = match dialect {
        ApiDialect::QuicksilverV1 => Some("quicksilver=v1"),
        ApiDialect::Frameless => Some("quicksilver=v2"),
        ApiDialect::OfficialGa => None,
    };
    let Some(expected) = expected else {
        return Ok(());
    };
    let value = client
        .get("openai-alpha")
        .filter(|value| !is_empty(value))
        .ok_or_else(invalid_header)?;
    if value.to_str().ok().map(str::trim) != Some(expected) {
        return Err(invalid_header());
    }
    out.insert(HeaderName::from_static("openai-alpha"), value.clone());
    Ok(())
}

fn authorization(
    client: &HeaderMap,
    profile: &UpstreamProfile,
    policy: CredentialPolicy,
) -> Result<HeaderValue, RelayError> {
    match policy {
        CredentialPolicy::Managed => managed_authorization(profile.managed_auth()),
        CredentialPolicy::ClientBearer | CredentialPolicy::Ephemeral => {
            let mut value = client
                .get(header::AUTHORIZATION)
                .filter(|value| valid_bearer(value))
                .cloned()
                .ok_or(RelayError::NoCredential)?;
            value.set_sensitive(true);
            Ok(value)
        }
    }
}

fn managed_authorization(
    managed_auth: Option<&crate::config::BearerToken>,
) -> Result<HeaderValue, RelayError> {
    managed_auth
        .ok_or(RelayError::NoCredential)?
        .authorization_header()
        .map_err(|_| RelayError::NoCredential)
}

fn valid_bearer(value: &HeaderValue) -> bool {
    let Ok(value) = value.to_str() else {
        return false;
    };
    let mut parts = value.split_ascii_whitespace();
    let Some(scheme) = parts.next() else {
        return false;
    };
    let Some(token) = parts.next() else {
        return false;
    };
    if parts.next().is_some() || !scheme.eq_ignore_ascii_case("bearer") || token.is_empty() {
        return false;
    }

    let mut padding = false;
    let mut has_base = false;
    let valid = token.bytes().all(|byte| {
        if byte == b'=' {
            padding = true;
            has_base
        } else if padding {
            false
        } else {
            let allowed = byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/');
            has_base |= allowed;
            allowed
        }
    });
    valid && has_base
}

fn is_empty(value: &HeaderValue) -> bool {
    value.to_str().is_ok_and(|value| value.trim().is_empty())
}

fn invalid_header() -> RelayError {
    RelayError::InvalidRealtimeHeader
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AccountId, BearerToken};
    use crate::realtime::contract::{SessionKind, Transport};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn profile(managed: &str) -> UpstreamProfile {
        UpstreamProfile::ApiKeyManaged {
            base_url: "https://api.openai.com/v1".into(),
            auth: BearerToken::new(managed),
        }
    }

    fn selected(dialect: ApiDialect, credential: CredentialPolicy) -> ProtocolSelection {
        ProtocolSelection {
            dialect,
            transport: Transport::Http,
            session_kind: SessionKind::Opaque,
            credential,
        }
    }

    fn flattened(map: &HeaderMap) -> Vec<(String, String, bool)> {
        let mut values = Vec::new();
        for name in map.keys() {
            for value in map.get_all(name).iter() {
                values.push((
                    name.as_str().to_string(),
                    value.to_str().unwrap().to_string(),
                    value.is_sensitive(),
                ));
            }
        }
        values.sort();
        values
    }

    #[test]
    fn request_map_is_exact_and_marks_identity_classes_sensitive() {
        let client = headers(&[
            ("Content-Type", "application/json"),
            ("Accept", "application/json"),
            ("accept", "text/event-stream"),
            ("OpenAI-Organization", "org-secret"),
            ("OpenAI-Project", "proj-secret"),
            ("OpenAI-Safety-Identifier", "safety-secret"),
            ("OpenAI-Beta", "realtime=v1"),
            ("openai-beta", "future=v2"),
            ("Idempotency-Key", "idem-secret"),
            ("Authorization", "Bearer caller-token"),
            ("Cookie", "session=secret"),
            ("Connection", "keep-alive"),
            ("Transfer-Encoding", "chunked"),
            ("X-GPT-Live-API-Key", "proxy-secret"),
            ("X-Custom", "drop-me"),
        ]);
        let out = upstream_headers(
            &client,
            &profile("must-not-win"),
            &selected(ApiDialect::OfficialGa, CredentialPolicy::ClientBearer),
        )
        .unwrap();

        assert_eq!(
            flattened(&out),
            vec![
                ("accept".into(), "application/json".into(), false),
                ("accept".into(), "text/event-stream".into(), false),
                ("authorization".into(), "Bearer caller-token".into(), true),
                ("content-type".into(), "application/json".into(), false),
                ("idempotency-key".into(), "idem-secret".into(), true),
                ("openai-beta".into(), "future=v2".into(), false),
                ("openai-beta".into(), "realtime=v1".into(), false),
                ("openai-organization".into(), "org-secret".into(), true),
                ("openai-project".into(), "proj-secret".into(), true),
                (
                    "openai-safety-identifier".into(),
                    "safety-secret".into(),
                    true
                ),
            ]
        );
    }

    #[test]
    fn empty_singletons_and_unknown_names_are_dropped() {
        let client = headers(&[
            ("content-type", "   "),
            ("openai-organization", ""),
            ("x-unknown", "must-not-cross"),
            ("cookie", "session=must-not-cross"),
        ]);
        let out = upstream_headers(
            &client,
            &profile("managed"),
            &selected(ApiDialect::OfficialGa, CredentialPolicy::Managed),
        )
        .unwrap();
        assert_eq!(
            flattened(&out),
            [("authorization".into(), "Bearer managed".into(), true)]
        );
    }

    #[test]
    fn managed_auth_is_inserted_last_and_client_authorization_is_ignored() {
        let client = headers(&[("authorization", "Bearer caller-token")]);
        let out = upstream_headers(
            &client,
            &profile("managed-token"),
            &selected(ApiDialect::OfficialGa, CredentialPolicy::Managed),
        )
        .unwrap();
        let auth = out.get(header::AUTHORIZATION).unwrap();
        assert_eq!(auth, "Bearer managed-token");
        assert!(auth.is_sensitive());
        assert!(!format!("{out:?}").contains("managed-token"));
        assert!(!format!("{out:?}").contains("caller-token"));
    }

    #[test]
    fn missing_managed_auth_fails_closed() {
        assert!(matches!(
            managed_authorization(None),
            Err(RelayError::NoCredential)
        ));
    }

    #[test]
    fn client_policies_require_one_valid_bearer() {
        for policy in [CredentialPolicy::ClientBearer, CredentialPolicy::Ephemeral] {
            for invalid in [
                None,
                Some("Basic abc"),
                Some("Bearer"),
                Some("Bearer a b"),
                Some("Bearer abc=tail"),
                Some("Bearer ="),
                Some("Bearer ==="),
            ] {
                let client = invalid
                    .map_or_else(HeaderMap::new, |value| headers(&[("authorization", value)]));
                assert!(matches!(
                    upstream_headers(
                        &client,
                        &profile("must-not-win"),
                        &selected(ApiDialect::OfficialGa, policy),
                    ),
                    Err(RelayError::NoCredential)
                ));
            }
            let client = headers(&[("authorization", "bEaReR abc-._~+/==")]);
            let out = upstream_headers(
                &client,
                &profile("must-not-win"),
                &selected(ApiDialect::OfficialGa, policy),
            )
            .unwrap();
            assert_eq!(
                out.get(header::AUTHORIZATION).unwrap(),
                "bEaReR abc-._~+/=="
            );
        }
    }

    #[test]
    fn duplicate_singletons_fail_but_list_headers_preserve_non_empty_order() {
        for name in SINGLETON_REQUEST_HEADERS {
            if name == "x-oai-attestation" {
                continue;
            }
            let client = headers(&[(name, "one"), (name, "two")]);
            assert!(
                upstream_headers(
                    &client,
                    &profile("managed"),
                    &selected(ApiDialect::OfficialGa, CredentialPolicy::Managed),
                )
                .is_err(),
                "duplicate {name} was accepted"
            );
        }

        let client = headers(&[
            ("accept", "first"),
            ("accept", ""),
            ("accept", "second"),
            ("openai-beta", "one"),
            ("openai-beta", "   "),
            ("openai-beta", "two"),
        ]);
        let out = upstream_headers(
            &client,
            &profile("managed"),
            &selected(ApiDialect::OfficialGa, CredentialPolicy::Managed),
        )
        .unwrap();
        assert_eq!(
            out.get_all("accept")
                .iter()
                .map(|value| value.to_str().unwrap())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );

        let repeated_attestation = headers(&[
            ("openai-alpha", "quicksilver=v2"),
            ("x-oai-attestation", "private-one"),
            ("x-oai-attestation", "private-two"),
        ]);
        let official = upstream_headers(
            &repeated_attestation,
            &profile("managed"),
            &selected(ApiDialect::OfficialGa, CredentialPolicy::Managed),
        )
        .unwrap();
        assert!(!official.contains_key("x-oai-attestation"));
        assert!(matches!(
            upstream_headers(
                &repeated_attestation,
                &profile("managed"),
                &selected(ApiDialect::Frameless, CredentialPolicy::Managed),
            ),
            Err(RelayError::InvalidRealtimeHeader)
        ));
        assert_eq!(
            out.get_all("openai-beta")
                .iter()
                .map(|value| value.to_str().unwrap())
                .collect::<Vec<_>>(),
            ["one", "two"]
        );
    }

    #[test]
    fn private_alpha_is_copied_only_when_it_matches_the_selected_private_dialect() {
        for (dialect, alpha) in [
            (ApiDialect::QuicksilverV1, "quicksilver=v1"),
            (ApiDialect::Frameless, "quicksilver=v2"),
        ] {
            let out = upstream_headers(
                &headers(&[
                    ("openai-alpha", alpha),
                    ("x-oai-attestation", "attestation-secret"),
                ]),
                &profile("managed"),
                &selected(dialect, CredentialPolicy::Managed),
            )
            .unwrap();
            assert_eq!(out.get("openai-alpha").unwrap(), alpha);
            assert!(out.get("x-oai-attestation").unwrap().is_sensitive());
            assert!(!format!("{out:?}").contains("attestation-secret"));
        }

        for alpha in ["future=v9", "quicksilver=v1"] {
            let out = upstream_headers(
                &headers(&[("openai-alpha", alpha)]),
                &profile("managed"),
                &selected(ApiDialect::OfficialGa, CredentialPolicy::Managed),
            )
            .unwrap();
            assert!(!out.contains_key("openai-alpha"));
            assert!(!out.contains_key("x-oai-attestation"));
        }
        assert!(upstream_headers(
            &headers(&[("openai-alpha", "quicksilver=v1")]),
            &profile("managed"),
            &selected(ApiDialect::Frameless, CredentialPolicy::Managed),
        )
        .is_err());
    }

    #[test]
    fn private_account_identity_is_proxy_owned_and_sensitive() {
        let profile = UpstreamProfile::ChatGptBackend {
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            auth: BearerToken::new("managed"),
            account_id: Some(AccountId::new("acct-secret")),
        };
        let out = upstream_headers(
            &headers(&[("openai-alpha", "quicksilver=v2")]),
            &profile,
            &selected(ApiDialect::Frameless, CredentialPolicy::Managed),
        )
        .unwrap();
        assert!(out.get("chatgpt-account-id").unwrap().is_sensitive());
        assert!(!format!("{out:?}").contains("acct-secret"));

        let official = upstream_headers(
            &HeaderMap::new(),
            &profile,
            &selected(ApiDialect::OfficialGa, CredentialPolicy::Managed),
        )
        .unwrap();
        assert!(
            !official.contains_key("chatgpt-account-id"),
            "private account routing must not leak into an official selection"
        );
    }

    #[test]
    fn response_map_is_exact_and_keeps_all_values_in_upstream_order() {
        let upstream = headers(&[
            ("Content-Type", "application/json"),
            ("Location", "/v1/realtime/calls/rtc_a"),
            ("Retry-After", "2"),
            ("retry-after", "4"),
            ("X-Request-ID", "req_1"),
            ("OpenAI-Processing-Ms", "12"),
            ("OpenAI-Version", "2026-07-01"),
            ("X-RateLimit-Limit-Requests", "100"),
            ("x-ratelimit-future-window", "30"),
            ("X-Rate-Limit-Near-Miss", "drop"),
            ("Set-Cookie", "session=secret"),
            ("Connection", "close"),
            ("Upgrade", "websocket"),
            ("Transfer-Encoding", "chunked"),
            ("X-GPT-Live-API-Key", "proxy-secret"),
            ("X-Arbitrary", "drop"),
        ]);
        let out = response_headers(&upstream);

        assert_eq!(
            out.get_all("retry-after")
                .iter()
                .map(|value| value.to_str().unwrap())
                .collect::<Vec<_>>(),
            ["2", "4"],
            "allowed response duplicates must retain upstream order"
        );

        assert_eq!(
            flattened(&out),
            vec![
                ("content-type".into(), "application/json".into(), false),
                ("location".into(), "/v1/realtime/calls/rtc_a".into(), false),
                ("openai-processing-ms".into(), "12".into(), false),
                ("openai-version".into(), "2026-07-01".into(), false),
                ("retry-after".into(), "2".into(), false),
                ("retry-after".into(), "4".into(), false),
                ("x-ratelimit-future-window".into(), "30".into(), false),
                ("x-ratelimit-limit-requests".into(), "100".into(), false),
                ("x-request-id".into(), "req_1".into(), false),
            ]
        );
    }

    #[test]
    fn websocket_browser_credential_wins_without_authorization() {
        let inbound = headers(&[
            ("origin", "https://app.test"),
            ("openai-beta", "realtime=v1"),
            (
                "sec-websocket-protocol",
                "realtime, openai-insecure-api-key.browser-secret",
            ),
            ("cookie", "must-not-cross"),
            ("host", "downstream.test"),
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-key", "must-not-cross"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-extensions", "permessage-deflate"),
        ]);
        let built = upstream_websocket_headers(
            &inbound,
            &profile("managed-must-not-cross"),
            &selected(ApiDialect::OfficialGa, CredentialPolicy::Managed),
            None,
        )
        .unwrap();

        assert_eq!(
            flattened(&built.headers),
            vec![
                ("openai-beta".into(), "realtime=v1".into(), false),
                ("origin".into(), "https://app.test".into(), false),
                (
                    "sec-websocket-protocol".into(),
                    "realtime, openai-insecure-api-key.browser-secret".into(),
                    true,
                ),
            ]
        );
        assert!(!built.headers.contains_key(header::AUTHORIZATION));
        assert!(built.protocols.browser_credential.is_some());
        assert!(!format!("{:?}", built.headers).contains("browser-secret"));
        assert!(!format!("{:?}", built.headers).contains("managed-must-not-cross"));
    }

    #[test]
    fn websocket_header_and_browser_auth_domains_are_unambiguous() {
        let browser = (
            "sec-websocket-protocol",
            "realtime, openai-insecure-api-key.browser",
        );
        for authorization in ["Bearer caller", ""] {
            assert!(matches!(
                upstream_websocket_headers(
                    &headers(&[browser, ("authorization", authorization)]),
                    &profile("managed"),
                    &selected(ApiDialect::OfficialGa, CredentialPolicy::Managed),
                    None,
                ),
                Err(RelayError::InvalidRealtimeSubprotocol)
            ));
        }

        for (protocol, header_name) in [
            ("realtime, openai-organization.org_1", "openai-organization"),
            ("realtime, openai-project.proj_1", "openai-project"),
        ] {
            assert!(matches!(
                upstream_websocket_headers(
                    &headers(&[
                        ("sec-websocket-protocol", protocol),
                        (header_name, "header-value"),
                    ]),
                    &profile("managed"),
                    &selected(ApiDialect::OfficialGa, CredentialPolicy::Managed),
                    None,
                ),
                Err(RelayError::InvalidRealtimeSubprotocol)
            ));
        }
    }

    #[test]
    fn websocket_browser_protocols_are_official_only() {
        for dialect in [ApiDialect::QuicksilverV1, ApiDialect::Frameless] {
            for protocol in [
                "realtime",
                "realtime, openai-insecure-api-key.private-secret",
                "realtime, openai-organization.org_1",
                "realtime, openai-project.proj_1",
            ] {
                assert!(matches!(
                    upstream_websocket_headers(
                        &headers(&[("sec-websocket-protocol", protocol)]),
                        &profile("managed"),
                        &selected(dialect, CredentialPolicy::Managed),
                        None,
                    ),
                    Err(RelayError::InvalidRealtimeSubprotocol)
                ));
            }
        }
    }

    #[test]
    fn websocket_metadata_singletons_and_utf8_are_strict() {
        for name in [
            "origin",
            "openai-organization",
            "openai-project",
            "openai-safety-identifier",
        ] {
            let mut repeated = HeaderMap::new();
            repeated.append(name, HeaderValue::from_static("first"));
            repeated.append(name, HeaderValue::from_static("second"));
            assert!(matches!(
                upstream_websocket_headers(
                    &repeated,
                    &profile("managed"),
                    &selected(ApiDialect::OfficialGa, CredentialPolicy::Managed),
                    None,
                ),
                Err(RelayError::InvalidRealtimeHeader)
            ));

            let mut non_utf8 = HeaderMap::new();
            non_utf8.insert(name, HeaderValue::from_bytes(&[0xff]).unwrap());
            assert!(matches!(
                upstream_websocket_headers(
                    &non_utf8,
                    &profile("managed"),
                    &selected(ApiDialect::OfficialGa, CredentialPolicy::Managed),
                    None,
                ),
                Err(RelayError::InvalidRealtimeHeader)
            ));
        }

        let mut beta = HeaderMap::new();
        beta.append("openai-beta", HeaderValue::from_static("one"));
        beta.append("openai-beta", HeaderValue::from_static(""));
        beta.append("openai-beta", HeaderValue::from_static("two"));
        let built = upstream_websocket_headers(
            &beta,
            &profile("managed"),
            &selected(ApiDialect::OfficialGa, CredentialPolicy::Managed),
            None,
        )
        .unwrap();
        assert_eq!(
            built
                .headers
                .get_all("openai-beta")
                .iter()
                .map(|value| value.to_str().unwrap())
                .collect::<Vec<_>>(),
            ["one", "two"]
        );
    }

    #[test]
    fn websocket_managed_and_client_bearers_follow_the_selected_policy() {
        let managed = upstream_websocket_headers(
            &headers(&[("authorization", "Bearer ignored-caller")]),
            &profile("managed-secret"),
            &selected(ApiDialect::OfficialGa, CredentialPolicy::Managed),
            None,
        )
        .unwrap();
        assert_eq!(
            managed.headers.get(header::AUTHORIZATION).unwrap(),
            "Bearer managed-secret"
        );
        assert!(managed
            .headers
            .get(header::AUTHORIZATION)
            .unwrap()
            .is_sensitive());

        let client_profile = UpstreamProfile::ApiKeyClient {
            base_url: "https://api.openai.com/v1".into(),
        };
        let client = upstream_websocket_headers(
            &headers(&[("authorization", "Bearer caller-secret")]),
            &client_profile,
            &selected(ApiDialect::OfficialGa, CredentialPolicy::ClientBearer),
            None,
        )
        .unwrap();
        assert_eq!(
            client.headers.get(header::AUTHORIZATION).unwrap(),
            "Bearer caller-secret"
        );
        assert!(matches!(
            upstream_websocket_headers(
                &HeaderMap::new(),
                &client_profile,
                &selected(ApiDialect::OfficialGa, CredentialPolicy::ClientBearer),
                None,
            ),
            Err(RelayError::NoCredential)
        ));
    }

    #[test]
    fn websocket_response_map_excludes_handshake_owned_headers() {
        let upstream = headers(&[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-accept", "secret"),
            ("sec-websocket-protocol", "realtime"),
            ("content-type", "text/plain"),
            ("retry-after", "2"),
            ("x-request-id", "req_1"),
            ("openai-processing-ms", "7"),
            ("openai-version", "2026-07-01"),
            ("x-ratelimit-future", "9"),
        ]);
        assert_eq!(
            flattened(&websocket_response_headers(&upstream)),
            vec![
                ("openai-processing-ms".into(), "7".into(), false),
                ("openai-version".into(), "2026-07-01".into(), false),
                ("x-ratelimit-future".into(), "9".into(), false),
                ("x-request-id".into(), "req_1".into(), false),
            ]
        );
    }
}
