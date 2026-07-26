//! Conversion between the downstream and upstream WebSocket message types.
//!
//! `axum::extract::ws::Message` and `tungstenite::protocol::Message` are
//! distinct enums even at a matched dependency version, and only the latter has
//! a `Frame` variant. Conversion is therefore explicit and total rather than a
//! transmute of convenience.

use axum::extract::ws::{CloseFrame as AxumClose, Message as AxumMessage};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame as TungClose;
use tokio_tungstenite::tungstenite::Message as TungMessage;

/// Default close code when a peer closes without one.
pub const DEFAULT_CLOSE_CODE: u16 = 1000;

/// Downstream to upstream.
///
/// Ping and pong return `None`: each leg answers its own keepalive, so
/// forwarding them would double the traffic and desynchronize the two sockets.
pub fn axum_to_tungstenite(message: AxumMessage) -> Option<TungMessage> {
    Some(match message {
        AxumMessage::Text(text) => TungMessage::Text(text.as_str().into()),
        AxumMessage::Binary(bytes) => TungMessage::Binary(bytes),
        AxumMessage::Close(frame) => TungMessage::Close(frame.map(|frame| TungClose {
            code: CloseCode::from(frame.code),
            reason: frame.reason.as_str().into(),
        })),
        AxumMessage::Ping(_) | AxumMessage::Pong(_) => return None,
    })
}

/// Upstream to downstream.
///
/// `Frame` is a raw-frame variant a read never produces; it is mapped to `None`
/// rather than guessed at.
pub fn tungstenite_to_axum(message: TungMessage) -> Option<AxumMessage> {
    Some(match message {
        TungMessage::Text(text) => AxumMessage::Text(text.as_str().into()),
        TungMessage::Binary(bytes) => AxumMessage::Binary(bytes),
        TungMessage::Close(frame) => AxumMessage::Close(frame.map(|frame| AxumClose {
            code: u16::from(frame.code),
            reason: frame.reason.as_str().into(),
        })),
        TungMessage::Ping(_) | TungMessage::Pong(_) | TungMessage::Frame(_) => return None,
    })
}

/// The code and reason to send downstream when the upstream closes.
///
/// A missing code becomes `1000` and a missing reason becomes empty, matching
/// the source behavior exactly.
pub fn close_parts(frame: Option<&TungClose>) -> (u16, String) {
    match frame {
        Some(frame) => {
            let code = u16::from(frame.code);
            let code = if code == 0 { DEFAULT_CLOSE_CODE } else { code };
            (code, frame.reason.to_string())
        }
        None => (DEFAULT_CLOSE_CODE, String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn text_survives_in_both_directions() {
        let original = "가볍게 얘기해봐요";
        let up = axum_to_tungstenite(AxumMessage::Text(original.into())).unwrap();
        assert!(matches!(&up, TungMessage::Text(t) if t.as_str() == original));

        let down = tungstenite_to_axum(up).unwrap();
        assert!(matches!(&down, AxumMessage::Text(t) if t.as_str() == original));
    }

    #[test]
    fn binary_stays_binary_at_the_variant_level() {
        // Byte equality alone would not catch a Binary-to-Text regression.
        let payload = Bytes::from_static(&[0x00, 0xff, 0x10]);
        let up = axum_to_tungstenite(AxumMessage::Binary(payload.clone())).unwrap();
        assert!(matches!(&up, TungMessage::Binary(b) if b == &payload));

        let down = tungstenite_to_axum(up).unwrap();
        assert!(matches!(&down, AxumMessage::Binary(b) if b == &payload));
    }

    #[test]
    fn a_close_frame_round_trips_its_code_and_reason() {
        let up = axum_to_tungstenite(AxumMessage::Close(Some(AxumClose {
            code: 1009,
            reason: "too many pending frames".into(),
        })))
        .unwrap();
        let TungMessage::Close(Some(frame)) = &up else {
            panic!("expected a close frame");
        };
        assert_eq!(u16::from(frame.code), 1009);
        assert_eq!(frame.reason.as_str(), "too many pending frames");

        let down = tungstenite_to_axum(up).unwrap();
        let AxumMessage::Close(Some(frame)) = down else {
            panic!("expected a close frame");
        };
        assert_eq!(frame.code, 1009);
        assert_eq!(frame.reason.as_str(), "too many pending frames");
    }

    #[test]
    fn an_unknown_close_code_survives_conversion() {
        let up = axum_to_tungstenite(AxumMessage::Close(Some(AxumClose {
            code: 4321,
            reason: "custom".into(),
        })))
        .unwrap();
        let down = tungstenite_to_axum(up).unwrap();
        let AxumMessage::Close(Some(frame)) = down else {
            panic!("expected a close frame");
        };
        assert_eq!(frame.code, 4321);
    }

    #[test]
    fn a_closeless_close_uses_the_documented_defaults() {
        let (code, reason) = close_parts(None);
        assert_eq!(code, DEFAULT_CLOSE_CODE);
        assert_eq!(reason, "");
    }

    #[test]
    fn close_parts_preserves_a_real_frame() {
        let frame = TungClose {
            code: CloseCode::from(1011u16),
            reason: "upstream error".into(),
        };
        assert_eq!(
            close_parts(Some(&frame)),
            (1011, "upstream error".to_string())
        );
    }

    #[test]
    fn keepalives_are_not_forwarded() {
        assert!(axum_to_tungstenite(AxumMessage::Ping(Bytes::new())).is_none());
        assert!(axum_to_tungstenite(AxumMessage::Pong(Bytes::new())).is_none());
        assert!(tungstenite_to_axum(TungMessage::Ping(Bytes::new())).is_none());
        assert!(tungstenite_to_axum(TungMessage::Pong(Bytes::new())).is_none());
    }
}
