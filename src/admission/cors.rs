//! CORS for the data plane.
//!
//! The six protocol header names must appear in `Access-Control-Allow-Headers`,
//! or a browser preflight strips them and the header-loss defect that
//! opencodex `75344b09` fixed returns through the front door.

use axum::response::Response;
use http::{header, HeaderMap, HeaderValue, StatusCode};

use crate::admission::origin::request_is_allowed;
use crate::config::Config;

pub const ALLOW_METHODS: &str = "GET, POST, PUT, PATCH, DELETE, OPTIONS";

pub const ALLOW_HEADERS: &str = "Content-Type, Authorization, X-GPT-Live-API-Key, X-Api-Key, \
ChatGPT-Account-Id, OpenAI-Alpha, X-Session-Id, Session-Id, Thread-Id, Originator, \
X-OAI-Attestation";

/// Add CORS headers to any response, including error responses.
pub fn apply_cors(response: &mut Response, request_headers: &HeaderMap, config: &Config) {
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static(ALLOW_METHODS),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static(ALLOW_HEADERS),
    );
    headers.insert(header::VARY, HeaderValue::from_static("Origin"));

    let echoed = request_headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        // Exactly the allow decision the boundary enforces. ORing in a separate
        // configured-origin check here would echo an origin whose request host
        // validation had already rejected.
        .filter(|_| request_is_allowed(request_headers, config))
        .and_then(|origin| HeaderValue::from_str(origin).ok())
        .unwrap_or_else(|| {
            HeaderValue::from_str(&format!("http://127.0.0.1:{}", config.bind.port()))
                .expect("a loopback origin is always a valid header value")
        });
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, echoed);
}

/// Preflight. Deliberately not authenticated: a browser cannot attach the
/// admission credential to an `OPTIONS`, so requiring it would break every
/// legitimate cross-origin caller.
pub fn preflight_status(request_headers: &HeaderMap, config: &Config) -> StatusCode {
    if request_is_allowed(request_headers, config) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::FORBIDDEN
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn config() -> Config {
        Config::from_source(|k| match k {
            "GPT_LIVE_TOKEN" => Some("t".to_string()),
            _ => None,
        })
        .expect("config")
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    /// The regression guard for opencodex `75344b09`: if a browser preflight
    /// strips these, the upstream falls back to v1 validation and 400s.
    #[test]
    fn all_six_protocol_headers_are_allowed() {
        // Tokenized, not substring-matched: `session-id` must be its own entry
        // rather than a fragment of `x-session-id`.
        let tokens: Vec<String> = ALLOW_HEADERS
            .split(',')
            .map(|t| t.trim().to_ascii_lowercase())
            .collect();
        for name in [
            "openai-alpha",
            "x-session-id",
            "session-id",
            "thread-id",
            "originator",
            "x-oai-attestation",
        ] {
            assert!(
                tokens.iter().any(|t| t == name),
                "{name} missing from Access-Control-Allow-Headers"
            );
        }
    }

    #[test]
    fn a_configured_origin_is_not_echoed_when_the_host_is_invalid() {
        let mut cfg = config();
        cfg.cors_allow_origins = vec!["https://app.test".to_string()];
        let mut res = Response::new(Body::empty());
        // Configured origin, but a Host the boundary rejects: echoing here would
        // contradict the enforcement decision.
        let req = headers(&[("host", "evil.test"), ("origin", "https://app.test")]);
        apply_cors(&mut res, &req, &cfg);
        assert_eq!(
            res.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "http://127.0.0.1:10110"
        );
    }

    #[test]
    fn the_account_header_remains_allowed() {
        assert!(ALLOW_HEADERS
            .to_ascii_lowercase()
            .contains("chatgpt-account-id"));
    }

    #[test]
    fn cors_headers_are_applied_to_any_response() {
        let cfg = config();
        let mut res = Response::new(Body::empty());
        let req = headers(&[("host", "127.0.0.1:10110")]);
        apply_cors(&mut res, &req, &cfg);

        let h = res.headers();
        assert_eq!(
            h.get(header::ACCESS_CONTROL_ALLOW_METHODS).unwrap(),
            ALLOW_METHODS
        );
        assert_eq!(h.get(header::VARY).unwrap(), "Origin");
        assert!(h.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_some());
    }

    #[test]
    fn an_allowed_origin_is_echoed() {
        let cfg = config();
        let mut res = Response::new(Body::empty());
        let req = headers(&[
            ("host", "127.0.0.1:10110"),
            ("origin", "http://localhost:3000"),
        ]);
        apply_cors(&mut res, &req, &cfg);
        assert_eq!(
            res.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "http://localhost:3000"
        );
    }

    #[test]
    fn a_rejected_origin_is_not_echoed() {
        let cfg = config();
        let mut res = Response::new(Body::empty());
        let req = headers(&[("host", "127.0.0.1:10110"), ("origin", "https://evil.test")]);
        apply_cors(&mut res, &req, &cfg);
        let echoed = res
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .unwrap();
        assert_ne!(echoed, "https://evil.test");
        assert_eq!(echoed, "http://127.0.0.1:10110");
    }

    #[test]
    fn preflight_allows_and_rejects_without_authentication() {
        let cfg = config();
        let allowed = headers(&[
            ("host", "127.0.0.1:10110"),
            ("origin", "http://localhost:3000"),
        ]);
        assert_eq!(preflight_status(&allowed, &cfg), StatusCode::NO_CONTENT);

        let rejected = headers(&[("host", "127.0.0.1:10110"), ("origin", "https://evil.test")]);
        assert_eq!(preflight_status(&rejected, &cfg), StatusCode::FORBIDDEN);
    }
}
