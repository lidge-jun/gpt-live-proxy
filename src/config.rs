//! Configuration model: bind address, upstream profile, credentials, and limits.
//!
//! Credentials arrive from the environment only. [`BearerToken`] redacts itself on
//! `Debug` so an accidental `{:?}` on [`Config`] cannot leak one (docs/002 D5).

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

/// Upstream call-create deadline (`LIVE_UPSTREAM_TIMEOUT_MS`).
pub const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(120);
/// Inbound call-create body cap (`LIVE_REQUEST_MAX_BYTES`).
pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Buffered upstream response cap (`LIVE_RESPONSE_MAX_BYTES`).
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

const DEFAULT_BIND: &str = "127.0.0.1:10110";
const CHATGPT_BACKEND_BASE: &str = "https://chatgpt.com/backend-api/codex";
const OPENAI_API_BASE: &str = "https://api.openai.com/v1";

/// A bearer credential that never renders itself.
#[derive(Clone, PartialEq, Eq)]
pub struct BearerToken(String);

impl BearerToken {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// The raw secret, crate-internal only: a `&str` renders without redaction, so
    /// this must never be reachable from outside the crate (docs/002 D5).
    #[allow(dead_code, reason = "consumed by the header merge in phase 020")]
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    /// An `Authorization` value carrying this token, already marked sensitive so
    /// header-map rendering redacts it. Fails only if the token contains bytes that
    /// are illegal in a header value.
    #[allow(dead_code, reason = "consumed by the header merge in phase 020")]
    pub(crate) fn authorization_header(
        &self,
    ) -> Result<http::HeaderValue, http::header::InvalidHeaderValue> {
        let mut value = http::HeaderValue::from_str(&format!("Bearer {}", self.0))?;
        value.set_sensitive(true);
        Ok(value)
    }

    /// Constant-time equality, for comparing an admission credential (docs/015).
    #[allow(dead_code, reason = "consumed by admission auth in phase 015")]
    pub(crate) fn ct_eq(&self, candidate: &str) -> bool {
        use subtle::ConstantTimeEq;
        let a = self.0.as_bytes();
        let b = candidate.as_bytes();
        // Length is not secret; equal-length comparison below is the constant-time part.
        a.len() == b.len() && a.ct_eq(b).into()
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Bearer <redacted>")
    }
}

impl fmt::Display for BearerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Which upstream the relay talks to, and therefore which body shape it must send.
#[derive(Debug, Clone)]
pub enum UpstreamProfile {
    /// ChatGPT `backend-api`: JSON call-create body; sideband still joins the API host.
    ChatGptBackend {
        base_url: String,
        auth: BearerToken,
        /// Upstream account-routing identifier. Not a bearer, but still an account
        /// identifier, so it is redacted in `Debug` alongside the token.
        account_id: Option<AccountId>,
    },
    /// OpenAI API key: multipart preserved verbatim; `/v1/realtime/calls` call-create.
    ApiKey { base_url: String, auth: BearerToken },
}

/// A ChatGPT account identifier that does not render itself.
#[derive(Clone, PartialEq, Eq)]
pub struct AccountId(String);

impl AccountId {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<account-id redacted>")
    }
}

impl UpstreamProfile {
    pub fn base_url(&self) -> &str {
        match self {
            Self::ChatGptBackend { base_url, .. } | Self::ApiKey { base_url, .. } => base_url,
        }
    }

    pub fn auth(&self) -> &BearerToken {
        match self {
            Self::ChatGptBackend { auth, .. } | Self::ApiKey { auth, .. } => auth,
        }
    }

    /// The redacting wrapper. Public callers cannot render the raw identifier.
    pub fn account_id(&self) -> Option<&AccountId> {
        match self {
            Self::ChatGptBackend { account_id, .. } => account_id.as_ref(),
            Self::ApiKey { .. } => None,
        }
    }

    /// The raw identifier, crate-internal only: needed by header construction in
    /// phase 020, and kept private for the same reason as [`BearerToken::expose`].
    #[allow(dead_code, reason = "consumed by the header merge in phase 020")]
    pub(crate) fn account_id_raw(&self) -> Option<&str> {
        match self {
            Self::ChatGptBackend { account_id, .. } => account_id.as_ref().map(AccountId::expose),
            Self::ApiKey { .. } => None,
        }
    }

