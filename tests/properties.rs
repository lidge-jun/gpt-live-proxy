use axum::body::Body;
use bytes::Bytes;
use futures_util::stream;
use gpt_live_proxy::error::RelayError;
use gpt_live_proxy::live::body::parse_private_multipart;
use gpt_live_proxy::realtime::contract::{
    ApiDialect, CredentialPolicy, ProtocolSelection, SessionKind, Transport,
};
use gpt_live_proxy::realtime::headers::validate_upstream_headers;
use gpt_live_proxy::realtime::path::{parse_rest_path, validate_call_id, RestOperation};
use gpt_live_proxy::realtime::query::decode_ordered;
use gpt_live_proxy::relay::body::read_capped;
use gpt_live_proxy::relay::pump::{PumpOutcome, CLOSE_FRAME_TOO_LARGE};
use http::{HeaderMap, HeaderName, HeaderValue};
use proptest::prelude::*;
use proptest::test_runner::{Config as ProptestConfig, RngSeed, TestRunner};

const PROPERTY_CASES: u32 = 256;
const PROPERTY_SEED: u64 = 20_260_726;

fn runner() -> TestRunner {
    TestRunner::new(ProptestConfig {
        cases: PROPERTY_CASES,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(PROPERTY_SEED),
        ..ProptestConfig::default()
    })
}

fn percent_encode_all(value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .map(|byte| format!("%{byte:02X}"))
        .collect()
}

fn official_selection() -> ProtocolSelection {
    ProtocolSelection {
        dialect: ApiDialect::OfficialGa,
        transport: Transport::Http,
        session_kind: SessionKind::Realtime,
        credential: CredentialPolicy::Managed,
    }
}

#[test]
fn ordered_query_decoding_round_trips_utf8_duplicates_and_empty_values() {
    let component = prop::collection::vec(any::<char>(), 0..12)
        .prop_map(|chars| chars.into_iter().collect::<String>());
    let strategy = prop::collection::vec((component.clone(), component), 1..16);

    assert!(decode_ordered(None).unwrap().is_empty());

    runner()
        .run(&strategy, |pairs| {
            let raw = pairs
                .iter()
                .map(|(key, value)| {
                    format!("{}={}", percent_encode_all(key), percent_encode_all(value))
                })
                .collect::<Vec<_>>()
                .join("&");
            let decoded = decode_ordered(Some(&raw)).expect("encoded UTF-8 must decode");
            prop_assert_eq!(decoded, pairs);
            Ok(())
        })
        .unwrap();
}

#[test]
fn public_rest_call_ids_round_trip_only_the_documented_alphabet_and_bound() {
    let strategy = proptest::string::string_regex("[A-Za-z0-9_-]{1,128}").unwrap();

    runner()
        .run(&strategy, |call_id| {
            prop_assert!(validate_call_id(&call_id).is_ok());
            let path = format!("/v1/realtime/calls/{}/accept", percent_encode_all(&call_id));
            prop_assert_eq!(
                parse_rest_path(&path),
                Ok(RestOperation::AcceptCall { call_id })
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn singleton_header_names_are_case_insensitive_and_duplicates_fail_closed() {
    const SINGLETONS: [&str; 7] = [
        "content-type",
        "openai-organization",
        "openai-project",
        "openai-safety-identifier",
        "idempotency-key",
        "openai-alpha",
        "authorization",
    ];
    let strategy = (
        0usize..SINGLETONS.len(),
        prop::collection::vec(any::<bool>(), 1..40),
    );

    runner()
        .run(&strategy, |(index, uppercase)| {
            let canonical = SINGLETONS[index];
            let cased = canonical
                .bytes()
                .enumerate()
                .map(|(position, byte)| {
                    if uppercase[position % uppercase.len()] {
                        byte.to_ascii_uppercase()
                    } else {
                        byte
                    }
                })
                .collect::<Vec<_>>();
            let name = HeaderName::from_bytes(&cased).expect("ASCII header name");
            let mut headers = HeaderMap::new();
            headers.append(name.clone(), HeaderValue::from_static("one"));
            headers.append(name, HeaderValue::from_static("two"));

            let result = validate_upstream_headers(&headers, &official_selection());
            if canonical == "authorization" {
                prop_assert!(matches!(result, Err(RelayError::AmbiguousAuthorization)));
            } else {
                prop_assert!(matches!(result, Err(RelayError::InvalidRealtimeHeader)));
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn multipart_boundary_tokens_preserve_sdp_and_session_exactly() {
    let boundary = proptest::string::string_regex("[A-Za-z0-9_-]{1,48}").unwrap();
    let sdp = proptest::string::string_regex("[A-Za-z0-9 =:/_.-]{0,96}").unwrap();
    let voice = proptest::string::string_regex("[A-Za-z0-9_-]{0,32}").unwrap();
    let strategy = (boundary, sdp, voice);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runner()
        .run(&strategy, |(boundary, sdp, voice)| {
            let session = serde_json::json!({ "voice": voice }).to_string();
            let body = format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"sdp\"\r\nContent-Type: application/sdp\r\n\r\n{sdp}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"session\"\r\nContent-Type: application/json\r\n\r\n{session}\r\n--{boundary}--\r\n"
            );
            let content_type = format!("multipart/form-data; boundary={boundary}");
            let parsed = runtime
                .block_on(parse_private_multipart(Bytes::from(body), &content_type))
                .expect("generated multipart must parse");
            prop_assert_eq!(parsed.sdp, sdp);
            prop_assert_eq!(parsed.session, Some(serde_json::json!({ "voice": voice })));
            prop_assert_eq!(parsed.sdp_fields, 1);
            prop_assert_eq!(parsed.session_fields, 1);
            Ok(())
        })
        .unwrap();
}

#[test]
fn capped_body_arithmetic_accepts_exact_totals_and_rejects_cap_plus_one() {
    let strategy = (0usize..2_048, 0usize..2_048, 0usize..4_096);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runner()
        .run(&strategy, |(first, second, cap)| {
            let body = Body::from_stream(stream::iter([
                Ok::<_, std::io::Error>(Bytes::from(vec![b'a'; first])),
                Ok::<_, std::io::Error>(Bytes::from(vec![b'b'; second])),
            ]));
            let result = runtime.block_on(read_capped(body, cap));
            match first.checked_add(second) {
                Some(total) if total <= cap => {
                    let bytes = result.expect("at-cap body must be accepted");
                    prop_assert_eq!(bytes.len(), total);
                }
                _ => prop_assert!(matches!(result, Err(RelayError::BodyTooLarge))),
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn pump_outcome_labels_are_stable() {
    assert_eq!(PumpOutcome::ClientClosed.label(), "client_closed");
    assert_eq!(
        PumpOutcome::UpstreamClosed {
            code: 1000,
            reason: "done".into(),
        }
        .label(),
        "upstream_closed"
    );
    assert_eq!(
        PumpOutcome::Aborted {
            code: 1009,
            reason: CLOSE_FRAME_TOO_LARGE,
        }
        .label(),
        "aborted"
    );
}
