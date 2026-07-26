//! Downstream admission: proving the *caller* may use this proxy.
//!
//! This is a different credential domain from the upstream bearer. Confusing the
//! two is the failure this module exists to prevent: an inbound
//! `Authorization: Bearer` may be an admission credential, an upstream credential,
//! or neither, and only the first is ever compared here — it is never forwarded.

use http::HeaderMap;

use crate::config::{Config, UpstreamCredentialMode};
use crate::error::RelayError;

/// Header names accepted as an admission credential, in precedence order.
pub const ADMISSION_HEADERS: [&str; 3] = ["x-gpt-live-api-key", "authorization", "x-api-key"];
pub const CLIENT_ADMISSION_HEADERS: [&str; 2] = ["x-gpt-live-api-key", "x-api-key"];

/// Strip a `Bearer ` prefix, case-insensitively, and trim what remains.
fn strip_bearer(raw: &str) -> &str {
    let trimmed = raw.trim();
    if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("bearer ") {
        trimmed[7..].trim()
    } else {
        trimmed
    }
}

/// Every non-empty value for one header name.
///
/// `get_all`, not `get`: a request may legally repeat a header, and inspecting
/// only the first value is how a duplicate-header bypass gets built.
fn values<'a>(headers: &'a HeaderMap, name: &str) -> impl Iterator<Item = &'a str> {
    let strip = name == "authorization";
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(move |v| if strip { strip_bearer(v) } else { v.trim() })
        .filter(|v| !v.is_empty())
}

