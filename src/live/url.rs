//! Call-create URL construction.
//!
//! The classified private dialect owns the path/query matrix. Official GA URLs
//! are built by `realtime::http` and never enter this module.

use crate::realtime::contract::ApiDialect;
use crate::wire::AVAS_QUERY;

/// Append the AVAS pair unless the URL already carries **both** parameters.
///
/// Checking both is deliberate: a URL with only `intent=` is not already
/// configured, and treating it as configured would silently drop `architecture`.
pub fn with_avas_query(url: &str) -> String {
    let has_intent = url.contains("?intent=") || url.contains("&intent=");
    let has_architecture = url.contains("?architecture=") || url.contains("&architecture=");
    if has_intent && has_architecture {
        return url.to_string();
    }
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}{AVAS_QUERY}")
}

/// Build one centrally classified private call-create target.
///
/// Backend-shaped ChatGPT traffic uses its private `/realtime/calls` endpoint;
/// both source-proven private dialects carry AVAS there. Direct API-key traffic
/// keeps the dialect split: V1 uses `/v1/realtime/calls` + AVAS, while Frameless
/// uses `/v1/live` with no query.
pub fn private_call_create_url(base: &str, backend_shape: bool, dialect: ApiDialect) -> String {
    if backend_shape {
        debug_assert!(dialect != ApiDialect::OfficialGa);
        return with_avas_query(&format!("{}/realtime/calls", base.trim_end_matches('/')));
    }

    match dialect {
        ApiDialect::QuicksilverV1 => {
            let root = strip_v1_suffix(base);
            with_avas_query(&format!("{root}/v1/realtime/calls"))
        }
        ApiDialect::Frameless => {
            let root = strip_v1_suffix(base);
            format!("{root}/v1/live")
        }
        ApiDialect::OfficialGa => {
            unreachable!("official GA call-create URLs belong to realtime::http")
        }
    }
}

/// Remove one trailing `/v1` so appending `/v1/...` cannot double it.
fn strip_v1_suffix(base: &str) -> &str {
    let trimmed = base.trim_end_matches('/');
    trimmed.strip_suffix("/v1").unwrap_or(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_v1_uses_realtime_calls_with_avas() {
        assert_eq!(
            private_call_create_url(
                "https://api.openai.com/v1",
                false,
                ApiDialect::QuicksilverV1,
            ),
            "https://api.openai.com/v1/realtime/calls?intent=quicksilver&architecture=avas"
        );
    }

    #[test]
    fn api_key_v1_tolerates_base_variations() {
        for base in [
            "https://api.openai.com/v1",
            "https://api.openai.com/v1/",
            "https://api.openai.com",
            "https://api.openai.com/",
        ] {
            assert_eq!(
                private_call_create_url(base, false, ApiDialect::QuicksilverV1),
                "https://api.openai.com/v1/realtime/calls?intent=quicksilver&architecture=avas",
                "base {base}"
            );
        }
    }

    #[test]
    fn backend_private_dialects_share_the_source_proven_avas_target() {
        for dialect in [ApiDialect::QuicksilverV1, ApiDialect::Frameless] {
            assert_eq!(
                private_call_create_url(
                    "https://chatgpt.com/backend-api/codex",
                    true,
                    dialect,
                ),
                "https://chatgpt.com/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas"
            );
        }
    }

    #[test]
    fn api_key_frameless_uses_live_without_a_query() {
        for base in [
            "https://api.openai.com/v1",
            "https://api.openai.com/v1/",
            "https://api.openai.com",
            "https://api.openai.com/",
        ] {
            assert_eq!(
                private_call_create_url(base, false, ApiDialect::Frameless),
                "https://api.openai.com/v1/live",
                "base {base}"
            );
        }
    }

    #[test]
    fn the_avas_query_is_idempotent_only_when_both_params_exist() {
        let complete = "https://h.test/x?intent=quicksilver&architecture=avas";
        assert_eq!(with_avas_query(complete), complete);

        // Only one of the two present: the pair is still appended.
        let partial = "https://h.test/x?intent=quicksilver";
        assert_eq!(
            with_avas_query(partial),
            "https://h.test/x?intent=quicksilver&intent=quicksilver&architecture=avas"
        );
    }

    #[test]
    fn the_separator_follows_an_existing_query() {
        assert_eq!(
            with_avas_query("https://h.test/x?a=b"),
            "https://h.test/x?a=b&intent=quicksilver&architecture=avas"
        );
        assert_eq!(
            with_avas_query("https://h.test/x"),
            "https://h.test/x?intent=quicksilver&architecture=avas"
        );
    }
}
