//! Bounded WebSocket relay pumps for official and private Realtime routes.
//!
//! Official routes connect upstream before accepting the downstream upgrade and
//! therefore enter the connected loop directly. Private routes preserve the
//! source-proven downstream-first handshake and its bounded pre-open queue.

use std::collections::VecDeque;
use std::future::Future;
use std::time::Duration;

use axum::extract::ws::{Message as AxumMessage, WebSocket};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Error as TungError;
use tokio_tungstenite::tungstenite::Message as TungMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::config::{MAX_WEBSOCKET_FRAME_BYTES, WEBSOCKET_IDLE_TIMEOUT, WEBSOCKET_SEND_TIMEOUT};
use crate::observability::{Direction, FrameLogger};
use crate::relay::ws_convert::{axum_to_tungstenite, close_parts, tungstenite_to_axum};

pub type UpstreamSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub type ConnectResult = Result<UpstreamSocket, String>;

/// Frames buffered while a private upstream handshake is in flight.
pub const MAX_PENDING_FRAMES: usize = 32;
/// Maximum RFC 6455 framing overhead for one masked frame.
pub use crate::config::MAX_WEBSOCKET_FRAME_OVERHEAD;

pub const CLOSE_TOO_MANY_PENDING: &str = "too many pending frames";
pub const CLOSE_QUEUED_FRAMES_TOO_LARGE: &str = "queued frames too large";
pub const CLOSE_FRAME_TOO_LARGE: &str = "frame too large";
pub const CLOSE_UPSTREAM_CONNECT_FAILED: &str = "upstream connect failed";
pub const CLOSE_UPSTREAM_ERROR: &str = "upstream error";
pub const CLOSE_UPSTREAM_SEND_FAILED: &str = "upstream send failed";
pub const CLOSE_CLIENT_SEND_FAILED: &str = "client send failed";
pub const CLOSE_UPSTREAM_SEND_TIMED_OUT: &str = "upstream send timed out";
pub const CLOSE_DOWNSTREAM_SEND_TIMED_OUT: &str = "downstream send timed out";
pub const CLOSE_CLIENT_CLOSED: &str = "client closed";
pub const CLOSE_IDLE_TIMEOUT: &str = "idle timeout";
/// Retained for parity with the source state machine; this pump cannot reach it.
pub const CLOSE_MISSING_UPSTREAM: &str = "missing upstream";

pub const CODE_POLICY: u16 = 1009;
pub const CODE_INTERNAL: u16 = 1011;
pub const CODE_NORMAL: u16 = 1000;
pub const CODE_GOING_AWAY: u16 = 1001;