    /// Body shape and call-create path both switch on this, exactly as upstream does:
    /// the decision is made from the base URL, not from the enum variant, so a custom
    /// base behaves identically (docs/000 §2.1).
    pub fn uses_backend_shape(&self) -> bool {
        self.base_url().contains("/backend-api")
    }

    /// True when the relay forwards multipart untouched instead of rewriting it.
    pub fn is_keyed(&self) -> bool {
        matches!(self, Self::ApiKey { .. })
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub upstream: UpstreamProfile,
    pub frame_log: Option<PathBuf>,
    pub upstream_timeout: Duration,
    pub max_body_bytes: usize,
    pub max_response_bytes: usize,
    /// Downstream admission credential; see docs/015. `None` plus a loopback bind
    /// means admission auth is disabled.
    pub admission_token: Option<BearerToken>,
    pub cors_allow_origins: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0} is required")]
    Missing(&'static str),
    #[error("{key} is invalid: {reason}")]
    Invalid { key: &'static str, reason: String },
}

impl Config {
    /// Build a config from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_source(|key| std::env::var(key).ok())
    }

    /// Environment-agnostic constructor so tests never mutate global state.
    pub fn from_source(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let bind_raw = get("GPT_LIVE_BIND").unwrap_or_else(|| DEFAULT_BIND.to_string());
        let bind: SocketAddr = bind_raw.parse().map_err(|e| ConfigError::Invalid {
            key: "GPT_LIVE_BIND",
            reason: format!("{e}"),
        })?;

        let token = get("GPT_LIVE_TOKEN")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or(ConfigError::Missing("GPT_LIVE_TOKEN"))?;
        let auth = BearerToken::new(token);

        let mode = get("GPT_LIVE_UPSTREAM_MODE").unwrap_or_else(|| "chatgpt".to_string());
        let upstream = match mode.as_str() {
            "chatgpt" => UpstreamProfile::ChatGptBackend {
                base_url: get("GPT_LIVE_BASE_URL")
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| CHATGPT_BACKEND_BASE.to_string()),
                auth,
                account_id: get("GPT_LIVE_ACCOUNT_ID")
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .map(AccountId::new),
            },
            "apikey" => UpstreamProfile::ApiKey {
                base_url: get("GPT_LIVE_BASE_URL")
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| OPENAI_API_BASE.to_string()),
                auth,
            },
            other => {
                return Err(ConfigError::Invalid {
                    key: "GPT_LIVE_UPSTREAM_MODE",
                    reason: format!("expected `chatgpt` or `apikey`, got `{other}`"),
                })
            }
        };

        // `OCX_LIVE_FRAME_LOG` stays accepted so an existing diagnostic workflow keeps
        // working (docs/002 D6). An empty primary value falls through to the alias
        // rather than disabling logging, so `GPT_LIVE_FRAME_LOG=` is not a silent
        // override of a configured alias.
        let frame_log = get("GPT_LIVE_FRAME_LOG")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .or_else(|| {
                get("OCX_LIVE_FRAME_LOG")
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
            })
            .map(PathBuf::from);

        let cors_allow_origins = get("GPT_LIVE_CORS_ORIGINS")
            .map(|raw| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self {
            bind,
            upstream,
            frame_log,
            upstream_timeout: UPSTREAM_TIMEOUT,
            max_body_bytes: MAX_BODY_BYTES,
            max_response_bytes: MAX_RESPONSE_BYTES,
            admission_token: get("GPT_LIVE_API_KEY")
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .map(BearerToken::new),
            cors_allow_origins,
        })
    }

    /// A loopback bind exempts callers from admission auth, matching the source
    /// behavior (docs/001 §11).
    pub fn requires_admission_auth(&self) -> bool {
        !is_loopback(&self.bind.ip())
    }
}

