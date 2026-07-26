//! Configuration model: bind address, upstream profile, credentials, and limits.
//!
//! Credentials arrive from the environment only. [`BearerToken`] redacts itself on
//! `Debug` so an accidental `{:?}` on [`Config`] cannot leak one (docs/002 D5).

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

/// Upstream request deadline (`GPT_LIVE_UPSTREAM_TIMEOUT_MS`).
pub const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(120);
/// Inbound request body cap (`GPT_LIVE_REQUEST_MAX_BYTES`).
pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Buffered upstream response cap (`GPT_LIVE_RESPONSE_MAX_BYTES`).
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_WEBSOCKET_FRAME_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_ACTIVE_REQUESTS: usize = 128;
pub const MAX_ACTIVE_CONNECTIONS: usize = 128;
pub const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(30);
pub const WEBSOCKET_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
pub const WEBSOCKET_SEND_TIMEOUT: Duration = Duration::from_secs(15);

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

/// Whether the proxy or the caller supplies the upstream bearer credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamCredentialMode {
    Managed,
    Client,
}

/// Resource and timeout limits resolved once from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub request_bytes: usize,
    pub response_bytes: usize,
    pub websocket_frame_bytes: usize,
    pub active_requests: usize,
    pub active_connections: usize,
    pub request_read_timeout: Duration,
    pub upstream_timeout: Duration,
    pub websocket_connect_timeout: Duration,
    pub websocket_send_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            request_bytes: MAX_BODY_BYTES,
            response_bytes: MAX_RESPONSE_BYTES,
            websocket_frame_bytes: MAX_WEBSOCKET_FRAME_BYTES,
            active_requests: MAX_ACTIVE_REQUESTS,
            active_connections: MAX_ACTIVE_CONNECTIONS,
            request_read_timeout: REQUEST_READ_TIMEOUT,
            upstream_timeout: UPSTREAM_TIMEOUT,
            websocket_connect_timeout: WEBSOCKET_CONNECT_TIMEOUT,
            websocket_send_timeout: WEBSOCKET_SEND_TIMEOUT,
        }
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
    /// OpenAI API key managed by the proxy.
    ApiKeyManaged { base_url: String, auth: BearerToken },
    /// OpenAI API key supplied by each client request.
    ApiKeyClient { base_url: String },
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
            Self::ChatGptBackend { base_url, .. }
            | Self::ApiKeyManaged { base_url, .. }
            | Self::ApiKeyClient { base_url } => base_url,
        }
    }

    /// The configured upstream credential, if this profile is proxy-managed.
    pub fn managed_auth(&self) -> Option<&BearerToken> {
        match self {
            Self::ChatGptBackend { auth, .. } => Some(auth),
            Self::ApiKeyManaged { auth, .. } => Some(auth),
            Self::ApiKeyClient { .. } => None,
        }
    }

    /// The redacting wrapper. Public callers cannot render the raw identifier.
    pub fn account_id(&self) -> Option<&AccountId> {
        match self {
            Self::ChatGptBackend { account_id, .. } => account_id.as_ref(),
            Self::ApiKeyManaged { .. } | Self::ApiKeyClient { .. } => None,
        }
    }

    /// The raw identifier, crate-internal only: needed by header construction in
    /// phase 020, and kept private for the same reason as [`BearerToken::expose`].
    #[allow(dead_code, reason = "consumed by the header merge in phase 020")]
    pub(crate) fn account_id_raw(&self) -> Option<&str> {
        match self {
            Self::ChatGptBackend { account_id, .. } => account_id.as_ref().map(AccountId::expose),
            Self::ApiKeyManaged { .. } | Self::ApiKeyClient { .. } => None,
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
        matches!(self, Self::ApiKeyManaged { .. } | Self::ApiKeyClient { .. })
    }

    pub fn credential_mode(&self) -> UpstreamCredentialMode {
        match self {
            Self::ChatGptBackend { .. } | Self::ApiKeyManaged { .. } => {
                UpstreamCredentialMode::Managed
            }
            Self::ApiKeyClient { .. } => UpstreamCredentialMode::Client,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub upstream: UpstreamProfile,
    pub frame_log: Option<PathBuf>,
    pub limits: Limits,
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

/// Reject a base URL that cannot be extended by path concatenation.
///
/// The call-create builders append `/realtime/calls` and a query, so a base
/// carrying its own query or fragment would produce `.../base#frag/realtime/calls?...`
/// — a malformed URL that fails far from its cause.
fn validate_base_url(raw: &str) -> Result<String, ConfigError> {
    let invalid = |reason: &str| ConfigError::Invalid {
        key: "GPT_LIVE_BASE_URL",
        reason: reason.to_string(),
    };

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(invalid("must not be empty"));
    }

    // Parsed, not prefix-checked: `http://` and `http:///path` pass a scheme
    // test but have no host, and would fail confusingly at relay time.
    let parsed = reqwest::Url::parse(trimmed).map_err(|err| invalid(&err.to_string()))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(invalid("must use the http or https scheme"));
    }
    // A host is required. Note `http:///path` is NOT the empty-host case it
    // looks like: the parser reads `path` as the host, which is a legitimate —
    // if unhelpful — base. `http://` genuinely has no host and is rejected by
    // the parser itself.
    match parsed.host_str() {
        None => return Err(invalid("must include a host")),
        Some(host) if host.trim().is_empty() => {
            return Err(invalid("must include a host"));
        }
        Some(_) => {}
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(invalid("must not carry a query or fragment"));
    }
    // Userinfo would end up in a log line that only intends to record a host,
    // and a credential in a URL is a bad idea regardless.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(invalid("must not embed credentials"));
    }
    Ok(trimmed.to_string())
}

