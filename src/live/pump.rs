//! The sideband relay pump.
//!
//! The obvious shape — await the upstream handshake, then start reading the
//! downstream socket — cannot implement a pre-open frame queue at all, because
//! no downstream frame can arrive before the handshake resolves. The queue
//! window is exactly the interval between accepting the downstream upgrade and
//! completing the upstream handshake, so both are polled concurrently.

use std::collections::VecDeque;

use axum::extract::ws::{Message as AxumMessage, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as TungMessage;

use crate::live::ws_convert::{axum_to_tungstenite, close_parts, tungstenite_to_axum};
use crate::observability::{Direction, FrameLogger};

/// Record a frame immediately before it is forwarded.
///
/// Before, not after: a record therefore proves the relay received the frame
/// and attempted to forward it, which is what attribution needs. It does not
/// claim the peer received it.
fn log_frame(logger: &FrameLogger, dir: Direction, message: &TungMessage) {
    match message {
        TungMessage::Text(text) => logger.log_text(dir, text.as_str()),
        TungMessage::Binary(bytes) => logger.log_binary(dir, bytes),
        _ => {}
    }
}

fn log_axum_frame(logger: &FrameLogger, dir: Direction, message: &AxumMessage) {
    match message {
        AxumMessage::Text(text) => logger.log_text(dir, text.as_str()),
        AxumMessage::Binary(bytes) => logger.log_binary(dir, bytes),
        _ => {}
    }
}

/// Frames buffered while the upstream handshake is still in flight.
///
/// A count, not a byte budget, exactly as the source bounds it.
pub const MAX_PENDING_FRAMES: usize = 32;

/// Close reasons, kept as constants so tests assert the wire text rather than a
/// paraphrase.
pub const CLOSE_TOO_MANY_PENDING: &str = "too many pending frames";
pub const CLOSE_UPSTREAM_CONNECT_FAILED: &str = "upstream connect failed";
pub const CLOSE_UPSTREAM_ERROR: &str = "upstream error";
pub const CLOSE_UPSTREAM_SEND_FAILED: &str = "upstream send failed";
pub const CLOSE_CLIENT_SEND_FAILED: &str = "client send failed";
pub const CLOSE_CLIENT_CLOSED: &str = "client closed";
/// Two source conditions have no counterpart here, and the divergence is stated
/// rather than implied: `missing upstream` and `upstream not open` describe a
/// nullable socket and a not-yet-open socket that the caller might still write
/// to. This state machine cannot reach either — the upstream is a value that
/// only exists after a successful handshake, and the queue covers the window
/// before it. The constant is retained so a future refactor that reintroduces a
/// nullable upstream has the exact wire text available.
pub const CLOSE_MISSING_UPSTREAM: &str = "missing upstream";

pub const CODE_POLICY: u16 = 1009;
pub const CODE_INTERNAL: u16 = 1011;
pub const CODE_NORMAL: u16 = 1000;

/// What ended the relay, so the caller can log it without re-deriving it.
impl PumpOutcome {
    /// A log-safe label. The upstream's close reason is peer-controlled and can
    /// carry transcript text or a token, so it never reaches a log line: only
    /// the kind and the numeric code do.
    pub fn label(&self) -> &'static str {
        match self {
            Self::ClientClosed => "client_closed",
            Self::UpstreamClosed { .. } => "upstream_closed",
            Self::Aborted { .. } => "aborted",
        }
    }

    /// The close code, which is a small integer and safe to log.
    pub fn code(&self) -> Option<u16> {
        match self {
            Self::ClientClosed => Some(CODE_NORMAL),
            Self::UpstreamClosed { code, .. } => Some(*code),
            Self::Aborted { code, .. } => Some(*code),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PumpOutcome {
    /// The downstream peer closed; the upstream was closed with 1000.
    ClientClosed,
    /// The upstream closed; the code and reason were propagated downstream.
    UpstreamClosed { code: u16, reason: String },
    /// Both sides were closed with this code and reason.
    Aborted { code: u16, reason: &'static str },
}

/// Decide whether a queued frame fits.
///
/// Extracted so the boundary is unit-testable without a live socket: the
/// interesting case is the 33rd frame, which no integration test can schedule
/// deterministically.
pub fn accept_pending(queue_len: usize) -> bool {
    queue_len < MAX_PENDING_FRAMES
}

/// Run the relay until either side finishes.
///
/// `connect` is the in-flight upstream handshake. It is polled alongside the
/// downstream stream so frames arriving during the handshake are queued rather
/// than lost, and so the 33rd such frame can be refused.
pub async fn run_pump<C, S>(
    mut downstream: WebSocket,
    connect: C,
    logger: FrameLogger,
) -> PumpOutcome
where
    C: std::future::Future<Output = Result<S, String>>,
    S: futures_util::Sink<TungMessage, Error = tokio_tungstenite::tungstenite::Error>
        + futures_util::Stream<Item = Result<TungMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let mut queue: VecDeque<TungMessage> = VecDeque::new();
    tokio::pin!(connect);

    // Phase 1: the handshake window. This is the only place the queue grows.
    let mut upstream = loop {
        tokio::select! {
            connected = &mut connect => match connected {
                Ok(stream) => break stream,
                Err(_) => {
                    close_downstream(downstream, CODE_INTERNAL, CLOSE_UPSTREAM_CONNECT_FAILED).await;
                    return PumpOutcome::Aborted {
                        code: CODE_INTERNAL,
                        reason: CLOSE_UPSTREAM_CONNECT_FAILED,
                    };
                }
            },
            inbound = downstream.next() => match inbound {
                Some(Ok(message)) => {
                    if matches!(message, AxumMessage::Close(_)) {
                        // The client left before the upstream opened; there is
                        // nothing to relay the close to.
                        return PumpOutcome::ClientClosed;
                    }
                    let Some(converted) = axum_to_tungstenite(message) else {
                        // A keepalive: answered by the library on this leg and
                        // deliberately not counted against the queue.
                        continue;
                    };
                    if !accept_pending(queue.len()) {
                        close_downstream(downstream, CODE_POLICY, CLOSE_TOO_MANY_PENDING).await;
                        return PumpOutcome::Aborted {
                            code: CODE_POLICY,
                            reason: CLOSE_TOO_MANY_PENDING,
                        };
                    }
                    queue.push_back(converted);
                }
                Some(Err(_)) | None => return PumpOutcome::ClientClosed,
            },
        }
    };

    // Flush in arrival order: the queue exists to preserve ordering, not merely
    // to avoid dropping frames.
    while let Some(message) = queue.pop_front() {
        log_frame(&logger, Direction::ClientToUpstream, &message);
        if upstream.send(message).await.is_err() {
            close_downstream(downstream, CODE_INTERNAL, CLOSE_UPSTREAM_SEND_FAILED).await;
            return PumpOutcome::Aborted {
                code: CODE_INTERNAL,
                reason: CLOSE_UPSTREAM_SEND_FAILED,
            };
        }
    }

    // Phase 2: steady state. No accounting here — the source has no post-open
    // backpressure, and adding some would change observable behavior.
    loop {
        tokio::select! {
            inbound = downstream.next() => match inbound {
                Some(Ok(AxumMessage::Close(_))) | Some(Err(_)) | None => {
                    // Asymmetric by contract: whatever the client said, the
                    // upstream is told 1000 / "client closed".
                    close_upstream(&mut upstream, CODE_NORMAL, CLOSE_CLIENT_CLOSED).await;
                    return PumpOutcome::ClientClosed;
                }
                Some(Ok(message)) => {
                    let Some(converted) = axum_to_tungstenite(message) else {
                        continue;
                    };
                    log_frame(&logger, Direction::ClientToUpstream, &converted);
                    if upstream.send(converted).await.is_err() {
                        close_downstream(downstream, CODE_INTERNAL, CLOSE_UPSTREAM_SEND_FAILED).await;
                        return PumpOutcome::Aborted {
                            code: CODE_INTERNAL,
                            reason: CLOSE_UPSTREAM_SEND_FAILED,
                        };
                    }
                }
            },
            outbound = upstream.next() => match outbound {
                Some(Ok(TungMessage::Close(frame))) => {
                    let (code, reason) = close_parts(frame.as_ref());
                    // Bounded like every other teardown: a backpressured or
                    // half-open downstream must not strand the pump just because
                    // this is the *successful* close path rather than an error one.
                    send_close_downstream(&mut downstream, code, &reason).await;
                    return PumpOutcome::UpstreamClosed { code, reason };
                }
                Some(Ok(message)) => {
                    let Some(converted) = tungstenite_to_axum(message) else {
                        continue;
                    };
                    log_axum_frame(&logger, Direction::UpstreamToClient, &converted);
                    if downstream.send(converted).await.is_err() {
                        // Close the upstream explicitly rather than dropping it:
                        // the outcome claims both sides were closed, so both
                        // sides must actually be told.
                        close_upstream(&mut upstream, CODE_INTERNAL, CLOSE_CLIENT_SEND_FAILED)
                            .await;
                        return PumpOutcome::Aborted {
                            code: CODE_INTERNAL,
                            reason: CLOSE_CLIENT_SEND_FAILED,
                        };
                    }
                }
                Some(Err(_)) => {
                    close_downstream(downstream, CODE_INTERNAL, CLOSE_UPSTREAM_ERROR).await;
                    return PumpOutcome::Aborted {
                        code: CODE_INTERNAL,
                        reason: CLOSE_UPSTREAM_ERROR,
                    };
                }
                None => {
                    // The upstream vanished without a close frame.
                    close_downstream(downstream, CODE_INTERNAL, CLOSE_UPSTREAM_ERROR).await;
                    return PumpOutcome::Aborted {
                        code: CODE_INTERNAL,
                        reason: CLOSE_UPSTREAM_ERROR,
                    };
                }
            },
        }
    }
}

/// How long teardown may take before the socket is simply dropped.
///
/// Both `send` and `close` await sink flushing with no bound of their own, so a
/// half-open peer that never drains would keep this task alive forever.
pub const CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Send a close frame upstream and complete the closing handshake.
///
/// Sending alone is not enough: returning immediately drops the socket, and the
/// peer then observes a TCP reset instead of the close it was told about. The
/// `close()` call flushes and waits for the handshake to finish — under a
/// timeout, because an unresponsive peer must not strand the task.
async fn close_upstream<S>(upstream: &mut S, code: u16, reason: &'static str)
where
    S: futures_util::Sink<TungMessage, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;

    let teardown = async {
        let _ = upstream
            .send(TungMessage::Close(Some(CloseFrame {
                code: CloseCode::from(code),
                reason: reason.into(),
            })))
            .await;
        let _ = upstream.close().await;
    };
    // On timeout the socket is dropped, which is worse for the peer than a
    // clean close but better than leaking the task.
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, teardown).await;
}

/// Send a close frame downstream under the teardown timeout.
///
/// Takes the reason by reference so the upstream's own (dynamic) reason can use
/// the same bounded path as the static error reasons.
async fn send_close_downstream(downstream: &mut WebSocket, code: u16, reason: &str) {
    let send = downstream.send(AxumMessage::Close(Some(axum::extract::ws::CloseFrame {
        code,
        reason: reason.into(),
    })));
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, send).await;
}

async fn close_downstream(mut downstream: WebSocket, code: u16, reason: &'static str) {
    send_close_downstream(&mut downstream, code, reason).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pending_boundary_is_exact() {
        assert!(accept_pending(0));
        assert!(accept_pending(MAX_PENDING_FRAMES - 1));
        // The 33rd frame arrives when 32 are already queued.
        assert!(!accept_pending(MAX_PENDING_FRAMES));
        assert!(!accept_pending(MAX_PENDING_FRAMES + 1));
    }

    #[test]
    fn the_bound_is_a_frame_count_not_a_byte_budget() {
        assert_eq!(MAX_PENDING_FRAMES, 32);
    }

    #[test]
    fn close_reasons_match_the_wire_text() {
        assert_eq!(CLOSE_TOO_MANY_PENDING, "too many pending frames");
        assert_eq!(CLOSE_UPSTREAM_CONNECT_FAILED, "upstream connect failed");
        assert_eq!(CLOSE_UPSTREAM_ERROR, "upstream error");
        assert_eq!(CLOSE_UPSTREAM_SEND_FAILED, "upstream send failed");
        assert_eq!(CLOSE_CLIENT_SEND_FAILED, "client send failed");
        assert_eq!(CLOSE_CLIENT_CLOSED, "client closed");
        assert_eq!(CLOSE_MISSING_UPSTREAM, "missing upstream");
    }

    #[test]
    fn close_codes_match_the_contract() {
        assert_eq!(CODE_POLICY, 1009);
        assert_eq!(CODE_INTERNAL, 1011);
        assert_eq!(CODE_NORMAL, 1000);
    }
}
