//! Generic capped request-body reading.

use bytes::Bytes;
use futures_util::StreamExt;

use crate::error::RelayError;

/// Read a body incrementally, aborting as soon as the cap is exceeded.
///
/// Incremental rather than "read then measure": a 10 GiB upload must not be
/// buffered before being rejected.
pub async fn read_capped(body: axum::body::Body, max_bytes: usize) -> Result<Bytes, RelayError> {
    let mut stream = body.into_data_stream();
    // A single growing buffer, not a vector of frames. Retaining each frame
    // separately would let a client send the cap as one-byte frames and pay for
    // it in `Bytes` metadata instead of payload — under the cap, over the memory.
    let mut buffer = bytes::BytesMut::new();

    while let Some(next) = stream.next().await {
        let chunk = next.map_err(classify_body_error)?;
        if chunk.is_empty() {
            continue;
        }
        if buffer.len().saturating_add(chunk.len()) > max_bytes {
            return Err(RelayError::BodyTooLarge);
        }
        buffer.extend_from_slice(&chunk);
    }

    Ok(buffer.freeze())
}

/// A client that vanished mid-body is a cancellation, not a malformed request.
///
/// Distinguishing them matters: `400` blames the caller for a body it never
/// finished sending, while `499` records that the caller left.
///
/// The decision walks the error's source chain looking for a real transport
/// error rather than matching on message text, which is locale- and
/// version-dependent.
fn classify_body_error(err: axum::Error) -> RelayError {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&err);
    while let Some(current) = source {
        if let Some(io) = current.downcast_ref::<std::io::Error>() {
            return if is_disconnect_kind(io.kind()) {
                RelayError::ClientCanceled
            } else {
                RelayError::BodyUnreadable(err.to_string())
            };
        }
        if let Some(hyper) = current.downcast_ref::<hyper::Error>() {
            // Hyper classifies an incomplete message as the peer going away
            // mid-body, which is precisely the cancellation case.
            if hyper.is_incomplete_message() || hyper.is_canceled() || hyper.is_closed() {
                return RelayError::ClientCanceled;
            }
        }
        source = current.source();
    }
    RelayError::BodyUnreadable(err.to_string())
}

/// IO error kinds that mean the peer went away rather than sent bad data.
fn is_disconnect_kind(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::TimedOut
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transport_disconnect_is_a_cancellation_not_a_bad_request() {
        // Classification walks the source chain for a real IO kind, so it does
        // not depend on message wording.
        for kind in [
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::UnexpectedEof,
        ] {
            let err = axum::Error::new(std::io::Error::new(kind, "peer went away"));
            assert!(
                matches!(classify_body_error(err), RelayError::ClientCanceled),
                "{kind:?} should be a cancellation"
            );
        }
    }

    #[test]
    fn a_data_error_remains_a_bad_request() {
        let err = axum::Error::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            // Deliberately contains the word "closed" to prove the classifier
            // is not matching on message text.
            "stream closed unexpectedly while decoding",
        ));
        assert!(matches!(
            classify_body_error(err),
            RelayError::BodyUnreadable(_)
        ));
    }

    #[tokio::test]
    async fn the_cap_is_exact() {
        let at_limit = axum::body::Body::from(vec![b'x'; 16]);
        assert_eq!(read_capped(at_limit, 16).await.unwrap().len(), 16);

        let over = axum::body::Body::from(vec![b'x'; 17]);
        assert!(matches!(
            read_capped(over, 16).await,
            Err(RelayError::BodyTooLarge)
        ));
    }

    #[tokio::test]
    async fn an_empty_body_reads_as_zero_bytes() {
        let body = axum::body::Body::empty();
        assert!(read_capped(body, 16).await.unwrap().is_empty());
    }
}
