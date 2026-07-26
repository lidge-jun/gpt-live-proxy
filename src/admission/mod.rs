//! The downstream trust boundary: nothing reaches a relay handler without passing
//! through here (docs/015).
//!
//! Order within a request is fixed: draining, then admission, then origin. CORS
//! wraps every response, including the rejections.

pub mod auth;
pub mod cors;
pub mod drain;
pub mod origin;

use http::HeaderMap;

use crate::config::Config;
use crate::error::{RelayError, RequestKind};

pub use drain::DrainState;

/// Run the whole boundary for one request.
pub fn guard(
    headers: &HeaderMap,
    config: &Config,
    drain: &DrainState,
    kind: RequestKind,
) -> Result<(), RelayError> {
    if drain.is_draining() {
        return Err(RelayError::Draining);
    }
    auth::check_admission(headers, config)?;
    auth::reject_forwarded_admission_secret(headers, config)?;
    origin::check_origin(headers, config, kind)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BearerToken;

    fn config_with(bind: &str, admission: Option<&str>) -> Config {
        let bind = bind.to_string();
        let mut cfg = Config::from_source(|k| match k {
            "GPT_LIVE_TOKEN" => Some("upstream".to_string()),
            "GPT_LIVE_BIND" => Some(bind.clone()),
            _ => None,
        })
        .expect("config");
        cfg.admission_token = admission.map(BearerToken::new);
        cfg
    }

    fn client_config_with(bind: &str, admission: Option<&str>) -> Config {
        let bind = bind.to_string();
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

    #[test]
    fn draining_precedes_every_other_check() {
        let cfg = config_with("0.0.0.0:10110", Some("secret"));
        let drain = DrainState::new();
        drain.begin();
        // No credential at all, but draining answers first.
        assert!(matches!(
            guard(&HeaderMap::new(), &cfg, &drain, RequestKind::Http),
            Err(RelayError::Draining)
        ));
    }

    #[test]
    fn admission_precedes_origin() {
        let cfg = config_with("0.0.0.0:10110", Some("secret"));
        let drain = DrainState::new();
        // Bad credential AND a foreign origin: the credential failure is reported.
        let map = headers(&[("x-api-key", "wrong"), ("origin", "https://evil.test")]);
        assert!(matches!(
            guard(&map, &cfg, &drain, RequestKind::Http),
            Err(RelayError::AdmissionRequired)
        ));
    }

    #[test]
    fn a_forwardable_admission_secret_is_caught_before_origin() {
        let cfg = config_with("0.0.0.0:10110", Some("secret"));
        let drain = DrainState::new();
        let map = headers(&[("authorization", "Bearer secret")]);
        assert!(matches!(
            guard(&map, &cfg, &drain, RequestKind::Http),
            Err(RelayError::AdmissionSecretNotForwardable)
        ));
    }

    #[test]
    fn duplicate_authorization_cannot_smuggle_the_admission_secret() {
        let cfg = config_with("0.0.0.0:10110", Some("secret"));
        let drain = DrainState::new();
        // Valid dedicated credential, then two Authorization values with the
        // admission secret hidden in the second one.
        let mut map = headers(&[("x-gpt-live-api-key", "secret")]);
        map.append(
            http::HeaderName::from_static("authorization"),
            http::HeaderValue::from_static("Bearer upstream-token"),
        );
        map.append(
            http::HeaderName::from_static("authorization"),
            http::HeaderValue::from_static("Bearer secret"),
        );

        // Both defenses hold independently: the duplicate is refused outright,
        // and the secret is detected in a non-first value.
        assert!(matches!(
            guard(&map, &cfg, &drain, RequestKind::Http),
            Err(RelayError::AmbiguousAuthorization)
        ));
        assert!(auth::authorization_is_admission_secret(&map, &cfg));
    }

    #[test]
    fn a_clean_loopback_request_passes() {
        let cfg = config_with("127.0.0.1:10110", None);
        let drain = DrainState::new();
        let map = headers(&[("host", "127.0.0.1:10110")]);
        assert!(guard(&map, &cfg, &drain, RequestKind::Http).is_ok());
    }

    #[test]
    fn client_network_mode_requires_a_dedicated_admission_credential() {
        let cfg = client_config_with("0.0.0.0:10110", Some("admission"));
        let drain = DrainState::new();

        let split = headers(&[
            ("x-api-key", "admission"),
            ("authorization", "Bearer upstream"),
        ]);
        assert!(guard(&split, &cfg, &drain, RequestKind::Http).is_ok());

        let authorization_only = headers(&[("authorization", "Bearer admission")]);
        assert!(matches!(
            guard(&authorization_only, &cfg, &drain, RequestKind::Http),
            Err(RelayError::AdmissionRequired)
        ));
    }

    #[test]
    fn loopback_still_rejects_the_admission_secret_as_upstream_auth_in_both_modes() {
        for cfg in [
            config_with("127.0.0.1:10110", Some("admission")),
            client_config_with("127.0.0.1:10110", Some("admission")),
        ] {
            let drain = DrainState::new();
            let map = headers(&[
                ("host", "127.0.0.1:10110"),
                ("authorization", "Bearer admission"),
            ]);
            assert!(matches!(
                guard(&map, &cfg, &drain, RequestKind::Http),
                Err(RelayError::AdmissionSecretNotForwardable)
            ));
        }
    }

    #[test]
    fn the_upgrade_surface_reports_its_own_origin_message() {
        let cfg = config_with("127.0.0.1:10110", None);
        let drain = DrainState::new();
        let map = headers(&[("host", "127.0.0.1:10110"), ("origin", "https://evil.test")]);
        let err = guard(&map, &cfg, &drain, RequestKind::WebSocketUpgrade).unwrap_err();
        assert_eq!(err.message(), "WebSocket upgrade blocked: non-local Origin");
    }
}
