//! Upstream header construction.
//!
//! The map is **built**, never cloned from the inbound request. That is the
//! whole point: cloning would carry the admission credential, cookies, and any
//! header a caller invented straight to OpenAI. Only the six negotiated protocol
//! headers cross the boundary, and the proxy owns authentication outright.
//!
//! Losing these six is what made a Frameless session validate as v1 and 400
//! (opencodex `75344b09`).

use http::{HeaderMap, HeaderName, HeaderValue};

use crate::config::UpstreamProfile;
use crate::error::RelayError;
use crate::wire::WireAdapter;

/// The only client headers forwarded upstream.
pub const CLIENT_PROTOCOL_HEADERS: [&str; 6] = [
    "openai-alpha",
    "x-session-id",
    "session-id",
    "thread-id",
    "originator",
    "x-oai-attestation",
];

/// Headers the proxy owns and a client may never influence.
const PROXY_OWNED: [&str; 2] = ["authorization", "chatgpt-account-id"];

/// Copy the whitelist, skipping empty values.
///
/// A repeated protocol header takes its first value: unlike `authorization`,
/// these are not credentials, and refusing the request over a duplicate would
/// be stricter than the contract.
pub fn client_protocol_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for name in CLIENT_PROTOCOL_HEADERS {
        let Some(value) = headers.get(name) else {
            continue;
        };
        if value.to_str().is_ok_and(|v| v.trim().is_empty()) {
            continue;
        }
        if let Ok(header_name) = HeaderName::from_bytes(name.as_bytes()) {
            let mut value = value.clone();
            // The attestation is client-supplied but bearer-grade: it is copied
            // upstream, so it must not surface in a header-map render.
            if name == "x-oai-attestation" {
                value.set_sensitive(true);
            }
            out.insert(header_name, value);
        }
    }
    out
}

/// Build the complete upstream header map.
///
/// Order is whitelist, then proxy-owned authentication, so authentication always
/// wins on conflict. `adapter` is threaded through only to assert agreement: an
/// absent `openai-alpha` stays absent, because inventing one would let the proxy
/// negotiate a protocol the client never asked for.
pub fn merge_upstream_headers(
    client: &HeaderMap,
    profile: &UpstreamProfile,
    adapter: Option<WireAdapter>,
) -> Result<HeaderMap, RelayError> {
    let mut out = client_protocol_headers(client);

    debug_assert!(
        match (
            out.get("openai-alpha").and_then(|v| v.to_str().ok()),
            adapter
        ) {
            (Some(value), Some(adapter)) => Some(value) == adapter.openai_alpha(),
            // No header forwarded means the relay must not have invented one.
            (None, _) => true,
            (Some(_), None) => true,
        },
        "openai-alpha disagrees with the resolved adapter"
    );

    // Proxy-owned authentication, applied last and marked sensitive so a
    // header-map render cannot echo it.
    //
    // A conversion failure is NOT skipped: silently omitting the credential
    // would send an unauthenticated request upstream, which fails confusingly
    // far from its cause. A configured value with control characters is a
    // configuration fault and is reported as one.
    let authorization = profile
        .auth()
        .authorization_header()
        .map_err(|_| RelayError::NoCredential)?;
    out.insert(http::header::AUTHORIZATION, authorization);

    if let Some(account) = profile.account_id_raw() {
        let mut value = HeaderValue::from_str(account).map_err(|_| RelayError::NoCredential)?;
        value.set_sensitive(true);
        out.insert(HeaderName::from_static("chatgpt-account-id"), value);
    }
    Ok(out)
}

