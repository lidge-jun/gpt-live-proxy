//! Origin and host policy for the data plane.
//!
//! The two surfaces reject with different exact messages, so [`RequestKind`] is
//! carried into the error rather than inferred later (docs/001 §11).

use http::uri::{Authority, Scheme};
use http::{HeaderMap, Uri};

use crate::config::Config;
use crate::error::{RelayError, RequestKind};

/// Parse a `Host` header into a strict authority.
///
/// Hand-rolled splitting is exactly what lets `localhost:10110@evil.test` or
/// `[::1]evil` read as loopback, so parsing is delegated to
/// [`http::uri::Authority`], and userinfo, schemes and paths are refused outright.
fn parse_host(raw: &str) -> Option<Authority> {
    let raw = raw.trim();
    if raw.is_empty() || raw.contains('/') || raw.contains('@') {
        return None;
    }
    let authority: Authority = raw.parse().ok()?;
    // Round-trip check: anything the parser normalized away was malformed input.
    if authority.as_str() != raw {
        return None;
    }
    // `Authority::host()` stops at the closing bracket, so `[::1]evil` would
    // otherwise report host `[::1]` and read as loopback. Require that the
    // authority is exactly its host plus an optional `:port`.
    let host = authority.host();
    let remainder = raw.strip_prefix(host)?;
    match remainder {
        "" => {}
        rest => {
            let port = rest.strip_prefix(':')?;
            if port.parse::<u16>().is_err() {
                return None;
            }
        }
    }
    Some(authority)
}

/// True when an already-parsed authority names a loopback interface.
fn authority_is_loopback(authority: &Authority) -> bool {
    let host = authority.host();
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    // Bracketed IPv6 arrives with its brackets retained.
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(v4) = host.parse::<std::net::Ipv4Addr>() {
        return v4.is_loopback();
    }
    host.parse::<std::net::Ipv6Addr>()
        .is_ok_and(|v6| v6.is_loopback())
}

/// An `Origin` is a scheme plus an authority and nothing else. A non-HTTP scheme,
/// a path, or userinfo disqualifies it.
fn parse_origin(raw: &str) -> Option<(Scheme, Authority)> {
    let raw = raw.trim();
    let uri: Uri = raw.parse().ok()?;
    let scheme = uri.scheme()?;
    if scheme != &Scheme::HTTP && scheme != &Scheme::HTTPS {
        return None;
    }
    if !matches!(uri.path(), "" | "/") || uri.query().is_some() {
        return None;
    }
    let authority = uri.authority()?;
    if authority.as_str().contains('@') {
        return None;
    }
    Some((scheme.clone(), authority.clone()))
}

/// The default port for a scheme, so `https://host` and `host:443` compare equal.
fn default_port(scheme: &Scheme) -> u16 {
    if scheme == &Scheme::HTTPS {
        443
    } else {
        80
    }
}

/// Same-origin comparison between an `Origin` and the addressed `Host`.
///
/// An origin is scheme + host + port, but `Host` carries no scheme. When `Host`
/// states a port we can still compare exactly, filling in the origin's default
/// port from its scheme. When `Host` omits the port we genuinely do not know
/// which scheme the client used — behind TLS termination it could be either —
/// and matching on host name alone would let `https://h` pass for a request that
/// actually arrived over `http://h`. Missing information therefore denies the
/// same-origin shortcut; such a deployment lists its origin in
/// `GPT_LIVE_CORS_ORIGINS` explicitly.
fn same_origin(scheme: &Scheme, origin: &Authority, host: &Authority) -> bool {
    if !host.host().eq_ignore_ascii_case(origin.host()) {
        return false;
    }
    let Some(host_port) = host.port_u16() else {
        return false;
    };
    host_port == origin.port_u16().unwrap_or_else(|| default_port(scheme))
}

/// True when the origin is loopback, identical to the addressed host
/// (same-origin), or explicitly configured.
fn origin_allowed(origin: &str, host: Option<&Authority>, config: &Config) -> bool {
    let Some((scheme, authority)) = parse_origin(origin) else {
        return false;
    };
    if authority_is_loopback(&authority) {
        return true;
    }
    // Same-origin: a page served from the proxy's own authority must not have to
    // redundantly configure itself. Compared canonically, so `https://h` and
    // `h:443` match while `http://h` and `https://h` on distinct ports do not.
    if host.is_some_and(|h| same_origin(&scheme, &authority, h)) {
        return true;
    }
    config
        .cors_allow_origins
        .iter()
        .any(|allowed| allowed.trim().eq_ignore_ascii_case(origin.trim()))
}