fn is_loopback(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn bearer_token_redacts_itself() {
        let token = BearerToken::new("sk-super-secret-value");
        assert_eq!(format!("{token:?}"), "Bearer <redacted>");
        assert_eq!(format!("{token}"), "<redacted>");
        assert!(!format!("{token:?}").contains("super-secret"));
    }

    #[test]
    fn config_debug_never_leaks_the_token() {
        let cfg = Config::from_source(source(&[("GPT_LIVE_TOKEN", "sk-leak-me-please")])).unwrap();
        let rendered = format!("{cfg:?}");
        assert!(
            !rendered.contains("leak-me"),
            "config Debug leaked: {rendered}"
        );
    }

    #[test]
    fn defaults_to_the_chatgpt_backend_profile() {
        let cfg = Config::from_source(source(&[("GPT_LIVE_TOKEN", "t")])).unwrap();
        assert_eq!(cfg.bind.to_string(), DEFAULT_BIND);
        assert_eq!(cfg.upstream.base_url(), CHATGPT_BACKEND_BASE);
        assert!(cfg.upstream.uses_backend_shape());
        assert!(!cfg.upstream.is_keyed());
    }

    #[test]
    fn apikey_mode_is_not_backend_shaped() {
        let cfg = Config::from_source(source(&[
            ("GPT_LIVE_TOKEN", "t"),
            ("GPT_LIVE_UPSTREAM_MODE", "apikey"),
        ]))
        .unwrap();
        assert_eq!(cfg.upstream.base_url(), OPENAI_API_BASE);
        assert!(!cfg.upstream.uses_backend_shape());
        assert!(cfg.upstream.is_keyed());
    }

    #[test]
    fn backend_shape_follows_the_url_not_the_variant() {
        // A ChatGPT profile pointed at a non-backend base must stop using the JSON shape.
        let profile = UpstreamProfile::ChatGptBackend {
            base_url: "https://example.test/v1".into(),
            auth: BearerToken::new("t"),
            account_id: None,
        };
        assert!(!profile.uses_backend_shape());
    }

    #[test]
    fn missing_token_is_an_error() {
        assert!(matches!(
            Config::from_source(source(&[])),
            Err(ConfigError::Missing("GPT_LIVE_TOKEN"))
        ));
    }

    #[test]
    fn unknown_upstream_mode_is_rejected() {
        let err = Config::from_source(source(&[
            ("GPT_LIVE_TOKEN", "t"),
            ("GPT_LIVE_UPSTREAM_MODE", "carrier-pigeon"),
        ]))
        .unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Invalid {
                key: "GPT_LIVE_UPSTREAM_MODE",
                ..
            }
        ));
    }

    #[test]
    fn frame_log_accepts_the_legacy_env_alias() {
        let cfg = Config::from_source(source(&[
            ("GPT_LIVE_TOKEN", "t"),
            ("OCX_LIVE_FRAME_LOG", "/tmp/frames.jsonl"),
        ]))
        .unwrap();
        assert_eq!(
            cfg.frame_log.unwrap().to_str().unwrap(),
            "/tmp/frames.jsonl"
        );
    }

    #[test]
    fn frame_log_primary_wins_over_the_alias() {
        let cfg = Config::from_source(source(&[
            ("GPT_LIVE_TOKEN", "t"),
            ("GPT_LIVE_FRAME_LOG", "/tmp/primary.jsonl"),
            ("OCX_LIVE_FRAME_LOG", "/tmp/alias.jsonl"),
        ]))
        .unwrap();
        assert_eq!(
            cfg.frame_log.unwrap().to_str().unwrap(),
            "/tmp/primary.jsonl"
        );
    }

    #[test]
    fn an_empty_primary_frame_log_falls_through_to_the_alias() {
        // An empty primary must not silently disable a configured alias.
        let cfg = Config::from_source(source(&[
            ("GPT_LIVE_TOKEN", "t"),
            ("GPT_LIVE_FRAME_LOG", "   "),
            ("OCX_LIVE_FRAME_LOG", "/tmp/alias.jsonl"),
        ]))
        .unwrap();
        assert_eq!(cfg.frame_log.unwrap().to_str().unwrap(), "/tmp/alias.jsonl");
    }

    #[test]
    fn an_unparsable_bind_is_rejected() {
        let err = Config::from_source(source(&[
            ("GPT_LIVE_TOKEN", "t"),
            ("GPT_LIVE_BIND", "not-an-address"),
        ]))
        .unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Invalid {
                key: "GPT_LIVE_BIND",
                ..
            }
        ));
    }

    #[test]
    fn ipv6_loopback_also_exempts_admission_auth() {
        let cfg = Config::from_source(source(&[
            ("GPT_LIVE_TOKEN", "t"),
            ("GPT_LIVE_BIND", "[::1]:10110"),
        ]))
        .unwrap();
        assert!(!cfg.requires_admission_auth());
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_from_secrets_and_urls() {
        let cfg = Config::from_source(source(&[
            ("GPT_LIVE_TOKEN", "  spaced-token  "),
            (
                "GPT_LIVE_BASE_URL",
                "  https://example.test/backend-api/x  ",
            ),
            ("GPT_LIVE_ACCOUNT_ID", "  acct-1  "),
        ]))
        .unwrap();
        assert_eq!(cfg.upstream.auth().expose(), "spaced-token");
        assert_eq!(
            cfg.upstream.base_url(),
            "https://example.test/backend-api/x"
        );
        assert_eq!(cfg.upstream.account_id_raw(), Some("acct-1"));
        // The public accessor hands back the redacting wrapper, not a raw &str.
        assert_eq!(
            format!("{:?}", cfg.upstream.account_id().unwrap()),
            "<account-id redacted>"
        );
    }

    #[test]
    fn a_keyed_profile_on_a_backend_url_uses_the_backend_shape() {
        let cfg = Config::from_source(source(&[
            ("GPT_LIVE_TOKEN", "t"),
            ("GPT_LIVE_UPSTREAM_MODE", "apikey"),
            ("GPT_LIVE_BASE_URL", "https://proxy.test/backend-api/codex"),
        ]))
        .unwrap();
        // Shape follows the URL, so a keyed profile can still be backend-shaped.
        assert!(cfg.upstream.uses_backend_shape());
        assert!(cfg.upstream.is_keyed());
    }

    #[test]
    fn config_debug_leaks_neither_token_nor_account_id() {
        let cfg = Config::from_source(source(&[
            ("GPT_LIVE_TOKEN", "upstream-secret-aaa"),
            ("GPT_LIVE_API_KEY", "admission-secret-bbb"),
            ("GPT_LIVE_ACCOUNT_ID", "account-ccc"),
        ]))
        .unwrap();
        let rendered = format!("{cfg:?}");
        for needle in ["upstream-secret-aaa", "admission-secret-bbb", "account-ccc"] {
            assert!(
                !rendered.contains(needle),
                "Config Debug leaked {needle}: {rendered}"
            );
        }
    }

    #[test]
    fn authorization_header_is_marked_sensitive() {
        let value = BearerToken::new("tok").authorization_header().unwrap();
        assert!(value.is_sensitive());
        assert_eq!(format!("{value:?}"), "Sensitive");
    }

    #[test]
    fn constant_time_comparison_matches_only_the_exact_secret() {
        let token = BearerToken::new("correct-horse");
        assert!(token.ct_eq("correct-horse"));
        assert!(!token.ct_eq("correct-hors"));
        assert!(!token.ct_eq("correct-horsee"));
        assert!(!token.ct_eq(""));
    }

    #[test]
    fn loopback_bind_disables_admission_auth() {
        let loop_cfg = Config::from_source(source(&[("GPT_LIVE_TOKEN", "t")])).unwrap();
        assert!(!loop_cfg.requires_admission_auth());

        let public = Config::from_source(source(&[
            ("GPT_LIVE_TOKEN", "t"),
            ("GPT_LIVE_BIND", "0.0.0.0:10110"),
        ]))
        .unwrap();
        assert!(public.requires_admission_auth());
    }

    #[test]
    fn cors_origins_are_split_and_trimmed() {
        let cfg = Config::from_source(source(&[
            ("GPT_LIVE_TOKEN", "t"),
            ("GPT_LIVE_CORS_ORIGINS", "https://a.test, https://b.test ,"),
        ]))
        .unwrap();
        assert_eq!(cfg.cors_allow_origins, ["https://a.test", "https://b.test"]);
    }
}
