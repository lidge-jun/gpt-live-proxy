//! Call-create URL construction.
//!
//! The AVAS decision belongs to [`WireAdapter`]; this module only assembles the
//! path and appends what the adapter says applies (docs/020).

use crate::wire::{WireAdapter, AVAS_QUERY};

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

/// `{base minus a trailing /v1}/v1/realtime/calls` plus the AVAS query.
pub fn keyed_call_create_url(base: &str) -> String {
    let root = strip_v1_suffix(base);
    with_avas_query(&format!("{root}/v1/realtime/calls"))
}

/// The forwarding path.
///
/// A backend-shaped base posts to `{base}/realtime/calls`; a direct API base
/// uses the separate Frameless `/live` contract. Whether the AVAS query applies
/// is the adapter's decision, and an unknown adapter falls back to the profile
/// default — which is what the source does, since it never inspects
/// `openai-alpha`.
pub fn forward_call_create_url(
    base: &str,
    backend_shape: bool,
    adapter: Option<WireAdapter>,
) -> String {
    let root = base.trim_end_matches('/');
    if backend_shape {
        let url = format!("{root}/realtime/calls");
        let wants = adapter.is_none_or(|a| a.wants_avas_query(true));
        return if wants { with_avas_query(&url) } else { url };
    }
    let url = format!("{root}/live");
    if adapter.is_some_and(|a| a.wants_avas_query(false)) {
        with_avas_query(&url)
    } else {
        url
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
    fn the_keyed_url_matches_the_contract() {
        assert_eq!(
            keyed_call_create_url("https://api.openai.com/v1"),
            "https://api.openai.com/v1/realtime/calls?intent=quicksilver&architecture=avas"
        );
    }

    #[test]
    fn the_keyed_url_tolerates_base_variations() {
        for base in [
            "https://api.openai.com/v1",
            "https://api.openai.com/v1/",
            "https://api.openai.com",
            "https://api.openai.com/",
        ] {
            assert_eq!(
                keyed_call_create_url(base),
                "https://api.openai.com/v1/realtime/calls?intent=quicksilver&architecture=avas",
                "base {base}"
            );
        }
    }

    #[test]
    fn the_chatgpt_backend_url_matches_the_contract() {
        assert_eq!(
            forward_call_create_url("https://chatgpt.com/backend-api/codex", true, None),
            "https://chatgpt.com/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas"
        );
    }

    #[test]
    fn a_direct_frameless_base_posts_to_live_without_the_avas_query() {
        assert_eq!(
            forward_call_create_url(
                "https://api.openai.com/v1",
                false,
                Some(WireAdapter::FramelessBidi)
            ),
            "https://api.openai.com/v1/live"
        );
    }

    /// V1 carries the query on every base, including a non-backend one.
    #[test]
    fn v1_carries_the_avas_query_even_on_a_direct_base() {
        assert_eq!(
            forward_call_create_url("https://api.openai.com/v1", false, Some(WireAdapter::V1)),
            "https://api.openai.com/v1/live?intent=quicksilver&architecture=avas"
        );
    }

    /// The builder and the adapter must never disagree about the AVAS decision.
    #[test]
    fn the_builder_agrees_with_the_adapter_truth_table() {
        for adapter in [
            WireAdapter::V1,
            WireAdapter::FramelessBidi,
            WireAdapter::RealtimeV2,
        ] {
            for backend_shape in [true, false] {
                let url =
                    forward_call_create_url("https://host.test/base", backend_shape, Some(adapter));
                assert_eq!(
                    url.contains(AVAS_QUERY),
                    adapter.wants_avas_query(backend_shape),
                    "{adapter:?} backend_shape={backend_shape} produced {url}"
                );
            }
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
