//! `Location` header parsing.
//!
//! The call id is never in the response body; it is a path segment of the
//! `Location` header (docs/000 §2.6). The relay passes the header through
//! untouched, so this exists for tests and for any future client mode.

/// Extract the call id: drop the query, then scan path segments right to left
/// and take the first that looks like a call id.
pub fn parse_call_id(location: &str) -> Option<String> {
    let path = location.split('?').next().unwrap_or(location);
    path.rsplit('/')
        .find(|segment| is_call_id_segment(segment))
        .map(str::to_string)
}

/// Either `rtc_` with a non-empty suffix, or a 36-character 8-4-4-4-12 hex UUID.
fn is_call_id_segment(segment: &str) -> bool {
    if let Some(suffix) = segment.strip_prefix("rtc_") {
        return !suffix.is_empty();
    }
    is_hex_uuid(segment)
}

fn is_hex_uuid(segment: &str) -> bool {
    if segment.len() != 36 {
        return false;
    }
    for (index, byte) in segment.bytes().enumerate() {
        let expect_dash = matches!(index, 8 | 13 | 18 | 23);
        if expect_dash {
            if byte != b'-' {
                return false;
            }
        } else if !byte.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_observed_location_form_parses() {
        assert_eq!(
            parse_call_id("/v1/realtime/calls/rtc_u0_E52xSxAjvyO0yAcpamyDl").as_deref(),
            Some("rtc_u0_E52xSxAjvyO0yAcpamyDl")
        );
    }

    #[test]
    fn a_query_is_discarded_before_scanning() {
        assert_eq!(
            parse_call_id("/v1/realtime/calls/rtc_abc?foo=bar/rtc_decoy").as_deref(),
            Some("rtc_abc")
        );
    }

    #[test]
    fn a_uuid_segment_parses() {
        let uuid = "01234567-89ab-cdef-0123-456789abcdef";
        assert_eq!(
            parse_call_id(&format!("/v1/live/{uuid}")).as_deref(),
            Some(uuid)
        );
    }

    #[test]
    fn the_rightmost_valid_segment_wins() {
        assert_eq!(
            parse_call_id("/rtc_early/middle/rtc_late").as_deref(),
            Some("rtc_late")
        );
    }

    #[test]
    fn a_bare_rtc_prefix_is_not_a_call_id() {
        assert_eq!(parse_call_id("/v1/live/rtc_"), None);
    }

    #[test]
    fn a_malformed_uuid_is_rejected() {
        for candidate in [
            "01234567-89ab-cdef-0123-456789abcdeg", // non-hex
            "0123456789abcdef0123456789abcdef0123", // right length, no dashes
            "01234567-89ab-cdef-0123-456789abcde",  // too short
        ] {
            assert_eq!(
                parse_call_id(&format!("/x/{candidate}")),
                None,
                "{candidate}"
            );
        }
    }

    #[test]
    fn an_absent_call_id_yields_none() {
        assert_eq!(parse_call_id("/v1/realtime/calls"), None);
        assert_eq!(parse_call_id(""), None);
    }

    #[test]
    fn an_absolute_url_parses_too() {
        assert_eq!(
            parse_call_id("https://api.openai.com/v1/live/rtc_xyz").as_deref(),
            Some("rtc_xyz")
        );
    }
}