/// The admission credential the caller actually presented.
///
/// Precedence is strict: the first header name in [`ADMISSION_HEADERS`] that has
/// any non-empty value *wins outright*. A wrong value in a higher-priority header
/// is a rejection, not an invitation to keep looking — otherwise a caller could
/// have one bad credential ignored because a lower-priority header happens to
/// hold a good one.
///
/// Within a single header name every value is considered, because a repeated
/// header is a smuggling vector rather than a precedence question.
fn presented<'a>(
    headers: &'a HeaderMap,
    names: &[&str],
) -> Result<Option<Vec<&'a str>>, RelayError> {
    for name in names {
        let strip = *name == "authorization";
        let mut found = Vec::new();
        for value in headers.get_all(*name).iter() {
            let value = value.to_str().map_err(|_| RelayError::AdmissionRequired)?;
            let value = if strip {
                strip_bearer(value)
            } else {
                value.trim()
            };
            if !value.is_empty() {
                found.push(value);
            }
        }
        if !found.is_empty() {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// Enforce admission. A loopback bind exempts callers entirely (docs/001 §11).
pub fn check_admission(headers: &HeaderMap, config: &Config) -> Result<(), RelayError> {
    reject_ambiguous_authorization(headers)?;

    if !config.requires_admission_auth() {
        return Ok(());
    }

    let Some(expected) = config.admission_token.as_ref() else {
        // Bound to a non-loopback address with no configured credential: fail closed
        // rather than serving the relay to the network unauthenticated.
        return Err(RelayError::AdmissionRequired);
    };

    let candidates = match config.upstream.credential_mode() {
        UpstreamCredentialMode::Managed => &ADMISSION_HEADERS[..],
        UpstreamCredentialMode::Client => &CLIENT_ADMISSION_HEADERS[..],
    };
    match presented(headers, candidates)? {
        Some(found) if found.iter().any(|s| expected.ct_eq(s)) => Ok(()),
        _ => Err(RelayError::AdmissionRequired),
    }
}

/// True when **any** `Authorization` value is the admission secret.
///
/// Such a value must never be relayed upstream: it authenticates the caller to
/// this proxy and means nothing to OpenAI, so forwarding it both fails and leaks
/// the proxy's own secret to a third party.
///
/// Checking every value matters: a caller could otherwise place a real upstream
/// bearer first and the admission secret second, passing a first-value-only check
/// while the relay still forwards the secret.
pub fn authorization_is_admission_secret(headers: &HeaderMap, config: &Config) -> bool {
    let Some(expected) = config.admission_token.as_ref() else {
        return false;
    };
    values(headers, "authorization").any(|supplied| expected.ct_eq(supplied))
}

/// A repeated `Authorization` is always ambiguous for a relay that must forward
/// exactly one credential, so it is refused outright rather than resolved by a
/// guess about which value the caller meant.
pub fn reject_ambiguous_authorization(headers: &HeaderMap) -> Result<(), RelayError> {
    if headers.get_all("authorization").iter().count() > 1 {
        return Err(RelayError::AmbiguousAuthorization);
    }
    Ok(())
}

/// Reject a request whose `Authorization` carries the admission secret.
pub fn reject_forwarded_admission_secret(
    headers: &HeaderMap,
    config: &Config,
) -> Result<(), RelayError> {
    if authorization_is_admission_secret(headers, config) {
        return Err(RelayError::AdmissionSecretNotForwardable);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BearerToken;

    fn config_with(bind: &str, admission: Option<&str>) -> Config {
        let bind = bind.to_string();
        let admission = admission.map(str::to_string);
        let mut cfg = Config::from_source(|k| match k {
            "GPT_LIVE_TOKEN" => Some("upstream-token".to_string()),
            "GPT_LIVE_BIND" => Some(bind.clone()),
            _ => None,
        })
        .expect("config");
        cfg.admission_token = admission.map(BearerToken::new);
        cfg
    }

    fn client_config_with(bind: &str, admission: Option<&str>) -> Config {
        let bind = bind.to_string();
        let admission = admission.map(str::to_string);
        let mut cfg = Config::from_source(|k| match k {
            "GPT_LIVE_UPSTREAM_MODE" => Some("apikey".to_string()),
            "GPT_LIVE_CREDENTIAL_MODE" => Some("client".to_string()),
            "GPT_LIVE_BIND" => Some(bind.clone()),
            _ => None,
        })
        .expect("config");
        cfg.admission_token = admission.map(BearerToken::new);
        cfg
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                http::HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn repeated_headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                http::HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn loopback_bind_skips_admission_entirely() {
        let cfg = config_with("127.0.0.1:10110", None);
        assert!(check_admission(&HeaderMap::new(), &cfg).is_ok());
    }

    #[test]
    fn duplicate_authorization_is_rejected_on_loopback_in_both_modes() {
        for cfg in [
            config_with("127.0.0.1:10110", None),
            client_config_with("127.0.0.1:10110", None),
        ] {
            let map = repeated_headers(&[
                ("authorization", "Bearer first"),
                ("authorization", "Bearer second"),
            ]);
            assert!(matches!(
                check_admission(&map, &cfg),
                Err(RelayError::AmbiguousAuthorization)
            ));
        }
    }

    #[test]
    fn non_loopback_without_a_configured_token_fails_closed() {
        let cfg = config_with("0.0.0.0:10110", None);
        assert!(matches!(
            check_admission(&headers(&[("x-gpt-live-api-key", "anything")]), &cfg),
            Err(RelayError::AdmissionRequired)
        ));
    }

    #[test]
    fn non_loopback_rejects_a_missing_credential() {
        let cfg = config_with("0.0.0.0:10110", Some("secret"));
        assert!(matches!(
            check_admission(&HeaderMap::new(), &cfg),
            Err(RelayError::AdmissionRequired)
        ));
    }

    #[test]
    fn every_accepted_header_name_works() {
        let cfg = config_with("0.0.0.0:10110", Some("secret"));
        for (name, value) in [
            ("x-gpt-live-api-key", "secret"),
            ("authorization", "Bearer secret"),
            ("x-api-key", "secret"),
        ] {
            assert!(
                check_admission(&headers(&[(name, value)]), &cfg).is_ok(),
                "{name} should be accepted"
            );
        }
    }

    #[test]
    fn bearer_prefix_is_matched_case_insensitively() {
        let cfg = config_with("0.0.0.0:10110", Some("secret"));
        for value in ["Bearer secret", "bearer secret", "BEARER   secret  "] {
            assert!(check_admission(&headers(&[("authorization", value)]), &cfg).is_ok());
        }
    }

    #[test]
    fn a_wrong_credential_is_rejected() {
        let cfg = config_with("0.0.0.0:10110", Some("secret"));
        assert!(matches!(
            check_admission(&headers(&[("x-gpt-live-api-key", "wrong")]), &cfg),
            Err(RelayError::AdmissionRequired)
        ));
    }

    #[test]
    fn an_empty_candidate_falls_through_to_the_next_header() {
        let cfg = config_with("0.0.0.0:10110", Some("secret"));
        // An empty dedicated header must not shadow a valid x-api-key.
        let map = headers(&[("x-gpt-live-api-key", ""), ("x-api-key", "secret")]);
        assert!(check_admission(&map, &cfg).is_ok());
    }

    #[test]
    fn precedence_prefers_the_dedicated_header() {
        let cfg = config_with("0.0.0.0:10110", Some("secret"));
        // The dedicated header wins, so a non-admission upstream bearer in
        // `authorization` does not break admission.
        let map = headers(&[
            ("x-gpt-live-api-key", "secret"),
            ("authorization", "Bearer some-upstream-chatgpt-token"),
        ]);
        assert!(check_admission(&map, &cfg).is_ok());
    }

    #[test]
    fn a_wrong_high_priority_credential_is_not_rescued_by_a_lower_one() {
        // Precedence is strict: once the dedicated header is present it decides.
        let cfg = config_with("0.0.0.0:10110", Some("secret"));
        let map = headers(&[("x-gpt-live-api-key", "wrong"), ("x-api-key", "secret")]);
        assert!(matches!(
            check_admission(&map, &cfg),
            Err(RelayError::AdmissionRequired)
        ));
    }

    #[test]
    fn a_wrong_authorization_is_not_rescued_by_x_api_key() {
        let cfg = config_with("0.0.0.0:10110", Some("secret"));
        let map = headers(&[("authorization", "Bearer wrong"), ("x-api-key", "secret")]);
        assert!(matches!(
            check_admission(&map, &cfg),
            Err(RelayError::AdmissionRequired)
        ));
    }

    #[test]
    fn client_mode_reserves_authorization_for_upstream_credentials() {
        let cfg = client_config_with("0.0.0.0:10110", Some("admission-secret"));
        let split = headers(&[
            ("x-api-key", "admission-secret"),
            ("authorization", "Bearer upstream-secret"),
        ]);
        assert!(check_admission(&split, &cfg).is_ok());
        assert!(reject_forwarded_admission_secret(&split, &cfg).is_ok());

        let authorization_only = headers(&[("authorization", "Bearer admission-secret")]);
        assert!(matches!(
            check_admission(&authorization_only, &cfg),
            Err(RelayError::AdmissionRequired)
        ));
        assert!(matches!(
            reject_forwarded_admission_secret(&authorization_only, &cfg),
            Err(RelayError::AdmissionSecretNotForwardable)
        ));
    }

    #[test]
    fn client_mode_dedicated_header_precedence_is_strict() {
        let cfg = client_config_with("0.0.0.0:10110", Some("secret"));
        let map = headers(&[("x-gpt-live-api-key", "wrong"), ("x-api-key", "secret")]);
        assert!(matches!(
            check_admission(&map, &cfg),
            Err(RelayError::AdmissionRequired)
        ));
    }

    #[test]
    fn non_utf8_higher_priority_admission_is_a_decisive_rejection() {
        for cfg in [
            config_with("0.0.0.0:10110", Some("secret")),
            client_config_with("0.0.0.0:10110", Some("secret")),
        ] {
            let mut map = headers(&[("x-api-key", "secret")]);
            map.insert(
                http::HeaderName::from_static("x-gpt-live-api-key"),
                http::HeaderValue::from_bytes(&[0xff]).unwrap(),
            );
            assert!(matches!(
                check_admission(&map, &cfg),
                Err(RelayError::AdmissionRequired)
            ));
        }
    }

    #[test]
    fn repeated_dedicated_values_retain_any_match_semantics_in_both_modes() {
        for cfg in [
            config_with("0.0.0.0:10110", Some("secret")),
            client_config_with("0.0.0.0:10110", Some("secret")),
        ] {
            let map = repeated_headers(&[
                ("x-gpt-live-api-key", "wrong"),
                ("x-gpt-live-api-key", "secret"),
            ]);
            assert!(check_admission(&map, &cfg).is_ok());
        }
    }

    #[test]
    fn client_network_mode_without_a_configured_admission_secret_fails_closed() {
        let cfg = client_config_with("0.0.0.0:10110", None);
        assert!(matches!(
            check_admission(&headers(&[("x-gpt-live-api-key", "anything")]), &cfg),
            Err(RelayError::AdmissionRequired)
        ));
    }

    #[test]
    fn an_admission_bearer_is_never_forwardable() {
        let cfg = config_with("0.0.0.0:10110", Some("secret"));
        let map = headers(&[("authorization", "Bearer secret")]);
        assert!(authorization_is_admission_secret(&map, &cfg));
        assert!(matches!(
            reject_forwarded_admission_secret(&map, &cfg),
            Err(RelayError::AdmissionSecretNotForwardable)
        ));
    }

    #[test]
    fn an_upstream_bearer_is_forwardable() {
        let cfg = config_with("0.0.0.0:10110", Some("secret"));
        let map = headers(&[("authorization", "Bearer a-real-upstream-token")]);
        assert!(!authorization_is_admission_secret(&map, &cfg));
        assert!(reject_forwarded_admission_secret(&map, &cfg).is_ok());
    }

    #[test]
    fn the_split_credential_pattern_is_accepted() {
        // The documented way to satisfy both domains at once: proxy credential in
        // the dedicated header, upstream bearer in `authorization`.
        let cfg = config_with("0.0.0.0:10110", Some("secret"));
        let map = headers(&[
            ("x-gpt-live-api-key", "secret"),
            ("authorization", "Bearer upstream-token"),
        ]);
        assert!(check_admission(&map, &cfg).is_ok());
        assert!(reject_forwarded_admission_secret(&map, &cfg).is_ok());
    }

    #[test]
    fn admission_check_is_independent_of_the_forwarding_check() {
        // A caller who puts ONLY the admission secret in `authorization` passes
        // admission but must still be refused, because the relay would otherwise
        // forward the proxy's own secret upstream.
        let cfg = config_with("0.0.0.0:10110", Some("secret"));
        let map = headers(&[("authorization", "Bearer secret")]);
        assert!(check_admission(&map, &cfg).is_ok());
        assert!(reject_forwarded_admission_secret(&map, &cfg).is_err());
    }

    #[test]
    fn loopback_never_treats_a_bearer_as_an_admission_secret_when_none_is_set() {
        let cfg = config_with("127.0.0.1:10110", None);
        let map = headers(&[("authorization", "Bearer anything")]);
        assert!(!authorization_is_admission_secret(&map, &cfg));
    }
}