fn positive_usize(
    raw: Option<String>,
    key: &'static str,
    default: usize,
) -> Result<usize, ConfigError> {
    let Some(raw) = raw else {
        return Ok(default);
    };
    let value = raw.parse::<usize>().map_err(|err| ConfigError::Invalid {
        key,
        reason: format!("expected a positive integer: {err}"),
    })?;
    if value == 0 {
        return Err(ConfigError::Invalid {
            key,
            reason: "expected a positive integer".to_string(),
        });
    }
    Ok(value)
}

fn positive_semaphore_permits(
    raw: Option<String>,
    key: &'static str,
    default: usize,
) -> Result<usize, ConfigError> {
    let value = positive_usize(raw, key, default)?;
    if value > tokio::sync::Semaphore::MAX_PERMITS {
        return Err(ConfigError::Invalid {
            key,
            reason: format!("must not exceed {}", tokio::sync::Semaphore::MAX_PERMITS),
        });
    }
    Ok(value)
}

fn positive_millis(
    raw: Option<String>,
    key: &'static str,
    default: Duration,
) -> Result<Duration, ConfigError> {
    let Some(raw) = raw else {
        return Ok(default);
    };
    let value = raw.parse::<u64>().map_err(|err| ConfigError::Invalid {
        key,
        reason: format!("expected a positive integer: {err}"),
    })?;
    if value == 0 {
        return Err(ConfigError::Invalid {
            key,
            reason: "expected a positive integer".to_string(),
        });
    }
    Ok(Duration::from_millis(value))
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

        let credential_mode = match get("GPT_LIVE_CREDENTIAL_MODE").as_deref() {
            None | Some("managed") => UpstreamCredentialMode::Managed,
            Some("client") => UpstreamCredentialMode::Client,
            Some(other) => {
                return Err(ConfigError::Invalid {
                    key: "GPT_LIVE_CREDENTIAL_MODE",
                    reason: format!("expected `managed` or `client`, got `{other}`"),
                });
            }
        };

        let mode = get("GPT_LIVE_UPSTREAM_MODE").unwrap_or_else(|| "chatgpt".to_string());
        let managed_auth = || {
            get("GPT_LIVE_TOKEN")
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .map(BearerToken::new)
                .ok_or(ConfigError::Missing("GPT_LIVE_TOKEN"))
        };
        let upstream = match (mode.as_str(), credential_mode) {
            ("chatgpt", UpstreamCredentialMode::Managed) => UpstreamProfile::ChatGptBackend {
                // An explicitly set but empty value is a configuration mistake,
                // not a request for the default.
                base_url: match get("GPT_LIVE_BASE_URL") {
                    Some(raw) => validate_base_url(&raw)?,
                    None => CHATGPT_BACKEND_BASE.to_string(),
                },
                auth: managed_auth()?,
                account_id: get("GPT_LIVE_ACCOUNT_ID")
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .map(AccountId::new),
            },
            ("chatgpt", UpstreamCredentialMode::Client) => {
                return Err(ConfigError::Invalid {
                    key: "GPT_LIVE_CREDENTIAL_MODE",
                    reason: "client credentials require GPT_LIVE_UPSTREAM_MODE=apikey".to_string(),
                });
            }
            ("apikey", UpstreamCredentialMode::Managed) => UpstreamProfile::ApiKeyManaged {
                base_url: match get("GPT_LIVE_BASE_URL") {
                    Some(raw) => validate_base_url(&raw)?,
                    None => OPENAI_API_BASE.to_string(),
                },
                auth: managed_auth()?,
            },
            ("apikey", UpstreamCredentialMode::Client) => UpstreamProfile::ApiKeyClient {
                base_url: match get("GPT_LIVE_BASE_URL") {
                    Some(raw) => validate_base_url(&raw)?,
                    None => OPENAI_API_BASE.to_string(),
                },
            },
            (other, _) => {
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

        let limits = Limits {
            request_bytes: positive_usize(
                get("GPT_LIVE_REQUEST_MAX_BYTES"),
                "GPT_LIVE_REQUEST_MAX_BYTES",
                MAX_BODY_BYTES,
            )?,
            response_bytes: positive_usize(
                get("GPT_LIVE_RESPONSE_MAX_BYTES"),
                "GPT_LIVE_RESPONSE_MAX_BYTES",
                MAX_RESPONSE_BYTES,
            )?,
            websocket_frame_bytes: positive_usize(
                get("GPT_LIVE_WS_FRAME_MAX_BYTES"),
                "GPT_LIVE_WS_FRAME_MAX_BYTES",
                MAX_WEBSOCKET_FRAME_BYTES,
            )?,
            active_requests: positive_semaphore_permits(
                get("GPT_LIVE_MAX_REQUESTS"),
                "GPT_LIVE_MAX_REQUESTS",
                MAX_ACTIVE_REQUESTS,
            )?,
            active_connections: positive_semaphore_permits(
                get("GPT_LIVE_MAX_CONNECTIONS"),
                "GPT_LIVE_MAX_CONNECTIONS",
                MAX_ACTIVE_CONNECTIONS,
            )?,
            request_read_timeout: positive_millis(
                get("GPT_LIVE_REQUEST_READ_TIMEOUT_MS"),
                "GPT_LIVE_REQUEST_READ_TIMEOUT_MS",
                REQUEST_READ_TIMEOUT,
            )?,
            upstream_timeout: positive_millis(
                get("GPT_LIVE_UPSTREAM_TIMEOUT_MS"),
                "GPT_LIVE_UPSTREAM_TIMEOUT_MS",
                UPSTREAM_TIMEOUT,
            )?,
            websocket_connect_timeout: positive_millis(
                get("GPT_LIVE_WS_CONNECT_TIMEOUT_MS"),
                "GPT_LIVE_WS_CONNECT_TIMEOUT_MS",
                WEBSOCKET_CONNECT_TIMEOUT,
            )?,
            websocket_send_timeout: positive_millis(
                get("GPT_LIVE_WS_SEND_TIMEOUT_MS"),
                "GPT_LIVE_WS_SEND_TIMEOUT_MS",
                WEBSOCKET_SEND_TIMEOUT,
            )?,
        };

        Ok(Self {
            bind,
            upstream,
            frame_log,
            limits,
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
        assert_eq!(
            cfg.upstream.credential_mode(),
            UpstreamCredentialMode::Managed
        );
        assert!(cfg.upstream.managed_auth().is_some());
    }

    #[test]
    fn apikey_client_mode_starts_without_or_storing_a_managed_token() {
        let cfg = Config::from_source(source(&[
            ("GPT_LIVE_UPSTREAM_MODE", "apikey"),
            ("GPT_LIVE_CREDENTIAL_MODE", "client"),
            ("GPT_LIVE_TOKEN", "must-not-be-retained"),
        ]))
        .unwrap();

        assert_eq!(
            cfg.upstream.credential_mode(),
            UpstreamCredentialMode::Client
        );
        assert!(cfg.upstream.managed_auth().is_none());
        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains("must-not-be-retained"), "{rendered}");
    }

    #[test]
    fn apikey_client_mode_needs_no_token() {
        let cfg = Config::from_source(source(&[
            ("GPT_LIVE_UPSTREAM_MODE", "apikey"),
            ("GPT_LIVE_CREDENTIAL_MODE", "client"),
        ]))
        .unwrap();
        assert!(cfg.upstream.managed_auth().is_none());
    }

    #[test]
    fn managed_apikey_mode_still_requires_a_token() {
        assert!(matches!(
            Config::from_source(source(&[("GPT_LIVE_UPSTREAM_MODE", "apikey")])),
            Err(ConfigError::Missing("GPT_LIVE_TOKEN"))
        ));
    }

    #[test]
    fn chatgpt_client_mode_is_rejected_even_when_a_token_exists() {
        let err = Config::from_source(source(&[
            ("GPT_LIVE_CREDENTIAL_MODE", "client"),
            ("GPT_LIVE_TOKEN", "ignored"),
        ]))
        .unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Invalid {
                key: "GPT_LIVE_CREDENTIAL_MODE",
                ..
            }
        ));
    }

    #[test]
    fn unknown_credential_mode_is_rejected() {
        let err = Config::from_source(source(&[
            ("GPT_LIVE_TOKEN", "t"),
            ("GPT_LIVE_CREDENTIAL_MODE", "automatic"),
        ]))
        .unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Invalid {
                key: "GPT_LIVE_CREDENTIAL_MODE",
                ..
            }
        ));
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
        assert_eq!(
            cfg.upstream.managed_auth().unwrap().expose(),
            "spaced-token"
        );
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
    fn a_base_url_with_a_query_or_fragment_is_rejected() {
        // Path concatenation would otherwise produce .../base#frag/realtime/calls?...
        for bad in [
            "https://h.test/base?x=1",
            "https://h.test/base#frag",
            "ftp://h.test/base",
            "h.test/base",
            "http://",
            "",
            "   ",
            "https://user:secret@h.test/v1",
            "https://user@h.test/v1",
        ] {
            let err = Config::from_source(source(&[
                ("GPT_LIVE_TOKEN", "t"),
                ("GPT_LIVE_BASE_URL", bad),
            ]))
            .unwrap_err();
            assert!(
                matches!(
                    err,
                    ConfigError::Invalid {
                        key: "GPT_LIVE_BASE_URL",
                        ..
                    }
                ),
                "{bad} should be rejected"
            );
        }
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

    #[test]
    fn limits_have_the_exact_documented_defaults() {
        let cfg = Config::from_source(source(&[("GPT_LIVE_TOKEN", "t")])).unwrap();
        assert_eq!(
            cfg.limits,
            Limits {
                request_bytes: 16 * 1024 * 1024,
                response_bytes: 16 * 1024 * 1024,
                websocket_frame_bytes: 16 * 1024 * 1024,
                active_requests: 128,
                active_connections: 128,
                request_read_timeout: Duration::from_secs(30),
                upstream_timeout: Duration::from_secs(120),
                websocket_connect_timeout: Duration::from_secs(15),
                websocket_send_timeout: Duration::from_secs(15),
            }
        );
    }

    #[test]
    fn every_limit_uses_its_exact_environment_key() {
        let cfg = Config::from_source(source(&[
            ("GPT_LIVE_TOKEN", "t"),
            ("GPT_LIVE_REQUEST_MAX_BYTES", "101"),
            ("GPT_LIVE_RESPONSE_MAX_BYTES", "102"),
            ("GPT_LIVE_WS_FRAME_MAX_BYTES", "103"),
            ("GPT_LIVE_MAX_REQUESTS", "104"),
            ("GPT_LIVE_MAX_CONNECTIONS", "105"),
            ("GPT_LIVE_REQUEST_READ_TIMEOUT_MS", "106"),
            ("GPT_LIVE_UPSTREAM_TIMEOUT_MS", "107"),
            ("GPT_LIVE_WS_CONNECT_TIMEOUT_MS", "108"),
            ("GPT_LIVE_WS_SEND_TIMEOUT_MS", "109"),
        ]))
        .unwrap();

        assert_eq!(cfg.limits.request_bytes, 101);
        assert_eq!(cfg.limits.response_bytes, 102);
        assert_eq!(cfg.limits.websocket_frame_bytes, 103);
        assert_eq!(cfg.limits.active_requests, 104);
        assert_eq!(cfg.limits.active_connections, 105);
        assert_eq!(cfg.limits.request_read_timeout, Duration::from_millis(106));
        assert_eq!(cfg.limits.upstream_timeout, Duration::from_millis(107));
        assert_eq!(
            cfg.limits.websocket_connect_timeout,
            Duration::from_millis(108)
        );
        assert_eq!(
            cfg.limits.websocket_send_timeout,
            Duration::from_millis(109)
        );
    }

    #[test]
    fn every_limit_rejects_zero_malformed_and_overflow_with_its_exact_key() {
        let keys = [
            "GPT_LIVE_REQUEST_MAX_BYTES",
            "GPT_LIVE_RESPONSE_MAX_BYTES",
            "GPT_LIVE_WS_FRAME_MAX_BYTES",
            "GPT_LIVE_MAX_REQUESTS",
            "GPT_LIVE_MAX_CONNECTIONS",
            "GPT_LIVE_REQUEST_READ_TIMEOUT_MS",
            "GPT_LIVE_UPSTREAM_TIMEOUT_MS",
            "GPT_LIVE_WS_CONNECT_TIMEOUT_MS",
            "GPT_LIVE_WS_SEND_TIMEOUT_MS",
        ];
        for key in keys {
            for bad in [
                "0",
                "not-a-number",
                "999999999999999999999999999999999999999999999999999999999999",
            ] {
                let err = Config::from_source(|candidate| match candidate {
                    "GPT_LIVE_TOKEN" => Some("t".to_string()),
                    candidate if candidate == key => Some(bad.to_string()),
                    _ => None,
                })
                .unwrap_err();
                assert!(
                    matches!(err, ConfigError::Invalid { key: actual, .. } if actual == key),
                    "{key}={bad} returned {err}"
                );
            }
        }
    }

    #[test]
    fn semaphore_limits_reject_values_that_would_panic_at_startup() {
        for key in ["GPT_LIVE_MAX_REQUESTS", "GPT_LIVE_MAX_CONNECTIONS"] {
            let maximum = tokio::sync::Semaphore::MAX_PERMITS.to_string();
            let cfg = Config::from_source(|candidate| match candidate {
                "GPT_LIVE_TOKEN" => Some("t".to_string()),
                candidate if candidate == key => Some(maximum.clone()),
                _ => None,
            })
            .unwrap();
            let observed = if key == "GPT_LIVE_MAX_REQUESTS" {
                cfg.limits.active_requests
            } else {
                cfg.limits.active_connections
            };
            assert_eq!(observed, tokio::sync::Semaphore::MAX_PERMITS);

            for too_large in [
                (tokio::sync::Semaphore::MAX_PERMITS + 1).to_string(),
                usize::MAX.to_string(),
            ] {
                let err = Config::from_source(|candidate| match candidate {
                    "GPT_LIVE_TOKEN" => Some("t".to_string()),
                    candidate if candidate == key => Some(too_large.clone()),
                    _ => None,
                })
                .unwrap_err();
                assert!(
                    matches!(err, ConfigError::Invalid { key: actual, .. } if actual == key),
                    "{key}={too_large} returned {err}"
                );
            }
        }
    }
}
