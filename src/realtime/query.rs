//! Strict, ordered decoding for official Realtime WebSocket queries.

use percent_encoding::percent_decode;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum QueryDecodeError {
    #[error("malformed percent escape in Realtime query")]
    MalformedPercentEscape,
    #[error("Realtime query is not valid UTF-8")]
    InvalidUtf8,
}

/// Decode an application/x-www-form-urlencoded query exactly once while
/// retaining pair order, duplicates, empty keys, and empty values.
pub fn decode_ordered(raw: Option<&str>) -> Result<Vec<(String, String)>, QueryDecodeError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };

    raw.split('&')
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            Ok((decode_component(key)?, decode_component(value)?))
        })
        .collect()
}

fn decode_component(raw: &str) -> Result<String, QueryDecodeError> {
    let bytes = raw.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return Err(QueryDecodeError::MalformedPercentEscape);
            }
            index += 3;
        } else {
            index += 1;
        }
    }

    let plus_as_space: Vec<u8> = bytes
        .iter()
        .map(|byte| if *byte == b'+' { b' ' } else { *byte })
        .collect();
    percent_decode(&plus_as_space)
        .decode_utf8()
        .map(String::from)
        .map_err(|_| QueryDecodeError::InvalidUtf8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_order_duplicates_and_empty_components() {
        assert_eq!(
            decode_ordered(Some("model=a&x=&model=b&=value&bare&")).unwrap(),
            [
                ("model".into(), "a".into()),
                ("x".into(), "".into()),
                ("model".into(), "b".into()),
                ("".into(), "value".into()),
                ("bare".into(), "".into()),
                ("".into(), "".into()),
            ]
        );
        assert!(decode_ordered(None).unwrap().is_empty());
    }

    #[test]
    fn decodes_plus_and_percent_exactly_once_as_utf8() {
        assert_eq!(
            decode_ordered(Some("mo%64el=gpt%2Brealtime+2&sentinel=%ED%95%9C%EA%B8%80")).unwrap(),
            [
                ("model".into(), "gpt+realtime 2".into()),
                ("sentinel".into(), "한글".into()),
            ]
        );
        assert_eq!(
            decode_ordered(Some("model=%252B")).unwrap(),
            [("model".into(), "%2B".into())]
        );
    }

    #[test]
    fn rejects_every_malformed_escape_and_decoded_non_utf8() {
        for raw in ["model=%", "model=%0", "model=%GG", "%G0=x", "model=x%2"] {
            assert_eq!(
                decode_ordered(Some(raw)),
                Err(QueryDecodeError::MalformedPercentEscape),
                "raw={raw}"
            );
        }
        assert_eq!(
            decode_ordered(Some("model=%FF")),
            Err(QueryDecodeError::InvalidUtf8)
        );
    }
}
