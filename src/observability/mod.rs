//! Tracing setup and the credential-safe header renderer.

pub mod frame_log;

use http::HeaderMap;

pub use frame_log::{Direction, FrameLogger, FrameRecord};

/// Header names whose values are never rendered, whether or not the value was
/// marked sensitive at construction.
///
/// `x-oai-attestation` is here because it is client-supplied but copied
/// upstream: it is bearer-grade material, not an ordinary protocol header.
const CREDENTIAL_HEADERS: [&str; 5] = [
    "authorization",
    "chatgpt-account-id",
    "x-api-key",
    "x-gpt-live-api-key",
    "x-oai-attestation",
];

/// The only sanctioned way to render headers.
///
/// A `HeaderMap`'s own `Debug` hides values marked sensitive, but relying on
/// that alone is fragile: a value constructed anywhere without `set_sensitive`
/// would print in full. This redacts by name as well, so both mechanisms have
/// to fail before a credential leaks.
pub fn redacted_headers(headers: &HeaderMap) -> String {
    let mut rendered: Vec<String> = headers
        .iter()
        .map(|(name, value)| {
            let name_str = name.as_str();
            let is_credential = value.is_sensitive()
                || CREDENTIAL_HEADERS
                    .iter()
                    .any(|candidate| candidate.eq_ignore_ascii_case(name_str));
            if is_credential {
                format!("{name_str}: <redacted>")
            } else {
                match value.to_str() {
                    Ok(value) => format!("{name_str}: {value}"),
                    Err(_) => format!("{name_str}: <non-utf8>"),
                }
            }
        })
        .collect();
    rendered.sort();
    format!("{{{}}}", rendered.join(", "))
}

/// Initialize tracing from `GPT_LIVE_LOG`, defaulting to `info`.
pub fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_env("GPT_LIVE_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn credentials_are_redacted_by_name() {
        let map = headers(&[
            ("authorization", "Bearer super-secret"),
            ("chatgpt-account-id", "acct-secret"),
            ("x-api-key", "key-secret"),
            ("x-gpt-live-api-key", "admission-secret"),
            ("x-oai-attestation", "att-secret"),
            ("openai-alpha", "quicksilver=v2"),
        ]);
        let rendered = redacted_headers(&map);

        for secret in [
            "super-secret",
            "acct-secret",
            "key-secret",
            "admission-secret",
            "att-secret",
        ] {
            assert!(!rendered.contains(secret), "{secret} leaked: {rendered}");
        }
        // Non-credential values remain visible, which is the point of the log.
        assert!(rendered.contains("quicksilver=v2"));
    }

    #[test]
    fn redaction_is_case_insensitive() {
        let map = headers(&[("Authorization", "Bearer secret")]);
        assert!(!redacted_headers(&map).contains("secret"));
    }

    /// Belt and braces: a value marked sensitive is redacted even under a name
    /// the list does not know about.
    #[test]
    fn a_sensitive_value_is_redacted_regardless_of_its_name() {
        let mut map = HeaderMap::new();
        let mut value = HeaderValue::from_static("unexpected-secret");
        value.set_sensitive(true);
        map.insert(HeaderName::from_static("x-something-custom"), value);
        assert!(!redacted_headers(&map).contains("unexpected-secret"));
    }

    #[test]
    fn a_non_utf8_value_is_labelled_not_printed() {
        let mut map = HeaderMap::new();
        map.insert(
            HeaderName::from_static("x-weird"),
            HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
        );
        assert!(redacted_headers(&map).contains("<non-utf8>"));
    }

    #[test]
    fn the_rendering_is_stable() {
        let map = headers(&[("b-header", "2"), ("a-header", "1")]);
        // Sorted, so a log line does not churn between runs.
        assert_eq!(redacted_headers(&map), "{a-header: 1, b-header: 2}");
    }
}