/// True when a header name is proxy-owned. Used by tests and diagnostics.
pub fn is_proxy_owned(name: &str) -> bool {
    PROXY_OWNED
        .iter()
        .any(|owned| owned.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AccountId, BearerToken};

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

    fn backend_profile() -> UpstreamProfile {
        UpstreamProfile::ChatGptBackend {
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            auth: BearerToken::new("upstream-token"),
            account_id: Some(AccountId::new("acct-42")),
        }
    }

    fn keyed_profile() -> UpstreamProfile {
        UpstreamProfile::ApiKey {
            base_url: "https://api.openai.com/v1".into(),
            auth: BearerToken::new("sk-test-key"),
        }
    }

    #[test]
    fn all_six_protocol_headers_are_forwarded() {
        let client = headers(&[
            ("openai-alpha", "quicksilver=v2"),
            ("x-session-id", "sess-1"),
            ("session-id", "conv-1"),
            ("thread-id", "thread-1"),
            ("originator", "codex_cli_rs"),
            ("x-oai-attestation", "att-1"),
        ]);
        let out = merge_upstream_headers(&client, &backend_profile(), None).unwrap();
        for name in CLIENT_PROTOCOL_HEADERS {
            assert!(out.contains_key(name), "{name} was dropped");
        }
    }

    #[test]
    fn empty_protocol_values_are_dropped() {
        let client = headers(&[("openai-alpha", ""), ("thread-id", "   ")]);
        let out = merge_upstream_headers(&client, &backend_profile(), None).unwrap();
        assert!(!out.contains_key("openai-alpha"));
        assert!(!out.contains_key("thread-id"));
    }

    #[test]
    fn an_absent_protocol_header_is_never_invented() {
        let client = HeaderMap::new();
        // The adapter is supplied independently of the request, since
        // negotiation parsing can only yield Some when the header is present.
        let out = merge_upstream_headers(
            &client,
            &backend_profile(),
            Some(WireAdapter::FramelessBidi),
        )
        .unwrap();
        assert!(
            !out.contains_key("openai-alpha"),
            "absent must stay absent: inventing it negotiates a protocol the client never asked for"
        );
    }

    #[test]
    fn proxy_authentication_beats_a_client_bearer() {
        let client = headers(&[
            ("authorization", "Bearer caller-supplied"),
            ("chatgpt-account-id", "caller-account"),
        ]);
        let out = merge_upstream_headers(&client, &backend_profile(), None).unwrap();
        assert_eq!(
            out.get("authorization").unwrap().to_str().unwrap(),
            "Bearer upstream-token"
        );
        assert_eq!(
            out.get("chatgpt-account-id").unwrap().to_str().unwrap(),
            "acct-42"
        );
    }

    #[test]
    fn credential_headers_are_marked_sensitive() {
        let out = merge_upstream_headers(&HeaderMap::new(), &backend_profile(), None).unwrap();
        assert!(out.get("authorization").unwrap().is_sensitive());
        assert!(out.get("chatgpt-account-id").unwrap().is_sensitive());
        // And a Debug render of the whole map shows neither secret.
        let rendered = format!("{out:?}");
        assert!(!rendered.contains("upstream-token"), "{rendered}");
        assert!(!rendered.contains("acct-42"), "{rendered}");
    }

    #[test]
    fn the_keyed_profile_sends_no_account_header() {
        let out = merge_upstream_headers(&HeaderMap::new(), &keyed_profile(), None).unwrap();
        assert_eq!(
            out.get("authorization").unwrap().to_str().unwrap(),
            "Bearer sk-test-key"
        );
        assert!(!out.contains_key("chatgpt-account-id"));
    }

    /// The map is built, not cloned: nothing outside the whitelist survives.
    #[test]
    fn nothing_outside_the_whitelist_is_forwarded() {
        let client = headers(&[
            ("x-openai-fedramp", "true"),
            ("x-gpt-live-api-key", "admission-secret"),
            ("cookie", "session=abc"),
            ("user-agent", "curl/8"),
            ("x-forwarded-for", "10.0.0.1"),
            ("x-custom-anything", "leak me"),
            ("openai-alpha", "quicksilver=v2"),
        ]);
        let out = merge_upstream_headers(&client, &backend_profile(), None).unwrap();

        for name in [
            "x-openai-fedramp",
            "x-gpt-live-api-key",
            "cookie",
            "user-agent",
            "x-forwarded-for",
            "x-custom-anything",
        ] {
            assert!(!out.contains_key(name), "{name} leaked upstream");
        }
        assert_eq!(out.len(), 3, "whitelist entry plus the two owned headers");
    }

    /// The admission credential authenticates the caller to THIS proxy and is
    /// meaningless upstream; forwarding it would also leak the proxy's secret.
    #[test]
    fn the_admission_header_never_reaches_the_upstream() {
        let client = headers(&[("x-gpt-live-api-key", "admission-secret")]);
        let out = merge_upstream_headers(&client, &keyed_profile(), None).unwrap();
        let rendered = format!("{out:?}");
        assert!(!rendered.contains("admission-secret"));
        assert!(!out.contains_key("x-gpt-live-api-key"));
    }

    /// The attestation is client-supplied but copied upstream, so it is treated
    /// as bearer-grade rather than as an ordinary protocol header.
    #[test]
    fn the_attestation_is_marked_sensitive() {
        let client = headers(&[("x-oai-attestation", "att-secret-value")]);
        let out = merge_upstream_headers(&client, &keyed_profile(), None).unwrap();
        assert!(out.get("x-oai-attestation").unwrap().is_sensitive());
        let rendered = format!("{out:?}");
        assert!(!rendered.contains("att-secret-value"), "{rendered}");
    }

    /// Silently dropping a malformed credential would send an unauthenticated
    /// request upstream, failing far from its cause.
    #[test]
    fn a_malformed_configured_credential_is_an_error_not_a_silent_omission() {
        let profile = UpstreamProfile::ApiKey {
            base_url: "https://api.openai.com/v1".into(),
            auth: BearerToken::new("bad\nvalue"),
        };
        assert!(merge_upstream_headers(&HeaderMap::new(), &profile, None).is_err());

        let profile = UpstreamProfile::ChatGptBackend {
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            auth: BearerToken::new("fine"),
            account_id: Some(AccountId::new("bad\nvalue")),
        };
        assert!(merge_upstream_headers(&HeaderMap::new(), &profile, None).is_err());
    }

    #[test]
    fn proxy_owned_names_are_recognized_case_insensitively() {
        assert!(is_proxy_owned("Authorization"));
        assert!(is_proxy_owned("ChatGPT-Account-Id"));
        assert!(!is_proxy_owned("openai-alpha"));
    }
}
