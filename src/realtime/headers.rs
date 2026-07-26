//! Exact allowlist-based header policy for public Realtime traffic.

use http::{header, HeaderMap, HeaderName, HeaderValue};

use crate::config::UpstreamProfile;
use crate::error::RelayError;
use crate::realtime::contract::{ApiDialect, CredentialPolicy, ProtocolSelection};

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
}