/// How long the final closing handshake may retain a relay task.
pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosePolicy {
    /// Preserve downstream and upstream close code/reason in the opposite leg.
    Transparent,
    /// Normalize downstream closes to `1000 / client closed` upstream.
    PrivateNormalized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PumpPolicy {
    pub frame_bytes: usize,
    pub send_timeout: Duration,
    pub idle_timeout: Duration,
    pub close_policy: ClosePolicy,
}

impl PumpPolicy {
    pub const fn private_default() -> Self {
        Self {
            frame_bytes: MAX_WEBSOCKET_FRAME_BYTES,
            send_timeout: WEBSOCKET_SEND_TIMEOUT,
            idle_timeout: WEBSOCKET_IDLE_TIMEOUT,
            close_policy: ClosePolicy::PrivateNormalized,
        }
    }
}

impl Default for PumpPolicy {
    fn default() -> Self {
        Self::private_default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PumpOutcome {
    ClientClosed,
    UpstreamClosed { code: u16, reason: String },
    Aborted { code: u16, reason: &'static str },
}

impl PumpOutcome {
    pub fn label(&self) -> &'static str {
        match self {
            Self::ClientClosed => "client_closed",
            Self::UpstreamClosed { .. } => "upstream_closed",
            Self::Aborted { .. } => "aborted",
        }
    }

    pub fn code(&self) -> Option<u16> {
        match self {
            Self::ClientClosed => Some(CODE_NORMAL),
            Self::UpstreamClosed { code, .. } | Self::Aborted { code, .. } => Some(*code),
        }
    }
}

/// Compatibility helper for the source-proven frame-count boundary.
pub fn accept_pending(queue_len: usize) -> bool {
    queue_len < MAX_PENDING_FRAMES
}

fn message_bytes_axum(message: &AxumMessage) -> usize {
    match message {
        AxumMessage::Text(text) => text.len(),
        AxumMessage::Binary(bytes) => bytes.len(),
        AxumMessage::Ping(_) | AxumMessage::Pong(_) | AxumMessage::Close(_) => 0,
    }
}

fn message_bytes_tungstenite(message: &TungMessage) -> usize {
    match message {
        TungMessage::Text(text) => text.len(),
        TungMessage::Binary(bytes) => bytes.len(),
        TungMessage::Ping(_)
        | TungMessage::Pong(_)
        | TungMessage::Close(_)
        | TungMessage::Frame(_) => 0,
    }
}

fn queued_bytes_after(current: usize, next: usize, cap: usize) -> Option<usize> {
    current.checked_add(next).filter(|total| *total <= cap)
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendFailure {
    Failed,
    TimedOut,
}

trait FrameReadFailure {
    fn is_too_large(&self) -> bool;
}

impl FrameReadFailure for () {
    fn is_too_large(&self) -> bool {
        false
    }
}

impl FrameReadFailure for TungError {
    fn is_too_large(&self) -> bool {
        matches!(self, TungError::Capacity(_))
    }
}

impl FrameReadFailure for axum::Error {
    fn is_too_large(&self) -> bool {
        use std::error::Error;
        self.source()
            .and_then(|source| source.downcast_ref::<TungError>())
            .is_some_and(FrameReadFailure::is_too_large)
    }
}

async fn bounded_send<S, Item>(
    sink: &mut S,
    item: Item,
    deadline: Duration,
) -> Result<(), SendFailure>
where
    S: Sink<Item> + Unpin,
{
    match tokio::time::timeout(deadline, sink.send(item)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(SendFailure::Failed),
        Err(_) => Err(SendFailure::TimedOut),
    }
}

fn tungstenite_close(code: u16, reason: &str) -> TungMessage {
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;

    TungMessage::Close(Some(CloseFrame {
        code: CloseCode::from(code),
        reason: reason.to_owned().into(),
    }))
}

fn axum_close(code: u16, reason: &str) -> AxumMessage {
    AxumMessage::Close(Some(axum::extract::ws::CloseFrame {
        code,
        reason: reason.to_owned().into(),
    }))
}

async fn bounded_close<S, Item>(sink: &mut S)
where
    S: Sink<Item> + Unpin,
{
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, sink.close()).await;
}

async fn tell_upstream<U>(upstream: &mut U, code: u16, reason: &str, deadline: Duration)
where
    U: Sink<TungMessage> + Unpin,
{
    let _ = bounded_send(upstream, tungstenite_close(code, reason), deadline).await;
    bounded_close(upstream).await;
}

async fn tell_downstream<D>(downstream: &mut D, code: u16, reason: &str, deadline: Duration)
where
    D: Sink<AxumMessage> + Unpin,
{
    let _ = bounded_send(downstream, axum_close(code, reason), deadline).await;
    bounded_close(downstream).await;
}

async fn abort_both<D, U>(
    downstream: &mut D,
    upstream: &mut U,
    code: u16,
    reason: &'static str,
    deadline: Duration,
) -> PumpOutcome
where
    D: Sink<AxumMessage> + Unpin,
    U: Sink<TungMessage> + Unpin,
{
    // Each operation is independently bounded. A stalled leg cannot prevent
    // the other peer from receiving its terminal frame.
    tokio::join!(
        tell_downstream(downstream, code, reason, deadline),
        tell_upstream(upstream, code, reason, deadline)
    );
    PumpOutcome::Aborted { code, reason }
}

/// Relay an official WebSocket whose upstream handshake already succeeded.
pub async fn run_public_pump(
    downstream: WebSocket,
    upstream: UpstreamSocket,
    mut policy: PumpPolicy,
    logger: FrameLogger,
) -> PumpOutcome {
    policy.close_policy = ClosePolicy::Transparent;
    run_connected(downstream, upstream, policy, logger).await
}

/// Relay a private WebSocket while connecting upstream concurrently.
pub async fn run_private_pump(
    downstream: WebSocket,
    connect: impl Future<Output = ConnectResult>,
    mut policy: PumpPolicy,
    logger: FrameLogger,
) -> PumpOutcome {
    policy.close_policy = ClosePolicy::PrivateNormalized;
    run_private_with(downstream, connect, policy, logger).await
}

/// Backwards-compatible private entry point used by the existing sideband
/// handler until it passes configured policy explicitly.
pub async fn run_pump<C, S>(downstream: WebSocket, connect: C, logger: FrameLogger) -> PumpOutcome
where
    C: Future<Output = Result<S, String>>,
    S: Sink<TungMessage, Error = tokio_tungstenite::tungstenite::Error>
        + Stream<Item = Result<TungMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    run_private_with(downstream, connect, PumpPolicy::private_default(), logger).await
}

async fn run_private_with<D, U, C, DE, UE>(
    mut downstream: D,
    connect: C,
    policy: PumpPolicy,
    logger: FrameLogger,
) -> PumpOutcome
where
    D: Sink<AxumMessage> + Stream<Item = Result<AxumMessage, DE>> + Unpin,
    U: Sink<TungMessage> + Stream<Item = Result<TungMessage, UE>> + Unpin,
    DE: FrameReadFailure,
    UE: FrameReadFailure,
    C: Future<Output = Result<U, String>>,
{
    let mut queue = VecDeque::new();
    let mut queued_bytes = 0usize;
    tokio::pin!(connect);

    let mut upstream = loop {
        tokio::select! {
            connected = &mut connect => match connected {
                Ok(stream) => break stream,
                Err(_) => {
                    tell_downstream(
                        &mut downstream,
                        CODE_INTERNAL,
                        CLOSE_UPSTREAM_CONNECT_FAILED,
                        policy.send_timeout,
                    ).await;
                    return PumpOutcome::Aborted {
                        code: CODE_INTERNAL,
                        reason: CLOSE_UPSTREAM_CONNECT_FAILED,
                    };
                }
            },
            inbound = downstream.next() => match inbound {
                Some(Err(error)) if error.is_too_large() => {
                    tell_downstream(
                        &mut downstream,
                        CODE_POLICY,
                        CLOSE_FRAME_TOO_LARGE,
                        policy.send_timeout,
                    ).await;
                    return PumpOutcome::Aborted {
                        code: CODE_POLICY,
                        reason: CLOSE_FRAME_TOO_LARGE,
                    };
                }
                Some(Ok(AxumMessage::Close(_))) | Some(Err(_)) | None => {
                    return PumpOutcome::ClientClosed;
                }
                Some(Ok(message)) => {
                    let bytes = message_bytes_axum(&message);
                    if bytes > policy.frame_bytes {
                        tell_downstream(
                            &mut downstream,
                            CODE_POLICY,
                            CLOSE_FRAME_TOO_LARGE,
                            policy.send_timeout,
                        ).await;
                        return PumpOutcome::Aborted {
                            code: CODE_POLICY,
                            reason: CLOSE_FRAME_TOO_LARGE,
                        };
                    }
                    let Some(converted) = axum_to_tungstenite(message) else {
                        continue;
                    };
                    if !accept_pending(queue.len()) {
                        tell_downstream(
                            &mut downstream,
                            CODE_POLICY,
                            CLOSE_TOO_MANY_PENDING,
                            policy.send_timeout,
                        ).await;
                        return PumpOutcome::Aborted {
                            code: CODE_POLICY,
                            reason: CLOSE_TOO_MANY_PENDING,
                        };
                    }
                    let Some(total) = queued_bytes_after(queued_bytes, bytes, policy.frame_bytes)
                    else {
                        tell_downstream(
                            &mut downstream,
                            CODE_POLICY,
                            CLOSE_QUEUED_FRAMES_TOO_LARGE,
                            policy.send_timeout,
                        ).await;
                        return PumpOutcome::Aborted {
                            code: CODE_POLICY,
                            reason: CLOSE_QUEUED_FRAMES_TOO_LARGE,
                        };
                    };
                    queued_bytes = total;
                    queue.push_back(converted);
                }
            },
        }
    };

    let idle_deadline = idle_deadline(&policy);
    while let Some(message) = queue.pop_front() {
        log_frame(&logger, Direction::ClientToUpstream, &message);
        let send_result = if let Some(deadline) = idle_deadline {
            tokio::select! {
                result = bounded_send(&mut upstream, message, policy.send_timeout) => Some(result),
                _ = tokio::time::sleep_until(deadline) => None,
            }
        } else {
            Some(bounded_send(&mut upstream, message, policy.send_timeout).await)
        };
        let Some(send_result) = send_result else {
            return abort_both(
                &mut downstream,
                &mut upstream,
                CODE_GOING_AWAY,
                CLOSE_IDLE_TIMEOUT,
                policy.send_timeout,
            )
            .await;
        };
        match send_result {
            Ok(()) => {}
            Err(SendFailure::Failed) => {
                tell_downstream(
                    &mut downstream,
                    CODE_INTERNAL,
                    CLOSE_UPSTREAM_SEND_FAILED,
                    policy.send_timeout,
                )
                .await;
                return PumpOutcome::Aborted {
                    code: CODE_INTERNAL,
                    reason: CLOSE_UPSTREAM_SEND_FAILED,
                };
            }
            Err(SendFailure::TimedOut) => {
                return abort_both(
                    &mut downstream,
                    &mut upstream,
                    CODE_INTERNAL,
                    CLOSE_UPSTREAM_SEND_TIMED_OUT,
                    policy.send_timeout,
                )
                .await;
            }
        }
    }

    run_connected_from_deadline(downstream, upstream, policy, logger, idle_deadline).await
}

/// Production-used connected relay loop. Keeping both transports generic gives
/// unit tests a deterministic pending writer without relying on kernel buffers.
async fn run_connected<D, U, DE, UE>(
    downstream: D,
    upstream: U,
    policy: PumpPolicy,
    logger: FrameLogger,
) -> PumpOutcome
where
    D: Sink<AxumMessage> + Stream<Item = Result<AxumMessage, DE>> + Unpin,
    U: Sink<TungMessage> + Stream<Item = Result<TungMessage, UE>> + Unpin,
    DE: FrameReadFailure,
    UE: FrameReadFailure,
{
    let deadline = idle_deadline(&policy);
    run_connected_from_deadline(downstream, upstream, policy, logger, deadline).await
}

fn idle_deadline(policy: &PumpPolicy) -> Option<tokio::time::Instant> {
    (!policy.idle_timeout.is_zero()).then(|| tokio::time::Instant::now() + policy.idle_timeout)
}

async fn run_connected_from_deadline<D, U, DE, UE>(
    mut downstream: D,
    mut upstream: U,
    policy: PumpPolicy,
    logger: FrameLogger,
    mut idle_deadline: Option<tokio::time::Instant>,
) -> PumpOutcome
where
    D: Sink<AxumMessage> + Stream<Item = Result<AxumMessage, DE>> + Unpin,
    U: Sink<TungMessage> + Stream<Item = Result<TungMessage, UE>> + Unpin,
    DE: FrameReadFailure,
    UE: FrameReadFailure,
{
    loop {
        let deadline_for_wait = idle_deadline;
        let idle = async move {
            match deadline_for_wait {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(idle);
        tokio::select! {
            _ = &mut idle => {
                return abort_both(
                    &mut downstream,
                    &mut upstream,
                    CODE_GOING_AWAY,
                    CLOSE_IDLE_TIMEOUT,
                    policy.send_timeout,
                ).await;
            }
            inbound = downstream.next() => match inbound {
                Some(Ok(AxumMessage::Close(frame))) => {
                    match policy.close_policy {
                        ClosePolicy::Transparent => {
                            let (code, reason) = match frame {
                                Some(frame) => (frame.code, frame.reason.to_string()),
                                None => (CODE_NORMAL, String::new()),
                            };
                            tell_upstream(&mut upstream, code, &reason, policy.send_timeout).await;
                        }
                        ClosePolicy::PrivateNormalized => {
                            tell_upstream(
                                &mut upstream,
                                CODE_NORMAL,
                                CLOSE_CLIENT_CLOSED,
                                policy.send_timeout,
                            ).await;
                        }
                    }
                    return PumpOutcome::ClientClosed;
                }
                Some(Err(error)) if error.is_too_large() => {
                    return abort_both(
                        &mut downstream,
                        &mut upstream,
                        CODE_POLICY,
                        CLOSE_FRAME_TOO_LARGE,
                        policy.send_timeout,
                    ).await;
                }
                Some(Err(_)) | None => {
                    tell_upstream(
                        &mut upstream,
                        CODE_NORMAL,
                        CLOSE_CLIENT_CLOSED,
                        policy.send_timeout,
                    ).await;
                    return PumpOutcome::ClientClosed;
                }
                Some(Ok(message)) => {
                    idle_deadline = idle_deadline.map(|_| {
                        tokio::time::Instant::now() + policy.idle_timeout
                    });
                    let bytes = message_bytes_axum(&message);
                    if bytes > policy.frame_bytes {
                        return abort_both(
                            &mut downstream,
                            &mut upstream,
                            CODE_POLICY,
                            CLOSE_FRAME_TOO_LARGE,
                            policy.send_timeout,
                        ).await;
                    }
                    let Some(converted) = axum_to_tungstenite(message) else {
                        continue;
                    };
                    log_frame(&logger, Direction::ClientToUpstream, &converted);
                    match bounded_send(&mut upstream, converted, policy.send_timeout).await {
                        Ok(()) => {}
                        Err(SendFailure::Failed) => {
                            tell_downstream(
                                &mut downstream,
                                CODE_INTERNAL,
                                CLOSE_UPSTREAM_SEND_FAILED,
                                policy.send_timeout,
                            ).await;
                            return PumpOutcome::Aborted {
                                code: CODE_INTERNAL,
                                reason: CLOSE_UPSTREAM_SEND_FAILED,
                            };
                        }
                        Err(SendFailure::TimedOut) => {
                            return abort_both(
                                &mut downstream,
                                &mut upstream,
                                CODE_INTERNAL,
                                CLOSE_UPSTREAM_SEND_TIMED_OUT,
                                policy.send_timeout,
                            ).await;
                        }
                    }
                }
            },
            outbound = upstream.next() => match outbound {
                Some(Ok(TungMessage::Close(frame))) => {
                    let (code, reason) = close_parts(frame.as_ref());
                    tell_downstream(&mut downstream, code, &reason, policy.send_timeout).await;
                    return PumpOutcome::UpstreamClosed { code, reason };
                }
                Some(Ok(message)) => {
                    idle_deadline = idle_deadline.map(|_| {
                        tokio::time::Instant::now() + policy.idle_timeout
                    });
                    let bytes = message_bytes_tungstenite(&message);
                    if bytes > policy.frame_bytes {
                        return abort_both(
                            &mut downstream,
                            &mut upstream,
                            CODE_POLICY,
                            CLOSE_FRAME_TOO_LARGE,
                            policy.send_timeout,
                        ).await;
                    }
                    let Some(converted) = tungstenite_to_axum(message) else {
                        continue;
                    };
                    log_axum_frame(&logger, Direction::UpstreamToClient, &converted);
                    match bounded_send(&mut downstream, converted, policy.send_timeout).await {
                        Ok(()) => {}
                        Err(SendFailure::Failed) => {
                            tell_upstream(
                                &mut upstream,
                                CODE_INTERNAL,
                                CLOSE_CLIENT_SEND_FAILED,
                                policy.send_timeout,
                            ).await;
                            return PumpOutcome::Aborted {
                                code: CODE_INTERNAL,
                                reason: CLOSE_CLIENT_SEND_FAILED,
                            };
                        }
                        Err(SendFailure::TimedOut) => {
                            return abort_both(
                                &mut downstream,
                                &mut upstream,
                                CODE_INTERNAL,
                                CLOSE_DOWNSTREAM_SEND_TIMED_OUT,
                                policy.send_timeout,
                            ).await;
                        }
                    }
                }
                Some(Err(error)) if error.is_too_large() => {
                    return abort_both(
                        &mut downstream,
                        &mut upstream,
                        CODE_POLICY,
                        CLOSE_FRAME_TOO_LARGE,
                        policy.send_timeout,
                    ).await;
                }
                Some(Err(_)) | None => {
                    tell_downstream(
                        &mut downstream,
                        CODE_INTERNAL,
                        CLOSE_UPSTREAM_ERROR,
                        policy.send_timeout,
                    ).await;
                    return PumpOutcome::Aborted {
                        code: CODE_INTERNAL,
                        reason: CLOSE_UPSTREAM_ERROR,
                    };
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    use super::*;

    struct ScriptedSocket<I, O> {
        inbound: VecDeque<Result<I, ()>>,
        sent: Arc<Mutex<Vec<O>>>,
        pending_write: bool,
        fail_write: bool,
        signal_read: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl<I, O> ScriptedSocket<I, O> {
        fn new(inbound: impl IntoIterator<Item = Result<I, ()>>) -> (Self, Arc<Mutex<Vec<O>>>) {
            let sent = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    inbound: inbound.into_iter().collect(),
                    sent: sent.clone(),
                    pending_write: false,
                    fail_write: false,
                    signal_read: None,
                },
                sent,
            )
        }

        fn pending(inbound: impl IntoIterator<Item = Result<I, ()>>) -> (Self, Arc<Mutex<Vec<O>>>) {
            let (mut socket, sent) = Self::new(inbound);
            socket.pending_write = true;
            (socket, sent)
        }

        fn failed(inbound: impl IntoIterator<Item = Result<I, ()>>) -> (Self, Arc<Mutex<Vec<O>>>) {
            let (mut socket, sent) = Self::new(inbound);
            socket.fail_write = true;
            (socket, sent)
        }

        fn signal_first_read(&mut self, signal: tokio::sync::oneshot::Sender<()>) {
            self.signal_read = Some(signal);
        }
    }

    impl<I: Unpin, O: Unpin> Stream for ScriptedSocket<I, O> {
        type Item = Result<I, ()>;

        fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            if !self.inbound.is_empty() {
                if let Some(signal) = self.signal_read.take() {
                    let _ = signal.send(());
                }
            }
            match self.inbound.pop_front() {
                Some(item) => Poll::Ready(Some(item)),
                None => Poll::Pending,
            }
        }
    }

    impl<I: Unpin, O: Unpin> Sink<O> for ScriptedSocket<I, O> {
        type Error = ();

        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            if self.fail_write {
                Poll::Ready(Err(()))
            } else if self.pending_write {
                Poll::Pending
            } else {
                Poll::Ready(Ok(()))
            }
        }

        fn start_send(self: Pin<&mut Self>, item: O) -> Result<(), Self::Error> {
            self.sent.lock().unwrap().push(item);
            Ok(())
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    fn policy(frame_bytes: usize, close_policy: ClosePolicy) -> PumpPolicy {
        PumpPolicy {
            frame_bytes,
            send_timeout: Duration::from_millis(1),
            idle_timeout: Duration::ZERO,
            close_policy,
        }
    }

    fn tung_close_parts(message: &TungMessage) -> (u16, &str) {
        let TungMessage::Close(Some(frame)) = message else {
            panic!("expected close frame");
        };
        (u16::from(frame.code), frame.reason.as_str())
    }

    #[test]
    fn pending_frame_and_byte_boundaries_are_exact() {
        assert!(accept_pending(MAX_PENDING_FRAMES - 1));
        assert!(!accept_pending(MAX_PENDING_FRAMES));
        assert_eq!(queued_bytes_after(3, 5, 8), Some(8));
        assert_eq!(queued_bytes_after(3, 6, 8), None);
        assert_eq!(queued_bytes_after(usize::MAX, 1, usize::MAX), None);
    }

    #[test]
    fn message_cap_is_exact_for_text_and_binary() {
        let at = AxumMessage::Text("1234".into());
        let over = AxumMessage::Binary(bytes::Bytes::from_static(b"12345"));
        assert_eq!(message_bytes_axum(&at), 4);
        assert_eq!(message_bytes_axum(&over), 5);
        assert!(message_bytes_axum(&at) <= 4);
        assert!(message_bytes_axum(&over) > 4);
    }

    #[tokio::test]
    async fn upstream_send_timeout_uses_fixed_1011_literal() {
        let (downstream, downstream_sent) =
            ScriptedSocket::<AxumMessage, AxumMessage>::new([Ok(AxumMessage::Text("x".into()))]);
        let (upstream, _) = ScriptedSocket::<TungMessage, TungMessage>::pending([]);

        let outcome = run_connected(
            downstream,
            upstream,
            policy(8, ClosePolicy::Transparent),
            FrameLogger::disabled(),
        )
        .await;

        assert_eq!(
            outcome,
            PumpOutcome::Aborted {
                code: 1011,
                reason: CLOSE_UPSTREAM_SEND_TIMED_OUT,
            }
        );
        let sent = downstream_sent.lock().unwrap();
        let AxumMessage::Close(Some(frame)) = &sent[0] else {
            panic!("expected close frame");
        };
        assert_eq!(frame.code, 1011);
        assert_eq!(frame.reason.as_str(), "upstream send timed out");
    }

    #[tokio::test]
    async fn downstream_send_timeout_uses_fixed_1011_literal() {
        let (downstream, _) = ScriptedSocket::<AxumMessage, AxumMessage>::pending([]);
        let (upstream, upstream_sent) =
            ScriptedSocket::<TungMessage, TungMessage>::new([Ok(TungMessage::Text("x".into()))]);

        let outcome = run_connected(
            downstream,
            upstream,
            policy(8, ClosePolicy::Transparent),
            FrameLogger::disabled(),
        )
        .await;

        assert_eq!(
            outcome,
            PumpOutcome::Aborted {
                code: 1011,
                reason: CLOSE_DOWNSTREAM_SEND_TIMED_OUT,
            }
        );
        assert_eq!(
            tung_close_parts(&upstream_sent.lock().unwrap()[0]),
            (1011, "downstream send timed out")
        );
    }

    #[tokio::test]
    async fn idle_timeout_closes_both_legs_with_1001() {
        let (downstream, downstream_sent) = ScriptedSocket::<AxumMessage, AxumMessage>::new([]);
        let (upstream, upstream_sent) = ScriptedSocket::<TungMessage, TungMessage>::new([]);
        let mut idle_policy = policy(8, ClosePolicy::Transparent);
        idle_policy.idle_timeout = Duration::from_millis(5);

        let outcome =
            run_connected(downstream, upstream, idle_policy, FrameLogger::disabled()).await;

        assert_eq!(
            outcome,
            PumpOutcome::Aborted {
                code: CODE_GOING_AWAY,
                reason: CLOSE_IDLE_TIMEOUT,
            }
        );
        let down = downstream_sent.lock().unwrap();
        let AxumMessage::Close(Some(frame)) = &down[0] else {
            panic!("expected downstream close frame");
        };
        assert_eq!((frame.code, frame.reason.as_str()), (1001, "idle timeout"));
        assert_eq!(
            tung_close_parts(&upstream_sent.lock().unwrap()[0]),
            (1001, "idle timeout")
        );
    }

    #[tokio::test]
    async fn private_idle_timeout_starts_when_upstream_connects_before_queue_flush() {
        let (mut downstream, downstream_sent) =
            ScriptedSocket::<AxumMessage, AxumMessage>::new([Ok(AxumMessage::Text(
                "queued".into(),
            ))]);
        let (read_tx, read_rx) = tokio::sync::oneshot::channel();
        downstream.signal_first_read(read_tx);
        let (upstream, _) = ScriptedSocket::<TungMessage, TungMessage>::pending([]);
        let connect = async move {
            let _ = read_rx.await;
            Ok::<_, String>(upstream)
        };
        let mut idle_policy = policy(64, ClosePolicy::PrivateNormalized);
        idle_policy.send_timeout = Duration::from_millis(20);
        idle_policy.idle_timeout = Duration::from_millis(5);

        let outcome =
            run_private_with(downstream, connect, idle_policy, FrameLogger::disabled()).await;

        assert_eq!(
            outcome,
            PumpOutcome::Aborted {
                code: CODE_GOING_AWAY,
                reason: CLOSE_IDLE_TIMEOUT,
            }
        );
        let sent = downstream_sent.lock().unwrap();
        let AxumMessage::Close(Some(frame)) = &sent[0] else {
            panic!("expected downstream idle close");
        };
        assert_eq!((frame.code, frame.reason.as_str()), (1001, "idle timeout"));
    }

    #[tokio::test]
    async fn upstream_read_reset_uses_fixed_upstream_error_outcome() {
        let (downstream, downstream_sent) = ScriptedSocket::<AxumMessage, AxumMessage>::new([]);
        let (upstream, _) = ScriptedSocket::<TungMessage, TungMessage>::new([Err(())]);

        let outcome = run_connected(
            downstream,
            upstream,
            policy(8, ClosePolicy::Transparent),
            FrameLogger::disabled(),
        )
        .await;

        assert_eq!(
            outcome,
            PumpOutcome::Aborted {
                code: CODE_INTERNAL,
                reason: CLOSE_UPSTREAM_ERROR,
            }
        );
        let sent = downstream_sent.lock().unwrap();
        let AxumMessage::Close(Some(frame)) = &sent[0] else {
            panic!("expected downstream close frame");
        };
        assert_eq!(
            (frame.code, frame.reason.as_str()),
            (1011, "upstream error")
        );
    }

    #[tokio::test]
    async fn upstream_send_failure_uses_fixed_outcome() {
        let (downstream, downstream_sent) =
            ScriptedSocket::<AxumMessage, AxumMessage>::new([Ok(AxumMessage::Text("x".into()))]);
        let (upstream, _) = ScriptedSocket::<TungMessage, TungMessage>::failed([]);

        let outcome = run_connected(
            downstream,
            upstream,
            policy(8, ClosePolicy::Transparent),
            FrameLogger::disabled(),
        )
        .await;

        assert_eq!(
            outcome,
            PumpOutcome::Aborted {
                code: CODE_INTERNAL,
                reason: CLOSE_UPSTREAM_SEND_FAILED,
            }
        );
        let sent = downstream_sent.lock().unwrap();
        let AxumMessage::Close(Some(frame)) = &sent[0] else {
            panic!("expected downstream close frame");
        };
        assert_eq!(
            (frame.code, frame.reason.as_str()),
            (1011, "upstream send failed")
        );
    }

    #[tokio::test]
    async fn downstream_send_failure_uses_fixed_outcome() {
        let (downstream, _) = ScriptedSocket::<AxumMessage, AxumMessage>::failed([]);
        let (upstream, upstream_sent) =
            ScriptedSocket::<TungMessage, TungMessage>::new([Ok(TungMessage::Text("x".into()))]);

        let outcome = run_connected(
            downstream,
            upstream,
            policy(8, ClosePolicy::Transparent),
            FrameLogger::disabled(),
        )
        .await;

        assert_eq!(
            outcome,
            PumpOutcome::Aborted {
                code: CODE_INTERNAL,
                reason: CLOSE_CLIENT_SEND_FAILED,
            }
        );
        assert_eq!(
            tung_close_parts(&upstream_sent.lock().unwrap()[0]),
            (1011, "client send failed")
        );
    }

    #[tokio::test]
    async fn transparent_policy_preserves_client_close() {
        let (downstream, _) = ScriptedSocket::<AxumMessage, AxumMessage>::new([Ok(
            AxumMessage::Close(Some(axum::extract::ws::CloseFrame {
                code: 4001,
                reason: "public close".into(),
            })),
        )]);
        let (upstream, upstream_sent) = ScriptedSocket::<TungMessage, TungMessage>::new([]);

        let outcome = run_connected(
            downstream,
            upstream,
            policy(8, ClosePolicy::Transparent),
            FrameLogger::disabled(),
        )
        .await;

        assert_eq!(outcome, PumpOutcome::ClientClosed);
        assert_eq!(
            tung_close_parts(&upstream_sent.lock().unwrap()[0]),
            (4001, "public close")
        );
    }

    #[tokio::test]
    async fn private_policy_normalizes_client_close() {
        let (downstream, _) = ScriptedSocket::<AxumMessage, AxumMessage>::new([Ok(
            AxumMessage::Close(Some(axum::extract::ws::CloseFrame {
                code: 4001,
                reason: "private close".into(),
            })),
        )]);
        let (upstream, upstream_sent) = ScriptedSocket::<TungMessage, TungMessage>::new([]);

        let outcome = run_connected(
            downstream,
            upstream,
            policy(8, ClosePolicy::PrivateNormalized),
            FrameLogger::disabled(),
        )
        .await;

        assert_eq!(outcome, PumpOutcome::ClientClosed);
        assert_eq!(
            tung_close_parts(&upstream_sent.lock().unwrap()[0]),
            (1000, "client closed")
        );
    }

    #[tokio::test]
    async fn cap_plus_one_closes_both_legs_with_1009() {
        let (downstream, downstream_sent) = ScriptedSocket::<AxumMessage, AxumMessage>::new([Ok(
            AxumMessage::Binary(bytes::Bytes::from_static(b"12345")),
        )]);
        let (upstream, upstream_sent) = ScriptedSocket::<TungMessage, TungMessage>::new([]);

        let outcome = run_connected(
            downstream,
            upstream,
            policy(4, ClosePolicy::Transparent),
            FrameLogger::disabled(),
        )
        .await;

        assert_eq!(
            outcome,
            PumpOutcome::Aborted {
                code: 1009,
                reason: CLOSE_FRAME_TOO_LARGE,
            }
        );
        let down = downstream_sent.lock().unwrap();
        let AxumMessage::Close(Some(frame)) = &down[0] else {
            panic!("expected downstream close");
        };
        assert_eq!(
            (frame.code, frame.reason.as_str()),
            (1009, "frame too large")
        );
        assert_eq!(
            tung_close_parts(&upstream_sent.lock().unwrap()[0]),
            (1009, "frame too large")
        );
    }

    #[tokio::test]
    async fn frame_at_cap_is_forwarded_before_private_close_normalization() {
        let (downstream, _) = ScriptedSocket::<AxumMessage, AxumMessage>::new([
            Ok(AxumMessage::Text("1234".into())),
            Ok(AxumMessage::Close(None)),
        ]);
        let (upstream, upstream_sent) = ScriptedSocket::<TungMessage, TungMessage>::new([]);

        let outcome = run_connected(
            downstream,
            upstream,
            policy(4, ClosePolicy::PrivateNormalized),
            FrameLogger::disabled(),
        )
        .await;

        assert_eq!(outcome, PumpOutcome::ClientClosed);
        let sent = upstream_sent.lock().unwrap();
        assert!(matches!(&sent[0], TungMessage::Text(text) if text.as_str() == "1234"));
        assert_eq!(tung_close_parts(&sent[1]), (1000, "client closed"));
    }

    #[tokio::test]
    async fn private_queue_aggregate_cap_plus_one_uses_fixed_1009_literal() {
        let (downstream, downstream_sent) = ScriptedSocket::<AxumMessage, AxumMessage>::new([
            Ok(AxumMessage::Text("123".into())),
            Ok(AxumMessage::Binary(bytes::Bytes::from_static(b"45"))),
        ]);
        let connect =
            std::future::pending::<Result<ScriptedSocket<TungMessage, TungMessage>, String>>();

        let outcome = run_private_with(
            downstream,
            connect,
            policy(4, ClosePolicy::PrivateNormalized),
            FrameLogger::disabled(),
        )
        .await;

        assert_eq!(
            outcome,
            PumpOutcome::Aborted {
                code: 1009,
                reason: CLOSE_QUEUED_FRAMES_TOO_LARGE,
            }
        );
        let sent = downstream_sent.lock().unwrap();
        let AxumMessage::Close(Some(frame)) = &sent[0] else {
            panic!("expected close frame");
        };
        assert_eq!(frame.code, 1009);
        assert_eq!(frame.reason.as_str(), "queued frames too large");
    }

    #[test]
    fn constants_match_the_wire_contract() {
        assert_eq!(MAX_PENDING_FRAMES, 32);
        assert_eq!(MAX_WEBSOCKET_FRAME_OVERHEAD, 14);
        assert_eq!(CLOSE_FRAME_TOO_LARGE, "frame too large");
        assert_eq!(CLOSE_QUEUED_FRAMES_TOO_LARGE, "queued frames too large");
        assert_eq!(CLOSE_UPSTREAM_SEND_TIMED_OUT, "upstream send timed out");
        assert_eq!(CLOSE_DOWNSTREAM_SEND_TIMED_OUT, "downstream send timed out");
        assert_eq!(CODE_POLICY, 1009);
        assert_eq!(CODE_INTERNAL, 1011);
        assert_eq!(CODE_GOING_AWAY, 1001);
        assert_eq!(CLOSE_IDLE_TIMEOUT, "idle timeout");
    }
}