/// Enforce the origin policy.
///
/// Without admission auth the proxy is only reachable locally, so the `Host`
/// header must itself be loopback on the configured port; with admission auth the
/// caller has already proven itself, so host validation does not apply.
pub fn check_origin(
    headers: &HeaderMap,
    config: &Config,
    kind: RequestKind,
) -> Result<(), RelayError> {
    let blocked = || Err(RelayError::OriginBlocked(kind));
    let origin = headers.get("origin").and_then(|v| v.to_str().ok());
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_host);

    if !config.requires_admission_auth() {
        match host.as_ref() {
            Some(authority) if authority_is_loopback(authority) => {
                // A loopback host on an unexpected port is a different service.
                if let Some(port) = authority.port_u16() {
                    if port != config.bind.port() {
                        return blocked();
                    }
                }
            }
            // Missing, malformed, or non-loopback: blocked.
            _ => return blocked(),
        }
    }

    match origin {
        // A same-origin or non-browser request carries no Origin at all.
        None => Ok(()),
        Some(origin) if origin_allowed(origin, host.as_ref(), config) => Ok(()),
        Some(_) => blocked(),
    }
}

/// The single allow decision, shared by enforcement and CORS echo so the two can
/// never diverge.
pub fn request_is_allowed(headers: &HeaderMap, config: &Config) -> bool {
    check_origin(headers, config, RequestKind::Http).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BearerToken;

    fn config_with(bind: &str, admission: Option<&str>, origins: &[&str]) -> Config {
        let bind = bind.to_string();
        let mut cfg = Config::from_source(|k| match k {
            "GPT_LIVE_TOKEN" => Some("upstream-token".to_string()),
            "GPT_LIVE_BIND" => Some(bind.clone()),
            _ => None,
        })
        .expect("config");
        cfg.admission_token = admission.map(BearerToken::new);
        cfg.cors_allow_origins = origins.iter().map(|s| (*s).to_string()).collect();
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

    fn loopback(host: &str) -> bool {
        parse_host(host).as_ref().is_some_and(authority_is_loopback)
    }

    #[test]
    fn loopback_host_forms_are_recognized() {
        for host in [
            "127.0.0.1:10110",
            "127.0.0.1",
            "localhost:10110",
            "LOCALHOST",
            "[::1]:10110",
            "127.0.0.2",
        ] {
            assert!(loopback(host), "{host} should be loopback");
        }
        for host in ["example.test", "10.0.0.5:10110", "[2001:db8::1]:10110"] {
            assert!(!loopback(host), "{host} should not be loopback");
        }
    }

    /// The parsing defects a hand-rolled splitter would accept.
    #[test]
    fn malformed_authorities_are_never_loopback() {
        for host in [
            "localhost:10110@evil.test",
            "127.0.0.1@evil.test",
            "[::1]evil",
            "http://127.0.0.1:10110",
            "127.0.0.1/../evil",
            "localhost.evil.test",
            "localhost:notaport",
            "127.0.0.1:99999",
            "",
            "   ",
        ] {
            assert!(!loopback(host), "{host} must not be treated as loopback");
        }
    }

    #[test]
    fn only_http_origins_with_no_path_are_parsed() {
        assert!(parse_origin("http://localhost:3000").is_some());
        assert!(parse_origin("https://app.test").is_some());
        assert!(parse_origin("https://app.test/").is_some());
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://app.test/path",
            "https://user@app.test",
            "not-a-uri",
            "app.test",
        ] {
            assert!(
                parse_origin(bad).is_none(),
                "{bad} must not parse as an origin"
            );
        }
    }

    #[test]
    fn a_same_origin_request_is_allowed_under_admission_auth() {
        // A browser served from the proxy's own authority must not need to be
        // listed as an allowed origin.
        let cfg = config_with("0.0.0.0:10110", Some("secret"), &[]);
        let map = headers(&[
            ("host", "relay.example.test:10110"),
            ("origin", "http://relay.example.test:10110"),
        ]);
        assert!(check_origin(&map, &cfg, RequestKind::Http).is_ok());
    }

    #[test]
    fn a_scheme_less_host_denies_the_same_origin_shortcut() {
        // `Host` carries no scheme, so a port-less Host cannot prove that
        // `https://h` is the origin the request actually arrived from. Such a
        // deployment configures its origin explicitly instead.
        let cfg = config_with("0.0.0.0:10110", Some("secret"), &[]);
        let map = headers(&[
            ("host", "relay.example.test"),
            ("origin", "https://relay.example.test"),
        ]);
        assert!(check_origin(&map, &cfg, RequestKind::Http).is_err());

        // ...and configuring it works.
        let cfg = config_with(
            "0.0.0.0:10110",
            Some("secret"),
            &["https://relay.example.test"],
        );
        assert!(check_origin(&map, &cfg, RequestKind::Http).is_ok());
    }

    #[test]
    fn a_different_origin_is_still_blocked_under_admission_auth() {
        let cfg = config_with("0.0.0.0:10110", Some("secret"), &[]);
        let map = headers(&[
            ("host", "relay.example.test"),
            ("origin", "https://evil.test"),
        ]);
        assert!(check_origin(&map, &cfg, RequestKind::Http).is_err());
    }

    #[test]
    fn same_origin_canonicalizes_the_default_port() {
        let cfg = config_with("0.0.0.0:10110", Some("secret"), &[]);
        // `Host: h:443` and `Origin: https://h` are the same origin.
        let map = headers(&[
            ("host", "relay.example.test:443"),
            ("origin", "https://relay.example.test"),
        ]);
        assert!(check_origin(&map, &cfg, RequestKind::Http).is_ok());

        // `Host: h:80` and `Origin: https://h` are not.
        let map = headers(&[
            ("host", "relay.example.test:80"),
            ("origin", "https://relay.example.test"),
        ]);
        assert!(check_origin(&map, &cfg, RequestKind::Http).is_err());
    }

    #[test]
    fn a_different_port_on_the_same_name_is_not_same_origin() {
        let cfg = config_with("0.0.0.0:10110", Some("secret"), &[]);
        let map = headers(&[
            ("host", "relay.example.test:8443"),
            ("origin", "https://relay.example.test:9999"),
        ]);
        assert!(check_origin(&map, &cfg, RequestKind::Http).is_err());
    }

    #[test]
    fn a_malformed_host_is_blocked_without_admission_auth() {
        let cfg = config_with("127.0.0.1:10110", None, &[]);
        for host in [
            "localhost:10110@evil.test",
            "[::1]evil",
            "localhost:notaport",
        ] {
            let map = headers(&[("host", host)]);
            assert!(
                check_origin(&map, &cfg, RequestKind::Http).is_err(),
                "{host} must be blocked"
            );
        }
    }

    #[test]
    fn missing_origin_is_accepted() {
        let cfg = config_with("127.0.0.1:10110", None, &[]);
        let map = headers(&[("host", "127.0.0.1:10110")]);
        assert!(check_origin(&map, &cfg, RequestKind::Http).is_ok());
    }

    #[test]
    fn loopback_origin_is_accepted() {
        let cfg = config_with("127.0.0.1:10110", None, &[]);
        let map = headers(&[
            ("host", "127.0.0.1:10110"),
            ("origin", "http://localhost:3000"),
        ]);
        assert!(check_origin(&map, &cfg, RequestKind::Http).is_ok());
    }

    #[test]
    fn a_foreign_origin_is_blocked_with_the_http_message() {
        let cfg = config_with("127.0.0.1:10110", None, &[]);
        let map = headers(&[("host", "127.0.0.1:10110"), ("origin", "https://evil.test")]);
        let err = check_origin(&map, &cfg, RequestKind::Http).unwrap_err();
        assert_eq!(err.message(), "cross-origin data-plane request blocked");
    }

    #[test]
    fn a_foreign_origin_on_an_upgrade_uses_the_websocket_message() {
        let cfg = config_with("127.0.0.1:10110", None, &[]);
        let map = headers(&[("host", "127.0.0.1:10110"), ("origin", "https://evil.test")]);
        let err = check_origin(&map, &cfg, RequestKind::WebSocketUpgrade).unwrap_err();
        assert_eq!(err.message(), "WebSocket upgrade blocked: non-local Origin");
    }

    #[test]
    fn a_configured_origin_is_accepted() {
        let cfg = config_with("127.0.0.1:10110", None, &["https://app.test"]);
        let map = headers(&[("host", "127.0.0.1:10110"), ("origin", "https://app.test")]);
        assert!(check_origin(&map, &cfg, RequestKind::Http).is_ok());
    }

    #[test]
    fn a_non_loopback_host_is_blocked_without_admission_auth() {
        let cfg = config_with("127.0.0.1:10110", None, &[]);
        let map = headers(&[("host", "attacker.test")]);
        assert!(check_origin(&map, &cfg, RequestKind::Http).is_err());
    }

    #[test]
    fn a_loopback_host_on_the_wrong_port_is_blocked() {
        let cfg = config_with("127.0.0.1:10110", None, &[]);
        let map = headers(&[("host", "127.0.0.1:9999")]);
        assert!(check_origin(&map, &cfg, RequestKind::Http).is_err());
    }

    #[test]
    fn host_validation_does_not_apply_once_admission_auth_is_required() {
        let cfg = config_with("0.0.0.0:10110", Some("secret"), &[]);
        // No Host at all, and admission already proved the caller.
        assert!(check_origin(&HeaderMap::new(), &cfg, RequestKind::Http).is_ok());
    }

    #[test]
    fn a_foreign_origin_is_still_blocked_under_admission_auth() {
        let cfg = config_with("0.0.0.0:10110", Some("secret"), &[]);
        let map = headers(&[("origin", "https://evil.test")]);
        assert!(check_origin(&map, &cfg, RequestKind::Http).is_err());
    }
}
